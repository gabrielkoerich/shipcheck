use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

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

/// Whether a push's run posts its checks: the results are in the default home where no agent
/// wrote them, and this machine can post
pub fn posts(home_overridden: bool, can_post: bool) -> bool {
    !home_overridden && can_post
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

/// Whether the App cannot reach the repository: the service's 403 and the App key's 404 say it is
/// not installed, a bare 403 or 404 means the same
pub fn not_installed(e: &str) -> bool {
    e.contains("not installed")
        || [
            "HTTP 403",
            "HTTP 404",
            "http status: 403",
            "http status: 404",
        ]
        .iter()
        .any(|s| e.ends_with(s))
}

/// Why a post to `repo` failed, and what fixes it
pub fn reason_and_fix(repo: &str, e: &str) -> (String, String) {
    if not_installed(e) {
        return (
            format!("the shipcheck App is not installed on {repo}"),
            install_fix(repo),
        );
    }

    (e.to_string(), "run `shipcheck doctor`".into())
}

pub fn install_fix(repo: &str) -> String {
    format!(
        "add {repo} to the shipcheck App: https://github.com/settings/installations (shipcheck-run, Configure)"
    )
}

/// The line a failed post leaves for `status`, `run`, the pre-push hook and the daemon's log
pub fn unposted_line(repo: &str, e: &str) -> String {
    let (reason, fix) = reason_and_fix(repo, e);
    format!("checks not posted: {reason}, fix: {fix}")
}

fn unposted_file(repo: &str) -> PathBuf {
    run::home().join("background").join(repo).join("unposted")
}

/// Keeps `repo`'s last failed post until a post succeeds, `error` is None for a success
pub fn remember_post(repo: &str, error: Option<&str>) {
    let file = unposted_file(repo);

    match error {
        Some(e) => {
            let _ = fs::create_dir_all(file.parent().unwrap_or(&file));
            let _ = fs::write(&file, unposted_line(repo, e));
        }
        None => {
            let _ = fs::remove_file(&file);
        }
    }
}

/// The line of `repo`'s last failed post, None once a post succeeded
pub fn unposted(repo: &str) -> Option<String> {
    fs::read_to_string(unposted_file(repo))
        .ok()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
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
    let text = fs::read_to_string(&path).unwrap_or_default();
    let Some((mut keep, older)) = next_record(&text, &plan.sha, post, our_run) else {
        return;
    };

    for pid in older {
        if let Ok(pid) = pid.parse() {
            crate::runner::signal_group(pid, libc::SIGTERM);
        }
    }

    if let Ok(child) = spawn(cwd, plan, base, name, post, trigger, home_ok) {
        let pid = child.id();
        reap();

        if let Ok(mut children) = CHILDREN.lock() {
            let sha = &plan.sha[..12.min(plan.sha.len())];
            children.push((child, format!("{repo} {name}: run of {sha}")));
        }

        keep.push(format!(
            "{pid} {}{}",
            plan.sha,
            if post { " post" } else { "" }
        ));
        let _ = fs::create_dir_all(path.parent().unwrap_or(&path));
        let _ = fs::write(&path, keep.join("\n") + "\n");
    }
}

/// Before a run of `sha` starts: None when a run of it that does what this one would is still going,
/// else the record lines to keep and the process groups of older runs to stop
fn next_record(
    record: &str,
    sha: &str,
    post: bool,
    alive: impl Fn(&str) -> bool,
) -> Option<(Vec<String>, Vec<String>)> {
    let (mut keep, mut older) = (Vec::new(), Vec::new());

    for line in record.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(s)) = (fields.next(), fields.next()) else {
            continue;
        };

        if s == sha && alive(pid) {
            if !post || fields.next() == Some("post") {
                return None;
            }

            keep.push(line.to_string());
            continue;
        }

        // Only a group a background run of ours still leads, never a number that may now be another's
        if alive(pid) {
            older.push(pid.to_string());
        }
    }

    Some((keep, older))
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

/// The processes this machine's records list for `sha`, under any branch or PR
fn listed_runs(repo_id: &str, sha: &str) -> Vec<String> {
    let dir = run::home().join("background").join(repo_id);

    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .flat_map(|text| {
            text.lines()
                .filter_map(|line| {
                    let mut fields = line.split_whitespace();
                    match (fields.next(), fields.next()) {
                        (Some(pid), Some(s)) if s == sha => Some(pid.to_string()),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Whether some run of `sha` started here and none still goes
fn all_ended(pids: &[String], alive: impl Fn(&str) -> bool) -> bool {
    !pids.is_empty() && !pids.iter().any(|p| alive(p))
}

/// Whether this machine ran `sha` and every such run ended, so a claim one left holds nothing
pub fn died_here(repo_id: &str, sha: &str) -> bool {
    all_ended(&listed_runs(repo_id, sha), our_run)
}

/// Stops the runs of `sha` that still go on this machine
pub fn stop_here(repo_id: &str, sha: &str) {
    for pid in listed_runs(repo_id, sha) {
        if our_run(&pid)
            && let Ok(pid) = pid.parse()
        {
            crate::runner::signal_group(pid, libc::SIGTERM);
        }
    }
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

// The runs this process started with what they ran, a child nobody waits on stays a zombie under a long-lived daemon
static CHILDREN: Mutex<Vec<(Child, String)>> = Mutex::new(Vec::new());

/// Waits on the runs this process started that have ended, and says how each one ended
pub fn reap() {
    if let Ok(mut children) = CHILDREN.lock() {
        children.retain_mut(|(child, what)| match child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                println!("{what} ended, {status}");
                false
            }
            Err(_) => false,
        });
    }
}

/// How often a posting run notes its progress
pub const BEAT: Duration = Duration::from_secs(30);
/// How long a run may go without progress before the daemon stops it
pub const QUIET: Duration = Duration::from_secs(15 * 60);

/// Where run `pid` notes the last time it made progress
pub fn beat_path(repo_id: &str, pid: u32) -> PathBuf {
    run::home()
        .join("background")
        .join(repo_id)
        .join(format!("{pid}.beat"))
}

/// The newest change of any file under `dir`
fn newest(dir: &Path) -> Option<SystemTime> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;

            if meta.is_dir() {
                newest(&e.path())
            } else {
                meta.modified().ok()
            }
        })
        .max()
}

/**
 * Notes the time in `path` at once, then every `every` while its job logs under `logs` grew or it
 * waits for a run slot, until `done`. A thread beating on a timer would go on while the run hangs,
 * so only progress beats. The note goes when the run ends
 **/
pub fn heartbeat(path: &Path, logs: &[PathBuf], done: &AtomicBool, every: Duration) {
    let beat = || {
        let _ = fs::write(path, run::unix_now().to_string());
    };
    let changed = || logs.iter().filter_map(|d| newest(d)).max();
    let mut last = changed();
    beat();

    'run: loop {
        let started = std::time::Instant::now();

        while started.elapsed() < every {
            if done.load(Ordering::Relaxed) {
                break 'run;
            }

            std::thread::sleep(Duration::from_millis(20).min(every));
        }

        let now = changed();

        if now != last || run::WAITING_FOR_SLOT.load(Ordering::Relaxed) {
            last = now;
            beat();
        }
    }

    let _ = fs::remove_file(path);
}

/// Whether a run noted at `at` went quiet by `now`
fn went_quiet(at: u64, now: u64, quiet: Duration) -> bool {
    now.saturating_sub(at) > quiet.as_secs()
}

/// Whether a run of `sha` that still goes here made no progress for `quiet`. A run without a note
/// is older than heartbeats, or just started
pub fn stuck_here(repo_id: &str, sha: &str, quiet: Duration) -> bool {
    let now = run::unix_now();

    listed_runs(repo_id, sha)
        .iter()
        .filter(|pid| our_run(pid))
        .filter_map(|pid| pid.parse().ok())
        .filter_map(|pid| fs::read_to_string(beat_path(repo_id, pid)).ok())
        .filter_map(|at| at.trim().parse().ok())
        .any(|at| went_quiet(at, now, quiet))
}

fn spawn(
    cwd: &Path,
    plan: &Plan,
    base: &str,
    name: &str,
    post: bool,
    trigger: &str,
    home_ok: bool,
) -> Result<Child, String> {
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
    Ok(child)
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

/// GitHub answers 422 to a check run on a commit it does not have yet. ureq ends that error with
/// `http status: 422`, the service says `GitHub refused to <what>: 422 <message>`
fn unknown_sha(e: &str) -> bool {
    e.ends_with("http status: 422")
        || e.split_once("GitHub refused to ")
            .and_then(|(_, rest)| rest.split_once(": "))
            .is_some_and(|(_, answer)| answer.starts_with("422 "))
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
        None => {
            let (entry, ids) = std::thread::scope(|s| {
                let marks = s.spawn(|| marked(true));
                let entry = run();
                (entry, marks.join().unwrap_or_default())
            });

            match entry {
                Ok(entry) => (entry, ids),
                // Left in progress, the checks would read as another machine's claim until it went stale
                Err(e) => {
                    if current() {
                        let bodies: Vec<Value> =
                            names.iter().map(|n| check::errored(n, sha, &e)).collect();
                        let _ = retry(waits, || finish(send, &bodies, &ids));
                    }

                    return Err(e);
                }
            }
        }
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
        bodies: Mutex<Vec<Value>>,
    }

    impl Fake {
        fn new(unknown: usize) -> Self {
            Fake {
                unknown: Mutex::new(unknown),
                posts: Mutex::new(Vec::new()),
                bodies: Mutex::new(Vec::new()),
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

            self.bodies.lock().unwrap().push(body.clone());
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

        fn bodies(&self) -> Vec<Value> {
            self.bodies.lock().unwrap().clone()
        }
    }

    #[test]
    fn a_newer_commit_stops_the_older_runs_and_one_run_per_commit_goes() {
        let alive = |pid: &str| pid != "9";
        let record = "11 aaa\n12 bbb post\n9 ccc\n";

        // A newer commit stops every older run still going, and forgets the dead one
        assert_eq!(
            next_record(record, "ddd", false, alive),
            Some((vec![], vec!["11".into(), "12".into()]))
        );
        // The commit already running is left alone, posting or not
        assert_eq!(next_record(record, "aaa", false, alive), None);
        assert_eq!(next_record(record, "bbb", true, alive), None);
        assert_eq!(next_record(record, "bbb", false, alive), None);
        // A push after the commit's run adds a run that posts, beside it
        assert_eq!(
            next_record(record, "aaa", true, alive),
            Some((vec!["11 aaa".into()], vec!["12".into()]))
        );
        // A run that died is started again
        assert_eq!(
            next_record(record, "ccc", false, alive),
            Some((vec![], vec!["11".into(), "12".into()]))
        );
        assert_eq!(next_record("", "aaa", true, alive), Some((vec![], vec![])));
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
    fn a_post_the_app_cannot_make_says_how_to_add_the_repository() {
        for e in [
            "POST /v1/checks: the shipcheck App is not installed on acme/app, or you have no access to it, install it from https://github.com/apps/shipcheck-run",
            "the shipcheck App is not installed on acme/app",
            "GET /v1/pulls: HTTP 404",
            "GET https://api.github.com/repos/acme/app/pulls?state=open: http status: 403",
        ] {
            assert_eq!(
                unposted_line("acme/app", e),
                "checks not posted: the shipcheck App is not installed on acme/app, fix: add acme/app to the shipcheck App: https://github.com/settings/installations (shipcheck-run, Configure)",
                "{e}"
            );
        }

        let other = "POST /v1/checks: acme needs write, maintain or admin on acme/app to post its check, and has read";
        assert!(!not_installed(other));
        assert_eq!(
            unposted_line("acme/app", other),
            format!("checks not posted: {other}, fix: run `shipcheck doctor`")
        );
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
    fn a_run_that_could_not_run_fails_its_checks_with_the_error() {
        let fake = Fake::new(0);
        let got = post_run(
            &|b, id| fake.send(b, id),
            &|| Ok(Checks::new()),
            &"a".repeat(40),
            &names(),
            None,
            || Err("docker version failed, is a Docker engine running?".into()),
            finals,
            || true,
            &[],
        );
        assert!(got.unwrap_err().contains("docker version failed"));
        let posts = fake.posts();
        assert_eq!(
            posts[2..],
            [
                ("shipcheck/test".into(), "completed".into(), Some(101)),
                ("shipcheck".into(), "completed".into(), Some(102)),
            ],
            "every check it marked running ends, or the daemon waits on it as a claim"
        );
        assert!(
            fake.bodies()
                .iter()
                .filter(|b| b["status"] == "completed")
                .all(|b| b["conclusion"] == "failure"
                    && b["output"]["text"]
                        .as_str()
                        .is_some_and(|t| t.contains("docker version failed")))
        );
    }

    #[test]
    fn a_replaced_run_that_could_not_run_leaves_the_checks_to_the_newer_one() {
        let fake = Fake::new(0);
        let got = post_run(
            &|b, id| fake.send(b, id),
            &|| Ok(Checks::new()),
            &"a".repeat(40),
            &names(),
            None,
            || Err("no docker".into()),
            finals,
            || false,
            &[],
        );
        assert!(got.is_err());
        assert!(
            fake.posts()
                .iter()
                .all(|(_, status, _)| status == "in_progress")
        );
    }

    #[test]
    fn a_claim_is_dead_only_when_every_run_of_the_head_here_ended() {
        let alive = |pid: &str| pid == "7";
        assert!(all_ended(&["5".into(), "6".into()], alive));
        assert!(!all_ended(&["5".into(), "7".into()], alive));
        assert!(
            !all_ended(&[], alive),
            "a head this machine never ran may be another machine's claim"
        );
    }

    #[test]
    fn a_run_beats_only_while_it_makes_progress() {
        let dir = tempfile::tempdir().unwrap();
        let (beat, logs) = (dir.path().join("1.beat"), dir.path().join("logs"));
        fs::create_dir_all(logs.join("ci.yml")).unwrap();
        let done = AtomicBool::new(false);
        let every = Duration::from_millis(50);
        let wait_for = |what: &str, cond: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);

            while !cond() {
                assert!(std::time::Instant::now() < deadline, "{what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };

        std::thread::scope(|s| {
            s.spawn(|| heartbeat(&beat, std::slice::from_ref(&logs), &done, every));
            wait_for("the run notes its start", &|| beat.exists());

            fs::remove_file(&beat).unwrap();
            std::thread::sleep(every * 4);
            assert!(!beat.exists(), "a run whose logs do not grow must not beat");

            std::thread::sleep(Duration::from_millis(1100));
            fs::write(logs.join("ci.yml/test.log"), "line").unwrap();
            wait_for("a log that grew is progress", &|| beat.exists());

            done.store(true, Ordering::Relaxed);
        });
        assert!(!beat.exists(), "the note goes when the run ends");
    }

    #[test]
    fn a_run_is_stuck_after_the_quiet_time() {
        assert!(!went_quiet(1_000, 1_000 + 900, QUIET));
        assert!(went_quiet(1_000, 1_000 + 901, QUIET));
    }

    #[test]
    fn reap_waits_on_ended_runs() {
        let child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        CHILDREN.lock().unwrap().push((child, "test run".into()));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let listed = || CHILDREN.lock().unwrap().iter().any(|(c, _)| c.id() == pid);

        while listed() {
            assert!(std::time::Instant::now() < deadline, "true must end");
            reap();
            std::thread::sleep(Duration::from_millis(10));
        }

        // A zombie still answers signal 0, a reaped process does not
        assert_ne!(unsafe { libc::kill(pid as libc::pid_t, 0) }, 0);
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
        assert!(posts(false, true));
        assert!(!posts(false, false), "not signed in posts nothing");
        assert!(
            !posts(true, true),
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
