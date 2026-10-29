use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::background;
use crate::github::{Checks, OpenPr};
use crate::record::{self, Run};
use crate::service::Sender;
use crate::{check, run};

/// How long a run can last: each workflow's deadline, once per attempt, and 10 minutes to set up and
/// post. A claim older than this is a run that died, so this machine may take the PR
fn stale_after(config: &crate::config::RepoConfig) -> u64 {
    config.timeout_minutes * 60 * (u64::from(config.retries) + 1) + 600
}

/// How long a run may make no progress before it counts as stuck. A job that prints nothing still
/// goes until its own timeout, so the limit is longer than any one job may take
fn quiet_after(config: &crate::config::RepoConfig) -> Duration {
    Duration::from_secs(config.timeout_minutes * 60 + 300)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    #[serde(default = "ten")]
    pub interval_seconds: u64,
    #[serde(default = "yes")]
    pub run_misses: bool,
    /// Whose PRs this daemon runs, for an App key, where no sign-in names the user
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    #[serde(default)]
    pub repos: BTreeMap<String, RepoEntry>,
    /// A person's `--home-ok` on the daemon, passed on to the runs it starts
    #[serde(skip)]
    pub home_ok: bool,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepoEntry {
    pub path: PathBuf,
    #[serde(default = "origin_main")]
    pub base: String,
}

fn ten() -> u64 {
    10
}

fn yes() -> bool {
    true
}

fn origin_main() -> String {
    "origin/main".into()
}

impl DaemonConfig {
    pub fn path() -> PathBuf {
        run::home().join("daemon.toml")
    }

    pub fn load() -> Result<Self, String> {
        Self::load_from(&Self::path())
    }

    /// No file is a daemon with no repositories yet
    fn load_from(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Adds a repository or changes its entry, and says whether the file changed
    pub fn add(repo: &str, path: &Path, base: &str) -> Result<bool, String> {
        Self::add_to(&Self::path(), repo, path, base)
    }

    pub(crate) fn add_to(file: &Path, repo: &str, path: &Path, base: &str) -> Result<bool, String> {
        let mut config = Self::load_from(file)?;
        let entry = RepoEntry {
            path: path.to_path_buf(),
            base: base.to_string(),
        };

        if config.repos.get(repo) == Some(&entry) {
            return Ok(false);
        }

        config.repos.insert(repo.to_string(), entry);
        let text = toml::to_string(&config).map_err(|e| e.to_string())?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }

        std::fs::write(file, text).map_err(|e| format!("{}: {e}", file.display()))?;
        Ok(true)
    }
}

/**
 * The base every command reads shipcheck.toml and the workflows from: this checkout's daemon entry,
 * then the default branch the remote reports, then `origin/main` with a warning. The hooks run in
 * git's foreground, so they skip the remote and only say when `origin/main` is likely wrong
 **/
pub fn command_base(command: &str, repo: &Path) -> String {
    command_base_in(&DaemonConfig::path(), command, repo)
}

fn command_base_in(file: &Path, command: &str, repo: &Path) -> String {
    if let Some(entry) = entry_in(file, repo) {
        return entry.base;
    }

    if matches!(command, "on-commit" | "on-push") {
        if !main_is_default(repo) {
            eprintln!(
                "shipcheck: no daemon entry for this checkout, so the hook reads shipcheck.toml from origin/main. Run `shipcheck install` to use the default branch"
            );
        }

        return origin_main();
    }

    run::remote_base(repo).unwrap_or_else(|| {
        eprintln!(
            "warning: origin did not say its default branch, so shipcheck reads shipcheck.toml from origin/main"
        );
        origin_main()
    })
}

/// The base from this checkout's daemon entry or the remote, None when neither gives one
pub fn known_base(repo: &Path) -> Option<String> {
    entry_in(&DaemonConfig::path(), repo)
        .map(|entry| entry.base)
        .or_else(|| run::remote_base(repo))
}

/// The daemon's entry for this repository, when it names this checkout and not another clone
fn entry_in(file: &Path, repo: &Path) -> Option<RepoEntry> {
    let checkout = |p: &Path| crate::autostart::main_checkout(p).canonicalize().ok();
    let here = checkout(repo)?;
    run::repo_id(repo)
        .ok()
        .and_then(|id| DaemonConfig::load_from(file).ok()?.repos.remove(&id))
        .filter(|entry| checkout(&entry.path).as_ref() == Some(&here))
}

/// Whether the local refs agree main is the default branch, without asking the remote
fn main_is_default(repo: &Path) -> bool {
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let head = git(&["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"]);
    head.is_none_or(|h| h == "origin/main")
        && git(&["rev-parse", "--verify", "-q", "refs/remotes/origin/main"]).is_some()
}

/**
 * Whether a result on `base` may be posted: the daemon's base for this checkout, or the default
 * branch the remote reports, never a ref the caller picked, whose shipcheck.toml could run anything.
 * An entry counts only for the checkout it names, and a remote that does not answer allows nothing
 **/
pub fn posts_from(base: &str, repo: &Path) -> bool {
    posts_from_in(&DaemonConfig::path(), base, repo)
}

fn posts_from_in(file: &Path, base: &str, repo: &Path) -> bool {
    entry_in(file, repo).is_some_and(|entry| entry.base == base)
        || run::remote_base(repo).as_deref() == Some(base)
}

/**
 * Polls GitHub for open PR heads with no completed `shipcheck` check, posts a stored result at
 * once, and runs a same-repository head that has none. It reads `daemon.toml` again every cycle,
 * so a repository `shipcheck install` adds needs no restart, and waits while this machine cannot
 * post, so a later `shipcheck login` is enough
 **/
/// `home_ok` is a person's `--home-ok`, passed on to the runs it starts
pub fn serve(home_ok: bool) -> ! {
    let mut senders: BTreeMap<String, Sender> = BTreeMap::new();
    let mut done: HashSet<(String, String)> = HashSet::new();
    let mut waiting: HashMap<(String, String), Instant> = HashMap::new();
    let mut said: HashSet<String> = HashSet::new();
    // The head each PR had when the daemon last checked it, to see merges and new heads
    let mut heads: BTreeMap<(String, u64), String> = BTreeMap::new();
    // The head each PR had while it waited for GitHub to list its push
    let mut waits: BTreeMap<(String, u64), String> = BTreeMap::new();
    // The heads checked while the base had no shipcheck.toml, with the base's tip then
    let mut unconfigured: HashMap<(String, String), String> = HashMap::new();

    loop {
        let config = match DaemonConfig::load() {
            Ok(config) => DaemonConfig { home_ok, ..config },
            Err(e) => {
                eprintln!("{e}");
                std::thread::sleep(Duration::from_secs(10));
                continue;
            }
        };

        if config.run_misses && me(&config).is_none() && said.insert(String::new()) {
            eprintln!(
                "runs no PR, no login known for this machine: run `shipcheck login` again, or set login in daemon.toml"
            );
        }

        for (repo, entry) in &config.repos {
            if !senders.contains_key(repo) {
                match Sender::new(repo) {
                    Ok(sender) => {
                        senders.insert(repo.clone(), sender);
                    }
                    Err(e) => {
                        if said.insert(repo.clone()) {
                            eprintln!("{repo}: waiting, {e}");
                        }

                        continue;
                    }
                }
            }

            let state = (
                &mut done,
                &mut waiting,
                &mut heads,
                &mut waits,
                &mut unconfigured,
            );

            let refused = format!("{repo} unposted");

            match cycle(&config, repo, entry, &senders[repo], state) {
                // Said once per repository, each poll gets the same answer until the App is added
                Err(e) if background::not_installed(&e) => {
                    if said.insert(refused) {
                        background::remember_post(repo, Some(&e));
                        eprintln!("{repo}: {}", background::unposted_line(repo, &e));
                    }
                }
                Err(e) => eprintln!("{repo}: {e}"),
                Ok(()) => {
                    said.remove(&refused);
                }
            }
        }

        background::reap();
        std::thread::sleep(Duration::from_secs(config.interval_seconds));
    }
}

fn git(entry: &RepoEntry, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(&entry.path)
        .args(args)
        // A service has no terminal, so a credential prompt would hang the daemon
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .is_ok_and(|s| s.success())
}

fn cycle(
    config: &DaemonConfig,
    repo: &str,
    entry: &RepoEntry,
    sender: &Sender,
    (done, waiting, heads, waits, unconfigured): State,
) -> Result<(), String> {
    let base_branch = entry.base.strip_prefix("origin/").unwrap_or(&entry.base);
    let open = sender.open_prs()?;
    merges(repo, sender, &open, heads);
    let prs = into_base(open, &entry.base);

    track_heads(repo, &prs, heads, sender);

    // Never refused: silence about who pushed it says nothing against them
    for (number, old) in abandoned(waits, repo, &prs) {
        println!(
            "{repo}#{number}: {} replaced before GitHub named who pushed it",
            &old[..old.len().min(12)]
        );
        let fields = serde_json::json!({"pr": number, "sha": old, "reason": "abandoned_while_waiting_for_push"});
        record::keep(repo, &record::event(repo, "skipped", fields));
        waits.remove(&(repo.to_string(), number));
    }

    let id = |pr: &OpenPr| (repo.to_string(), pr.sha.clone());
    if prs
        .iter()
        .all(|pr| done.contains(&id(pr)) && !unconfigured.contains_key(&id(pr)))
    {
        return Ok(());
    }

    if !git(entry, &["fetch", "-q", "origin", base_branch]) {
        return Err(format!(
            "could not fetch {base_branch}, so its config cannot be trusted"
        ));
    }

    let tip = base_tip(entry);
    recheck(unconfigured, done, repo, &tip);

    for pr in prs {
        let id = (repo.to_string(), pr.sha.clone());

        if done.contains(&id) {
            continue;
        }

        // One PR that cannot be checked must not hold back the others
        let since = waiting.get(&id).map_or(Duration::ZERO, Instant::elapsed);

        let start = |plan: &run::Plan, name: &str| {
            background::start(
                &entry.path,
                plan,
                &entry.base,
                name,
                true,
                "daemon",
                config.home_ok,
            )
        };

        match handle(config, repo, entry, sender, &pr, since, waits, &start) {
            Ok(Some(verdict)) => {
                println!("{repo}#{}: {verdict} on {}", pr.number, &pr.sha[..12]);
                heads.insert((repo.to_string(), pr.number), pr.sha.clone());
                waiting.remove(&id);

                if verdict == NO_CONFIG {
                    unconfigured.insert(id.clone(), tip.clone());
                }

                done.insert(id);
            }
            Ok(None) => {
                if let std::collections::hash_map::Entry::Vacant(e) = waiting.entry(id) {
                    e.insert(Instant::now());
                    println!("{repo}#{}: waiting on {}", pr.number, &pr.sha[..12]);
                }
            }
            Err(e) => eprintln!("{repo}#{}: {e}", pr.number),
        }
    }

    Ok(())
}

/// The PRs into `base`, as `origin/main` or `main`
fn into_base(open: Vec<OpenPr>, base: &str) -> Vec<OpenPr> {
    let branch = base.strip_prefix("origin/").unwrap_or(base);
    // shipcheck.toml on the configured branch only says what a PR into that branch must pass
    open.into_iter().filter(|pr| pr.base == branch).collect()
}

type State<'a> = (
    &'a mut HashSet<(String, String)>,
    &'a mut HashMap<(String, String), Instant>,
    &'a mut BTreeMap<(String, u64), String>,
    &'a mut BTreeMap<(String, u64), String>,
    &'a mut HashMap<(String, String), String>,
);

const NO_CONFIG: &str = "not checked, no shipcheck.toml on main";

/// The commit the fetched base is at, empty when git cannot say
fn base_tip(entry: &RepoEntry) -> String {
    Command::new("git")
        .arg("-C")
        .arg(&entry.path)
        .args([
            "rev-parse",
            "-q",
            "--verify",
            &format!("{}^{{commit}}", entry.base),
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Checks again the heads of `repo` marked with no config once the base moved, as a merge of shipcheck.toml does
fn recheck(
    unconfigured: &mut HashMap<(String, String), String>,
    done: &mut HashSet<(String, String)>,
    repo: &str,
    tip: &str,
) {
    unconfigured.retain(|id, at| {
        let moved = id.0 == repo && at != tip;

        if moved {
            done.remove(id);
        }

        !moved
    });
}

/// The heads that waited for their push event and that a new head on the same PR replaced
fn abandoned(
    waits: &BTreeMap<(String, u64), String>,
    repo: &str,
    prs: &[OpenPr],
) -> Vec<(u64, String)> {
    prs.iter()
        .filter_map(|pr| {
            let old = waits.get(&(repo.to_string(), pr.number))?;
            (*old != pr.sha).then(|| (pr.number, old.clone()))
        })
        .collect()
}

/// Records `pr_merged` for each PR the daemon checked that left the open list merged, one GET each.
/// The hosted service reads no closed PR, so through it the daemon only forgets them
fn merges(
    repo: &str,
    sender: &Sender,
    open: &[OpenPr],
    heads: &mut BTreeMap<(String, u64), String>,
) {
    let gone: Vec<(String, u64)> = heads
        .keys()
        .filter(|(r, n)| r == repo && !open.iter().any(|pr| pr.number == *n))
        .cloned()
        .collect();

    for key in gone {
        let pr = match sender.pull(key.1) {
            Ok(Some(pr)) => pr,
            Ok(None) => {
                heads.remove(&key);
                continue;
            }
            Err(e) => {
                eprintln!("{repo}#{}: {e}", key.1);
                continue;
            }
        };

        // Past the first 100 open PRs, still open
        if pr["state"] == "open" {
            continue;
        }

        let sha = heads.remove(&key).unwrap_or_default();

        if pr["merged_at"].is_string() {
            let fields = serde_json::json!({
                "pr": key.1,
                "sha": sha,
                "merge_sha": pr["merge_commit_sha"],
                "merged_by": pr["merged_by"]["login"],
                "merged_at": pr["merged_at"],
                "author": pr["user"]["login"],
            });
            record::keep(repo, &record::event(repo, "pr_merged", fields));
        }
    }
}

/// A daemon event about one head, such as a rerun or a skip, with its reason
fn note(repo: &str, name: &str, pr: &OpenPr, reason: &str) {
    let fields = serde_json::json!({"pr": pr.number, "sha": pr.sha, "reason": reason});
    record::keep(repo, &record::event(repo, name, fields));
}

// A refused head stops being a mark after this, the store stays small
const REFUSED_DAYS: u64 = 30;

fn refused_path(repo: &str) -> PathBuf {
    run::home()
        .join("daemon/refused")
        .join(format!("{repo}.json"))
}

/// The heads this machine refused to run, as sha and the time it refused, without old ones
fn refused(file: &Path, now: u64) -> BTreeMap<String, u64> {
    let mut found: BTreeMap<String, u64> = std::fs::read_to_string(file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    found.retain(|_, at| now.saturating_sub(*at) < REFUSED_DAYS * 86_400);
    found
}

fn refuse(file: &Path, sha: &str, now: u64) {
    let mut found = refused(file, now);
    found.insert(sha.to_string(), now);

    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    let _ = std::fs::write(file, serde_json::json!(found).to_string());
}

/**
 * A refused head that `head` contains. "Update branch" on GitHub is a push by whoever clicked it,
 * and it can carry a teammate's commit that person never fetched, so a later head is only as
 * allowed as the commits under it
 **/
fn tainted(checkout: &Path, head: &str, refused: &BTreeMap<String, u64>) -> Option<String> {
    refused
        .keys()
        .find(|sha| {
            Command::new("git")
                .arg("-C")
                .arg(checkout)
                .args(["merge-base", "--is-ancestor", sha.as_str(), head])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
        .cloned()
}

#[derive(Debug, PartialEq)]
enum Pushed {
    Run,
    /// No source names the pusher yet. GitHub lists events from 30 seconds to hours late, so the
    /// head waits and every poll asks again, never giving up on a PR
    Wait,
    /// Another login pushed it, and main does not list them
    Other,
}

/**
 * Whether the daemon may run a head: both the PR's opener and the pusher of the head must be its
 * own user or a login `daemon_authors` on main lists, since anyone with write access can push to
 * any PR's branch. Never by a commit's author email, which anyone can set
 **/
fn pushed(opener: &str, pusher: Option<&str>, me: &str, listed: &[String]) -> Pushed {
    let allowed = |login: &str| {
        !login.is_empty()
            && (login.eq_ignore_ascii_case(me)
                || listed.iter().any(|l| l.eq_ignore_ascii_case(login)))
    };

    match pusher {
        _ if !allowed(opener) => Pushed::Other,
        Some(p) if allowed(p) => Pushed::Run,
        Some(_) => Pushed::Other,
        None => Pushed::Wait,
    }
}

/// Why a head does not run, its `skipped` reason, and whether to mark it refused. None when it
/// runs or still waits
fn refusal(verdict: &Pushed) -> Option<(&'static str, &'static str, bool)> {
    match verdict {
        Pushed::Run | Pushed::Wait => None,
        Pushed::Other => Some((
            "not run, opened or pushed by a login that is not this machine's and daemon_authors on main does not list",
            "pusher_not_allowed",
            true,
        )),
    }
}

/// The login whose PRs run: `login` in daemon.toml, else the one `shipcheck login` signed in
fn me(config: &DaemonConfig) -> Option<String> {
    config
        .login
        .clone()
        .or_else(crate::service::signed_in_login)
}

/// Posts the PR comment after the checks, so a comment GitHub refuses costs one line, never the
/// post. With no secrets to mask it posts none
pub fn comment(
    sender: &impl Remote,
    prs: Option<&[u64]>,
    plan: &run::Plan,
    base: &str,
    entry: &run::Entry,
) -> bool {
    let branch = base.strip_prefix("origin/").unwrap_or(base);
    let said = run::repo_id(&plan.repo)
        .and_then(|id| run::secret_values(&id))
        .and_then(|secrets| sender.comment(prs, branch, &plan.sha, entry, &secrets));

    match said {
        Ok(urls) => {
            for url in &urls {
                println!("updated the PR comment: {url}");
            }

            !urls.is_empty()
        }
        Err(e) => {
            eprintln!("{}: no PR comment, {e}", &plan.sha[..12]);
            false
        }
    }
}

#[derive(Debug, PartialEq)]
enum Action {
    /// A run of this tree goes on this machine
    Local,
    /// The tree has a result, post it with no run
    Post,
    /// Another machine shows a run in progress
    Claimed,
    Run,
    Queue,
}

/// Whether a check went in progress before `cutoff`, as GitHub writes times
fn overdue(existing: &Checks, cutoff: &str) -> bool {
    existing.values().any(|c| {
        c.status == "in_progress" && !c.started_at.is_empty() && c.started_at.as_str() < cutoff
    })
}

/// What to do with a head that has no completed `shipcheck` check. `cutoff` is the time before
/// which an in-progress claim is stale, as GitHub writes times
fn decide(existing: &Checks, local: bool, result: bool, may_run: bool, cutoff: &str) -> Action {
    let claimed = existing
        .values()
        .any(|c| c.status == "in_progress" && c.started_at.as_str() >= cutoff);

    if local {
        Action::Local
    } else if result {
        Action::Post
    } else if claimed {
        Action::Claimed
    } else if may_run {
        Action::Run
    } else {
        Action::Queue
    }
}

/// Notes each PR's head as soon as it is seen, so a head replaced before any verdict still gets
/// its unfinished checks cancelled
fn track_heads(
    repo: &str,
    prs: &[OpenPr],
    heads: &mut BTreeMap<(String, u64), String>,
    remote: &dyn Remote,
) {
    for pr in prs {
        let key = (repo.to_string(), pr.number);

        if let Some(old) = heads.get(&key).filter(|old| **old != pr.sha) {
            let fields = serde_json::json!({
                "pr": pr.number,
                "sha": pr.sha,
                "previous_sha": old,
                "reason": "head_changed_on_github",
            });
            record::keep(repo, &record::event(repo, "rerun", fields));

            if !prs.iter().any(|p| p.sha == *old)
                && let Err(e) = cancel_replaced(remote, old)
            {
                println!("{repo}#{}: {e}", pr.number);
            }
        }

        heads.insert(key, pr.sha.clone());
    }
}

/// Completes what a replaced head left queued or running, as Actions cancels a superseded run
fn cancel_replaced(remote: &dyn Remote, sha: &str) -> Result<(), String> {
    for (name, c) in remote.find_checks(sha)? {
        if c.status == "completed" {
            continue;
        }

        let body = serde_json::json!({
            "name": name,
            "head_sha": sha,
            "status": "completed",
            "conclusion": "cancelled",
            "output": {"title": "replaced", "summary": "A newer commit replaced this one.", "text": ""},
        });
        remote.send(&body, Some(c.id))?;
    }

    Ok(())
}

/// What `handle` asks of GitHub, so a test can answer it
pub trait Remote: Sync {
    fn send(&self, body: &serde_json::Value, id: Option<u64>) -> Result<(u64, String), String>;
    fn find_checks(&self, sha: &str) -> Result<Checks, String>;
    fn pusher(
        &self,
        pr: u64,
        branch: &str,
        sha: &str,
        trusted: &[String],
    ) -> Result<Option<(String, String)>, String>;
    fn comment(
        &self,
        prs: Option<&[u64]>,
        base: &str,
        sha: &str,
        entry: &run::Entry,
        secrets: &[String],
    ) -> Result<Vec<String>, String>;
}

impl Remote for Sender {
    fn send(&self, body: &serde_json::Value, id: Option<u64>) -> Result<(u64, String), String> {
        Sender::send(self, body, id)
    }

    fn find_checks(&self, sha: &str) -> Result<Checks, String> {
        Sender::find_checks(self, sha)
    }

    fn pusher(
        &self,
        pr: u64,
        branch: &str,
        sha: &str,
        trusted: &[String],
    ) -> Result<Option<(String, String)>, String> {
        Sender::pusher(self, pr, branch, sha, trusted)
    }

    fn comment(
        &self,
        prs: Option<&[u64]>,
        base: &str,
        sha: &str,
        entry: &run::Entry,
        secrets: &[String],
    ) -> Result<Vec<String>, String> {
        Sender::comment(self, prs, base, sha, entry, secrets)
    }
}

/// Posts the checks for one PR, or returns None while it waits for a run. `start` starts the
/// background run of a plan, with its name
#[allow(clippy::too_many_arguments)]
fn handle(
    config: &DaemonConfig,
    repo: &str,
    entry: &RepoEntry,
    sender: &impl Remote,
    pr: &OpenPr,
    since: Duration,
    waits: &mut BTreeMap<(String, u64), String>,
    start: &dyn Fn(&run::Plan, &str),
) -> Result<Option<&'static str>, String> {
    let (number, sha) = (pr.number, &pr.sha);
    let mut rec = Run::new("daemon", run::unix_now());
    rec.pr = Some(number);
    rec.author = Some(pr.author.clone()).filter(|a| !a.is_empty());
    rec.branch = Some(pr.branch.clone()).filter(|b| !b.is_empty());
    let existing = sender.find_checks(sha)?;

    // `shipcheck` completes last, after every job check
    if existing
        .get(check::NAME)
        .is_some_and(|c| c.status == "completed")
    {
        return Ok(Some("already checked"));
    }

    if !git(entry, &["cat-file", "-e", &format!("{sha}^{{commit}}")]) {
        git(
            entry,
            &["fetch", "-q", "origin", &format!("pull/{number}/head")],
        );
    }

    // The check reflects the owner's rules, so a repository with no shipcheck.toml on main gets none
    if !git(
        entry,
        &["cat-file", "-e", &format!("{}:shipcheck.toml", entry.base)],
    ) {
        return Ok(Some(NO_CONFIG));
    }

    let plan = run::plan(&entry.path, sha, &entry.base)?;

    if !plan.config.daemon {
        return Ok(Some("not checked, daemon = false on main"));
    }

    let same_repo = pr.head_repo.eq_ignore_ascii_case(repo);

    // A host check runs a commit's code as this user outside any container, so never a fork's, not even from the cache
    if plan.config.hosts() && !same_repo {
        return Ok(Some("not checked, host mode never checks a fork's PR"));
    }

    let names = check::names(&plan);
    let send = |body: &serde_json::Value, id| sender.send(body, id);
    let local = run::running(&plan.key) || background::running_here(repo, sha, false);
    let result = if local { None } else { run::lookup(&plan) };
    let cutoff = check::iso(run::unix_now().saturating_sub(stale_after(&plan.config)));

    // A fork's code would run on this machine with its secrets, so only its own author runs it
    let me = me(config);
    let may_run = config.run_misses && same_repo && me.is_some();

    // A claim this machine's own run left when it died holds nothing, so the head runs again now
    let claims = if !local && background::died_here(repo, sha) {
        Checks::new()
    } else {
        existing.clone()
    };

    match decide(&claims, local, result.is_some(), may_run, &cutoff) {
        Action::Local => {
            // A run past the time its jobs could take, or with no progress for a while, is stuck.
            // Once it is stopped, the next poll runs the head again
            if background::running_here(repo, sha, false)
                && (overdue(&existing, &cutoff)
                    || background::stuck_here(repo, sha, quiet_after(&plan.config)))
            {
                println!(
                    "{repo}#{number}: the run of {} is stuck, stopping it",
                    &sha[..12]
                );
                background::stop_here(repo, sha);
                return Ok(None);
            }

            // A run that posts marks its own checks, a mark from here could land after its result
            if !background::running_here(repo, sha, true) {
                background::mark(&send, &existing, sha, &names, true)?;
            }

            Ok(None)
        }
        Action::Post => {
            let result = result.ok_or("no result")?;
            // A fork's log could show what its own code printed of this machine, so only same-repo PRs get one
            let bodies = check::final_bodies(&plan, &result, same_repo);
            let ids = existing.iter().map(|(n, c)| (n.clone(), c.id)).collect();
            rec.cache_hit = true;
            rec.first_check = Some(run::unix_now());
            let secrets = run::repo_id(&plan.repo)
                .and_then(|id| run::secret_values(&id))
                .unwrap_or_default();
            let logs = run::log_dir(&plan.key);

            if let Err(e) = background::finish(&send, &bodies, &ids) {
                background::remember_post(repo, Some(&e));
                rec.post_error = Some(e.clone());
                record::keep(
                    repo,
                    &record::line(repo, &plan, &result, &rec, &logs, &secrets),
                );
                return Err(e);
            }

            background::remember_post(repo, None);
            rec.posted_checks = true;

            // A fork's summary is its own code's Markdown, so only a same-repo PR gets the comment
            if same_repo {
                rec.posted_comment = comment(sender, Some(&[number]), &plan, &entry.base, &result);
            }

            let line = record::line(repo, &plan, &result, &rec, &logs, &secrets);
            record::keep(repo, &line);
            record::share(&plan, repo, &line);

            Ok(Some(if result.passed {
                "posted success"
            } else {
                "posted failure"
            }))
        }
        Action::Claimed => {
            if since.is_zero() {
                note(repo, "skipped", pr, "claimed_by_another_machine");
            }

            Ok(None)
        }
        Action::Run => {
            let marks = refused_path(repo);

            if let Some(bad) = tainted(&entry.path, sha, &refused(&marks, run::unix_now())) {
                println!(
                    "{repo}#{number}: {} contains {}, a head this machine refused",
                    &sha[..12],
                    &bad[..bad.len().min(12)]
                );
                note(repo, "skipped", pr, "contains_refused_head");
                background::mark(&send, &existing, sha, &names, false)?;
                return Ok(Some("not run, it contains a head this machine refused"));
            }

            let me = me.as_deref().unwrap_or_default();
            let trusted: Vec<String> = std::iter::once(me.to_string())
                .chain(plan.config.daemon_authors.iter().cloned())
                .filter(|l| !l.is_empty())
                .collect();
            let pusher = sender.pusher(number, &pr.branch, sha, &trusted)?;

            if let Some((login, source)) = &pusher {
                println!(
                    "{repo}#{number}: {login} pushed {}, from {source}",
                    &sha[..12]
                );
            }

            let verdict = pushed(
                &pr.author,
                pusher.as_ref().map(|(login, _)| login.as_str()),
                me,
                &plan.config.daemon_authors,
            );

            if verdict == Pushed::Wait {
                if since.is_zero() {
                    println!(
                        "{repo}#{number}: GitHub has not said who pushed {} yet, asking again every poll",
                        &sha[..12]
                    );
                }

                waits.insert((repo.to_string(), number), sha.clone());
                background::mark(&send, &existing, sha, &names, false)?;
                return Ok(None);
            }

            let refused = refusal(&verdict);

            waits.remove(&(repo.to_string(), number));

            // Only a known pusher who is not allowed marks the head, heads on top of it inherit the mark
            if let Some((why, reason, mark)) = refused {
                if mark {
                    refuse(&marks, sha, run::unix_now());
                }

                note(repo, "skipped", pr, reason);
                background::mark(&send, &existing, sha, &names, false)?;
                return Ok(Some(why));
            }

            // A new `shipcheck` run is the claim, the latest one on the commit wins
            let ours = sender
                .send(&check::waiting(check::NAME, sha, true), None)?
                .0;
            let mut jobs = existing.clone();
            jobs.remove(check::NAME);
            let job_names: Vec<String> = names.into_iter().filter(|n| n != check::NAME).collect();
            background::mark(&send, &jobs, sha, &job_names, true)?;

            if sender.find_checks(sha)?.get(check::NAME).map(|c| c.id) != Some(ours) {
                note(repo, "skipped", pr, "claimed_by_another_machine");
                return Ok(None);
            }

            println!(
                "{repo}#{number}: new head {}, running its checks",
                &sha[..12]
            );
            note(repo, "rerun", pr, "no_result_for_tree");
            // The branch's record, as the hook writes it, so a push and the daemon never run two heads of one PR at once
            let name = if pr.branch.is_empty() {
                format!("pr:{number}")
            } else {
                pr.branch.clone()
            };
            start(&plan, &name);
            Ok(None)
        }
        Action::Queue => {
            background::mark(&send, &existing, sha, &names, false)?;

            // It never runs here, so asking again every poll would only spend API calls until its head changes
            if !same_repo {
                return Ok(Some("queued, the daemon never runs a fork's PR"));
            }

            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::Value;

    use super::*;

    #[test]
    fn config_defaults() {
        let cfg: DaemonConfig =
            toml::from_str("[repos]\n\"acme/app\" = { path = \"/src/app\" }\n").unwrap();
        assert_eq!(cfg.interval_seconds, 10);
        assert!(cfg.run_misses, "a PR with no result runs by default");
        assert_eq!(cfg.repos["acme/app"].base, "origin/main");
    }

    fn check(status: &str, started_at: &str) -> crate::github::Check {
        crate::github::Check {
            id: 1,
            status: status.into(),
            started_at: started_at.into(),
        }
    }

    #[test]
    fn a_claim_goes_stale_once_every_attempt_could_have_timed_out() {
        let mut config: crate::config::RepoConfig = toml::from_str("").unwrap();
        assert_eq!(
            stale_after(&config),
            70 * 60,
            "30 minutes, 2 attempts, 10 to set up"
        );
        config.timeout_minutes = 10;
        config.retries = 0;
        assert_eq!(stale_after(&config), 20 * 60);
    }

    #[test]
    fn a_quiet_run_is_stuck_only_after_a_job_could_have_timed_out() {
        let mut config: crate::config::RepoConfig = toml::from_str("").unwrap();
        assert_eq!(
            quiet_after(&config),
            Duration::from_secs(35 * 60),
            "30 by default"
        );
        config.timeout_minutes = 10;
        assert_eq!(quiet_after(&config), Duration::from_secs(15 * 60));
    }

    #[test]
    fn decides_what_a_head_needs() {
        let cutoff = "2026-10-23T09:00:00Z";
        let none = Checks::new();
        let mut claimed = Checks::new();
        claimed.insert(
            "shipcheck".into(),
            check("in_progress", "2026-10-23T11:00:00Z"),
        );
        let mut stale = Checks::new();
        stale.insert(
            "shipcheck/test".into(),
            check("in_progress", "2026-10-23T08:00:00Z"),
        );
        let mut queued = Checks::new();
        queued.insert("shipcheck".into(), check("queued", ""));

        assert_eq!(decide(&none, true, true, true, cutoff), Action::Local);
        assert_eq!(
            decide(&claimed, false, true, true, cutoff),
            Action::Post,
            "a stored result posts with no run"
        );
        assert_eq!(
            decide(&claimed, false, false, true, cutoff),
            Action::Claimed,
            "another machine runs it"
        );
        assert_eq!(
            decide(&stale, false, false, true, cutoff),
            Action::Run,
            "a claim from a run that died is taken over"
        );
        assert_eq!(decide(&queued, false, false, true, cutoff), Action::Run);
        assert_eq!(
            decide(&none, false, false, false, cutoff),
            Action::Queue,
            "a fork's PR or run_misses = false never runs"
        );
    }

    #[test]
    fn a_run_past_its_time_is_overdue() {
        let cutoff = "2026-10-23T09:00:00Z";
        let at = |status, started| Checks::from([("shipcheck".into(), check(status, started))]);
        assert!(overdue(&at("in_progress", "2026-10-23T08:59:59Z"), cutoff));
        assert!(!overdue(&at("in_progress", "2026-10-23T09:00:00Z"), cutoff));
        assert!(!overdue(&at("queued", ""), cutoff));
        assert!(
            !overdue(&at("completed", "2026-10-23T08:00:00Z"), cutoff),
            "a finished check is no run"
        );
    }

    #[test]
    fn runs_only_heads_its_own_user_or_a_listed_login_opened_and_pushed() {
        let none: &[String] = &[];
        let listed = ["Bob".to_string()];
        assert_eq!(
            pushed("ann", Some("Ann"), "ann", none),
            Pushed::Run,
            "logins match in any case"
        );
        assert_eq!(
            pushed("bob", Some("bob"), "ann", &listed),
            Pushed::Run,
            "main lists bob"
        );
        assert_eq!(
            pushed("ann", Some("bob"), "ann", &listed),
            Pushed::Run,
            "both allowed"
        );
        assert_eq!(
            pushed("ann", Some("eve"), "ann", &listed),
            Pushed::Other,
            "a teammate's push onto my PR"
        );
        assert_eq!(
            pushed("eve", Some("ann"), "ann", &listed),
            Pushed::Other,
            "my push onto a teammate's PR"
        );
        assert_eq!(
            pushed("eve", None, "ann", none),
            Pushed::Other,
            "no wait for a PR it would refuse"
        );
        assert_eq!(
            pushed("ann", None, "ann", &listed),
            Pushed::Wait,
            "no push event yet"
        );
        assert_eq!(pushed("", Some(""), "", &["".to_string()]), Pushed::Other);
    }

    #[test]
    fn a_head_on_top_of_a_refused_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        git(&["checkout", "-q", "-b", "teammate"]);
        git(&["commit", "-q", "--allow-empty", "-m", "theirs"]);
        let theirs = git(&["rev-parse", "HEAD"]);
        // Update branch: a merge by me that carries their commit
        git(&["checkout", "-q", "main"]);
        git(&["merge", "-q", "--no-ff", "-m", "update", "teammate"]);
        let update = git(&["rev-parse", "HEAD"]);
        git(&["checkout", "-q", "-b", "mine", "main~1"]);
        git(&["commit", "-q", "--allow-empty", "-m", "unrelated"]);
        let unrelated = git(&["rev-parse", "HEAD"]);

        let file = repo.join("refused.json");
        refuse(&file, &theirs, 1_000_000);
        let marks = refused(&file, 1_000_000);
        assert_eq!(tainted(repo, &update, &marks), Some(theirs.clone()));
        assert_eq!(
            tainted(repo, &unrelated, &marks),
            None,
            "an unrelated head still runs"
        );
        assert!(
            refused(&file, 1_000_000 + REFUSED_DAYS * 86_400).is_empty(),
            "marks expire"
        );
        // A head whose tree has a result posts before the gate is read
        let mut queued = Checks::new();
        queued.insert("shipcheck".into(), check("queued", ""));
        assert_eq!(decide(&queued, false, true, true, ""), Action::Post);
    }

    #[test]
    fn only_a_known_pusher_who_is_not_allowed_marks_a_head_refused() {
        let (_, reason, mark) = refusal(&Pushed::Other).unwrap();
        assert_eq!(reason, "pusher_not_allowed");
        assert!(mark);
        assert!(refusal(&Pushed::Wait).is_none());
        assert!(refusal(&Pushed::Run).is_none());
    }

    #[test]
    fn a_head_with_no_config_is_checked_again_when_the_base_moves() {
        let id = |repo: &str, sha: &str| (repo.to_string(), sha.to_string());
        let mut done = HashSet::from([id("acme/app", "a"), id("acme/lib", "b")]);
        let mut unconfigured = HashMap::from([
            (id("acme/app", "a"), "tip1".to_string()),
            (id("acme/lib", "b"), "tip1".to_string()),
        ]);
        recheck(&mut unconfigured, &mut done, "acme/app", "tip1");
        assert_eq!(done.len(), 2, "the base did not move");
        recheck(&mut unconfigured, &mut done, "acme/app", "tip2");
        assert_eq!(done, HashSet::from([id("acme/lib", "b")]));
        assert_eq!(unconfigured.len(), 1, "another repository's head stays");
    }

    #[test]
    fn a_replaced_head_gets_its_unfinished_checks_cancelled_and_keeps_the_finished_ones() {
        let mut queued = check("queued", "");
        queued.id = 7;
        let fake = Fake {
            existing: Checks::from([
                (check::NAME.to_string(), queued),
                ("shipcheck/fmt".to_string(), check("completed", "")),
            ]),
            ..Default::default()
        };
        cancel_replaced(&fake, "abc").unwrap();
        let sent = fake.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].1, Some(7));
        assert_eq!(sent[0].0["name"], check::NAME);
        assert_eq!(sent[0].0["conclusion"], "cancelled");
    }

    #[test]
    fn a_head_replaced_while_it_waited_is_abandoned() {
        let pr = |number, sha: &str| OpenPr {
            number,
            sha: sha.into(),
            base: "main".into(),
            head_repo: "acme/app".into(),
            branch: "fix".into(),
            author: "ann".into(),
        };
        let mut waits = BTreeMap::new();
        waits.insert(("acme/app".to_string(), 1), "old".to_string());
        waits.insert(("acme/app".to_string(), 2), "same".to_string());
        let prs = [pr(1, "new"), pr(2, "same")];
        assert_eq!(
            abandoned(&waits, "acme/app", &prs),
            vec![(1, "old".to_string())]
        );
        assert!(abandoned(&waits, "acme/lib", &prs).is_empty());
    }

    #[test]
    fn the_hooks_take_the_registered_base_and_never_ask_the_remote() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, origin) = (dir.path().join("app"), dir.path().join("acme/app.git"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&origin).unwrap();
        let git = |at: &Path, args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(at)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&origin, &["init", "-q", "--bare", "-b", "master"]);
        git(&repo, &["init", "-q", "-b", "master"]);
        git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "a",
            ],
        );
        git(&repo, &["push", "-q", "origin", "master"]);
        let file = dir.path().join("daemon.toml");

        // The remote says master, so only a call to it gives master here
        assert_eq!(command_base_in(&file, "run", &repo), "origin/master");

        for hook in ["on-commit", "on-push"] {
            assert_eq!(command_base_in(&file, hook, &repo), "origin/main");
        }

        // Only the remote's default branch posts until the daemon names a base
        assert!(posts_from_in(&file, "origin/master", &repo));
        assert!(!posts_from_in(&file, "origin/main", &repo));
        assert!(!posts_from_in(&file, "origin/dev", &repo));

        DaemonConfig::add_to(&file, "acme/app", &repo, "origin/dev").unwrap();

        // This checkout's entry comes first for every command
        for command in ["on-commit", "on-push", "run", "post"] {
            assert_eq!(command_base_in(&file, command, &repo), "origin/dev");
        }

        assert!(posts_from_in(&file, "origin/dev", &repo));
        assert!(posts_from_in(&file, "origin/master", &repo));
        assert!(!posts_from_in(&file, "origin/evil", &repo));
        assert!(!posts_from_in(&file, "evil", &repo));

        // Another clone of the same repository does not get the entry's base
        let other = dir.path().join("other");
        git(
            dir.path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        assert!(!posts_from_in(&file, "origin/dev", &other));
        assert!(posts_from_in(&file, "origin/master", &other));

        // Offline, nothing posts, not even the fallback origin/main
        git(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                dir.path().join("gone").to_str().unwrap(),
            ],
        );
        DaemonConfig::add_to(&file, "acme/app", &other, "origin/dev").unwrap();
        assert!(!posts_from_in(&file, "origin/master", &repo));
        assert!(!posts_from_in(&file, "origin/main", &repo));

        // With no entry for this checkout and no answer, a command falls back to origin/main
        assert_eq!(command_base_in(&file, "run", &repo), "origin/main");
        assert_eq!(command_base_in(&file, "on-push", &repo), "origin/main");
        assert!(!main_is_default(&repo));
    }

    #[test]
    fn add_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("home/daemon.toml");
        let add =
            |repo, path, base| DaemonConfig::add_to(&file, repo, Path::new(path), base).unwrap();
        assert!(add("acme/app", "/src/app", "origin/main"));
        assert!(
            !add("acme/app", "/src/app", "origin/main"),
            "a second install changes nothing"
        );
        assert!(add("acme/lib", "/src/lib", "origin/dev"));
        let cfg = DaemonConfig::load_from(&file).unwrap();
        assert_eq!(cfg.repos.len(), 2);
        assert_eq!(cfg.repos["acme/lib"].base, "origin/dev");
    }

    #[test]
    fn config_rejects_unknown_fields() {
        assert!(toml::from_str::<DaemonConfig>("repos = {}\npoll = 5\n").is_err());
    }

    #[test]
    fn handles_only_prs_into_its_branch() {
        let open = || {
            vec![
                open_pr(1, "a", "main", "acme/app"),
                open_pr(2, "b", "dev", "acme/app"),
                open_pr(3, "c", "origin/main", "acme/app"),
            ]
        };
        let numbers = |prs: Vec<OpenPr>| prs.iter().map(|pr| pr.number).collect::<Vec<_>>();
        assert_eq!(numbers(into_base(open(), "origin/main")), vec![1]);
        assert_eq!(numbers(into_base(open(), "main")), vec![1]);
        assert_eq!(numbers(into_base(open(), "origin/dev")), vec![2]);
        assert!(into_base(open(), "origin/release").is_empty());
    }

    fn open_pr(number: u64, sha: &str, base: &str, head_repo: &str) -> OpenPr {
        OpenPr {
            number,
            sha: sha.into(),
            base: base.into(),
            head_repo: head_repo.into(),
            branch: "fix".into(),
            author: "ann".into(),
        }
    }

    const ACT: &str = "workflows = [\"ci.yml\"]\n";
    const HOST: &str = "runner = \"host\"\n\n[host]\nchecks = { test = \"true\" }\n";

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A checkout with `config` on origin/main and a PR head on top, and that head
    fn checkout(origin: &str, config: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        std::fs::create_dir_all(path.join(".github/workflows")).unwrap();
        let workflow = "on: pull_request\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n";
        std::fs::write(path.join(".github/workflows/ci.yml"), workflow).unwrap();
        if !config.is_empty() {
            std::fs::write(path.join("shipcheck.toml"), config).unwrap();
        }

        run_git(path, &["init", "-q", "-b", "main"]);
        run_git(path, &["add", "-A"]);
        run_git(path, &["commit", "-q", "-m", "base"]);
        run_git(path, &["remote", "add", "origin", origin]);
        run_git(path, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        std::fs::write(path.join("change.txt"), "x").unwrap();
        run_git(path, &["add", "-A"]);
        run_git(path, &["commit", "-q", "-m", "head"]);
        let sha = run_git(path, &["rev-parse", "HEAD"]);
        (dir, sha)
    }

    /// Runs test `name` again in a child with a home of its own, and says whether this is the child
    fn own_home(name: &str) -> bool {
        if std::env::var_os("SHIPCHECK_DAEMON_CHILD").is_some() {
            return true;
        }

        let home = tempfile::tempdir().unwrap();
        // An act-mode plan reads `act --version`, and GitHub's test job has no act
        let bin = home.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("act"), "#!/bin/sh\necho act version 0.0.0\n").unwrap();
        std::fs::set_permissions(
            bin.join("act"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("daemon::tests::{name}")])
            .env("SHIPCHECK_DAEMON_CHILD", "1")
            .env("SHIPCHECK_HOME", home.path())
            .env("PATH", path)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && said.contains("1 passed"), "{said}");
        false
    }

    /// GitHub as `handle` sees it: the checks it was sent, and a rival's claim when set
    #[derive(Default)]
    struct Fake {
        existing: Checks,
        pusher: Option<String>,
        rival: Option<u64>,
        sent: Mutex<Vec<(Value, Option<u64>)>>,
    }

    impl Remote for Fake {
        fn send(&self, body: &Value, id: Option<u64>) -> Result<(u64, String), String> {
            let mut sent = self.sent.lock().unwrap();
            sent.push((body.clone(), id));
            Ok((id.unwrap_or(100 + sent.len() as u64), String::new()))
        }

        fn find_checks(&self, _sha: &str) -> Result<Checks, String> {
            let sent = self.sent.lock().unwrap();
            let mut found = self.existing.clone();

            for (i, (body, id)) in sent.iter().enumerate() {
                let mut c = check(body["status"].as_str().unwrap(), "");
                c.id = id.unwrap_or(101 + i as u64);
                found.insert(body["name"].as_str().unwrap().into(), c);
            }

            if let Some(id) = self.rival.filter(|_| !sent.is_empty()) {
                let mut c = check("in_progress", "");
                c.id = id;
                found.insert(check::NAME.into(), c);
            }

            Ok(found)
        }

        fn pusher(
            &self,
            _pr: u64,
            _branch: &str,
            _sha: &str,
            _trusted: &[String],
        ) -> Result<Option<(String, String)>, String> {
            Ok(self.pusher.clone().map(|l| (l, "events".to_string())))
        }

        fn comment(
            &self,
            _prs: Option<&[u64]>,
            _base: &str,
            _sha: &str,
            _entry: &run::Entry,
            _secrets: &[String],
        ) -> Result<Vec<String>, String> {
            Ok(Vec::new())
        }
    }

    impl Fake {
        fn by(pusher: &str) -> Self {
            Fake {
                pusher: Some(pusher.into()),
                ..Fake::default()
            }
        }

        fn bodies(&self) -> Vec<Value> {
            let sent = self.sent.lock().unwrap();
            sent.iter().map(|(body, _)| body.clone()).collect()
        }

        fn statuses(&self) -> Vec<String> {
            let bodies = self.bodies();
            bodies
                .iter()
                .map(|b| b["status"].as_str().unwrap().to_string())
                .collect()
        }
    }

    type Handled = (Result<Option<&'static str>, String>, Vec<String>);

    /// `handle` on `pr` of `repo` as login ann, with the names of the runs it started
    fn run_handle(fake: &Fake, repo: &str, path: &Path, pr: &OpenPr) -> Handled {
        let config = DaemonConfig {
            interval_seconds: 10,
            run_misses: true,
            login: Some("ann".into()),
            repos: BTreeMap::new(),
            home_ok: false,
        };
        let entry = RepoEntry {
            path: path.to_path_buf(),
            base: "origin/main".into(),
        };
        let started = Mutex::new(Vec::new());
        let start = |_: &run::Plan, name: &str| started.lock().unwrap().push(name.to_string());
        let mut waits = BTreeMap::new();
        let got = handle(
            &config,
            repo,
            &entry,
            fake,
            pr,
            Duration::ZERO,
            &mut waits,
            &start,
        );
        (got, started.into_inner().unwrap())
    }

    /// The `skipped` reasons in this home's records of `repo`
    fn skips(repo: &str) -> Vec<String> {
        let text = std::fs::read_to_string(record::file(&run::home(), repo)).unwrap_or_default();
        text.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["event"] == "skipped")
            .map(|v| v["reason"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_head_with_a_completed_shipcheck_check_sends_nothing() {
        let mut fake = Fake::by("ann");
        fake.existing
            .insert(check::NAME.into(), check("completed", ""));
        let pr = open_pr(5, "abc", "main", "acme/app");
        let (got, started) = run_handle(&fake, "acme/app", Path::new("/nonexistent"), &pr);
        assert_eq!(got, Ok(Some("already checked")));
        assert!(fake.bodies().is_empty() && started.is_empty());
    }

    #[test]
    fn host_mode_never_checks_a_forks_pr() {
        let (dir, sha) = checkout("https://github.com/acme/app.git", HOST);
        let fake = Fake::by("ann");
        let pr = open_pr(5, &sha, "main", "eve/app");
        let (got, started) = run_handle(&fake, "acme/app", dir.path(), &pr);
        assert_eq!(
            got,
            Ok(Some("not checked, host mode never checks a fork's PR"))
        );
        assert!(fake.bodies().is_empty() && started.is_empty());
    }

    #[test]
    fn no_shipcheck_toml_on_main_starts_nothing() {
        let (dir, sha) = checkout("https://github.com/acme/app.git", "");
        let fake = Fake::by("ann");
        let pr = open_pr(5, &sha, "main", "acme/app");
        let (got, started) = run_handle(&fake, "acme/app", dir.path(), &pr);
        assert_eq!(got, Ok(Some(NO_CONFIG)));
        assert!(fake.bodies().is_empty() && started.is_empty());
    }

    #[test]
    fn a_mixed_config_never_checks_a_forks_pr() {
        // Its plan reads `act --version`, which own_home fakes, since GitHub's test job has no act
        if !own_home("a_mixed_config_never_checks_a_forks_pr") {
            return;
        }

        // Act is the default runner here, still the host checks beside its job run natively
        let mixed = "[host]\nchecks = { lint = \"true\" }\n\n[jobs.\"ci.yml/test\"]\n";
        let (dir, sha) = checkout("https://github.com/acme/app.git", mixed);
        let fake = Fake::by("ann");
        let pr = open_pr(5, &sha, "main", "eve/app");
        let (got, started) = run_handle(&fake, "acme/app", dir.path(), &pr);
        assert_eq!(
            got,
            Ok(Some("not checked, host mode never checks a fork's PR"))
        );
        assert!(fake.bodies().is_empty() && started.is_empty());
    }

    /// A failed result for the head stored in this home, with a log a same-repo PR's checks show
    fn store_failure(path: &Path, sha: &str) {
        let plan = run::plan(path, sha, "origin/main").unwrap();
        let key = plan.key.clone();
        let logs = run::log_dir(&key);
        std::fs::create_dir_all(logs.join("ci.yml")).unwrap();
        std::fs::write(logs.join("ci.yml/test.log"), "boom from the head\n").unwrap();
        std::fs::write(logs.join("ci.yml.log"), "boom from the head\n").unwrap();
        let result = run::Entry {
            key: key.clone(),
            sha: sha.into(),
            tree: plan.tree,
            base: plan.base,
            passed: false,
            workflows: vec![run::WorkflowResult {
                file: "ci.yml".into(),
                exit: Some(1),
                attempts: 1,
                log_sha256: crate::cache::hex(&<sha2::Sha256 as sha2::Digest>::digest(
                    b"boom from the head\n",
                )),
                jobs: BTreeMap::from([("test".into(), "failure".into())]),
                passed: false,
                shards: BTreeMap::new(),
                times: BTreeMap::new(),
                summaries: BTreeMap::new(),
            }],
            finished_unix: 0,
            skipped: None,
            ci_changes: Vec::new(),
        };
        let cache = run::home().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let text = serde_json::to_string(&result).unwrap();
        std::fs::write(cache.join(format!("{key}.json")), text).unwrap();
    }

    #[test]
    fn a_forks_pr_never_runs_and_gets_no_log_tail() {
        if !own_home("a_forks_pr_never_runs_and_gets_no_log_tail") {
            return;
        }

        let (dir, sha) = checkout("https://github.com/acme/app.git", ACT);
        let fork = open_pr(5, &sha, "main", "eve/app");
        let fake = Fake::by("ann");
        let (got, started) = run_handle(&fake, "acme/app", dir.path(), &fork);
        assert_eq!(
            got,
            Ok(Some("queued, the daemon never runs a fork's PR")),
            "a fork's PR would be polled again every cycle"
        );
        assert!(started.is_empty(), "a fork's PR started a run");
        assert!(!fake.statuses().is_empty());
        assert!(
            fake.statuses().iter().all(|s| s == "queued"),
            "{:?}",
            fake.statuses()
        );

        store_failure(dir.path(), &sha);
        let tail = |fake: &Fake| fake.bodies().iter().any(|b| b.to_string().contains("boom"));
        let fake = Fake::by("ann");
        let (got, started) = run_handle(&fake, "acme/app", dir.path(), &fork);
        assert_eq!(got, Ok(Some("posted failure")));
        assert!(started.is_empty());
        assert!(!tail(&fake), "a fork's check holds its log tail");

        let fake = Fake::by("ann");
        let same = open_pr(5, &sha, "main", "acme/app");
        let (got, _) = run_handle(&fake, "acme/app", dir.path(), &same);
        assert_eq!(got, Ok(Some("posted failure")));
        // A home with no keychain, such as a host check's jail, posts no tail for any PR
        if run::secret_values("acme/app").is_ok() {
            assert!(tail(&fake), "a same-repo PR's check holds its log tail");
        }
    }

    #[test]
    fn a_repository_with_no_config_on_main_is_not_polled_again() {
        if !own_home("a_repository_with_no_config_on_main_is_not_polled_again") {
            return;
        }

        let (dir, _) = checkout("https://github.com/acme/app.git", ACT);
        run_git(dir.path(), &["rm", "-q", "shipcheck.toml"]);
        run_git(dir.path(), &["commit", "-q", "-m", "no config"]);
        run_git(
            dir.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        std::fs::write(dir.path().join("more.txt"), "y").unwrap();
        run_git(dir.path(), &["add", "-A"]);
        run_git(dir.path(), &["commit", "-q", "-m", "head"]);
        let sha = run_git(dir.path(), &["rev-parse", "HEAD"]);
        let fake = Fake::by("ann");
        let pr = open_pr(5, &sha, "main", "acme/app");
        let (got, started) = run_handle(&fake, "acme/app", dir.path(), &pr);
        assert_eq!(got, Ok(Some("not checked, no shipcheck.toml on main")));
        assert!(fake.bodies().is_empty() && started.is_empty());
    }

    #[test]
    fn a_claim_another_machine_won_starts_no_run() {
        if !own_home("a_claim_another_machine_won_starts_no_run") {
            return;
        }

        let (dir, sha) = checkout("https://github.com/acme/app.git", HOST);
        let mut fake = Fake::by("ann");
        fake.rival = Some(999);
        let pr = open_pr(5, &sha, "main", "acme/app");
        let (got, started) = run_handle(&fake, "acme/app", dir.path(), &pr);
        assert_eq!(got, Ok(None));
        assert!(started.is_empty(), "both machines run it");
        assert_eq!(skips("acme/app"), vec!["claimed_by_another_machine"]);
    }

    #[test]
    fn a_claim_this_machine_won_starts_the_run() {
        if !own_home("a_claim_this_machine_won_starts_the_run") {
            return;
        }

        let (dir, sha) = checkout("https://github.com/acme/app.git", HOST);
        let fake = Fake::by("ann");
        let pr = open_pr(5, &sha, "main", "acme/app");
        let (got, started) = run_handle(&fake, "acme/app", dir.path(), &pr);
        assert_eq!(got, Ok(None));
        assert_eq!(
            started,
            vec!["fix"],
            "the branch's record, which a push to it shares, so the newer head stops the older"
        );
        assert_eq!(
            fake.bodies()[0]["name"],
            check::NAME,
            "the claim goes first"
        );
        assert!(fake.statuses().iter().all(|s| s == "in_progress"));
    }

    #[test]
    fn a_head_replaced_before_any_verdict_still_gets_its_checks_cancelled() {
        if !own_home("a_head_replaced_before_any_verdict_still_gets_its_checks_cancelled") {
            return;
        }

        let mut queued = check("queued", "");
        queued.id = 7;
        let fake = Fake {
            existing: Checks::from([(check::NAME.to_string(), queued)]),
            ..Default::default()
        };
        let mut heads = BTreeMap::new();
        track_heads(
            "acme/app",
            &[open_pr(5, "first", "main", "acme/app")],
            &mut heads,
            &fake,
        );
        assert!(
            fake.sent.lock().unwrap().is_empty(),
            "a new head cancels nothing"
        );
        track_heads(
            "acme/app",
            &[open_pr(5, "second", "main", "acme/app")],
            &mut heads,
            &fake,
        );
        let sent = fake.sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(
            (sent[0].0["head_sha"].as_str(), sent[0].1),
            (Some("first"), Some(7))
        );
        assert_eq!(sent[0].0["conclusion"], "cancelled");
        assert_eq!(heads[&("acme/app".to_string(), 5)], "second");
    }

    #[test]
    fn a_refused_pusher_is_skipped_and_queued() {
        if !own_home("a_refused_pusher_is_skipped_and_queued") {
            return;
        }

        let (dir, sha) = checkout("https://github.com/acme/app.git", HOST);
        let fake = Fake::by("eve");
        let pr = open_pr(5, &sha, "main", "acme/app");
        let (got, started) = run_handle(&fake, "acme/app", dir.path(), &pr);
        assert!(got.unwrap().unwrap().starts_with("not run"));
        assert!(started.is_empty());
        assert_eq!(skips("acme/app"), vec!["pusher_not_allowed"]);
        assert!(!fake.statuses().is_empty());
        assert!(
            fake.statuses().iter().all(|s| s == "queued"),
            "{:?}",
            fake.statuses()
        );
        assert!(refused(&refused_path("acme/app"), run::unix_now()).contains_key(&sha));
    }

    #[test]
    fn a_same_repo_pr_matches_its_repository_in_any_case() {
        if !own_home("a_same_repo_pr_matches_its_repository_in_any_case") {
            return;
        }

        // The daemon keys a repository by its origin URL, GitHub names it in its own case
        let (dir, sha) = checkout("https://github.com/Acme/App.git", HOST);
        let repo = run::repo_id(dir.path()).unwrap();
        assert_eq!(repo, "Acme/App");
        let fake = Fake::by("ann");
        let pr = open_pr(5, &sha, "main", "acme/app");
        let (got, _) = run_handle(&fake, &repo, dir.path(), &pr);
        assert_ne!(
            got,
            Ok(Some("not checked, host mode never checks a fork's PR")),
            "acme/app is Acme/App, not a fork"
        );
    }
}
