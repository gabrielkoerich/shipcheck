use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use crate::check;
use crate::github::Checks;
use crate::run::{self, Entry, Plan};

/// Seconds between posts while GitHub does not have the pushed commit yet, about two minutes in all
pub const WAITS: [u64; 7] = [2, 4, 8, 16, 30, 30, 30];

/// The record of one branch's or PR's background runs, as lines of `<process group> <sha> [post]`
pub fn record(repo_id: &str, name: &str) -> PathBuf {
    run::home()
        .join("background")
        .join(repo_id)
        .join(format!("{}.pid", name.replace('/', "%2F")))
}

/// Whether a push's run posts its checks: main has a `shipcheck.toml`, the results are in the
/// default home where no agent wrote them, and this machine can post
pub fn posts(defaults: bool, home_overridden: bool, can_post: bool) -> bool {
    !defaults && !home_overridden && can_post
}

/// Whether a background run may post from this home. A moved home may hold an agent's results,
/// so only a person's `--home-ok` lets it post, as for `post` and the daemon
pub fn home_allows_post(home_overridden: bool, home_ok: bool) -> bool {
    !home_overridden || home_ok
}

/// Appends a line to the log the background runs write to
pub fn note(line: &str) {
    let dir = run::home().join("logs");
    let _ = fs::create_dir_all(&dir);

    if let Ok(mut log) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("background.log"))
    {
        let _ = writeln!(log, "{line}");
    }
}

/**
 * Starts a background run of the plan's commit under `name`, a branch or `pr:<n>`. With `post` it
 * posts the checks, so it starts even when the tree passed. One commit per name, like
 * `cancel-in-progress` in Actions: a newer commit stops the older runs' process groups first. A run
 * of the same commit is left alone, unless it does not post and this one does, so a push right
 * after a commit adds a run that waits for the commit's run and posts its result
 **/
/// `home_ok` passes a person's `--home-ok` on, from a daemon started with it
pub fn start(
    cwd: &Path,
    plan: &Plan,
    base: &str,
    name: &str,
    post: bool,
    trigger: &str,
    home_ok: bool,
) {
    if !post && run::lookup(plan).is_some_and(|e| e.passed) {
        return;
    }

    let Ok(repo) = run::repo_id(&plan.repo) else {
        return;
    };
    let path = record(&repo, name);
    let mut keep = Vec::new();

    for line in fs::read_to_string(&path).unwrap_or_default().lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(sha)) = (fields.next(), fields.next()) else {
            continue;
        };

        if sha == plan.sha && our_run(pid) {
            if !post || fields.next() == Some("post") {
                return;
            }

            keep.push(line.to_string());
            continue;
        }

        // Only a group a background run of ours still leads, never a number that may now be another's
        if our_run(pid) {
            let _ = Command::new("kill")
                .args(["-s", "TERM", "--", &format!("-{pid}")])
                .status();
        }
    }

    if let Ok(pid) = spawn(cwd, plan, base, name, post, trigger, home_ok) {
        keep.push(format!(
            "{pid} {}{}",
            plan.sha,
            if post { " post" } else { "" }
        ));
        let _ = fs::create_dir_all(path.parent().unwrap_or(&path));
        let _ = fs::write(&path, keep.join("\n") + "\n");
    }
}

/// Fetches the branch of `base`, such as `origin/main`, from origin
pub fn fetch_base(cwd: &Path, base: &str) -> bool {
    let Some(branch) = base.strip_prefix("origin/") else {
        return false;
    };

    Command::new("git")
        .args(["fetch", "-q", "origin", branch])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether a background run of `sha` goes on this machine, under any branch or PR
/// `posting` counts only the runs that post their own checks
pub fn running_here(repo_id: &str, sha: &str, posting: bool) -> bool {
    let dir = run::home().join("background").join(repo_id);

    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .any(|text| {
            text.lines().any(|line| {
                let mut fields = line.split_whitespace();
                matches!((fields.next(), fields.next()), (Some(pid), Some(s))
                    if s == sha && (!posting || fields.next() == Some("post")) && our_run(pid))
            })
        })
}

/// Whether a record still lists process `pid`, which a newer commit's run removes
pub fn listed(record: &str, pid: u32) -> bool {
    let pid = pid.to_string();
    record
        .lines()
        .any(|line| line.split_whitespace().next() == Some(pid.as_str()))
}

/// Whether this run is still the current one for its record. The hook writes the record right
/// after it starts the run, so a run that finds itself missing looks again for a second
pub fn current(record: &Path, pid: u32) -> bool {
    for _ in 0..10 {
        if listed(&fs::read_to_string(record).unwrap_or_default(), pid) {
            return true;
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    false
}

/// Whether `pid` is a running shipcheck process that leads its own process group, as a background
/// run started by `spawn` does
fn our_run(pid: &str) -> bool {
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) || pid == "0" || pid == "1" {
        return false;
    }

    let Ok(out) = Command::new("ps")
        .args(["-o", "pgid=", "-o", "comm=", "-p", pid])
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.split_whitespace();

    matches!(
        (fields.next(), fields.next()),
        (Some(pgid), Some(comm)) if pgid == pid && comm.rsplit('/').next() == Some("shipcheck")
    )
}

fn spawn(
    cwd: &Path,
    plan: &Plan,
    base: &str,
    name: &str,
    post: bool,
    trigger: &str,
    home_ok: bool,
) -> Result<u32, String> {
    use std::os::unix::process::CommandExt;

    let log_dir = run::home().join("logs");
    fs::create_dir_all(&log_dir).map_err(|e| e.to_string())?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("background.log"))
        .map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let args = run_args(&plan.sha, base, name, post, home_ok);

    // Its own process group, so Ctrl-C in the terminal after a commit does not stop the run.
    // git points hooks at the commit's index, which would break every git call the run makes
    let child = Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_PREFIX")
        .env("SHIPCHECK_TRIGGER", trigger)
        .env("SHIPCHECK_BRANCH", name)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(child.id())
}

/// What a background run passes to its own binary, which the binary's tests parse
pub fn run_args(sha: &str, base: &str, name: &str, post: bool, home_ok: bool) -> Vec<String> {
    let mut args: Vec<String> = ["run", "--no-fetch", "--sha", sha, "--base", base]
        .map(String::from)
        .into();

    if post {
        args.extend(["--background".to_string(), name.to_string()]);
    }

    if home_ok {
        args.push("--home-ok".into());
    }

    args
}

/// GitHub answers 422 to a check run on a commit it does not have yet
fn unknown_sha(e: &str) -> bool {
    e.contains("422")
}

/// Calls `f` again after each wait while GitHub does not know the commit
pub fn retry<T>(waits: &[u64], mut f: impl FnMut() -> Result<T, String>) -> Result<T, String> {
    let mut waits = waits.iter();

    loop {
        match f() {
            Err(e) if unknown_sha(&e) => match waits.next() {
                Some(s) => std::thread::sleep(Duration::from_secs(*s)),
                None => return Err(e),
            },
            other => return other,
        }
    }
}

/// Creates a check run, or with an id updates it, and returns its id and page
pub type Send<'a> = dyn Fn(&Value, Option<u64>) -> Result<(u64, String), String> + Sync + 'a;

/// Reads shipcheck's latest check run per name on the commit
pub type Find<'a> = dyn Fn() -> Result<Checks, String> + Sync + 'a;

/// Shows each named check without a result: in_progress while a run goes, queued while none does.
/// Returns the id of every named check
pub fn mark(
    send: &Send,
    existing: &Checks,
    sha: &str,
    names: &[String],
    running: bool,
) -> Result<BTreeMap<String, u64>, String> {
    let wanted = if running { "in_progress" } else { "queued" };
    let mut ids = BTreeMap::new();

    for name in names {
        // GitHub keeps a completed run's conclusion but takes a new title, so a late mark would say running
        let id = match existing.get(name) {
            Some(c) if c.status == wanted || c.status == "completed" => c.id,
            Some(c) => send(&check::waiting(name, sha, running), Some(c.id))?.0,
            None => send(&check::waiting(name, sha, running), None)?.0,
        };
        ids.insert(name.clone(), id);
    }

    Ok(ids)
}

/// Posts the results, updating the runs in `ids` of the same name, and returns the last page
pub fn finish(
    send: &Send,
    bodies: &[Value],
    ids: &BTreeMap<String, u64>,
) -> Result<String, String> {
    let mut url = String::new();

    for body in bodies {
        let id = body["name"].as_str().and_then(|n| ids.get(n)).copied();
        url = send(body, id)?.1;
    }

    Ok(url)
}

/**
 * A run that posts its checks, as a push's or the daemon's background run does. Without a cached
 * result, it marks every check in progress as soon as GitHub has the commit, while `run` goes. Then
 * it completes them with `finals`, unless `current` says a newer commit replaced this one, and
 * returns None. A failed mark is only a line, the result still posts
 **/
#[allow(clippy::too_many_arguments)]
pub fn post_run(
    send: &Send,
    find: &Find,
    sha: &str,
    names: &[String],
    cached: Option<Entry>,
    run: impl FnOnce() -> Result<Entry, String>,
    finals: impl FnOnce(&Entry) -> Vec<Value>,
    current: impl FnOnce() -> bool,
    waits: &[u64],
) -> Result<Option<Entry>, String> {
    // A service without the read endpoint, or a list GitHub refuses, means creating new runs
    let found = || match find() {
        Err(e) if !unknown_sha(&e) => Ok(Checks::new()),
        other => other,
    };
    // This run posts the new result, so a check completed by an earlier run gets a new one in progress
    let open = || -> Result<Checks, String> {
        let mut checks = found()?;
        checks.retain(|_, c| c.status != "completed");
        Ok(checks)
    };
    let marked = |running: bool| {
        retry(waits, || mark(send, &open()?, sha, names, running)).unwrap_or_else(|e| {
            eprintln!(
                "{}: could not mark the checks: {e}",
                &sha[..sha.len().min(12)]
            );
            BTreeMap::new()
        })
    };
    let (entry, ids) = match cached {
        Some(entry) => {
            let ids = retry(waits, found)
                .map(|found| found.into_iter().map(|(n, c)| (n, c.id)).collect())
                .unwrap_or_default();
            (entry, ids)
        }
        None => std::thread::scope(|s| {
            let marks = s.spawn(|| marked(true));
            let entry = run();
            let ids = marks.join().unwrap_or_default();
            entry.map(|e| (e, ids))
        })?,
    };

    if !current() {
        return Ok(None);
    }

    let bodies = finals(&entry);
    retry(waits, || finish(send, &bodies, &ids))?;
    Ok(Some(entry))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::github::Check;

    fn entry() -> Entry {
        serde_json::from_value(json!({
            "key": "k", "sha": "a".repeat(40), "tree": "t", "base": "origin/main",
            "passed": true, "workflows": [], "finished_unix": 0
        }))
        .unwrap()
    }

    fn finals(_: &Entry) -> Vec<Value> {
        vec![
            json!({"name": "shipcheck/test", "status": "completed"}),
            json!({"name": "shipcheck", "status": "completed"}),
        ]
    }

    fn names() -> Vec<String> {
        vec!["shipcheck/test".into(), "shipcheck".into()]
    }

    /// A fake GitHub that answers 422 to the first `unknown` posts, and records every post
    struct Fake {
        unknown: Mutex<usize>,
        posts: Mutex<Vec<(String, String, Option<u64>)>>,
    }

    impl Fake {
        fn new(unknown: usize) -> Self {
            Fake {
                unknown: Mutex::new(unknown),
                posts: Mutex::new(Vec::new()),
            }
        }

        fn send(&self, body: &Value, id: Option<u64>) -> Result<(u64, String), String> {
            let mut unknown = self.unknown.lock().unwrap();

            if *unknown > 0 {
                *unknown -= 1;
                return Err(
                    "POST /v1/checks: GitHub refused to create the check run: 422 No commit found"
                        .into(),
                );
            }

            let mut posts = self.posts.lock().unwrap();
            posts.push((
                body["name"].as_str().unwrap().into(),
                body["status"].as_str().unwrap().into(),
                id,
            ));
            Ok((id.unwrap_or(100 + posts.len() as u64), String::new()))
        }

        fn posts(&self) -> Vec<(String, String, Option<u64>)> {
            self.posts.lock().unwrap().clone()
        }
    }

    #[test]
    fn retries_only_github_saying_it_has_no_such_commit() {
        // As the two posters word it: ureq with the App key, and the hosted service
        for e in [
            "POST https://api.github.com/repos/acme/app/check-runs: http status: 422",
            "POST /v1/checks: GitHub refused to create the check run: 422 No commit found for SHA: abc",
        ] {
            assert!(unknown_sha(e), "{e}");
        }

        // A 422 in a URL is no 422 answer, a SHA or a repository name can hold one
        for e in [
            "GET https://api.github.com/repos/acme/app/commits/4221aa0000000000000000000000000000000000/check-runs?app_id=1: http status: 404",
            "POST https://api.github.com/repos/acme/app422/check-runs: http status: 401",
            "POST /v1/checks: the shipcheck App is not installed on acme/app422",
        ] {
            assert!(!unknown_sha(e), "{e}");
        }
    }

    #[test]
    fn marks_then_completes_the_same_runs() {
        let fake = Fake::new(2);
        let got = post_run(
            &|b, id| fake.send(b, id),
            &|| Ok(Checks::new()),
            &"a".repeat(40),
            &names(),
            None,
            || Ok(entry()),
            finals,
            || true,
            &[0, 0, 0],
        )
        .unwrap();
        assert!(got.is_some());
        assert_eq!(
            fake.posts(),
            vec![
                ("shipcheck/test".into(), "in_progress".into(), None),
                ("shipcheck".into(), "in_progress".into(), None),
                ("shipcheck/test".into(), "completed".into(), Some(101)),
                ("shipcheck".into(), "completed".into(), Some(102)),
            ],
            "the commit is unknown twice, then each check is created once and completed in place"
        );
    }

    #[test]
    fn mark_leaves_completed_checks() {
        let fake = Fake::new(0);
        let check = |id, status: &str| Check {
            id,
            status: status.into(),
            started_at: String::new(),
        };
        let existing = Checks::from([
            ("shipcheck/test".into(), check(7, "completed")),
            ("shipcheck".into(), check(8, "queued")),
        ]);
        let ids = mark(&|b, id| fake.send(b, id), &existing, "s", &names(), true).unwrap();
        assert_eq!(ids["shipcheck/test"], 7);
        assert_eq!(
            fake.posts(),
            vec![("shipcheck".into(), "in_progress".into(), Some(8))],
            "a daemon poll during the final posts must not turn a result back into running"
        );
    }

    #[test]
    fn a_rerun_shows_its_completed_checks_in_progress_again() {
        let fake = Fake::new(0);
        let check = |id, status: &str| Check {
            id,
            status: status.into(),
            started_at: String::new(),
        };
        let found = Checks::from([
            ("shipcheck/test".into(), check(7, "completed")),
            ("shipcheck".into(), check(8, "in_progress")),
        ]);
        post_run(
            &|b, id| fake.send(b, id),
            &|| Ok(found.clone()),
            &"a".repeat(40),
            &names(),
            None,
            || Ok(entry()),
            finals,
            || true,
            &[],
        )
        .unwrap();
        assert_eq!(
            fake.posts(),
            vec![
                ("shipcheck/test".into(), "in_progress".into(), None),
                ("shipcheck/test".into(), "completed".into(), Some(101)),
                ("shipcheck".into(), "completed".into(), Some(8)),
            ],
            "the old failed run stays as it was, a new run shows the rerun and gets its result"
        );
    }

    #[test]
    fn marks_without_the_read_endpoint() {
        let fake = Fake::new(0);
        post_run(
            &|b, id| fake.send(b, id),
            &|| Err("GET /v1/checks: no route GET /v1/checks".into()),
            &"a".repeat(40),
            &names(),
            None,
            || Ok(entry()),
            finals,
            || true,
            &[],
        )
        .unwrap();
        assert_eq!(
            fake.posts().len(),
            4,
            "in progress first, then completed in place"
        );
        assert_eq!(fake.posts()[3].2, Some(102));
    }

    #[test]
    fn cached_result_posts_at_once() {
        let fake = Fake::new(0);
        let mut existing = Checks::new();
        existing.insert(
            "shipcheck".into(),
            Check {
                id: 7,
                status: "queued".into(),
                started_at: String::new(),
            },
        );
        post_run(
            &|b, id| fake.send(b, id),
            &|| Ok(existing.clone()),
            &"a".repeat(40),
            &names(),
            Some(entry()),
            || panic!("a cached result must not run"),
            finals,
            || true,
            &[],
        )
        .unwrap();
        assert_eq!(
            fake.posts(),
            vec![
                ("shipcheck/test".into(), "completed".into(), None),
                ("shipcheck".into(), "completed".into(), Some(7)),
            ]
        );
    }

    #[test]
    fn replaced_run_posts_no_result() {
        let fake = Fake::new(0);
        let got = post_run(
            &|b, id| fake.send(b, id),
            &|| Ok(Checks::new()),
            &"a".repeat(40),
            &names(),
            None,
            || Ok(entry()),
            finals,
            || false,
            &[],
        )
        .unwrap();
        assert!(got.is_none());
        assert!(
            fake.posts()
                .iter()
                .all(|(_, status, _)| status == "in_progress"),
            "a run a newer commit replaced must not complete any check"
        );
    }

    #[test]
    fn gives_up_after_the_waits() {
        let fake = Fake::new(10);
        let got = retry(&[0, 0], || {
            fake.send(&json!({"name": "shipcheck", "status": "queued"}), None)
        });
        assert!(got.unwrap_err().contains("422"));
        assert_eq!(*fake.unknown.lock().unwrap(), 7, "one try and one per wait");
        assert!(retry(&[0], || Err::<(), _>("HTTP 500".into())).is_err());
    }

    #[test]
    fn posts_only_when_signed_in() {
        assert!(posts(false, false, true));
        assert!(!posts(false, false, false), "not signed in posts nothing");
        assert!(
            !posts(true, false, true),
            "no shipcheck.toml on main is no check"
        );
        assert!(
            !posts(false, true, true),
            "a moved home may hold an agent's results"
        );
    }

    #[test]
    fn a_moved_home_posts_only_with_home_ok() {
        assert!(home_allows_post(false, false));
        assert!(
            !home_allows_post(true, false),
            "a moved home may hold an agent's results"
        );
        assert!(home_allows_post(true, true));
    }

    #[test]
    fn record_lists_the_current_run() {
        assert!(listed("41 abc post\n42 def\n", 42));
        assert!(!listed("41 abc post\n", 4));
        assert!(!listed("", 4));
    }
}
