use crate::cache;
use crate::clock;
use crate::config::RepoConfig;
use crate::daemon_config::DaemonConfig;
use crate::daemon_config::Fetch;
use crate::daemon_config::RepoEntry;
use crate::git;
use crate::github::Check;
use crate::history;
use crate::paths;
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

fn check(status: &str, started_at: &str) -> Check {
    Check {
        id: 1,
        status: status.into(),
        started_at: started_at.into(),
    }
}

#[test]
fn a_claim_goes_stale_once_every_attempt_could_have_timed_out() {
    let mut config: RepoConfig = toml::from_str("").unwrap();
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

    assert_eq!(
        decide(&none, Here::Running, Miss::Run, cutoff),
        Action::Local
    );
    assert_eq!(
        decide(&claimed, Here::Result, Miss::Run, cutoff),
        Action::Post,
        "a stored result posts with no run"
    );
    assert_eq!(
        decide(&claimed, Here::Nothing, Miss::Run, cutoff),
        Action::Claimed,
        "another machine runs it"
    );
    assert_eq!(
        decide(&stale, Here::Nothing, Miss::Run, cutoff),
        Action::Run,
        "a claim from a run that died is taken over"
    );
    assert_eq!(
        decide(&queued, Here::Nothing, Miss::Run, cutoff),
        Action::Run
    );
    assert_eq!(
        decide(&none, Here::Nothing, Miss::Queue, cutoff),
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
    assert_eq!(pushed("", Some(""), "", &[String::new()]), Pushed::Other);
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
    assert_eq!(tainted(repo, &update, &marks), Some(theirs));
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
    assert_eq!(decide(&queued, Here::Result, Miss::Run, ""), Action::Post);
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
    assert_eq!(
        abandoned(&waits, "acme/lib", &prs),
        Vec::<(u64, std::string::String)>::new()
    );
}

#[test]
fn add_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join("home/daemon.toml");
    let add = |repo, path, base| DaemonConfig::add_to(&file, repo, Path::new(path), base).unwrap();
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
    pull: Option<Value>,
    sent: Mutex<Vec<(Value, Option<u64>)>>,
}

impl Remote for Fake {
    fn send(&self, body: &Value, id: Option<u64>) -> Result<(u64, String), String> {
        let mut sent = self.sent.lock().unwrap();
        sent.push((body.clone(), id));
        Ok((
            id.unwrap_or(100 + u64::try_from(sent.len()).expect("a count fits a u64")),
            String::new(),
        ))
    }

    fn find_checks(&self, _sha: &str) -> Result<Checks, String> {
        let sent = self.sent.lock().unwrap();
        let mut found = self.existing.clone();

        for (i, (body, id)) in sent.iter().enumerate() {
            let mut c = check(body["status"].as_str().unwrap(), "");
            c.id = id.unwrap_or(101 + u64::try_from(i).expect("a count fits a u64"));
            found.insert(body["name"].as_str().unwrap().into(), c);
        }

        if let Some(id) = self.rival.filter(|_| !sent.is_empty()) {
            let mut c = check("in_progress", "");
            c.id = id;
            found.insert(check::NAME.into(), c);
        }

        Ok(found)
    }

    fn pull(&self, _number: u64) -> Result<Option<Value>, String> {
        Ok(self.pull.clone())
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
        home_ok: MovedHome::Refused,
        fetch: Fetch::Origin,
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
    let text = std::fs::read_to_string(history::file(&paths::home(), repo)).unwrap_or_default();
    text.lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["event"] == "skipped")
        .map(|v| v["reason"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_stuck_run_fails_its_checks_and_the_head_never_runs_again() {
    let mut fake = Fake::by("ann");
    let names = vec!["shipcheck/test".to_string(), check::NAME.to_string()];

    for name in &names {
        let mut c = check("in_progress", "2026-10-31T15:00:00Z");
        c.id = if name == check::NAME { 1 } else { 2 };
        fake.existing.insert(name.clone(), c);
    }

    let send = |b: &Value, id| fake.send(b, id);
    fail_stuck(
        &send,
        &fake.existing,
        &names,
        "abc",
        "made no progress for 15 minutes",
    )
    .unwrap();
    let bodies = fake.bodies();
    assert_eq!(bodies.len(), 2, "{bodies:?}");

    for body in &bodies {
        assert_eq!(body["status"], "completed");
        assert_eq!(body["conclusion"], "failure");
        assert!(
            body["output"]["text"]
                .as_str()
                .unwrap()
                .contains("made no progress")
        );
    }

    // The next poll sees the failed checks GitHub now holds and starts nothing
    let mut next = Fake::by("ann");
    next.existing = fake.find_checks("abc").unwrap();
    let pr = open_pr(5, "abc", "main", "acme/app");
    let (got, started) = run_handle(&next, "acme/app", Path::new("/nonexistent"), &pr);
    assert_eq!(got, Ok(Some("already checked")));
    assert!(next.bodies().is_empty() && started.is_empty());
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
fn fetch_https_reaches_a_github_ssh_remote_over_https_and_origin_leaves_it() {
    let dir = tempfile::tempdir().unwrap();
    run_git(dir.path(), &["init", "-q"]);
    let url = |fetch: Fetch, remote: &str| {
        let _ = Command::new("git")
            .args(["remote", "remove", "origin"])
            .current_dir(dir.path())
            .output();
        run_git(dir.path(), &["remote", "add", "origin", remote]);
        let mut args: Vec<String> = fetch.options();
        args.extend(["ls-remote", "--get-url", "origin"].map(String::from));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git(dir.path(), &args)
    };

    for remote in [
        "git@github.com:acme/app.git",
        "ssh://git@github.com/acme/app.git",
    ] {
        assert_eq!(url(Fetch::Https, remote), "https://github.com/acme/app.git");
        assert_eq!(url(Fetch::Origin, remote), remote);
    }

    // Another host keeps its own URL
    assert_eq!(
        url(Fetch::Https, "git@gitlab.com:acme/app.git"),
        "git@gitlab.com:acme/app.git"
    );

    let config: DaemonConfig = toml::from_str("fetch = \"https\"").unwrap();
    assert_eq!(config.fetch, Fetch::Https);
    let written = toml::to_string(&toml::from_str::<DaemonConfig>("").unwrap()).unwrap();
    assert!(!written.contains("fetch"), "{written}");
}

#[test]
fn only_an_unwatched_clone_with_a_shipcheck_toml_is_a_candidate() {
    let root = tempfile::tempdir().unwrap();
    let clone = |name: &str, config: bool| {
        let path = root.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        if config {
            std::fs::write(path.join("shipcheck.toml"), HOST).unwrap();
        }
        std::fs::write(path.join("README"), name).unwrap();
        run_git(&path, &["init", "-q", "-b", "main"]);
        run_git(&path, &["add", "-A"]);
        run_git(&path, &["commit", "-q", "-m", "base"]);
        let url = format!("https://github.com/acme/{name}.git");
        run_git(&path, &["remote", "add", "origin", &url]);
        run_git(&path, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        path
    };
    let watched = clone("watched", true);
    let wanted = clone("wanted", true);
    clone("plain", false);
    run_git(&wanted, &["worktree", "add", "-q", "../wanted-tree"]);
    let config = DaemonConfig {
        interval_seconds: 10,
        run_misses: true,
        login: None,
        repos: BTreeMap::from([(
            "acme/watched".to_string(),
            RepoEntry {
                path: watched,
                base: "origin/main".into(),
            },
        )]),
        home_ok: MovedHome::Refused,
        fetch: Fetch::Origin,
    };
    let mut seen = HashSet::new();
    let found = candidates(&config, &mut seen);
    assert_eq!(found, [(wanted, "acme/wanted".to_string())]);
    assert!(candidates(&config, &mut seen).is_empty(), "asked twice");
}

/// `handle_main` on the checkout at `path`, with the names of the runs it started
fn run_main(fake: &Fake, path: &Path) -> (Result<&'static str, String>, Vec<String>) {
    let config = DaemonConfig {
        interval_seconds: 10,
        run_misses: true,
        login: Some("ann".into()),
        repos: BTreeMap::new(),
        home_ok: MovedHome::Refused,
        fetch: Fetch::Origin,
    };
    let entry = RepoEntry {
        path: path.to_path_buf(),
        base: "origin/main".into(),
    };
    let started = Mutex::new(Vec::new());
    let start = |_: &run::Plan, name: &str| started.lock().unwrap().push(name.to_string());
    let got = handle_main(&config, &entry, fake, &start);
    (got, started.into_inner().unwrap())
}

#[test]
fn a_merge_checks_the_tip_of_main_unless_it_has_checks() {
    let (dir, _) = checkout("https://github.com/acme/app.git", HOST);
    let (got, started) = run_main(&Fake::default(), dir.path());
    assert_eq!(got, Ok("checking its tip"));
    assert_eq!(started, ["main"]);

    let mut checked = Fake::default();
    checked
        .existing
        .insert(check::NAME.into(), check("completed", ""));
    let (got, started) = run_main(&checked, dir.path());
    assert_eq!(got, Ok("already checked"));
    assert_eq!(started, Vec::<std::string::String>::new());

    // A release bump on main carries the skip-CI marker
    run_git(
        dir.path(),
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "chore: bump [skip ci]",
        ],
    );
    run_git(
        dir.path(),
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    let (got, started) = run_main(&Fake::default(), dir.path());
    assert_eq!(got, Ok("checking its tip"), "the tip a badge reads");
    assert_eq!(started, ["main"]);

    let (bare, _) = checkout("https://github.com/acme/app.git", "");
    let (got, started) = run_main(&Fake::default(), bare.path());
    assert_eq!(got, Ok(NO_CONFIG));
    assert_eq!(started, Vec::<std::string::String>::new());
}

#[test]
fn a_repository_whose_events_socket_is_open_and_settled_is_read_every_five_minutes() {
    let ten = Duration::from_secs(10);
    let s = Duration::from_secs;
    assert!(
        due(Socket::Open, Heads::Settled, None, ten),
        "never read yet"
    );
    assert!(!due(Socket::Open, Heads::Settled, Some(s(60)), ten));
    assert!(due(Socket::Open, Heads::Settled, Some(s(300)), ten));
    // A head waiting, such as for its pusher, or no socket, polls as before
    assert!(due(Socket::Open, Heads::Unsettled, Some(ten), ten));
    assert!(!due(Socket::Open, Heads::Unsettled, Some(s(9)), ten));
    assert!(due(Socket::Closed, Heads::Settled, Some(ten), ten));
}

#[test]
fn a_failed_events_socket_waits_longer_each_time_and_a_socket_that_lasted_starts_over() {
    let s = Duration::from_secs;
    let mut wait = FIRST_RETRY / 2;
    let waits: Vec<u64> = (0..8)
        .map(|_| {
            wait = backoff(wait, s(1));
            wait.as_secs()
        })
        .collect();
    assert_eq!(waits, [10, 20, 40, 80, 160, 320, 600, 600]);
    assert_eq!(backoff(s(600), s(301)), FIRST_RETRY);
}

#[test]
fn a_pr_that_left_checks_the_tip_when_only_github_can_say_it_merged() {
    // A merge writes a `pr_merged` record
    if !own_home("a_pr_that_left_checks_the_tip_when_only_github_can_say_it_merged") {
        return;
    }

    let open = [open_pr(2, "b", "main", "acme/app")];
    let heads = |n: u64| {
        BTreeMap::from([
            (("acme/app".to_string(), n), "a".to_string()),
            (("acme/app".to_string(), 2), "b".to_string()),
        ])
    };

    // Through the service, which reads no closed PR
    let mut left = heads(1);
    assert!(merges(
        "acme/app",
        &Fake::default(),
        &open,
        &mut left,
        "main"
    ));
    assert_eq!(left.len(), 1, "the PR that left is forgotten");

    let closed = |merged: bool, base: &str| Fake {
        pull: Some(serde_json::json!({
            "state": "closed",
            "merged_at": if merged { Value::from("2026-11-05T00:00:00Z") } else { Value::Null },
            "base": {"ref": base},
        })),
        ..Fake::default()
    };
    assert!(merges(
        "acme/app",
        &closed(true, "main"),
        &open,
        &mut heads(1),
        "main"
    ));
    assert!(!merges(
        "acme/app",
        &closed(false, "main"),
        &open,
        &mut heads(1),
        "main"
    ));
    assert!(!merges(
        "acme/app",
        &closed(true, "dev"),
        &open,
        &mut heads(1),
        "main"
    ));

    let mut still = heads(2);
    assert!(!merges(
        "acme/app",
        &Fake::default(),
        &open,
        &mut still,
        "main"
    ));
    assert_eq!(still.len(), 1);
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
    let logs = paths::log_dir(&key);
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
            log_sha256: cache::hex(&<sha2::Sha256 as sha2::Digest>::digest(
                b"boom from the head\n",
            )),
            jobs: BTreeMap::from([("test".into(), "failure".into())]),
            passed: false,
            shards: BTreeMap::new(),
            kept: BTreeMap::new(),
            times: BTreeMap::new(),
            summaries: BTreeMap::new(),
        }],
        finished_unix: 0,
        skipped: None,
        ci_changes: Vec::new(),
    };
    let cache = paths::home().join("cache");
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
    assert_ne!(fake.statuses(), Vec::<std::string::String>::new());
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
    assert_eq!(started, Vec::<std::string::String>::new());
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
    assert_eq!(started, Vec::<std::string::String>::new());
    assert_eq!(skips("acme/app"), vec!["pusher_not_allowed"]);
    assert_ne!(fake.statuses(), Vec::<std::string::String>::new());
    assert!(
        fake.statuses().iter().all(|s| s == "queued"),
        "{:?}",
        fake.statuses()
    );
    assert!(refused(&refused_path("acme/app"), clock::unix_now()).contains_key(&sha));
}

#[test]
fn a_same_repo_pr_matches_its_repository_in_any_case() {
    if !own_home("a_same_repo_pr_matches_its_repository_in_any_case") {
        return;
    }

    // The daemon keys a repository by its origin URL, GitHub names it in its own case
    let (dir, sha) = checkout("https://github.com/Acme/App.git", HOST);
    let repo = git::repo_id(dir.path()).unwrap();
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
