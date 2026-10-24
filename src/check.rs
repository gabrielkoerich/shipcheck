use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use crate::report;
use crate::run::{ALLOWED, Entry};
use crate::runner::host::HOST;
use crate::runner::shard::ShardRun;

pub const NAME: &str = "shipcheck";
const TAIL_LINES: usize = 100;
// GitHub rejects check run output fields longer than this
const MAX_FIELD: usize = 65_535;

fn clip(text: String) -> String {
    clip_to(text, MAX_FIELD)
}

/// The end of `text` in at most `max` bytes
fn clip_to(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }

    let mut cut = text.len() - max.saturating_sub(20);

    while !text.is_char_boundary(cut) {
        cut += 1;
    }

    format!("[...cut...]\n{}", &text[cut..])
}

/// How a result's file shows: a workflow file, or the host checks
pub fn name(file: &str) -> &str {
    if file == HOST { "host checks" } else { file }
}

pub fn summary(entry: &Entry) -> String {
    let mut s = format!(
        "| | |\n|---|---|\n| Tree | `{}` |\n| Base | `{}` |\n| Key | `{}` |\n\n",
        entry.tree, entry.base, entry.key
    );

    if !entry.ci_changes.is_empty() {
        s.push_str("This commit changes CI. Changes to workflows and `shipcheck.toml` did not run, the base's versions did. Changes to what a command runs, such as `package.json` scripts, did run. All of them apply to every PR once merged:\n\n");

        for change in &entry.ci_changes {
            s.push_str(&format!("- {change}\n"));
        }

        s.push('\n');
    }

    if let Some(reason) = &entry.skipped {
        s.push_str(&format!("Nothing ran: {reason}.\n"));
        return clip(s);
    }

    s.push_str(
        "| Workflow | Result | Exit | Attempts | Jobs | Log SHA-256 |\n|---|---|---|---|---|---|\n",
    );

    for wf in &entry.workflows {
        let jobs = wf
            .jobs
            .iter()
            .map(|(job, state)| match wf.shards.get(job) {
                Some(ShardRun {
                    fallback: Some(_), ..
                }) => {
                    format!("{job}: {state}, after an unsharded rerun")
                }
                Some(ShardRun {
                    shards: Some(n), ..
                }) => format!("{job}: {state}, tests in {n} shards"),
                _ => format!("{job}: {state}"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let exit = wf.exit.map_or("timeout".to_string(), |c| c.to_string());
        s.push_str(&format!(
            "| {} | {} | {exit} | {} | {jobs} | `{}` |\n",
            name(&wf.file),
            if wf.passed { "passed" } else { "failed" },
            wf.attempts,
            &wf.log_sha256[..12.min(wf.log_sha256.len())]
        ));
    }

    clip(s)
}

/// The last lines of each failed workflow's log, as stored under `logs/<key>/<file>.log`
// act masks secrets, but not a value it never saw whole, such as one split across lines
pub fn failure_text(entry: &Entry, log_dir: &Path, secrets: &[String]) -> String {
    let mut s = String::new();

    for wf in entry.workflows.iter().filter(|w| !w.passed) {
        let tail = tail(&log_dir.join(format!("{}.log", wf.file)), secrets);
        s.push_str(&format!("### {}\n\n```\n{tail}\n```\n\n", name(&wf.file)));
    }

    clip(s)
}

/// The last lines of a log with the secrets masked, empty when the log is missing
fn tail(path: &Path, secrets: &[String]) -> String {
    tail_of(&read_masked(path, secrets))
}

// Masked before any parsing, which cuts and escapes lines and so could split a secret
fn read_masked(path: &Path, secrets: &[String]) -> String {
    mask(fs::read_to_string(path).unwrap_or_default(), secrets)
}

fn tail_of(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    let mut tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");

    // A cut log still names its command, which a host check's log starts with
    if lines.len() > TAIL_LINES {
        let head = lines.first().filter(|l| l.starts_with("$ "));
        tail = head.map_or("[...]".to_string(), |h| format!("{h}\n[...]")) + "\n" + &tail;
    }

    tail
}

fn mask(mut text: String, secrets: &[String]) -> String {
    for secret in secrets {
        text = text.replace(secret.as_str(), "***");
    }

    text
}

/**
 * A failed job's details: the failing tests, each one's own output however far above the end it
 * is, then the end of the log. The tail keeps at most half the field, the failures get the rest,
 * and a failure that does not fit whole is left out after the first, which keeps its start
 **/
fn failed_text(path: &Path, host: bool, secrets: &[String]) -> String {
    let log = read_masked(path, secrets);
    let (names, blocks) = report::failures(&log, host);
    let tail = tail_of(&log);
    let mut s = String::new();

    if !names.is_empty() {
        s.push_str(&format!("Failed tests ({}):\n\n```\n", names.len()));

        for name in names.iter().take(report::MAX_TESTS) {
            s.push_str(&format!("{name}\n"));
        }

        if names.len() > report::MAX_TESTS {
            s.push_str(&format!("and {} more\n", names.len() - report::MAX_TESTS));
        }

        s.push_str("```\n\n");
    }

    if !blocks.is_empty() {
        let budget = MAX_FIELD.saturating_sub(s.len() + tail.len().min(MAX_FIELD / 2) + 300);
        let mut out = String::new();
        let mut kept = 0;

        for (_, lines) in &blocks {
            let block = lines.join("\n") + "\n\n";

            if out.len() + block.len() > budget {
                if kept == 0 {
                    let mut cut = budget;

                    while !block.is_char_boundary(cut) {
                        cut -= 1;
                    }

                    out = format!("{}\n[...]\n", &block[..cut]);
                    kept = 1;
                }

                break;
            }

            out.push_str(&block);
            kept += 1;
        }

        s.push_str(&format!("```\n{}\n```\n\n", out.trim_end()));

        if kept < blocks.len() {
            s.push_str(&format!(
                "{} more failed tests were left out, `shipcheck logs` has the rest.\n\n",
                blocks.len() - kept
            ));
        }
    }

    let room = MAX_FIELD.saturating_sub(s.len() + 10);
    s + &format!("```\n{}\n```\n", clip_to(tail, room))
}

/// Job ids per file a run reports: the `[host]` checks under [`HOST`], or each required workflow's jobs
pub type Jobs = BTreeMap<String, BTreeSet<String>>;

/**
 * Each job's check name, as `(file, job, name)`: `shipcheck/<job>`, or `shipcheck/<stem>/<job>`
 * for a job id that more than one workflow has, the stem being the file name without `.yml` or
 * `.yaml`. When those workflows' stems are not all different, as with `ci.yml` and `ci.yaml`, the
 * whole file name replaces the stem, so no two jobs share a name
 **/
pub fn job_names(jobs: &Jobs) -> Vec<(String, String, String)> {
    let stem = |file: &str| {
        file.strip_suffix(".yml")
            .or_else(|| file.strip_suffix(".yaml"))
            .unwrap_or(file)
            .to_string()
    };
    let mut files: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

    for (file, ids) in jobs {
        for job in ids {
            files.entry(job).or_default().push(file);
        }
    }

    let mut names = Vec::new();

    for (file, ids) in jobs {
        for job in ids {
            let shared = &files[job.as_str()];
            let stems: BTreeSet<String> = shared.iter().map(|f| stem(f)).collect();
            let name = match shared.len() {
                1 => format!("{NAME}/{job}"),
                n if stems.len() == n => format!("{NAME}/{}/{job}", stem(file)),
                _ => format!("{NAME}/{file}/{job}"),
            };
            names.push((file.clone(), job.clone(), name));
        }
    }

    names
}

/// The jobs `jobs` expects together with the ones `entry` has, so none of either goes unposted
fn all_jobs(jobs: &Jobs, entry: &Entry) -> Jobs {
    let mut all = jobs.clone();

    for wf in &entry.workflows {
        all.entry(wf.file.clone())
            .or_default()
            .extend(wf.jobs.keys().cloned());
    }

    all
}

/**
 * One check per job, each required in branch protection. A `paths` skip makes every one `skipped`,
 * which GitHub counts as passing. A job the result lacks fails, and so does every job of a workflow
 * that failed with no failed job, so all job checks pass only when every workflow passed. A job's
 * log tail goes in as in `body`, and with `all` for a job that passed too
 **/
pub fn job_bodies(
    sha: &str,
    jobs: &Jobs,
    entry: &Entry,
    logs: Option<(&Path, &[String])>,
    all: bool,
) -> Vec<Value> {
    let mut bodies = Vec::new();

    for (file, job, name) in job_names(&all_jobs(jobs, entry)) {
        if let Some(reason) = &entry.skipped {
            bodies.push(json!({
                "name": name,
                "head_sha": sha,
                "status": "completed",
                "conclusion": "skipped",
                "output": {"title": "skipped", "summary": format!("Nothing ran: {reason}."), "text": ""},
            }));
            continue;
        }

        let wf = entry.workflows.iter().find(|w| w.file == file);
        let state = wf
            .and_then(|w| w.jobs.get(&job))
            .map_or("missing from the result", String::as_str);
        let conclusion = match wf {
            Some(wf) if wf.jobs.contains_key(&job) => {
                let own = conclusion(state);
                let unexplained =
                    !wf.passed && wf.jobs.values().all(|s| conclusion(s) != "failure");

                if unexplained { "failure" } else { own }
            }
            _ => "failure",
        };
        let text = match logs {
            Some((dir, secrets)) if conclusion == "failure" => failed_text(
                &dir.join(&file).join(format!("{job}.log")),
                file == HOST,
                secrets,
            ),
            Some((dir, secrets)) if all => format!(
                "```\n{}\n```\n",
                tail(&dir.join(&file).join(format!("{job}.log")), secrets)
            ),
            None if conclusion == "failure" => {
                "Run `shipcheck logs` on the machine that ran it to see why.".to_string()
            }
            _ => String::new(),
        };
        let mut body = json!({
            "name": name,
            "head_sha": sha,
            "status": "completed",
            "conclusion": conclusion,
            "output": {
                "title": state,
                "summary": format!("`{job}` of {}: {state}, tree `{}`", self::name(&file), entry.tree),
                "text": clip(text),
            },
        });

        // GitHub shows the time between these as the job's duration
        if let Some([start, end]) = wf.and_then(|w| w.times.get(&job)) {
            body["started_at"] = iso(*start).into();
            body["completed_at"] = iso(*end).into();
        }

        bodies.push(body);
    }

    bodies
}

/// Every job check's name and then `shipcheck`, from the jobs on main
pub fn names(plan: &crate::run::Plan) -> Vec<String> {
    let jobs = crate::run::jobs(plan).unwrap_or_default();
    let mut names: Vec<String> = job_names(&jobs).into_iter().map(|(_, _, n)| n).collect();
    names.push(NAME.into());
    names
}

/// Every job check and then `shipcheck`, so a failed post leaves `shipcheck` open. Log tails go in
/// only `with_logs` and when the repository's secrets can be read, so none goes out unmasked
pub fn final_bodies(plan: &crate::run::Plan, entry: &Entry, with_logs: bool) -> Vec<Value> {
    use crate::run;

    let dir = run::log_dir(&plan.key);
    let secrets = run::repo_id(&plan.repo).and_then(|id| run::secret_values(&id));
    let logs = match &secrets {
        Ok(secrets) if with_logs => Some((dir.as_path(), secrets.as_slice())),
        _ => None,
    };
    let all = plan.config.logs == crate::config::Logs::All;
    // A workflow main cannot parse fails the run, whose result then names its jobs
    let jobs = run::jobs(plan).unwrap_or_default();
    let mut bodies = job_bodies(&plan.sha, &jobs, entry, logs, all);
    bodies.push(body(&plan.sha, entry, logs));
    bodies
}

/// A job state as a check conclusion: an allowed failure is `neutral`, which passes a required check
fn conclusion(state: &str) -> &'static str {
    match state {
        "success" => "success",
        "skipped" => "skipped",
        s if s.ends_with(ALLOWED) => "neutral",
        _ => "failure",
    }
}

/// Body of a check that has no result yet: `in_progress` while a run of its tree goes, which GitHub
/// shows as a yellow dot, or `queued` while no machine runs it
pub fn waiting(name: &str, sha: &str, running: bool) -> Value {
    let mut body = waiting_body(name, sha, running);

    if running {
        body["started_at"] = iso(crate::run::unix_now()).into();
    }

    body
}

fn waiting_body(name: &str, sha: &str, running: bool) -> Value {
    let (status, title, summary) = if running {
        (
            "in_progress",
            "running",
            "shipcheck is running this commit's checks.",
        )
    } else {
        (
            "queued",
            "waiting for a run",
            "No machine has run this commit's tree yet. `shipcheck run` or a push with the hooks installed starts one, and the daemon runs a same-repository PR once GitHub says who pushed its head.",
        )
    };

    json!({
        "name": name,
        "head_sha": sha,
        "status": status,
        "output": {"title": title, "summary": summary},
    })
}

/// Body for `POST /repos/{owner}/{repo}/check-runs`. Log tails go in only when `logs` is given,
/// because a log can hold paths, names or unmasked secrets and the check is visible to readers
pub fn body(sha: &str, entry: &Entry, logs: Option<(&Path, &[String])>) -> Value {
    let failed = entry.workflows.iter().filter(|w| !w.passed).count();
    let mut title = if entry.skipped.is_some() {
        "passed, nothing to run for this change".to_string()
    } else if entry.passed {
        "passed".to_string()
    } else if entry.could_not_run() {
        "could not run, Docker or the sandbox failed before the code ran".to_string()
    } else if let [wf] = entry.workflows.as_slice()
        && wf.file == HOST
    {
        let failed = wf.jobs.values().filter(|s| *s != "success").count();
        format!("failed, {failed} of {} host checks", wf.jobs.len())
    } else {
        format!("failed, {failed} of {} workflows", entry.workflows.len())
    };

    if !entry.ci_changes.is_empty() {
        title.push_str(", changes CI");
    }

    let start = entry
        .workflows
        .iter()
        .flat_map(|w| w.times.values().map(|t| t[0]))
        .min();
    let mut body = json!({
        "name": NAME,
        "head_sha": sha,
        "status": "completed",
        "conclusion": if entry.passed { "success" } else { "failure" },
        "output": {
            "title": title,
            "summary": summary(entry),
            "text": match logs {
                Some((dir, secrets)) => failure_text(entry, dir, secrets),
                None if entry.passed => String::new(),
                None => "Run `shipcheck logs` on the machine that ran it to see why.".to_string(),
            },
        },
    });

    if let Some(start) = start {
        body["started_at"] = iso(start).into();
        body["completed_at"] = iso(entry.finished_unix.max(start)).into();
    }

    body
}

/// Unix seconds as the UTC ISO 8601 time GitHub takes, by Howard Hinnant's days-to-civil algorithm
pub fn iso(secs: u64) -> String {
    let (days, rest) = ((secs / 86_400) as i64 + 719_468, secs % 86_400);
    let era = days.div_euclid(146_097);
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::run::WorkflowResult;

    fn entry(passed: bool) -> Entry {
        Entry {
            key: "k".into(),
            sha: "s".into(),
            tree: "t".into(),
            base: "b".into(),
            passed,
            workflows: vec![WorkflowResult {
                file: "ci.yml".into(),
                exit: Some(if passed { 0 } else { 1 }),
                attempts: if passed { 1 } else { 2 },
                log_sha256: "abcdef0123456789".into(),
                jobs: BTreeMap::from([(
                    "test".into(),
                    if passed { "success" } else { "failure" }.into(),
                )]),
                passed,
                shards: BTreeMap::from([(
                    "test".into(),
                    ShardRun {
                        shards: Some(4),
                        items: Some(10),
                        fallback: None,
                    },
                )]),
                times: BTreeMap::new(),
                summaries: BTreeMap::new(),
            }],
            finished_unix: 0,
            ci_changes: Vec::new(),
            skipped: None,
        }
    }

    #[test]
    fn waiting_has_no_conclusion() {
        for (running, status) in [(true, "in_progress"), (false, "queued")] {
            let body = waiting("shipcheck/test", "s", running);
            assert_eq!(body["name"], "shipcheck/test");
            assert_eq!(body["status"], status);
            assert_eq!(body["head_sha"], "s");
            assert!(body.get("conclusion").is_none());
        }
    }

    #[test]
    fn passing_body() {
        let b = body("abc", &entry(true), None);
        assert_eq!(b["name"], "shipcheck");
        assert_eq!(b["head_sha"], "abc");
        assert_eq!(b["conclusion"], "success");
        assert_eq!(b["output"]["title"], "passed");
        assert!(b["output"]["summary"].as_str().unwrap().contains(
            "| ci.yml | passed | 0 | 1 | test: success, tests in 4 shards | `abcdef012345` |"
        ));
        assert_eq!(b["output"]["text"], "");
    }

    #[test]
    fn names_ci_changes_without_failing() {
        let mut e = entry(true);
        e.ci_changes = vec![".github/workflows/ci.yml: job test gets continue-on-error".into()];
        let b = body("abc", &e, None);
        assert_eq!(b["conclusion"], "success");
        assert_eq!(b["output"]["title"], "passed, changes CI");
        let summary = b["output"]["summary"].as_str().unwrap();
        assert!(summary.contains("This commit changes CI"));
        assert!(summary.contains("- .github/workflows/ci.yml: job test gets continue-on-error\n"));
    }

    #[test]
    fn a_job_shipcheck_could_not_start_is_not_the_codes_failure() {
        let mut e = entry(false);
        let jobs = &mut e.workflows[0].jobs;
        jobs.insert("test".into(), crate::run::COULD_NOT_RUN.into());
        jobs.insert(
            "after".into(),
            "not run, a job it needs did not pass".into(),
        );
        assert!(e.could_not_run());
        assert_eq!(crate::run::exit_code(&e), 2);
        assert!(crate::report::text(&e, "abc", &[]).contains("could not run, tree t from abc"));
        assert_eq!(
            body("abc", &e, None)["output"]["title"],
            "could not run, Docker or the sandbox failed before the code ran"
        );

        // One job that ran and failed makes it the code's failure again
        e.workflows[0].jobs.insert("lint".into(), "failure".into());
        assert!(!e.could_not_run());
        assert_eq!(crate::run::exit_code(&e), 1);
        assert_eq!(crate::run::exit_code(&entry(true)), 0);
        assert!(!entry(false).could_not_run());
        assert!(!entry(true).could_not_run());
    }

    #[test]
    fn failing_body_keeps_logs_local_for_a_fork() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ci.yml.log"), "token=hunter2\n").unwrap();
        let b = body("abc", &entry(false), None);
        let text = b["output"]["text"].as_str().unwrap();
        assert!(!text.contains("hunter2"));
        assert!(text.contains("shipcheck logs"));
    }

    #[test]
    fn failing_body_has_the_log_tail() {
        let dir = tempfile::tempdir().unwrap();
        let log: String = (1..=150).map(|i| format!("line {i}\n")).collect();
        let log = log.replace("line 120\n", "line 120 hunter2\n");
        fs::write(dir.path().join("ci.yml.log"), log).unwrap();
        let secret = vec!["hunter2".to_string()];
        let b = body("abc", &entry(false), Some((dir.path(), &secret)));
        assert_eq!(b["conclusion"], "failure");
        assert_eq!(b["output"]["title"], "failed, 1 of 1 workflows");
        let text = b["output"]["text"].as_str().unwrap();
        assert!(text.contains("line 150"));
        assert!(text.contains("line 51\n"));
        assert!(!text.contains("line 50\n"));
        assert!(text.contains("line 120 ***"));
        assert!(!text.contains("hunter2"));
    }

    #[test]
    fn names_host_checks() {
        let mut e = entry(false);
        e.workflows[0].file = HOST.into();
        e.workflows[0].jobs = BTreeMap::from([
            ("lint".into(), "success".into()),
            ("test".into(), "failure".into()),
        ]);
        e.workflows[0].shards.clear();
        let b = body("abc", &e, None);
        assert_eq!(b["output"]["title"], "failed, 1 of 2 host checks");
        assert!(
            b["output"]["summary"]
                .as_str()
                .unwrap()
                .contains("| host checks | failed | 1 | 2 | lint: success, test: failure |")
        );
    }

    fn jobs(list: &[(&str, &[&str])]) -> Jobs {
        list.iter()
            .map(|(f, js)| (f.to_string(), js.iter().map(|j| j.to_string()).collect()))
            .collect()
    }

    #[test]
    fn names_jobs_and_prefixes_only_shared_ids() {
        let names: Vec<String> = job_names(&jobs(&[
            ("ci.yml", &["lint", "test"]),
            ("e2e.yaml", &["test", "run"]),
        ]))
        .into_iter()
        .map(|(_, _, n)| n)
        .collect();
        assert_eq!(
            names,
            [
                "shipcheck/lint",
                "shipcheck/ci/test",
                "shipcheck/run",
                "shipcheck/e2e/test"
            ]
        );
        assert_eq!(
            job_names(&jobs(&[(HOST, &["test"])]))[0].2,
            "shipcheck/test"
        );
        // Shared stems take the whole file name, so every name stays unique
        let names: Vec<String> = job_names(&jobs(&[
            ("ci.yml", &["test", "lint"]),
            ("ci.yaml", &["test"]),
            ("ci.yml.yml", &["test", "lint"]),
        ]))
        .into_iter()
        .map(|(_, _, n)| n)
        .collect();
        assert_eq!(
            names,
            [
                "shipcheck/ci.yaml/test",
                "shipcheck/ci/lint",
                "shipcheck/ci.yml/test",
                "shipcheck/ci.yml/lint",
                "shipcheck/ci.yml.yml/test",
            ]
        );
        assert_eq!(names.iter().collect::<BTreeSet<_>>().len(), names.len());
    }

    #[test]
    fn a_check_per_job_with_its_own_log() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = entry(false);
        e.workflows[0].jobs.insert("lint".into(), "success".into());
        e.workflows[0]
            .jobs
            .insert("docs".into(), "cancelled".into());
        e.workflows[0]
            .jobs
            .insert("bench".into(), format!("failure{ALLOWED}"));
        fs::create_dir(dir.path().join("ci.yml")).unwrap();
        fs::write(dir.path().join("ci.yml/test.log"), "boom hunter2\n").unwrap();
        let secret = vec!["hunter2".to_string()];
        // `gone` is a job main has that the result lacks
        let expected = jobs(&[("ci.yml", &["bench", "docs", "gone", "lint", "test"])]);
        let bodies = job_bodies("abc", &expected, &e, Some((dir.path(), &secret)), false);
        let by = |n: &str| bodies.iter().find(|b| b["name"] == n).unwrap().clone();
        assert_eq!(bodies.len(), 5);
        assert_eq!(by("shipcheck/lint")["conclusion"], "success");
        assert_eq!(by("shipcheck/lint")["status"], "completed");
        assert_eq!(by("shipcheck/lint")["output"]["text"], "");
        assert_eq!(by("shipcheck/bench")["conclusion"], "neutral");
        assert_eq!(by("shipcheck/docs")["conclusion"], "failure");
        assert_eq!(by("shipcheck/gone")["conclusion"], "failure");
        assert_eq!(
            by("shipcheck/gone")["output"]["title"],
            "missing from the result"
        );
        let test = by("shipcheck/test");
        assert_eq!(test["head_sha"], "abc");
        assert_eq!(test["conclusion"], "failure");
        assert_eq!(test["output"]["text"], "```\nboom ***\n```\n");
        let local = job_bodies("abc", &expected, &e, None, false);
        assert!(
            local[4]["output"]["text"]
                .as_str()
                .unwrap()
                .contains("shipcheck logs")
        );
    }

    #[test]
    fn a_failed_workflow_with_no_failed_job_fails_every_job() {
        let mut e = entry(false);
        e.workflows[0].jobs = BTreeMap::from([("test".into(), "skipped".into())]);
        let bodies = job_bodies("abc", &Jobs::new(), &e, None, false);
        assert_eq!(bodies[0]["name"], "shipcheck/test");
        assert_eq!(bodies[0]["conclusion"], "failure");
    }

    #[test]
    fn all_logs_and_durations() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = entry(true);
        e.workflows[0]
            .times
            .insert("test".into(), [1_700_000_000, 1_700_000_042]);
        e.finished_unix = 1_700_000_050;
        fs::create_dir(dir.path().join("ci.yml")).unwrap();
        fs::write(dir.path().join("ci.yml/test.log"), "ok hunter2\n").unwrap();
        let secret = vec!["hunter2".to_string()];
        let logs = Some((dir.path(), secret.as_slice()));
        let all = job_bodies("abc", &Jobs::new(), &e, logs, true);
        assert_eq!(all[0]["output"]["text"], "```\nok ***\n```\n");
        assert_eq!(all[0]["started_at"], "2023-11-14T22:13:20Z");
        assert_eq!(all[0]["completed_at"], "2023-11-14T22:14:02Z");
        let failed_only = job_bodies("abc", &Jobs::new(), &e, logs, false);
        assert_eq!(failed_only[0]["output"]["text"], "");
        let summary = body("abc", &e, None);
        assert_eq!(summary["started_at"], "2023-11-14T22:13:20Z");
        assert_eq!(summary["completed_at"], "2023-11-14T22:14:10Z");
        // An entry from before times has none, GitHub then uses the post time
        assert!(body("abc", &entry(true), None).get("started_at").is_none());
        assert!(
            waiting("shipcheck/test", "s", true)
                .get("started_at")
                .is_some()
        );
        assert!(
            waiting("shipcheck/test", "s", false)
                .get("started_at")
                .is_none()
        );
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00Z");
        let old: WorkflowResult = serde_json::from_str(
            r#"{"file":"ci.yml","exit":0,"attempts":1,"log_sha256":"x","jobs":{},"passed":true}"#,
        )
        .unwrap();
        assert!(old.times.is_empty());
    }

    #[test]
    fn a_paths_skip_skips_every_job() {
        let mut e = entry(true);
        e.workflows.clear();
        e.skipped = Some("no change under paths (src/**)".into());
        let bodies = job_bodies(
            "abc",
            &jobs(&[("ci.yml", &["lint", "test"])]),
            &e,
            None,
            false,
        );
        assert_eq!(bodies.len(), 2);

        for b in &bodies {
            assert_eq!(b["status"], "completed");
            assert_eq!(b["conclusion"], "skipped");
        }
    }

    #[test]
    fn a_long_log_keeps_its_command_then_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = "$ cargo test\n".to_string();
        log.extend((1..=150).map(|i| format!("line {i}\n")));
        fs::write(dir.path().join("long.log"), &log).unwrap();
        let text = tail(&dir.path().join("long.log"), &[]);
        assert!(text.starts_with("$ cargo test\n[...]\nline 51\n"), "{text}");
        assert!(text.ends_with("line 150"));
        fs::write(dir.path().join("short.log"), "$ true\nok\n").unwrap();
        assert_eq!(tail(&dir.path().join("short.log"), &[]), "$ true\nok");
        fs::write(dir.path().join("act.log"), "x\n".repeat(150)).unwrap();
        assert!(tail(&dir.path().join("act.log"), &[]).starts_with("[...]\nx\n"));
    }

    #[test]
    fn a_failed_job_shows_its_failed_tests_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = "$ cargo test\ntest tests::parses ... FAILED\n\nfailures:\n\n".to_string();
        log.push_str("---- tests::parses stdout ----\nthread 'tests::parses' panicked at src/lib.rs:20:9:\ntoken hunter2 rejected\n\n");
        log.push_str("failures:\n    tests::parses\n\n");
        log.extend((1..=150).map(|i| format!("line {i}\n")));
        fs::create_dir(dir.path().join(HOST)).unwrap();
        fs::write(dir.path().join("host/test.log"), &log).unwrap();
        let mut e = entry(false);
        e.workflows[0].file = HOST.into();
        let secret = vec!["hunter2".to_string()];
        let bodies = job_bodies("abc", &Jobs::new(), &e, Some((dir.path(), &secret)), false);
        let text = bodies[0]["output"]["text"].as_str().unwrap();
        assert!(
            text.starts_with(
                "Failed tests (1):\n\n```\ntests::parses\n```\n\n```\n---- tests::parses stdout ----\nthread 'tests::parses' panicked at src/lib.rs:20:9:\ntoken *** rejected\n```\n\n```\n$ cargo test\n[...]\nline 51\n"
            ),
            "{text}"
        );
        assert!(text.ends_with("line 150\n```\n"));
        assert!(!text.contains("hunter2"));
    }

    #[test]
    fn masks_a_secret_the_parser_would_cut_or_unprefix() {
        let dir = tempfile::tempdir().unwrap();
        let secret = "s3cr3t-value-0123456789".to_string();
        // The secret straddles the 400-byte cut of a block line, and sits in a test name
        let long = format!("{}{secret} tail", "x".repeat(390));
        let p = "[CI/test 13f69a23]   |";
        let log = format!(
            "{p} test a::{secret} ... FAILED\n{p} ---- a::{secret} stdout ----\n{p} {long}\n{p} failures:\n[CI/test 13f69a23]   ❌  Failure - Main Run cargo test [1s]\n"
        );
        let path = dir.path().join("test.log");
        fs::write(&path, log).unwrap();
        let text = failed_text(&path, false, std::slice::from_ref(&secret));
        assert!(text.contains("---- a::*** stdout ----"), "{text}");
        assert!(!text.contains("s3cr3t"), "{text}");
    }

    #[test]
    fn too_many_failures_keep_the_first_whole() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = "$ cargo test\n".to_string();

        for i in 0..200 {
            let line = format!("panic {i} ").repeat(40);
            log.push_str(&format!("---- t{i} stdout ----\n{line}\n{line}\n{line}\n"));
        }

        log.push_str("failures:\nend\n");
        let path = dir.path().join("test.log");
        fs::write(&path, &log).unwrap();
        let text = failed_text(&path, true, &[]);
        assert!(text.len() <= MAX_FIELD, "{}", text.len());
        let first = "panic 0 ".repeat(40);
        assert!(text.contains(&format!("---- t0 stdout ----\n{first}\n{first}\n{first}\n")));
        let (head, _) = text.split_once(" more failed tests").unwrap();
        let kept = head.matches(" stdout ----").count();
        assert!(kept > 10, "{kept}");
        assert!(text.contains(&format!(
            "{} more failed tests were left out, `shipcheck logs` has the rest.",
            200 - kept
        )));
        assert!(text.ends_with("failures:\nend\n```\n"));
        // One failure larger than the field keeps its start
        let path = dir.path().join("huge.log");
        fs::write(
            &path,
            format!("---- big stdout ----\n{}\nfailures:\n", "x".repeat(90_000)),
        )
        .unwrap();
        let text = failed_text(&path, true, &[]);
        assert!(text.len() <= MAX_FIELD, "{}", text.len());
        assert!(text.starts_with("```\n---- big stdout ----\nxxx"));
        assert!(!text.contains("left out"));
    }

    #[test]
    fn clips_to_the_github_limit() {
        let long = "é".repeat(MAX_FIELD);
        let clipped = clip(long);
        assert!(clipped.len() <= MAX_FIELD);
        assert!(clipped.starts_with("[...cut...]"));
    }
}
