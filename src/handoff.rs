//! Runs asked for from inside another sandbox, where a host check's own cannot start: the command
//! leaves a request in the worktree's git dir and the daemon starts the run outside

use crate::cache;
use crate::daemon_config::{DaemonConfig, RepoEntry};
use crate::paths;
use crate::run;
#[cfg(target_os = "macos")]
use crate::runner::host;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// How long the command waits for the daemon to take its request
const TAKEN: Duration = Duration::from_secs(15);

/// A request older than this was given up on by its command
const STALE: Duration = Duration::from_secs(60);

/// Runs the daemon serves at once. A request past it waits in its folder
const AT_ONCE: usize = 8;

const MAX_BYTES: u64 = 16 * 1024;

/// How long the output of a served request stays
const KEPT: Duration = Duration::from_hours(168);

static SERVING: AtomicUsize = AtomicUsize::new(0);

/**
 * What a sandboxed command asks the daemon to run. It names a commit, never a path, a base or a
 * home: the daemon takes the repository from the folder the request lay in and the base from its
 * own entry, so a request cannot point a run at another repository or another shipcheck.toml
 **/
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the request carries the command's flags as they are"
)]
pub struct Request {
    /// `run`, `status`, `logs` or `wait`
    pub cmd: String,
    /// The commit, 40 hex digits
    #[serde(default)]
    pub sha: Option<String>,
    /// A run's id, for `status` and `wait`
    #[serde(default)]
    pub target: Option<String>,
    /// The commit is a snapshot of the working copy or the index, which `shipcheck commit` commits
    #[serde(default)]
    pub snapshot: bool,
    #[serde(default)]
    pub staged: bool,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub jobs: Vec<String>,
    #[serde(default)]
    pub workflows: Vec<String>,
    #[serde(default)]
    pub only_failed: bool,
    #[serde(default)]
    pub json: bool,
    #[serde(default)]
    pub no_fetch: bool,
    #[serde(default)]
    pub no_heartbeat: bool,
    #[serde(default)]
    pub timeout: Option<String>,
}

fn hex(text: &str, lengths: std::ops::RangeInclusive<usize>) -> bool {
    lengths.contains(&text.len()) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A job, workflow or branch name that cannot be read as an option or a path out of the repository
fn name(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && !text.starts_with('-')
        && !text.contains("..")
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-/".contains(&b))
}

/// `90`, `90s`, `10m` or `1h`, as `wait --timeout` takes it
fn duration(text: &str) -> bool {
    let digits = text.strip_suffix(['s', 'm', 'h']).unwrap_or(text);
    (1..=6).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

impl Request {
    pub fn validate(&self) -> Result<(), String> {
        if !["run", "status", "logs", "wait"].contains(&self.cmd.as_str()) {
            return Err("unknown command".into());
        }

        match (&self.sha, &self.target) {
            (Some(sha), None) if hex(sha, 40..=40) => {}
            (None, Some(id)) if hex(id, 6..=64) && self.cmd != "run" => {}
            _ => return Err("needs one commit or one run id".into()),
        }

        if !self
            .jobs
            .iter()
            .chain(&self.workflows)
            .all(|n| name(n, 100))
        {
            return Err("a job or workflow name is not one".into());
        }

        if self.branch.as_ref().is_some_and(|b| !name(b, 200)) {
            return Err("the branch name is not one".into());
        }

        if self.timeout.as_ref().is_some_and(|t| !duration(t)) {
            return Err("the timeout is not a duration".into());
        }

        Ok(())
    }

    /// The command line the daemon runs, built from the fields and never copied from the request
    pub fn argv(&self) -> Vec<String> {
        let mut args = vec![self.cmd.clone()];

        match self.cmd.as_str() {
            "run" => {
                args.extend(self.no_fetch.then(|| "--no-fetch".to_string()));
                args.extend(self.only_failed.then(|| "--only-failed".to_string()));
                args.extend(self.json.then(|| "--json".to_string()));
                args.extend(self.no_heartbeat.then(|| "--no-heartbeat".to_string()));

                for (option, names) in [("--job", &self.jobs), ("--workflow", &self.workflows)] {
                    for n in names {
                        args.extend([option.to_string(), n.clone()]);
                    }
                }

                args.push("--sha".into());
            }
            "wait" => {
                args.extend(self.json.then(|| "--json".to_string()));
                args.extend(self.no_heartbeat.then(|| "--no-heartbeat".to_string()));

                if let Some(t) = &self.timeout {
                    args.extend(["--timeout".to_string(), t.clone()]);
                }
            }
            "status" => args.extend(self.json.then(|| "--json".to_string())),
            _ => {}
        }

        args.extend(self.sha.clone().or_else(|| self.target.clone()));
        args
    }
}

/// Whether this command must hand its run to the daemon: it sits in a sandbox that is not a host
/// check's own, or `SHIPCHECK_HANDOFF=1` says so where the kernel cannot tell, as on Linux
pub fn wanted() -> bool {
    std::env::var("SHIPCHECK_HANDOFF").is_ok_and(|v| v == "1") || nested()
}

#[cfg(target_os = "macos")]
fn nested() -> bool {
    matches!(
        sandbox_jail::Backend::detect(),
        Err(sandbox_jail::Error::Nested)
    ) && !host::in_a_jail()
}

#[cfg(not(target_os = "macos"))]
fn nested() -> bool {
    false
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The full id of the commit `rev` names, None when it names none
pub fn commit_of(cwd: &Path, rev: &str) -> Option<String> {
    let commit = format!("{rev}^{{commit}}");
    git(cwd, &["rev-parse", "--verify", "--quiet", &commit]).filter(|sha| hex(sha, 40..=40))
}

fn dir() -> PathBuf {
    paths::home().join("handoff")
}

fn new_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    Ok(cache::hex(&bytes))
}

/// Whether `id` is a request the daemon took, so `wait` can follow it with no request of its own
pub fn known(id: &str) -> bool {
    hex(id, 32..=32) && dir().join(format!("{id}.err")).exists()
}

const NO_DAEMON: &str = "the daemon did not take the run. Inside a sandbox the checks run through `shipcheck daemon`, which must be running and watch this repository: `shipcheck install` in the main checkout sets both up";

/**
 * Leaves the request for the daemon and returns its id once the daemon took it. The folder is in
 * the worktree's git dir, which every sandbox that lets its agent commit lets it write
 **/
pub fn send(cwd: &Path, request: &Request) -> Result<String, String> {
    request.validate()?;
    let requests = git(
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "shipcheck/requests",
        ],
    )
    .map(PathBuf::from)
    .ok_or("not a git repository")?;
    fs::create_dir_all(&requests).map_err(|e| format!("{}: {e}", requests.display()))?;
    let id = new_id()?;
    let file = requests.join(format!("{id}.json"));
    let temp = requests.join(format!("{id}.tmp"));
    let text = serde_json::to_string(request).map_err(|e| e.to_string())?;
    fs::write(&temp, text)
        .and_then(|()| fs::rename(&temp, &file))
        .map_err(|e| format!("{}: {e}", file.display()))?;
    let asked = Instant::now();

    while !dir().join(format!("{id}.err")).exists() {
        if asked.elapsed() > TAKEN {
            let _ = fs::remove_file(&file);
            return Err(NO_DAEMON.into());
        }

        thread::sleep(Duration::from_millis(100));
    }

    Ok(id)
}

/// Copies what `file` gained since `at` to `to`
fn copy_new(file: &Path, at: &mut u64, to: &mut dyn Write) {
    let Ok(mut f) = fs::File::open(file) else {
        return;
    };
    let mut text = Vec::new();

    if f.seek(SeekFrom::Start(*at)).is_ok() && f.read_to_end(&mut text).is_ok() {
        *at += u64::try_from(text.len()).unwrap_or(u64::MAX);
        let _ = to.write_all(&text);
        let _ = to.flush();
    }
}

/// Prints the output of request `id` as the daemon's run writes it, and ends with its exit code.
/// Past `timeout` it ends with 3 while the run still goes, as `wait --timeout` does
pub fn follow(id: &str, timeout: Option<Duration>) -> Result<ExitCode, String> {
    let file = |ext: &str| dir().join(format!("{id}.{ext}"));
    let (mut out_at, mut err_at) = (0, 0);
    let started = Instant::now();

    loop {
        // Read before the output, so what the run wrote last is printed too
        let ended = fs::read_to_string(file("json")).ok();
        copy_new(&file("err"), &mut err_at, &mut std::io::stderr());
        copy_new(&file("out"), &mut out_at, &mut std::io::stdout());

        if let Some(text) = ended {
            let code = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v["code"].as_u64())
                .ok_or("the daemon's result is not one")?;
            return Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)));
        }

        if timeout.is_some_and(|t| started.elapsed() >= t) {
            eprintln!("still running, `shipcheck wait {id}` waits for its result");
            return Ok(ExitCode::from(3));
        }

        thread::sleep(Duration::from_millis(200));
    }
}

/// The request folders of a checkout the daemon watches, each with the worktree it belongs to:
/// the main checkout's own, and one for each linked worktree
fn folders(checkout: &Path) -> Vec<(PathBuf, PathBuf)> {
    let common = checkout.join(".git");
    let mut found = vec![(common.join("shipcheck/requests"), checkout.to_path_buf())];
    let linked = fs::read_dir(common.join("worktrees"));

    for entry in linked.into_iter().flatten().flatten() {
        // Git writes the path of the worktree's `.git` file here
        let git_file = fs::read_to_string(entry.path().join("gitdir")).unwrap_or_default();

        if let Some(worktree) = Path::new(git_file.trim()).parent() {
            found.push((
                entry.path().join("shipcheck/requests"),
                worktree.to_path_buf(),
            ));
        }
    }

    // A folder that is a link out of the repository would have the daemon delete files elsewhere
    let inside = common.canonicalize().ok();
    found.retain(|(folder, _)| {
        folder
            .canonicalize()
            .ok()
            .zip(inside.as_ref())
            .is_some_and(|(real, common)| real.starts_with(common))
    });
    found
}

/// Reads and removes one request file. A link, a file of another user, a large or an old one is
/// removed unread, and nothing of a refused request is printed
fn take(file: &Path) -> Option<Request> {
    let opened = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(file);
    let _ = fs::remove_file(file);
    let mut opened = opened.ok()?;
    let meta = opened.metadata().ok()?;
    // SAFETY: getuid cannot fail and touches no memory
    let mine = meta.uid() == unsafe { libc::getuid() };
    let fresh = meta
        .modified()
        .ok()
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .is_none_or(|age| age < STALE);

    if !meta.is_file() || !mine || meta.len() > MAX_BYTES || !fresh {
        return None;
    }

    let mut text = String::new();
    opened.read_to_string(&mut text).ok()?;
    let request: Request = serde_json::from_str(&text).ok()?;
    request.validate().ok()?;
    Some(request)
}

fn request_id(file: &Path) -> Option<String> {
    let name = file.file_name()?.to_str()?;
    let id = name.strip_suffix(".json")?;
    hex(id, 32..=32).then(|| id.to_string())
}

/**
 * Runs one request with `exe`, shipcheck itself, in the checkout the daemon watches, and writes
 * the exit code beside the output. The run is an ordinary one: it takes the base from the daemon's
 * entry and stores its result in the cache, so the push that follows posts it. A run of a snapshot
 * also records what it tested for the worktree, as a run started there does
 **/
fn serve_one(exe: &Path, entry: &RepoEntry, worktree: &Path, id: &str, request: &Request) {
    let home = dir();
    let create = |ext: &str| {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(home.join(format!("{id}.{ext}")))
    };
    // The command waits for the second file, so both exist once it reads either
    let (Ok(out), Ok(mut err)) = (create("out"), create("err")) else {
        return;
    };
    let has_commit = request
        .sha
        .as_ref()
        .is_none_or(|sha| commit_of(&entry.path, sha).is_some());

    let code = if has_commit {
        let mut command = Command::new(exe);
        command
            .args(request.argv())
            .current_dir(&entry.path)
            .stdin(Stdio::null())
            .stdout(out)
            .env_remove("SHIPCHECK_HANDOFF")
            .env("SHIPCHECK_TRIGGER", "handoff");

        if let Some(branch) = &request.branch {
            command.env("SHIPCHECK_BRANCH", branch);
        }

        match err.try_clone().map(|e| command.stderr(e).status()) {
            Ok(Ok(status)) => status.code().unwrap_or(1),
            Ok(Err(e)) | Err(e) => {
                let _ = writeln!(err, "shipcheck: the daemon could not start the run: {e}");
                2
            }
        }
    } else {
        let _ = writeln!(
            err,
            "shipcheck: the daemon's checkout does not have this commit"
        );
        2
    };

    // Exit 0 and 1 are verdicts, anything else means the checks did not run
    if request.cmd == "run"
        && request.snapshot
        && code <= 1
        && let Some(tree) = request
            .sha
            .as_ref()
            .and_then(|sha| git(&entry.path, &["rev-parse", &format!("{sha}^{{tree}}")]))
    {
        let tested = run::Tested {
            tree,
            staged: request.staged,
            passed: code == 0,
            partial: !request.jobs.is_empty() || !request.workflows.is_empty(),
        };

        if let Err(e) = run::record_tested(worktree, &tested) {
            let _ = writeln!(err, "shipcheck: could not record the tested tree: {e}");
        }
    }

    let temp = home.join(format!("{id}.json.tmp"));
    let _ = fs::write(&temp, format!("{{\"code\":{code}}}"))
        .and_then(|()| fs::rename(&temp, home.join(format!("{id}.json"))));
}

/// Removes the output of requests served long ago
fn prune() {
    for entry in fs::read_dir(dir()).into_iter().flatten().flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| SystemTime::now().duration_since(at).ok())
            .is_some_and(|age| age > KEPT);

        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Starts every request waiting in the folders of the entry's checkout, each on its own thread
fn serve_checkout(exe: &Path, entry: &RepoEntry) {
    for (folder, worktree) in folders(&entry.path) {
        for file in fs::read_dir(&folder).into_iter().flatten().flatten() {
            let Some(id) = request_id(&file.path()) else {
                continue;
            };

            if SERVING.load(Ordering::SeqCst) >= AT_ONCE {
                return;
            }

            let Some(request) = take(&file.path()) else {
                eprintln!("handoff: refused a request in {}", folder.display());
                continue;
            };
            SERVING.fetch_add(1, Ordering::SeqCst);
            let (exe, worktree) = (exe.to_path_buf(), worktree.clone());
            let entry = RepoEntry {
                path: entry.path.clone(),
                base: entry.base.clone(),
            };
            thread::spawn(move || {
                serve_one(&exe, &entry, &worktree, &id, &request);
                SERVING.fetch_sub(1, Ordering::SeqCst);
            });
        }
    }
}

/// The daemon's thread that serves requests from the repositories in daemon.toml
pub fn serve() {
    thread::spawn(|| {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let mut pruned: Option<Instant> = None;

        loop {
            if fs::create_dir_all(dir()).is_ok()
                && let Ok(config) = DaemonConfig::load()
            {
                for entry in config.repos.values() {
                    serve_checkout(&exe, entry);
                }
            }

            if pruned.is_none_or(|at| at.elapsed() > Duration::from_secs(3600)) {
                prune();
                pruned = Some(Instant::now());
            }

            thread::sleep(Duration::from_millis(500));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn run_request() -> Request {
        Request {
            cmd: "run".into(),
            sha: Some("a".repeat(40)),
            ..Request::default()
        }
    }

    #[test]
    fn a_request_names_a_commit_and_nothing_that_reads_as_an_option_or_a_path() {
        assert_eq!(run_request().validate(), Ok(()));

        let refused = [
            Request {
                cmd: "post".into(),
                ..run_request()
            },
            Request {
                sha: Some("HEAD".into()),
                ..run_request()
            },
            Request {
                sha: None,
                target: Some("abcdef".into()),
                ..run_request()
            },
            Request {
                jobs: vec!["--base=evil".into()],
                ..run_request()
            },
            Request {
                workflows: vec!["../../ci.yml".into()],
                ..run_request()
            },
            Request {
                branch: Some("-x".into()),
                ..run_request()
            },
            Request {
                timeout: Some("10; x".into()),
                ..run_request()
            },
        ];

        for request in refused {
            assert!(request.validate().is_err(), "{request:?}");
        }

        let wait = Request {
            cmd: "wait".into(),
            target: Some("abcdef12".into()),
            timeout: Some("10m".into()),
            ..Request::default()
        };
        assert_eq!(wait.validate(), Ok(()));
    }

    #[test]
    fn a_request_cannot_carry_a_base_a_home_or_a_path() {
        for field in ["base", "home_ok", "background", "cwd", "env"] {
            let text = format!(
                r#"{{"cmd":"run","sha":"{}","{field}":"x"}}"#,
                "a".repeat(40)
            );
            assert!(serde_json::from_str::<Request>(&text).is_err(), "{field}");
        }
    }

    #[test]
    fn the_daemon_builds_the_command_line_from_the_fields() {
        let sha = "b".repeat(40);
        let request = Request {
            sha: Some(sha.clone()),
            jobs: vec!["test".into()],
            only_failed: true,
            json: true,
            ..run_request()
        };
        assert_eq!(
            request.argv(),
            [
                "run",
                "--only-failed",
                "--json",
                "--job",
                "test",
                "--sha",
                &sha
            ]
        );

        let wait = Request {
            cmd: "wait".into(),
            target: Some("abcdef12".into()),
            timeout: Some("90".into()),
            ..Request::default()
        };
        assert_eq!(wait.argv(), ["wait", "--timeout", "90", "abcdef12"]);

        let logs = Request {
            cmd: "logs".into(),
            sha: Some(sha.clone()),
            json: true,
            ..Request::default()
        };
        assert_eq!(logs.argv(), ["logs", &sha]);
    }

    #[test]
    fn a_link_a_large_file_and_a_bad_request_are_removed_unread() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.json");
        fs::write(&good, serde_json::to_string(&run_request()).unwrap()).unwrap();
        assert_eq!(take(&good), Some(run_request()));
        assert!(!good.exists(), "a request is served once");

        let secret = dir.path().join("secret");
        fs::write(&secret, serde_json::to_string(&run_request()).unwrap()).unwrap();
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert_eq!(take(&link), None);
        assert!(secret.exists(), "the file a link names stays");
        assert!(!link.exists());

        let large = dir.path().join("large.json");
        fs::write(&large, " ".repeat(usize::try_from(MAX_BYTES).unwrap() + 1)).unwrap();
        assert_eq!(take(&large), None);

        let bad = dir.path().join("bad.json");
        fs::write(&bad, r#"{"cmd":"trust"}"#).unwrap();
        assert_eq!(take(&bad), None);
        assert!(!bad.exists());
    }

    #[test]
    fn only_a_file_named_by_a_request_id_is_a_request() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            request_id(Path::new(&format!("/x/{id}.json"))),
            Some(id.to_string())
        );
        assert_eq!(request_id(Path::new(&format!("/x/{id}.tmp"))), None);
        assert_eq!(request_id(Path::new("/x/config.json")), None);
    }

    #[test]
    fn each_worktree_has_a_folder_and_a_link_out_of_the_repository_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let checkout = root.join("main");
        let linked = checkout.join(".git/worktrees/task");
        fs::create_dir_all(checkout.join(".git/shipcheck/requests")).unwrap();
        fs::create_dir_all(linked.join("shipcheck/requests")).unwrap();
        fs::write(
            linked.join("gitdir"),
            format!("{}/wt/.git\n", root.display()),
        )
        .unwrap();

        let away = checkout.join(".git/worktrees/away");
        fs::create_dir_all(away.join("shipcheck")).unwrap();
        fs::write(
            away.join("gitdir"),
            format!("{}/away/.git\n", root.display()),
        )
        .unwrap();
        fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), away.join("shipcheck/requests"))
            .unwrap();

        assert_eq!(
            folders(&checkout),
            [
                (checkout.join(".git/shipcheck/requests"), checkout.clone()),
                (linked.join("shipcheck/requests"), root.join("wt")),
            ]
        );
    }

    // The daemon's side and the command's side against a stand-in for shipcheck. It runs in a child
    // with a home of its own, since the home comes from the environment
    #[test]
    fn a_served_request_gives_the_command_its_output_and_exit_code() {
        const NAME: &str =
            "handoff::tests::a_served_request_gives_the_command_its_output_and_exit_code";

        if std::env::var_os("SHIPCHECK_HOME_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME])
                .env("SHIPCHECK_HOME_CHILD", "1")
                .env("SHIPCHECK_HOME", home.path())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().canonicalize().unwrap();
        let exe = root.join("fake");
        fs::write(
            &exe,
            "#!/bin/sh\necho \"ran $*\"\necho status >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let checkout = root.join("main");
        fs::create_dir_all(&checkout).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(&checkout)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .stdout(Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "one"]);
        let sha = commit_of(&checkout, "HEAD").unwrap();
        let entry = RepoEntry {
            path: checkout.clone(),
            base: "origin/main".into(),
        };
        fs::create_dir_all(dir()).unwrap();

        let request = Request {
            sha: Some(sha.clone()),
            snapshot: true,
            ..run_request()
        };
        let id = "0123456789abcdef0123456789abcdef";
        serve_one(&exe, &entry, &checkout, id, &request);

        assert!(known(id));
        let out = fs::read_to_string(dir().join(format!("{id}.out"))).unwrap();
        assert_eq!(out, format!("ran run --sha {sha}\n"));
        let result = fs::read_to_string(dir().join(format!("{id}.json"))).unwrap();
        assert_eq!(result, r#"{"code":1}"#);
        let tested = run::last_tested(&checkout).unwrap();
        assert!(!tested.passed && !tested.staged && !tested.partial);
        assert_eq!(tested.tree.len(), 40);
        let code = follow(id, None).unwrap();
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));

        // A commit the checkout lacks starts nothing
        let unknown = Request {
            sha: Some("c".repeat(40)),
            ..run_request()
        };
        let other = "fedcba9876543210fedcba9876543210";
        serve_one(&exe, &entry, &checkout, other, &unknown);
        let result = fs::read_to_string(dir().join(format!("{other}.json"))).unwrap();
        assert_eq!(result, r#"{"code":2}"#);
        let out = fs::read_to_string(dir().join(format!("{other}.out"))).unwrap();
        assert_eq!(out, "");
    }
}
