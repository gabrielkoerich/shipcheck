use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::check::{self, iso};
use crate::report;
use crate::run::{Entry, Plan, home, printable, unix_now};

pub const VERSION: u64 = 1;
const MAX_BYTES: u64 = 50 * 1024 * 1024;

/// What started a run and what it did on GitHub, which the entry does not hold
pub struct Run {
    /// `cli`, `hook-commit`, `hook-push` or `daemon`
    pub trigger: String,
    pub branch: Option<String>,
    pub pr: Option<u64>,
    pub author: Option<String>,
    pub started: u64,
    pub cache_hit: bool,
    pub partial: bool,
    pub posted_checks: bool,
    pub posted_comment: bool,
    /// Why a run that posts could not post its checks
    pub post_error: Option<String>,
    /// When the first check with a result went to GitHub, in Unix seconds
    pub first_check: Option<u64>,
}

impl Run {
    pub fn new(trigger: &str, started: u64) -> Run {
        Run {
            trigger: trigger.into(),
            branch: None,
            pr: None,
            author: None,
            started,
            cache_hit: false,
            partial: false,
            posted_checks: false,
            posted_comment: false,
            post_error: None,
            first_check: None,
        }
    }
}

/**
 * One run as a record line. It holds names, hashes, times and statuses only: never code, file
 * contents, log text, environment values or paths on this machine. Test names come from the logs,
 * so a name holding one of the repository's secrets is left out
 **/
pub fn line(
    repo: &str,
    plan: &Plan,
    entry: &Entry,
    run: &Run,
    logs: &Path,
    secrets: &[String],
) -> Value {
    let finished = unix_now();
    let reports = report::workflows(entry, logs);
    let mut ids: check::Jobs = BTreeMap::new();

    for wf in &entry.workflows {
        ids.entry(wf.file.clone())
            .or_default()
            .extend(wf.jobs.keys().cloned());
    }

    let names: BTreeMap<(String, String), String> = check::job_names(&ids)
        .into_iter()
        .map(|(file, job, name)| ((file, job), name))
        .collect();
    let mut jobs = Vec::new();

    for (wf, rep) in entry.workflows.iter().zip(&reports) {
        for job in &rep.jobs {
            let times = wf.times.get(&job.id);
            let tests: Vec<String> = job
                .failing_tests
                .iter()
                .filter(|t| {
                    !secrets
                        .iter()
                        .any(|s| !s.is_empty() && t.contains(s.as_str()))
                })
                .map(|t| printable(t))
                .collect();
            jobs.push(json!({
                "workflow": wf.file,
                "name": job.id,
                "check": names.get(&(wf.file.clone(), job.id.clone())),
                "status": job.status,
                "attempts": job.attempts,
                "started": times.map(|t| iso(t[0])),
                "finished": times.map(|t| iso(t[1])),
                "duration_s": times.map(|t| t[1].saturating_sub(t[0])),
                "failed_tests": tests,
                "failed_tests_total": job.failing_tests_total,
            }));
        }
    }

    let kind = crate::runner::Runners::new(&plan.config, &plan.repo).kind();
    json!({
        "v": VERSION,
        "event": "run",
        "id": format!("{:x}-{:x}", run.started, std::process::id()),
        "started": iso(run.started),
        "finished": iso(finished),
        "duration_s": finished.saturating_sub(run.started),
        "repo": repo,
        "base": plan.base,
        "branch": run.branch,
        "sha": plan.sha,
        "tree": plan.tree,
        "key": plan.key,
        "pr": run.pr,
        "author": run.author,
        "trigger": run.trigger,
        "machine": hostname(),
        "os": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "version": env!("CARGO_PKG_VERSION"),
        "mode": kind,
        "tools": run_tools(kind, &plan.tools),
        "cache_hit": run.cache_hit,
        "partial": run.partial,
        "skipped": entry.skipped.is_some(),
        "passed": entry.passed,
        "jobs": jobs,
        "posted": {
            "checks": run.posted_checks,
            "comment": run.posted_comment,
            "first_check_s": run.first_check.map(|t| t.saturating_sub(run.started)),
            "error": run.post_error,
        },
    })
}

/// A daemon event, such as `pr_merged`, with the version, the time and the repository added
pub fn event(repo: &str, name: &str, mut fields: Value) -> Value {
    fields["v"] = VERSION.into();
    fields["event"] = name.into();
    fields["at"] = iso(unix_now()).into();
    fields["repo"] = repo.into();
    fields
}

/// The runner's identity as one entry per tool: act's version, or each `[host] versions` command
/// with its output
/// act's identity is one line, a host check's version output can be several, so only mixed splits
fn run_tools(kind: &str, identity: &str) -> Vec<String> {
    match identity.split_once('\n') {
        Some((act, host)) if kind == "mixed" => [tools(act), tools(host)].concat(),
        _ => tools(identity),
    }
}

fn tools(identity: &str) -> Vec<String> {
    let mut parts = identity.split('\0');
    let mut out: Vec<String> = parts.next().map(String::from).into_iter().collect();

    while let (Some(command), Some(version)) = (parts.next(), parts.next()) {
        out.push(format!("{command}: {version}"));
    }

    out
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // Safe: gethostname writes at most buf.len() bytes into buf
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };

    if rc != 0 {
        return String::new();
    }

    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

pub fn file(home: &Path, repo: &str) -> PathBuf {
    home.join("records").join(format!("{repo}.jsonl"))
}

/// Appends one line, first moving a file past 50 MB to `<repo>.<n>.jsonl`. Records are never pruned
pub fn append(home: &Path, repo: &str, line: &Value) -> Result<(), String> {
    let path = file(home, repo);
    let dir = path.parent().ok_or("records has no parent")?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let n = rotated(&path).last().map_or(1, |(n, _)| n + 1);
        let _ = fs::rename(&path, rotated_path(&path, n));
    }

    let mut out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // One write per line, so lines of runs that finish together never mix
    out.write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes the line to this machine's records, saying so on stderr when it cannot
pub fn keep(repo: &str, line: &Value) {
    if let Err(e) = append(&home(), repo, line) {
        eprintln!("shipcheck: could not write the run record: {e}");
    }
}

/**
 * Uploads the line when `[team] records` is on in the base's shipcheck.toml, which the plan read.
 * The service stores it only for an account with a plan for team features. Any error or refusal is
 * one line on stderr, the daemon's or the background run's log, and never changes the run
 **/
pub fn share(plan: &Plan, repo: &str, line: &Value) {
    let shared = upload(
        plan.config.team.records,
        &crate::service::api(),
        crate::service::machine_token,
        repo,
        line,
    );

    if let Err(e) = shared {
        eprintln!("shipcheck: run record not uploaded: {e}");
    }
}

// The token is read only when the switch is on, since on a Mac a read can ask the user
fn upload(
    on: bool,
    api: &str,
    token: impl FnOnce() -> Option<String>,
    repo: &str,
    line: &Value,
) -> Result<(), String> {
    if !on {
        return Ok(());
    }

    let token = token().ok_or("no machine token, run `shipcheck login`")?;
    crate::service::post_record(api, &token, repo, line)
}

fn rotated_path(path: &Path, n: u64) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{stem}.{n}.jsonl"))
}

/// The rotated files of `path`, oldest first
fn rotated(path: &Path) -> Vec<(u64, PathBuf)> {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let prefix = format!("{stem}.");
    let mut found: Vec<(u64, PathBuf)> = fs::read_dir(path.parent().unwrap_or(Path::new(".")))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n = name
                .strip_prefix(&prefix)?
                .strip_suffix(".jsonl")?
                .parse()
                .ok()?;
            Some((n, e.path()))
        })
        .collect();
    found.sort();
    found
}

/// The last `limit` run lines of a repository, oldest first, from the current file and the newest
/// rotated one
pub fn runs(home: &Path, repo: &str, limit: usize) -> Vec<Value> {
    let path = file(home, repo);
    let mut files: Vec<PathBuf> = rotated(&path).pop().map(|(_, p)| p).into_iter().collect();
    files.push(path);
    let mut lines: Vec<Value> = files
        .iter()
        .flat_map(|f| {
            fs::read_to_string(f)
                .unwrap_or_default()
                .lines()
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|v| v["event"] == "run")
        .collect();
    let skip = lines.len().saturating_sub(limit);
    lines.drain(..skip);
    lines
}

/// `shipcheck history`: one row per run, newest last
pub fn table(runs: &[Value]) -> String {
    let mut out = format!(
        "{:<20} {:<12} {:<4} {:<7} {:>8} {:<5} {:<11} {}\n",
        "WHEN", "COMMIT", "MODE", "RESULT", "TIME", "CACHE", "BY", "POSTED"
    );

    for r in runs {
        let result = if r["partial"] == true {
            "partial"
        } else if r["skipped"] == true {
            "skipped"
        } else if r["passed"] == true {
            "pass"
        } else {
            "fail"
        };
        let when = r["started"]
            .as_str()
            .unwrap_or_default()
            .replace('T', " ")
            .replace('Z', "");
        let sha = r["sha"].as_str().unwrap_or_default();
        out.push_str(&format!(
            "{:<20} {:<12} {:<4} {:<7} {:>7}s {:<5} {:<11} {}\n",
            when,
            &sha[..sha.len().min(12)],
            r["mode"].as_str().unwrap_or_default(),
            result,
            r["duration_s"].as_u64().unwrap_or_default(),
            if r["cache_hit"] == true { "hit" } else { "run" },
            r["trigger"].as_str().unwrap_or_default(),
            posted_column(r),
        ));
    }

    out
}

/// `posted`, `not posted` with a short reason, or `-` for a run that does not post
fn posted_column(r: &Value) -> String {
    if r["posted"]["checks"] == true {
        return "posted".into();
    }

    match r["posted"]["error"].as_str() {
        Some(e) if crate::background::not_installed(e) => "not posted (no App)".into(),
        Some(_) => "not posted (error)".into(),
        None => "-".into(),
    }
}

/// The `posting:` line of `shipcheck logs` for `sha`, from its last run that posted or tried to
pub fn posting(runs: &[Value], repo: &str, sha: &str) -> Option<String> {
    let r = runs.iter().rev().find(|r| {
        r["sha"] == sha && (r["posted"]["checks"] == true || r["posted"]["error"].is_string())
    })?;

    if let Some(e) = r["posted"]["error"].as_str() {
        let (reason, fix) = crate::background::reason_and_fix(repo, e);
        return Some(format!("posting: not posted, {reason}, fix: {fix}"));
    }

    Some(format!(
        "posting: posted at {} by {}",
        r["finished"].as_str().unwrap_or_default(),
        r["trigger"].as_str().unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepoConfig;
    use crate::run::WorkflowResult;

    #[test]
    fn a_host_version_of_many_lines_stays_one_tool() {
        let host = "host\0rustc -vV\0rustc 1.90.0\nhost: aarch64-apple-darwin";
        let one = [
            "host",
            "rustc -vV: rustc 1.90.0\nhost: aarch64-apple-darwin",
        ];
        assert_eq!(run_tools("host", host), one);
        let mixed = format!("act version 0.2.89\n{host}");
        assert_eq!(
            run_tools("mixed", &mixed),
            [&["act version 0.2.89"][..], &one].concat()
        );
        assert_eq!(
            run_tools("act", "act version 0.2.89"),
            ["act version 0.2.89"]
        );
        let colima = "act version 0.2.89\0colima\0colima version 0.10.3";
        let both = ["act version 0.2.89", "colima: colima version 0.10.3"];
        assert_eq!(run_tools("act", colima), both);
        assert_eq!(run_tools("mixed", &format!("{colima}\nhost"))[..2], both);
    }

    fn plan() -> Plan {
        let config_text = b"workflows = [\"ci.yml\"]\n".to_vec();
        Plan {
            repo: PathBuf::from("/home/someone/secret-project"),
            sha: "a".repeat(40),
            tree: "b".repeat(40),
            base: "c".repeat(40),
            config: RepoConfig::parse(&String::from_utf8_lossy(&config_text)).unwrap(),
            config_text,
            workflows: BTreeMap::new(),
            key: "k".into(),
            ci_changes: Vec::new(),
            defaults: false,
            tools: "act version 0.2.89".into(),
        }
    }

    fn entry() -> Entry {
        Entry {
            key: "k".into(),
            sha: "a".repeat(40),
            tree: "b".repeat(40),
            base: "c".repeat(40),
            passed: false,
            workflows: vec![WorkflowResult {
                file: "ci.yml".into(),
                exit: Some(1),
                attempts: 1,
                log_sha256: String::new(),
                jobs: [("test".to_string(), "failure".to_string())].into(),
                passed: false,
                shards: BTreeMap::new(),
                times: [("test".to_string(), [1_700_000_000, 1_700_000_042])].into(),
                summaries: [("test".to_string(), "summary text".to_string())].into(),
            }],
            finished_unix: 1_700_000_042,
            skipped: None,
            ci_changes: Vec::new(),
        }
    }

    fn logs(dir: &Path) {
        fs::create_dir_all(dir.join("ci.yml")).unwrap();
        fs::write(
            dir.join("ci.yml/test.log"),
            "[ci/test] | test a::ok ... ok\n[ci/test] | test a::hunter2_leak ... FAILED\n[ci/test] | test a::plain ... FAILED\n[ci/test]   ❌  Failure - Main cargo test\n[ci/test] exitcode '101': failure\n",
        )
        .unwrap();
    }

    #[test]
    fn line_holds_the_fields_and_no_secret_or_log_text() {
        let dir = tempfile::tempdir().unwrap();
        logs(dir.path());
        let mut run = Run::new("daemon", 1_700_000_000);
        run.pr = Some(7);
        run.author = Some("octo".into());
        run.posted_checks = true;
        run.first_check = Some(1_700_000_050);
        let line = line(
            "acme/app",
            &plan(),
            &entry(),
            &run,
            dir.path(),
            &["hunter2".into()],
        );

        for field in [
            "v",
            "event",
            "id",
            "started",
            "finished",
            "repo",
            "base",
            "sha",
            "tree",
            "pr",
            "author",
            "trigger",
            "machine",
            "os",
            "version",
            "mode",
            "tools",
            "cache_hit",
            "partial",
            "passed",
            "jobs",
            "posted",
        ] {
            assert!(!line[field].is_null(), "{field} missing");
        }

        assert_eq!(line["v"], 1);
        assert_eq!(line["started"], "2023-11-14T22:13:20Z");
        assert_eq!(line["partial"], false);
        assert_eq!(line["posted"]["first_check_s"], 50);
        let job = &line["jobs"][0];
        assert_eq!(job["check"], "shipcheck/test");
        assert_eq!(job["duration_s"], 42);
        assert_eq!(job["failed_tests"], json!(["a::plain"]));
        let text = line.to_string();

        for leak in [
            "hunter2",
            "secret-project",
            "summary text",
            "Failure - Main",
        ] {
            assert!(!text.contains(leak), "{leak} in {text}");
        }
    }

    #[test]
    fn partial_runs_are_marked() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = Run::new("cli", 1_700_000_000);
        run.partial = true;
        let line = line("acme/app", &plan(), &entry(), &run, dir.path(), &[]);
        assert_eq!(line["partial"], true);
        assert!(table(&[line]).contains("partial"));
    }

    #[test]
    fn append_rotates_past_the_cap_and_history_reads_both() {
        let home = tempfile::tempdir().unwrap();
        let path = file(home.path(), "acme/app");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let big = fs::File::create(&path).unwrap();
        big.set_len(MAX_BYTES + 1).unwrap();
        append(
            home.path(),
            "acme/app",
            &json!({"event": "run", "sha": "1"}),
        )
        .unwrap();
        assert!(rotated_path(&path, 1).exists());
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 1);

        append(home.path(), "acme/app", &json!({"event": "pr_merged"})).unwrap();
        append(
            home.path(),
            "acme/app",
            &json!({"event": "run", "sha": "2"}),
        )
        .unwrap();
        let shas: Vec<_> = runs(home.path(), "acme/app", 10)
            .iter()
            .map(|r| r["sha"].clone())
            .collect();
        assert_eq!(shas, [json!("1"), json!("2")]);
        assert_eq!(runs(home.path(), "acme/app", 1)[0]["sha"], "2");
    }

    /// A service on a local port that answers every request with `status` and `body`, and counts them
    fn service(
        status: &'static str,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{BufRead, BufReader, Read};
        use std::sync::atomic::Ordering;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                count.fetch_add(1, Ordering::SeqCst);
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                let mut header = String::new();

                while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                    if let Some(n) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = n.trim().parse().unwrap();
                    }

                    header.clear();
                }

                let mut request = vec![0; length];
                reader.read_exact(&mut request).unwrap();
                let mut stream = stream;
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (api, seen)
    }

    #[test]
    fn nothing_is_sent_with_the_switch_off() {
        let (api, seen) = service("200 OK", "{}");
        assert!(!plan().config.team.records);
        let sent = upload(
            plan().config.team.records,
            &api,
            || panic!("the token is read with the switch off"),
            "acme/app",
            &json!({"event": "run"}),
        );
        assert_eq!(sent, Ok(()));
        // A request made would have reached the listener by now
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn a_refusal_changes_no_run() {
        let (api, seen) = service(
            "402 Payment Required",
            r#"{"error":"this account has no plan for team features"}"#,
        );
        let dir = tempfile::tempdir().unwrap();
        let entry = entry();
        let line = line(
            "acme/app",
            &plan(),
            &entry,
            &Run::new("daemon", 1),
            dir.path(),
            &[],
        );
        let before = (entry.passed, line.clone());
        let sent = upload(true, &api, || Some("shc_x".into()), "acme/app", &line);
        assert_eq!(
            sent,
            Err("POST /v1/records: this account has no plan for team features".into())
        );
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!((entry.passed, line), before);
    }

    #[test]
    fn history_table() {
        let row = json!({
            "started": "2026-10-23T14:02:11Z", "sha": "0123456789abcdef", "mode": "act",
            "passed": true, "duration_s": 93, "cache_hit": false, "trigger": "hook-push",
        });
        let mut posted = row.clone();
        posted["posted"] = json!({"checks": true, "error": null});
        let mut refused = row.clone();
        refused["posted"] = json!({"checks": false, "error": "POST /v1/checks: the shipcheck App is not installed on acme/app, or you have no access to it"});
        let text = table(&[row, posted, refused.clone()]);
        let lines: Vec<_> = text.lines().collect();
        assert!(lines[0].starts_with("WHEN"));
        assert!(lines[0].ends_with("BY          POSTED"), "{}", lines[0]);
        let start = "2026-10-23 14:02:11  0123456789ab act  pass         93s run   hook-push  ";
        assert_eq!(lines[1], format!("{start} -"));
        assert_eq!(lines[2], format!("{start} posted"));
        assert_eq!(lines[3], format!("{start} not posted (no App)"));

        assert_eq!(
            posting(&[refused], "acme/app", "0123456789abcdef").unwrap(),
            "posting: not posted, the shipcheck App is not installed on acme/app, fix: add acme/app to the shipcheck App: https://github.com/settings/installations (shipcheck-run, Configure)"
        );
    }
}
