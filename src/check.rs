use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use crate::run::Entry;
use crate::runner::host::HOST;
use crate::runner::shard::ShardRun;

pub const NAME: &str = "shipcheck";
const TAIL_LINES: usize = 100;
// GitHub rejects check run output fields longer than this
const MAX_FIELD: usize = 65_535;

fn clip(text: String) -> String {
    if text.len() <= MAX_FIELD {
        return text;
    }

    let mut cut = text.len() - (MAX_FIELD - 20);

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
        let log = fs::read_to_string(log_dir.join(format!("{}.log", wf.file))).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        let mut tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");

        for secret in secrets {
            tail = tail.replace(secret.as_str(), "***");
        }

        s.push_str(&format!("### {}\n\n```\n{tail}\n```\n\n", name(&wf.file)));
    }

    clip(s)
}

/// Body of a check that has no result yet: `in_progress` while a run of its tree goes, which GitHub
/// shows as a yellow dot, or `queued` while no machine runs it
pub fn waiting(sha: &str, running: bool) -> Value {
    let (status, title, summary) = if running {
        (
            "in_progress",
            "running",
            "shipcheck is running this commit's workflows.",
        )
    } else {
        (
            "queued",
            "waiting for a run",
            "No machine has run this commit's tree yet. `shipcheck run`, or a push with the hooks installed, starts one.",
        )
    };

    json!({
        "name": NAME,
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

    json!({
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
    })
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
            }],
            finished_unix: 0,
            ci_changes: Vec::new(),
            skipped: None,
        }
    }

    #[test]
    fn waiting_has_no_conclusion() {
        for (running, status) in [(true, "in_progress"), (false, "queued")] {
            let body = waiting("s", running);
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
    fn failing_body_keeps_logs_local_by_default() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ci.yml.log"), "token=hunter2\n").unwrap();
        let b = body("abc", &entry(false), None);
        let text = b["output"]["text"].as_str().unwrap();
        assert!(!text.contains("hunter2"));
        assert!(text.contains("shipcheck logs"));
    }

    #[test]
    fn failing_body_has_the_log_tail_when_asked() {
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

    #[test]
    fn clips_to_the_github_limit() {
        let long = "é".repeat(MAX_FIELD);
        let clipped = clip(long);
        assert!(clipped.len() <= MAX_FIELD);
        assert!(clipped.starts_with("[...cut...]"));
    }
}
