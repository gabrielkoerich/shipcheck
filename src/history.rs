use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

pub(crate) const MAX_BYTES: u64 = 50 * 1024 * 1024;

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

/// How long a full run of a repository takes on this machine, without its time in the queue
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Typical {
    pub median_s: u64,
    pub slowest_s: u64,
    pub runs: usize,
}

/// From the last ten full runs that ran: no partial run, no cached result, no `paths` skip
pub fn typical(runs: &[Value]) -> Option<Typical> {
    let mut times: Vec<u64> = runs
        .iter()
        .rev()
        .filter(|r| r["partial"] != true && r["cache_hit"] != true && r["skipped"] != true)
        .filter_map(|r| {
            let queued = r["queued_s"].as_u64().unwrap_or_default();
            Some(r["duration_s"].as_u64()?.saturating_sub(queued))
        })
        .take(10)
        .collect();
    times.sort_unstable();

    Some(Typical {
        median_s: *times.get(times.len() / 2)?,
        slowest_s: *times.last()?,
        runs: times.len(),
    })
}

pub fn flaky(runs: &[Value]) -> Vec<Value> {
    struct Seen {
        times: u64,
        loads: Vec<f64>,
        last: String,
    }

    let mut seen: BTreeMap<(String, Option<String>), Seen> = BTreeMap::new();

    for run in runs.iter().filter(|r| r["cache_hit"] != true) {
        for job in run["jobs"].as_array().into_iter().flatten() {
            if job["status"] != "success" || job["attempts"].as_u64().unwrap_or(1) < 2 {
                continue;
            }

            let name = format!(
                "{}/{}",
                job["workflow"].as_str().unwrap_or_default(),
                job["name"].as_str().unwrap_or_default()
            );
            let mut tests: Vec<Option<String>> = job["retried_tests"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t.as_str().map(|t| Some(t.to_string())))
                .collect();

            if tests.is_empty() {
                tests.push(None);
            }

            for test in tests {
                let entry = seen.entry((name.clone(), test)).or_insert(Seen {
                    times: 0,
                    loads: Vec::new(),
                    last: String::new(),
                });
                entry.times += 1;
                entry.loads.extend(run["load_1m"].as_f64());
                entry.last = run["finished"].as_str().unwrap_or_default().to_string();
            }
        }
    }

    let mut rows: Vec<Value> = seen
        .into_iter()
        .map(|((job, test), seen)| {
            let load = (!seen.loads.is_empty())
                .then(|| seen.loads.iter().sum::<f64>() / f64::from(u32::try_from(seen.loads.len()).unwrap_or(u32::MAX)))
                .map(|l| (l * 100.0).round() / 100.0);
            json!({"job": job, "test": test, "times": seen.times, "load_1m": load, "last": seen.last})
        })
        .collect();
    // Stable, so equal counts stay in the order of their names
    rows.sort_by_key(|r| std::cmp::Reverse(r["times"].as_u64()));
    rows
}

/// The flaky report as a table, `LOAD` being the machine's average load when the test flaked
pub fn flaky_table(rows: &[Value]) -> String {
    let mut out = format!(
        "{:>5} {:>6} {:<20} {:<28} {}\n",
        "TIMES", "LOAD", "LAST", "JOB", "TEST"
    );

    for r in rows {
        let _ = writeln!(
            out,
            "{:>5} {:>6} {:<20} {:<28} {}",
            r["times"].as_u64().unwrap_or_default(),
            r["load_1m"]
                .as_f64()
                .map_or("-".to_string(), |l| format!("{l:.2}")),
            r["last"]
                .as_str()
                .unwrap_or_default()
                .replace('T', " ")
                .replace('Z', ""),
            r["job"].as_str().unwrap_or_default(),
            r["test"].as_str().unwrap_or("(no test named)")
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::clock;

    #[test]
    fn flaky_counts_the_tests_that_passed_on_a_retry_most_first() {
        let job = |name: &str, status: &str, attempts: u64, tests: Value| json!({"workflow": "ci.yml", "name": name, "status": status, "attempts": attempts, "retried_tests": tests});
        let run = |load: Value, finished: &str, jobs: Value| json!({"event": "run", "load_1m": load, "finished": finished, "jobs": jobs});
        let runs = [
            run(
                json!(4.0),
                "2026-11-01T10:00:00Z",
                json!([job("test", "success", 2, json!(["timer", "port"]))]),
            ),
            run(
                json!(8.0),
                "2026-11-02T10:00:00Z",
                json!([
                    job("test", "success", 3, json!(["timer"])),
                    // A record from before the tests were kept, a first-try pass and a real failure
                    json!({"workflow": "ci.yml", "name": "e2e", "status": "success", "attempts": 2}),
                    job("lint", "success", 1, json!([])),
                    job("docs", "failure", 2, json!([])),
                ]),
            ),
        ];
        let rows = flaky(&runs);
        assert_eq!(
            rows,
            [
                json!({"job": "ci.yml/test", "test": "timer", "times": 2, "load_1m": 6.0, "last": "2026-11-02T10:00:00Z"}),
                json!({"job": "ci.yml/e2e", "test": null, "times": 1, "load_1m": 8.0, "last": "2026-11-02T10:00:00Z"}),
                json!({"job": "ci.yml/test", "test": "port", "times": 1, "load_1m": 4.0, "last": "2026-11-01T10:00:00Z"}),
            ]
        );
        let table = flaky_table(&rows);
        assert!(
            table
                .lines()
                .nth(1)
                .unwrap()
                .starts_with("    2   6.00 2026-11-02 10:00:00")
        );
        assert!(table.lines().nth(2).unwrap().ends_with("(no test named)"));
        assert_eq!(flaky(&[]), Vec::<Value>::new());
    }

    #[test]
    fn typical_is_of_the_last_ten_runs_that_ran_without_their_queue_time() {
        let run = |secs: u64, extra: Value| {
            let mut run = json!({"event": "run", "duration_s": secs});
            run.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            run
        };
        assert_eq!(typical(&[]), None);
        assert_eq!(
            typical(&[run(5, json!({"cache_hit": true}))]),
            None,
            "a cached result ran nothing"
        );

        let mut runs = vec![run(9000, json!({}))];
        runs.extend((1..=10).map(|n| run(n * 60, json!({}))));
        runs.push(run(1, json!({"partial": true})));
        runs.push(run(2, json!({"skipped": true})));
        runs.push(run(3, json!({"cache_hit": true})));
        runs.push(run(700, json!({"queued_s": 400})));
        // The eleventh run back is out, and the newest one took 300s once it had a slot
        assert_eq!(
            typical(&runs),
            Some(Typical {
                median_s: 360,
                slowest_s: 600,
                runs: 10
            })
        );
        assert_eq!(
            (clock(42), clock(367), clock(3600)),
            ("42s".into(), "6m07s".into(), "60m00s".into())
        );
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
}
