use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use crate::check;
use crate::run::{Entry, printable};
use crate::runner::host::HOST;
use crate::runner::shard::ShardRun;

pub const MAX_TESTS: usize = 30;
const TAIL_LINES: usize = 60;
const MAX_LINE: usize = 400;

/// What `shipcheck run` says about one job: the step that failed, the tests and the output tail
#[derive(Debug, Serialize, PartialEq)]
pub struct JobReport {
    pub id: String,
    pub status: String,
    pub attempts: usize,
    /// How its pytest step ran with `shard_tests`, when it had one
    pub shards: Option<ShardRun>,
    pub failed_step: Option<String>,
    pub exit: Option<i32>,
    /// Plain words for a failure that is not the code's, such as a stale checkout
    pub note: Option<&'static str>,
    pub failing_tests: Vec<String>,
    /// How many failing tests the log named, more than `failing_tests` holds once capped
    pub failing_tests_total: usize,
    pub tail: Vec<String>,
    pub log: PathBuf,
    pub attempt_logs: Vec<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct WorkflowReport {
    pub file: String,
    pub passed: bool,
    pub exit: Option<i32>,
    pub attempts: u32,
    pub log: PathBuf,
    pub jobs: Vec<JobReport>,
}

/// The failed step of one job log, as act prints it
#[derive(Debug, Default, PartialEq)]
struct StepFailure {
    step: Option<String>,
    exit: Option<i32>,
    output: Vec<String>,
}

/// A line of act's log without its `[workflow/job id]` prefix and its indent
fn body(line: &str) -> &str {
    let line = line.trim_end_matches('\r');
    let line = match line.strip_prefix('[') {
        Some(rest) => rest.split_once("] ").map_or(line, |(_, r)| r),
        None => line,
    };
    line.trim_start()
}

/// Job output from act's lines: what the job printed, and act's own notes such as `::error::`
fn output_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    lines
        .map(body)
        .filter(|l| !l.starts_with('🐳'))
        .map(|l| clean(l.strip_prefix("| ").or(l.strip_prefix('|')).unwrap_or(l)))
        .collect()
}

/// A log line escaped for a terminal and capped at `MAX_LINE` bytes
fn clean(line: &str) -> String {
    let mut l = printable(line.trim_end_matches('\r'));

    if l.len() > MAX_LINE {
        let mut cut = MAX_LINE;

        while !l.is_char_boundary(cut) {
            cut -= 1;
        }

        l.truncate(cut);
        l.push_str("[...]");
    }

    l
}

/// The first step that failed the job, so a composite action names its inner step. A step with
/// `continue-on-error` prints "Failed but continue next step" first and does not count
fn step_failure(log: &str) -> StepFailure {
    let lines: Vec<&str> = log.lines().collect();
    let mut starts: Vec<(&str, usize)> = Vec::new();
    let mut found = StepFailure::default();
    let mut range = None;

    for (i, line) in lines.iter().enumerate() {
        let l = body(line);

        if let Some(name) = l.strip_prefix("⭐ Run Main ") {
            starts.push((name, i));
        } else if let Some(rest) = l.strip_prefix("❌  Failure - Main ") {
            let continued = i > 0 && body(lines[i - 1]) == "Failed but continue next step";

            if found.step.is_some() || continued {
                continue;
            }

            let name = match rest.rsplit_once(" [") {
                Some((name, _)) if rest.ends_with(']') => name,
                _ => rest,
            };
            let from = starts
                .iter()
                .rev()
                .find(|(n, _)| *n == name)
                .map_or(0, |(_, s)| s + 1);
            found.step = Some(printable(name));
            range = Some((from, i));
        } else if found.step.is_some()
            && found.exit.is_none()
            && let Some(rest) = l.strip_prefix("exitcode '")
        {
            found.exit = rest.split('\'').next().and_then(|c| c.parse().ok());
        }
    }

    found.output = match range {
        Some((from, to)) => output_lines(lines[from..to].iter().copied()),
        None => output_lines(lines.iter().copied()),
    };
    found
}

/// Names of the failing tests in a job's output, in the order the runners printed them
fn failing_tests(lines: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut phpunit = false;

    for line in lines {
        let t = line.trim();
        let name = if let Some(r) = t.strip_prefix("FAILED ").or(t.strip_prefix("ERROR ")) {
            // pytest's short summary, `FAILED path::test - message`
            let first = r.split_whitespace().next().unwrap_or("");
            first
                .contains(".py")
                .then(|| r.split(" - ").next().unwrap_or(r))
        } else if let Some(r) = t.strip_prefix("test ")
            && let Some(n) = r.strip_suffix(" ... FAILED")
        {
            Some(n)
        } else if let Some(r) = t.strip_prefix("FAIL [") {
            // nextest, `FAIL [   0.004s] crate tests::name`
            r.split_once(']').map(|(_, n)| n.trim())
        } else if let Some(r) = t.strip_prefix("--- FAIL: ") {
            Some(r.split(" (").next().unwrap_or(r))
        } else if let Some(r) = t.strip_prefix("● ") {
            (r != "Test suite failed to run").then_some(r)
        } else if let Some(r) = t.strip_prefix("FAIL ") {
            // jest's `FAIL file`, vitest's `FAIL  file > suite > test`
            Some(r.trim())
        } else if t.starts_with("rspec ./") {
            t.strip_prefix("rspec ")
        } else if t.starts_with("There w") && (t.contains(" failure") || t.contains(" error")) {
            // phpunit's `There was 1 failure:` heads a list of `1) Class::method`
            phpunit = t.ends_with(':');
            None
        } else if phpunit
            && let Some((n, rest)) = t.split_once(") ")
            && n.parse::<u32>().is_ok()
        {
            Some(rest)
        } else {
            None
        };

        if let Some(name) = name.filter(|n| !n.is_empty())
            && !names.iter().any(|n| n == name)
        {
            names.push(name.to_string());
        }
    }

    names
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Cargo,
    Nextest,
    Pytest,
    Jest,
    Vitest,
    Go,
}

/// The test whose own output starts at this line, and the runner that printed it
fn block_header(t: &str) -> Option<(Kind, &str)> {
    if let Some(r) = t.strip_prefix("---- ")
        && let Some(n) = r
            .strip_suffix(" stdout ----")
            .or(r.strip_suffix(" stderr ----"))
    {
        Some((Kind::Cargo, n))
    } else if let Some(r) = t
        .strip_prefix("--- STDOUT:")
        .or(t.strip_prefix("--- STDERR:"))
    {
        Some((Kind::Nextest, r.strip_suffix(" ---").unwrap_or(r).trim()))
    } else if t.starts_with("___") && t.ends_with('_') {
        Some((Kind::Pytest, t.trim_matches('_').trim()))
    } else if let Some(r) = t.strip_prefix("● ") {
        Some((Kind::Jest, r))
    } else if let Some(r) = t.strip_prefix("FAIL ")
        && r.contains(" > ")
    {
        Some((Kind::Vitest, r.trim()))
    } else if let Some(r) = t.strip_prefix("--- FAIL: ") {
        Some((Kind::Go, r.split(" (").next().unwrap_or(r)))
    } else {
        None
    }
    .filter(|(_, n)| !n.is_empty())
}

/// Whether this line ends a test's own output, as each runner prints the next thing
fn block_end(kind: Kind, line: &str, t: &str) -> bool {
    match kind {
        Kind::Cargo => t == "failures:" || t.starts_with("test result:"),
        // A status line such as `PASS [   0.004s] crate tests::name`
        Kind::Nextest => {
            t.starts_with("Summary [")
                || t.starts_with("------------")
                || t.split_once(" [").is_some_and(|(w, _)| {
                    !w.is_empty() && w.chars().all(|c| c.is_ascii_uppercase())
                })
        }
        Kind::Pytest => t.starts_with("===="),
        Kind::Jest => [
            "Test Suites:",
            "Tests:",
            "PASS ",
            "FAIL ",
            "Summary of all failing tests",
        ]
        .iter()
        .any(|p| t.starts_with(p)),
        Kind::Vitest => t.starts_with('⎯') || t.starts_with("Test Files"),
        Kind::Go => !line.starts_with([' ', '\t']) || t.is_empty(),
    }
}

/**
 * Each failing test's own output, its panic, assertion or traceback, as `(name, lines)` in the
 * order the runner printed them. Nextest prints stdout and stderr apart and both go to one test.
 * Jest prints every block again in its summary, so a name seen before opens nothing. `go test -v`
 * prints a test's lines above its `--- FAIL`, between it and its `=== RUN`
 **/
pub fn failure_blocks(lines: &[String]) -> Vec<(String, Vec<String>)> {
    let mut blocks: Vec<(String, Vec<String>)> = Vec::new();
    let mut open: Option<(Kind, usize)> = None;

    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();

        if let Some((kind, name)) = block_header(t) {
            let seen = blocks.iter().position(|(n, _)| n == name);
            open = match seen {
                Some(at) if kind == Kind::Nextest => {
                    blocks[at].1.push(line.clone());
                    Some((kind, at))
                }
                Some(_) => None,
                None => {
                    let mut own = vec![line.clone()];

                    if kind == Kind::Go {
                        let run = format!("=== RUN   {name}");
                        let from = lines[..i].iter().rposition(|l| l.trim() == run);
                        own.extend(
                            lines[from.map_or(i, |f| f + 1)..i]
                                .iter()
                                .filter(|l| l.starts_with([' ', '\t']))
                                .filter(|l| {
                                    !l.trim().starts_with("--- ") && !l.trim().starts_with("=== ")
                                })
                                .cloned(),
                        );
                    }

                    blocks.push((name.to_string(), own));
                    Some((kind, blocks.len() - 1))
                }
            };
        } else if let Some((kind, at)) = open {
            if block_end(kind, line, t) {
                open = None;
            } else {
                blocks[at].1.push(line.clone());
            }
        }
    }

    for (_, lines) in &mut blocks {
        while lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.pop();
        }
    }

    blocks
}

/// The failing tests a job's log names and each one's own output, for its check
pub fn failures(log: &str, host: bool) -> (Vec<String>, Vec<(String, Vec<String>)>) {
    let output = job_output(log, host).output;
    (failing_tests(&output), failure_blocks(&output))
}

/// A host check's log is its command's raw output, with no act steps to find
fn job_output(log: &str, host: bool) -> StepFailure {
    if host {
        StepFailure {
            output: log.lines().map(clean).collect(),
            ..StepFailure::default()
        }
    } else {
        step_failure(log)
    }
}

fn note(status: &str, step: Option<&str>) -> Option<&'static str> {
    if step == Some("shipcheck checkout") {
        return Some(
            "the job saw a stale or empty checkout, not the one shipcheck wrote. This is the Docker engine's file sharing, not the code. Run again, and run `shipcheck doctor` if it repeats",
        );
    }

    match status {
        s if s.starts_with("no image") => Some(
            "unsupported: a macOS or Windows job, which runs only on GitHub, or a Linux label with no image in `[act] platforms`. Tell the repository owner",
        ),
        s if s.starts_with("timed out") => Some("the run went past `timeout_minutes`"),
        s if s.starts_with("killed") => Some("the job started and never ended"),
        s if s.contains("shipcheck could not run it") => {
            Some("shipcheck could not start the job, the run's output above says why")
        }
        _ => None,
    }
}

fn failed(status: &str) -> bool {
    status != "success" && status != "skipped" && !status.contains("allowed by continue-on-error")
}

fn job_report(
    dir: &Path,
    id: &str,
    status: &str,
    shards: Option<ShardRun>,
    host: bool,
) -> JobReport {
    let log = dir.join(format!("{id}.log"));
    let prefix = format!("{id}.attempt-");
    let mut attempt_logs: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with(&prefix) && name.ends_with(".log")
        })
        .map(|e| e.path())
        .collect();
    attempt_logs.sort();
    let mut report = JobReport {
        id: id.to_string(),
        status: printable(status),
        attempts: attempt_logs.len() + 1,
        shards,
        failed_step: None,
        exit: None,
        note: None,
        failing_tests: Vec::new(),
        failing_tests_total: 0,
        tail: Vec::new(),
        log: log.clone(),
        attempt_logs,
    };

    // A job that never ran has no log, and its status says why
    if !failed(status) || status.starts_with("not run") {
        return report;
    }

    let text = fs::read(&log).unwrap_or_default();
    let text = String::from_utf8_lossy(&text);
    let found = job_output(&text, host);
    let tests = failing_tests(&found.output);
    report.note = note(status, found.step.as_deref());
    report.failing_tests_total = tests.len();
    report.failing_tests = tests.into_iter().take(MAX_TESTS).collect();
    report.tail = found.output[found.output.len().saturating_sub(TAIL_LINES)..].to_vec();
    report.failed_step = found.step;
    report.exit = found.exit;
    report
}

/// Reads the logs of every workflow in the entry from `log_dir`, as `run` wrote them
pub fn workflows(entry: &Entry, log_dir: &Path) -> Vec<WorkflowReport> {
    entry
        .workflows
        .iter()
        .map(|wf| WorkflowReport {
            file: wf.file.clone(),
            passed: wf.passed,
            exit: wf.exit,
            attempts: wf.attempts,
            log: log_dir.join(format!("{}.log", wf.file)),
            jobs: wf
                .jobs
                .iter()
                .map(|(id, status)| {
                    let shards = wf.shards.get(id).cloned();
                    job_report(&log_dir.join(&wf.file), id, status, shards, wf.file == HOST)
                })
                .collect(),
        })
        .collect()
}

/// The text `shipcheck run` prints: one line per workflow that passed, and for each failed job the
/// step, the tests and the end of its output. Job output stays on this machine, so none is redacted
pub fn text(entry: &Entry, tested: &str, workflows: &[WorkflowReport]) -> String {
    let mut s = String::new();

    if !entry.ci_changes.is_empty() {
        s.push_str("this commit changes CI: workflow and shipcheck.toml changes did not run, the base's versions did, changes to what a command runs did run:\n");

        for change in &entry.ci_changes {
            s.push_str(&format!("  {change}\n"));
        }
    }

    if let Some(reason) = &entry.skipped {
        s.push_str(&format!("nothing to run: {reason}\n"));
    }

    for wf in workflows {
        let exit = wf.exit.map_or("timed out".into(), |c| format!("exit {c}"));
        let attempts = match wf.attempts {
            1 => "1 attempt".to_string(),
            n => format!("{n} attempts"),
        };

        if wf.passed {
            s.push_str(&format!("{}: passed, {attempts}\n", check::name(&wf.file)));
            continue;
        }

        s.push_str(&format!(
            "{}: failed, {exit}, {attempts}\n",
            check::name(&wf.file)
        ));

        for job in &wf.jobs {
            let shards = match &job.shards {
                Some(ShardRun {
                    fallback: Some(why),
                    ..
                }) => format!(", after an unsharded rerun ({why})"),
                Some(ShardRun {
                    shards: Some(n), ..
                }) => format!(", tests in {n} shards"),
                _ => String::new(),
            };
            s.push_str(&format!("  {}: {}{shards}\n", job.id, job.status));

            if !failed(&job.status) || job.status.starts_with("not run") {
                continue;
            }

            if let Some(step) = &job.failed_step {
                let exit = job.exit.map_or(String::new(), |c| format!(", exit {c}"));
                s.push_str(&format!("    failed step: {step}{exit}\n"));
            }

            if let Some(note) = job.note {
                s.push_str(&format!("    {note}\n"));
            }

            if !job.failing_tests.is_empty() {
                s.push_str(&format!(
                    "    failing tests ({}):\n",
                    job.failing_tests_total
                ));

                for t in &job.failing_tests {
                    s.push_str(&format!("      {t}\n"));
                }

                if job.failing_tests_total > job.failing_tests.len() {
                    s.push_str(&format!(
                        "      and {} more, in the log\n",
                        job.failing_tests_total - job.failing_tests.len()
                    ));
                }
            }

            if !job.tail.is_empty() {
                let of = match (&job.failed_step, wf.file == HOST) {
                    (Some(_), _) => "the step",
                    (None, true) => "the check",
                    (None, false) => "the job",
                };
                s.push_str(&format!("    last {} lines of {of}:\n", job.tail.len()));

                for line in &job.tail {
                    s.push_str(&format!("      {line}\n"));
                }
            }

            s.push_str(&format!("    full log: {}\n", job.log.display()));

            for log in &job.attempt_logs {
                s.push_str(&format!("    earlier attempt: {}\n", log.display()));
            }
        }
    }

    if entry.passed {
        s.push_str(&format!("passed, tree {} from {tested}\n", entry.tree));
    } else {
        s.push_str(&format!(
            "failed, tree {} from {tested}. `shipcheck logs` prints every log\n",
            entry.tree
        ));
    }

    s
}

/// The object `shipcheck run --json` prints, a contract agents parse
pub fn json(entry: &Entry, tested: &str, workflows: &[WorkflowReport], partial: bool) -> Value {
    json!({
        "partial": partial,
        "tree": entry.tree,
        "sha": tested,
        "passed": entry.passed,
        "skipped": entry.skipped,
        "ci_changes": entry.ci_changes,
        "workflows": workflows,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::run::WorkflowResult;

    const PREFIX: &str = "[CI/test 13f69a23]";

    fn act_log(steps: &[(&str, &[&str], bool)]) -> String {
        let mut log = format!("{PREFIX} ⭐ Run Set up job\n{PREFIX}   ✅  Success - Set up job\n");

        for (name, output, ok) in steps {
            log.push_str(&format!("{PREFIX} ⭐ Run Main {name}\n"));
            log.push_str(&format!(
                "{PREFIX}   🐳  docker exec cmd=[bash -e /var/run/act/workflow/1] user= workdir=\n"
            ));

            for line in *output {
                log.push_str(&format!("{PREFIX}   | {line}\n"));
            }

            if *ok {
                log.push_str(&format!("{PREFIX}   ✅  Success - Main {name} [1.2s]\n"));
            } else {
                log.push_str(&format!("{PREFIX}   ❌  Failure - Main {name} [1.2s]\n"));
                log.push_str(&format!("{PREFIX} exitcode '1': failure\n"));
            }
        }

        log.push_str(&format!("{PREFIX} 🏁  Job failed\n"));
        log
    }

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(String::from).collect()
    }

    #[test]
    fn finds_the_failed_step_its_exit_and_output() {
        let log = act_log(&[
            ("shipcheck start", &["shipcheck-start n test"], true),
            ("Run uv sync", &["Installed 5 packages"], true),
            (
                "Run pytest",
                &["collected 2 items", "FAILED t.py::a - x"],
                false,
            ),
        ]);
        let f = step_failure(&log);
        assert_eq!(f.step.as_deref(), Some("Run pytest"));
        assert_eq!(f.exit, Some(1));
        assert_eq!(f.output, lines("collected 2 items\nFAILED t.py::a - x"));
    }

    #[test]
    fn skips_a_continue_on_error_step_and_names_the_inner_composite_step() {
        let log = format!(
            "{PREFIX} ⭐ Run Main Run bean-check\n\
             {PREFIX} Failed but continue next step\n\
             {PREFIX}   ❌  Failure - Main Run bean-check [1.7s]\n\
             {PREFIX} ⭐ Run Main ./.github/actions/setup\n\
             {PREFIX} ⭐ Run Main Install deps\n\
             {PREFIX}   | E: no such package\n\
             {PREFIX}   ❌  Failure - Main Install deps [1s]\n\
             {PREFIX}   ❌  Failure - Main ./.github/actions/setup [2s]\n\
             {PREFIX} exitcode '100': failure\n"
        );
        let f = step_failure(&log);
        assert_eq!(f.step.as_deref(), Some("Install deps"));
        assert_eq!(f.exit, Some(100));
        assert_eq!(f.output, lines("E: no such package"));
    }

    #[test]
    fn without_a_failed_step_the_output_is_the_whole_log() {
        let f = step_failure(&format!("{PREFIX}   | hello\r\n{PREFIX}   |   indented\n"));
        assert_eq!(f.step, None);
        assert_eq!(f.output, lines("hello\n  indented"));
    }

    #[test]
    fn escapes_control_characters_and_caps_long_lines() {
        let f = step_failure(&format!(
            "{PREFIX}   | \x1b[31mred\n{PREFIX}   | {}\n",
            "é".repeat(300)
        ));
        assert_eq!(f.output[0], "\\u{1b}[31mred");
        assert!(f.output[1].len() <= MAX_LINE + 5);
        assert!(f.output[1].ends_with("[...]"));
    }

    #[test]
    fn parses_pytest() {
        let out = lines(
            "tests/test_a.py::test_one FAILED [ 50%]\n\
             =========================== short test summary info ============================\n\
             FAILED tests/test_a.py::test_one - assert 1 == 2\n\
             FAILED tests/test_a.py::test_two[param-1] - ValueError: bad\n\
             ERROR tests/test_b.py - ModuleNotFoundError: No module named 'x'\n\
             ERROR: something else",
        );
        assert_eq!(
            failing_tests(&out),
            [
                "tests/test_a.py::test_one",
                "tests/test_a.py::test_two[param-1]",
                "tests/test_b.py"
            ]
        );
    }

    #[test]
    fn parses_cargo_and_nextest() {
        let out = lines(
            "test cache::tests::key ... ok\n\
             test run::tests::plan ... FAILED\n\
             test result: FAILED. 1 passed; 1 failed\n\
             \x20       FAIL [   0.004s] shipcheck report::tests::json\n\
             \x20       FAIL [   0.004s] shipcheck report::tests::json",
        );
        assert_eq!(
            failing_tests(&out),
            ["run::tests::plan", "shipcheck report::tests::json"]
        );
    }

    #[test]
    fn parses_go() {
        let out = lines(
            "=== RUN   TestAdd\n--- FAIL: TestAdd (0.00s)\n    --- FAIL: TestAdd/negative (0.00s)\nFAIL\tgithub.com/x/y\t0.003s",
        );
        assert_eq!(failing_tests(&out), ["TestAdd", "TestAdd/negative"]);
    }

    #[test]
    fn parses_jest_and_vitest() {
        let out = lines(
            "FAIL src/sum.test.js\n  ● math › adds\n  ● Test suite failed to run\n FAIL  src/b.test.ts > suite > works",
        );
        assert_eq!(
            failing_tests(&out),
            [
                "src/sum.test.js",
                "math › adds",
                "src/b.test.ts > suite > works"
            ]
        );
    }

    #[test]
    fn parses_rspec() {
        let out = lines("Failed examples:\n\nrspec ./spec/user_spec.rb:12 # User saves");
        assert_eq!(failing_tests(&out), ["./spec/user_spec.rb:12 # User saves"]);
    }

    #[test]
    fn parses_phpunit() {
        let out = lines(
            "1) not a test\nThere were 2 failures:\n\n1) Tests\\FooTest::testBar\nFailed asserting that false is true.\n\n/app/tests/FooTest.php:12\n\n2) Tests\\FooTest::testBaz\n\nThere was 1 error:\n\n1) Tests\\BarTest::testQux",
        );
        assert_eq!(
            failing_tests(&out),
            [
                "Tests\\FooTest::testBar",
                "Tests\\FooTest::testBaz",
                "Tests\\BarTest::testQux"
            ]
        );
    }

    fn blocks(text: &str) -> Vec<(String, Vec<String>)> {
        failure_blocks(&lines(text))
    }

    fn block(name: &str, text: &str) -> (String, Vec<String>) {
        (name.to_string(), lines(text))
    }

    #[test]
    fn finds_cargo_test_failures_among_passes() {
        let mut log: String = (0..40)
            .map(|i| format!("test tests::ok_{i} ... ok\n"))
            .collect();
        log.push_str(
            "test tests::parses ... FAILED\n\
             test tests::divides ... FAILED\n\
             \n\
             failures:\n\
             \n\
             ---- tests::parses stdout ----\n\
             \n\
             thread 'tests::parses' panicked at src/lib.rs:20:9:\n\
             assertion `left == right` failed\n\
             \x20 left: 3\n\
             \x20right: 4\n\
             note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\
             \n\
             ---- tests::divides stdout ----\n\
             \n\
             thread 'tests::divides' panicked at src/lib.rs:25:9:\n\
             attempt to divide by zero\n\
             \n\
             \n\
             failures:\n\
             \x20   tests::parses\n\
             \x20   tests::divides\n\
             \n\
             test result: FAILED. 40 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out\n",
        );
        let out = lines(&log);
        assert_eq!(failing_tests(&out), ["tests::parses", "tests::divides"]);
        assert_eq!(
            failure_blocks(&out),
            [
                block(
                    "tests::parses",
                    "---- tests::parses stdout ----\n\nthread 'tests::parses' panicked at src/lib.rs:20:9:\nassertion `left == right` failed\n  left: 3\n right: 4\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace"
                ),
                block(
                    "tests::divides",
                    "---- tests::divides stdout ----\n\nthread 'tests::divides' panicked at src/lib.rs:25:9:\nattempt to divide by zero"
                ),
            ]
        );
    }

    #[test]
    fn finds_nextest_stdout_and_stderr() {
        let found = blocks(
            "    Starting 3 tests across 1 binary\n\
             \x20       PASS [   0.003s] demo tests::adds\n\
             \x20       FAIL [   0.004s] demo tests::parses\n\
             \n\
             --- STDOUT:              demo tests::parses ---\n\
             \n\
             running 1 test\n\
             test tests::parses ... FAILED\n\
             \n\
             --- STDERR:              demo tests::parses ---\n\
             thread 'tests::parses' panicked at src/lib.rs:20:9:\n\
             assertion `left == right` failed\n\
             \n\
             \x20       PASS [   0.002s] demo tests::subs\n\
             ------------\n\
             \x20    Summary [   0.010s] 3 tests run: 2 passed, 1 failed, 0 skipped\n\
             \x20       FAIL [   0.004s] demo tests::parses\n\
             error: test run failed",
        );
        assert_eq!(
            found,
            [block(
                "demo tests::parses",
                "--- STDOUT:              demo tests::parses ---\n\nrunning 1 test\ntest tests::parses ... FAILED\n\n--- STDERR:              demo tests::parses ---\nthread 'tests::parses' panicked at src/lib.rs:20:9:\nassertion `left == right` failed"
            )]
        );
    }

    #[test]
    fn finds_pytest_assert_and_traceback() {
        let found = blocks(
            "============================= test session starts ==============================\n\
             collected 5 items\n\
             \n\
             tests/test_calc.py ..F.                                                  [ 80%]\n\
             tests/test_io.py F                                                       [100%]\n\
             \n\
             =================================== FAILURES ===================================\n\
             _________________________________ test_divide __________________________________\n\
             \n\
             \x20   def test_divide():\n\
             >       assert divide(4, 2) == 3\n\
             E       assert 2.0 == 3\n\
             \n\
             tests/test_calc.py:12: AssertionError\n\
             __________________________________ test_read ___________________________________\n\
             \n\
             \x20   def test_read():\n\
             >       load(\"missing.txt\")\n\
             \n\
             tests/test_io.py:4: in load\n\
             \x20   return open(path).read()\n\
             E   FileNotFoundError: [Errno 2] No such file or directory: 'missing.txt'\n\
             =========================== short test summary info ============================\n\
             FAILED tests/test_calc.py::test_divide - assert 2.0 == 3\n\
             FAILED tests/test_io.py::test_read - FileNotFoundError: [Errno 2] No such file...\n\
             ========================= 2 failed, 3 passed in 0.03s ==========================",
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, "test_divide");
        assert_eq!(
            found[0].1.last().unwrap(),
            "tests/test_calc.py:12: AssertionError"
        );
        assert_eq!(found[1].0, "test_read");
        assert!(
            found[1]
                .1
                .last()
                .unwrap()
                .starts_with("E   FileNotFoundError")
        );
    }

    #[test]
    fn finds_jest_blocks_once() {
        let found = blocks(
            "FAIL src/sum.test.js\n\
             \x20 math\n\
             \x20   ✓ subtracts (2 ms)\n\
             \x20   ✕ adds (3 ms)\n\
             \n\
             \x20 ● math › adds\n\
             \n\
             \x20   expect(received).toBe(expected) // Object.is equality\n\
             \n\
             \x20   Expected: 4\n\
             \x20   Received: 5\n\
             \n\
             \x20     at Object.<anonymous> (src/sum.test.js:4:21)\n\
             \n\
             PASS src/other.test.js\n\
             Summary of all failing tests\n\
             FAIL src/sum.test.js\n\
             \x20 ● math › adds\n\
             \n\
             \x20   Expected: 4\n\
             \n\
             Test Suites: 1 failed, 1 passed, 2 total\n\
             Tests:       1 failed, 3 passed, 4 total",
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "math › adds");
        assert_eq!(found[0].1.len(), 8);
        assert_eq!(
            found[0].1[7].trim(),
            "at Object.<anonymous> (src/sum.test.js:4:21)"
        );
    }

    #[test]
    fn finds_go_test_output_above_and_below_its_fail() {
        let found = blocks(
            "=== RUN   TestAdd\n\
             \x20   add_test.go:9: Add(2, 2) = 5, want 4\n\
             --- FAIL: TestAdd (0.00s)\n\
             === RUN   TestSub\n\
             --- PASS: TestSub (0.00s)\n\
             --- FAIL: TestMul (0.00s)\n\
             \x20   mul_test.go:7: Mul(2, 3) = 5, want 6\n\
             FAIL\n\
             FAIL\texample.com/calc\t0.002s",
        );
        assert_eq!(
            found,
            [
                block(
                    "TestAdd",
                    "--- FAIL: TestAdd (0.00s)\n    add_test.go:9: Add(2, 2) = 5, want 4"
                ),
                block(
                    "TestMul",
                    "--- FAIL: TestMul (0.00s)\n    mul_test.go:7: Mul(2, 3) = 5, want 6"
                ),
            ]
        );
    }

    #[test]
    fn finds_blocks_in_an_act_log() {
        let log = act_log(&[(
            "Run cargo test",
            &[
                "test a::b ... FAILED",
                "---- a::b stdout ----",
                "thread 'a::b' panicked at src/a.rs:3:5:",
                "failures:",
            ],
            false,
        )]);
        let (names, found) = failures(&log, false);
        assert_eq!(names, ["a::b"]);
        assert_eq!(
            found,
            [block(
                "a::b",
                "---- a::b stdout ----\nthread 'a::b' panicked at src/a.rs:3:5:"
            )]
        );
    }

    fn entry(jobs: &[(&str, &str)]) -> Entry {
        Entry {
            key: "k".into(),
            sha: "abc".into(),
            tree: "t".into(),
            base: "origin/main".into(),
            passed: false,
            workflows: vec![WorkflowResult {
                file: "ci.yml".into(),
                exit: Some(1),
                attempts: 2,
                log_sha256: String::new(),
                jobs: jobs
                    .iter()
                    .map(|(j, s)| (j.to_string(), s.to_string()))
                    .collect::<BTreeMap<_, _>>(),
                passed: false,
                shards: BTreeMap::from([(
                    "lint".to_string(),
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
            skipped: None,
            ci_changes: Vec::new(),
        }
    }

    fn pytest_logs(dir: &Path, failures: usize) {
        let mut output: Vec<String> = (0..80).map(|i| format!("line {i}")).collect();
        output.extend((0..failures).map(|i| format!("FAILED t.py::test_{i} - assert")));
        let output: Vec<&str> = output.iter().map(String::as_str).collect();
        fs::create_dir_all(dir.join("ci.yml")).unwrap();
        fs::write(
            dir.join("ci.yml/test.log"),
            act_log(&[("Run pytest", &output, false)]),
        )
        .unwrap();
        fs::write(dir.join("ci.yml/test.attempt-1.log"), "").unwrap();
    }

    #[test]
    fn caps_the_tests_and_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        pytest_logs(dir.path(), 40);
        let e = entry(&[("test", "failure"), ("lint", "success")]);
        let wfs = workflows(&e, dir.path());
        let test = &wfs[0].jobs[1];
        assert_eq!(test.failing_tests.len(), MAX_TESTS);
        assert_eq!(test.failing_tests_total, 40);
        assert_eq!(test.tail.len(), TAIL_LINES);
        assert_eq!(test.tail.last().unwrap(), "FAILED t.py::test_39 - assert");
        assert_eq!(test.attempts, 2);
        let text = text(&e, "abc", &wfs);
        assert!(text.contains("    failing tests (40):\n      t.py::test_0\n"));
        assert!(text.contains("      and 10 more, in the log\n"));
        assert!(text.contains("  lint: success, tests in 4 shards\n"));
        assert!(text.ends_with("failed, tree t from abc. `shipcheck logs` prints every log\n"));
    }

    #[test]
    fn json_shape() {
        let dir = tempfile::tempdir().unwrap();
        pytest_logs(dir.path(), 1);
        let e = entry(&[
            ("test", "failure"),
            ("deps", "not run, a job it needs did not pass"),
        ]);
        let v = json(&e, "working copy", &workflows(&e, dir.path()), false);
        assert_eq!(v["tree"], "t");
        assert_eq!(v["sha"], "working copy");
        assert_eq!(v["passed"], false);
        let wf = &v["workflows"][0];
        assert_eq!(wf["file"], "ci.yml");
        assert_eq!(wf["exit"], 1);
        assert_eq!(wf["log"], dir.path().join("ci.yml.log").to_str().unwrap());
        let deps = &wf["jobs"][0];
        assert_eq!(deps["id"], "deps");
        assert_eq!(deps["failed_step"], Value::Null);
        assert_eq!(deps["tail"], json!([]));
        let test = &wf["jobs"][1];
        assert_eq!(test["status"], "failure");
        assert_eq!(test["attempts"], 2);
        assert_eq!(test["failed_step"], "Run pytest");
        assert_eq!(test["exit"], 1);
        assert_eq!(test["failing_tests"], json!(["t.py::test_0"]));
        assert_eq!(test["failing_tests_total"], 1);
        assert_eq!(test["tail"].as_array().unwrap().len(), TAIL_LINES);
        assert_eq!(
            test["attempt_logs"][0],
            dir.path()
                .join("ci.yml/test.attempt-1.log")
                .to_str()
                .unwrap()
        );
    }

    #[test]
    fn names_a_stale_checkout_and_an_unsupported_job() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("ci.yml")).unwrap();
        fs::write(
            dir.path().join("ci.yml/test.log"),
            act_log(&[("shipcheck checkout", &["nonce mismatch"], false)]),
        )
        .unwrap();
        let e = entry(&[
            ("test", "failure"),
            ("mac", "no image for its runs-on label on this host"),
        ]);
        let text = text(&e, "abc", &workflows(&e, dir.path()));
        assert!(text.contains("stale or empty checkout"));
        assert!(text.contains("unsupported: a macOS or Windows job"));
    }

    #[test]
    fn a_host_check_shows_its_raw_output() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(HOST)).unwrap();
        fs::write(
            dir.path().join("host/test.log"),
            "$ cargo test\n[INFO] start\ntest a::b ... FAILED\n",
        )
        .unwrap();
        let mut e = entry(&[("test", "failure")]);
        e.workflows[0].file = HOST.into();
        let wfs = workflows(&e, dir.path());
        let test = &wfs[0].jobs[0];
        assert_eq!(test.failed_step, None);
        assert_eq!(test.failing_tests, ["a::b"]);
        assert_eq!(
            test.tail,
            lines("$ cargo test\n[INFO] start\ntest a::b ... FAILED")
        );
        let text = text(&e, "abc", &wfs);
        assert!(text.starts_with("host checks: failed, exit 1, 2 attempts\n  test: failure\n"));
        assert!(text.contains("    last 3 lines of the check:\n      $ cargo test\n"));
    }

    #[test]
    fn a_pass_is_one_line_per_workflow() {
        let mut e = entry(&[("test", "success")]);
        e.passed = true;
        e.workflows[0].passed = true;
        e.workflows[0].attempts = 1;
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            text(&e, "abc", &workflows(&e, dir.path())),
            "ci.yml: passed, 1 attempt\npassed, tree t from abc\n"
        );
    }
}
