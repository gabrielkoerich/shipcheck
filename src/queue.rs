use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::record::{Typical, clock};

/// What a run says of itself in the machine's queue, in its ticket while it waits and beside its
/// slot's lock while it runs
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Run {
    pub repo: String,
    pub path: String,
    pub branch: Option<String>,
    pub sha: String,
    pub trigger: String,
    pub pid: u32,
    pub queued_unix: u64,
    pub started_unix: Option<u64>,
    /// The jobs going now, as `<file>/<job>`
    pub jobs: Vec<String>,
}

/// The note of a run that holds slot `n`, removed when the run ends
pub struct Note {
    path: PathBuf,
    run: Mutex<Run>,
}

impl Note {
    pub fn start(dir: &Path, n: usize, mut run: Run, now: u64) -> Note {
        run.started_unix = Some(now);
        let note = Note {
            path: dir.join(format!("{n}.run")),
            run: Mutex::new(run),
        };
        note.change(|_| {});
        note
    }

    /// The note is only for `shipcheck queue`, so a write that fails never fails the run
    fn change(&self, change: impl FnOnce(&mut Run)) {
        let mut run = self.run.lock().unwrap_or_else(|e| e.into_inner());
        change(&mut run);
        let temp = self.path.with_extension("run.new");

        if fs::write(&temp, json!(*run).to_string()).is_ok() {
            let _ = fs::rename(&temp, &self.path);
        }
    }

    /// Lists `job` as going until the guard drops
    pub fn job(&self, job: String) -> Going<'_> {
        self.change(|run| run.jobs.push(job.clone()));
        Going { note: self, job }
    }
}

impl Drop for Note {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub struct Going<'a> {
    note: &'a Note,
    job: String,
}

impl Drop for Going<'_> {
    fn drop(&mut self) {
        self.note.change(|run| {
            if let Some(at) = run.jobs.iter().position(|j| *j == self.job) {
                run.jobs.remove(at);
            }
        });
    }
}

/// Whether a run holds the lock on `path`. A file nobody holds is left by a run that is gone
fn held(path: &Path) -> bool {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .is_ok_and(|f| f.try_lock().is_err())
}

fn read(path: &Path) -> Run {
    fs::read(path)
        .ok()
        .and_then(|text| serde_json::from_slice(&text).ok())
        .unwrap_or_default()
}

fn row(run: &Run, position: Option<usize>, now: u64) -> Value {
    let since = run.started_unix.unwrap_or(run.queued_unix);
    let mut row = json!(run);
    row["state"] = json!(if position.is_some() {
        "queued"
    } else {
        "running"
    });
    row["position"] = json!(position);
    row["elapsed_s"] = json!(now.saturating_sub(since));
    row
}

/// The note of slot `note` when a run holds the slot. One a killed run left is removed under the
/// slot's lock, so a free slot is never locked twice and no waiting run finds it busy
fn running(note: &Path) -> Option<Run> {
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(note.with_extension("lock"))
        .ok()?;

    if lock.try_lock().is_ok() {
        let _ = fs::remove_file(note);
        return None;
    }

    Some(read(note))
}

/// Every live run of the machine under `dir`: the running ones, oldest first, then the queued ones
/// in the order they get a slot
pub fn list(dir: &Path, now: u64) -> Vec<Value> {
    let names = |dir: &Path| -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        names
    };
    let mut running: Vec<Run> = names(dir)
        .iter()
        .filter(|n| n.ends_with(".run"))
        .filter_map(|n| running(&dir.join(n)))
        .collect();
    running.sort_by_key(|r| r.started_unix);
    let queue = dir.join("queue");
    let queued: Vec<Run> = names(&queue)
        .iter()
        .filter(|n| held(&queue.join(n)))
        .map(|n| read(&queue.join(n)))
        // A run takes its slot before it drops its ticket, so for a moment it has both
        .filter(|q| !running.iter().any(|r| r.pid == q.pid && q.pid != 0))
        .collect();

    running
        .iter()
        .map(|r| row(r, None, now))
        .chain(
            queued
                .iter()
                .enumerate()
                .map(|(i, r)| row(r, Some(i + 1), now)),
        )
        .collect()
}

/**
 * When each run of `queued` gets a slot, in seconds from now, and last when a run that arrives now
 * does. `running` holds the seconds each running run has left and `queued` the seconds each
 * waiting run takes, in the queue's order
 **/
fn starts(running: &[u64], queued: &[u64], slots: usize) -> Vec<u64> {
    let mut free = running.to_vec();
    free.sort_unstable();
    // With more runs than slots, the first ones to end free no slot
    free.drain(..free.len().saturating_sub(slots.max(1)));
    free.resize(slots.max(1), 0);
    let mut out = Vec::new();

    for takes in queued.iter().map(Some).chain([None]) {
        let (slot, at) = free
            .iter()
            .copied()
            .enumerate()
            .min_by_key(|(_, at)| *at)
            .unwrap_or_default();
        out.push(at);

        if let Some(takes) = takes {
            free[slot] = at + takes;
        }
    }

    out
}

/// The wait of a run that arrives now, as [`forecast`] gives it
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Wait {
    pub wait_s: u64,
    /// The queued runs that get a slot first
    pub ahead: usize,
    /// Whether every run ahead has a typical time, without which the wait is a lower bound
    pub known: bool,
}

/**
 * Gives every row its repository's typical time as `estimate_s`, and every queued row the wait
 * until it starts as `wait_s`. Returns the wait of a run that arrives now: `shipcheck run` when
 * `cli`, which goes ahead of the queued runs the hooks and the daemon started
 **/
pub fn forecast(
    rows: &mut [Value],
    slots: usize,
    cli: bool,
    typical: &mut dyn FnMut(&str) -> Option<u64>,
) -> Wait {
    for row in rows.iter_mut() {
        row["estimate_s"] = json!(typical(row["repo"].as_str().unwrap_or_default()));
    }

    let takes = |r: &Value| r["estimate_s"].as_u64();
    let (queued, running): (Vec<&Value>, Vec<&Value>) =
        rows.iter().partition(|r| r["state"] == "queued");
    let left: Vec<u64> = running
        .iter()
        .map(|r| {
            takes(r)
                .unwrap_or_default()
                .saturating_sub(r["elapsed_s"].as_u64().unwrap_or_default())
        })
        .collect();
    let all: Vec<u64> = queued
        .iter()
        .map(|r| takes(r).unwrap_or_default())
        .collect();
    let ahead: Vec<&&Value> = queued
        .iter()
        .filter(|r| !cli || r["trigger"] == "cli")
        .collect();
    let first: Vec<u64> = ahead.iter().map(|r| takes(r).unwrap_or_default()).collect();
    let wait = Wait {
        wait_s: starts(&left, &first, slots).pop().unwrap_or_default(),
        ahead: ahead.len(),
        known: running
            .iter()
            .chain(ahead.iter().copied())
            .all(|r| takes(r).is_some()),
    };
    let waits = starts(&left, &all, slots);

    for (row, wait) in rows
        .iter_mut()
        .filter(|r| r["state"] == "queued")
        .zip(waits)
    {
        row["wait_s"] = json!(wait);
    }

    wait
}

/// `shipcheck run`'s first line: how long the run takes, and its wait when it has one
pub fn estimate_line(typical: &Typical, wait: &Wait) -> String {
    let mut line = format!(
        "estimate: about {}, the median of the last {} full runs here, the slowest took {}",
        clock(typical.median_s),
        typical.runs,
        clock(typical.slowest_s)
    );

    if wait.wait_s > 0 || wait.ahead > 0 {
        line.push_str(&format!(
            ", after {} {} in the queue with {} ahead",
            if wait.known { "about" } else { "at least" },
            clock(wait.wait_s),
            wait.ahead
        ));
    }

    line
}

/// `shipcheck queue`: one row per run, the running ones first
pub fn table(rows: &[Value]) -> String {
    let mut out = format!(
        "{:<8} {:>3} {:<28} {:<24} {:<12} {:<7} {:>8} {:>8} {:>8}  {:<24} {}\n",
        "STATE",
        "POS",
        "REPOSITORY",
        "BRANCH",
        "COMMIT",
        "BY",
        "ELAPSED",
        "TYPICAL",
        "STARTS",
        "JOBS",
        "PATH"
    );
    let time = |v: &Value| v.as_u64().map_or("-".to_string(), clock);

    for r in rows {
        let text = |key: &str| r[key].as_str().filter(|s| !s.is_empty()).unwrap_or("-");
        let sha = text("sha");
        let jobs: Vec<&str> = r["jobs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        out.push_str(&format!(
            "{:<8} {:>3} {:<28} {:<24} {:<12} {:<7} {:>8} {:>8} {:>8}  {:<24} {}\n",
            text("state"),
            r["position"]
                .as_u64()
                .map_or("-".to_string(), |p| p.to_string()),
            text("repo"),
            text("branch"),
            &sha[..sha.len().min(12)],
            text("trigger"),
            time(&r["elapsed_s"]),
            time(&r["estimate_s"]),
            time(&r["wait_s"]),
            if jobs.is_empty() {
                "-".to_string()
            } else {
                jobs.join(",")
            },
            text("path"),
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock(path: &Path, run: &Run) -> fs::File {
        fs::write(path, json!(run).to_string()).unwrap();
        let file = fs::File::open(path).unwrap();
        file.try_lock().unwrap();
        file
    }

    fn column_of(rows: &[Value], key: &str) -> Vec<Value> {
        rows.iter().map(|r| r[key].clone()).collect()
    }

    #[test]
    fn a_run_starts_when_the_runs_ahead_free_a_slot() {
        // Two slots: the first waiting run takes the one free in 10s, the next the one free in 30s
        assert_eq!(starts(&[30, 10], &[100, 5], 2), [10, 30, 35]);
        assert_eq!(starts(&[], &[], 2), [0]);
        assert_eq!(starts(&[50], &[], 2), [0]);
        // Three runs on two slots, as when the limit went down: the first to end frees none
        assert_eq!(starts(&[10, 20, 30], &[], 2), [20]);
    }

    #[test]
    fn a_queued_run_and_a_running_run_are_different_states() {
        let dir = tempfile::tempdir().unwrap();
        let queue = dir.path().join("queue");
        fs::create_dir_all(&queue).unwrap();
        let run = |pid: u32, branch: &str| Run {
            repo: "owner/app".into(),
            path: format!("/work/{branch}"),
            branch: Some(branch.into()),
            sha: "0123456789abcdef".into(),
            trigger: "cli".into(),
            pid,
            queued_unix: 100,
            ..Run::default()
        };

        let slot = fs::File::create(dir.path().join("0.lock")).unwrap();
        slot.try_lock().unwrap();
        let note = Note::start(dir.path(), 0, run(1, "first"), 110);
        let going = note.job("ci.yml/test".into());
        // The note and the ticket of runs that died list nothing
        fs::write(dir.path().join("1.lock"), "").unwrap();
        fs::write(
            dir.path().join("1.run"),
            json!(run(8, "killed")).to_string(),
        )
        .unwrap();
        fs::write(queue.join("0-000-9"), json!(run(9, "gone")).to_string()).unwrap();
        let hook = Run {
            trigger: "hook-push".into(),
            ..run(3, "third")
        };
        let _hook = lock(&queue.join("1-001-3"), &hook);
        let _agent = lock(&queue.join("0-002-2"), &run(2, "second"));

        let rows = list(dir.path(), 130);
        let column = |key: &str| rows.iter().map(|r| r[key].clone()).collect::<Vec<_>>();
        assert_eq!(column("state"), ["running", "queued", "queued"]);
        assert_eq!(column("branch"), ["first", "second", "third"]);
        assert_eq!(column("position"), [json!(null), json!(1), json!(2)]);
        assert_eq!(column("elapsed_s"), [20, 30, 30]);
        assert_eq!(rows[0]["started_unix"], 110);
        assert_eq!(rows[0]["jobs"], json!(["ci.yml/test"]));
        assert_eq!(rows[1]["started_unix"], json!(null));
        assert!(!dir.path().join("1.run").exists());

        let text = table(&rows);
        assert!(
            text.lines()
                .nth(1)
                .unwrap()
                .starts_with("running    - owner/app")
        );
        assert!(
            text.lines()
                .nth(2)
                .unwrap()
                .starts_with("queued     1 owner/app")
        );

        let mut rows = rows;
        let wait = forecast(&mut rows, 1, true, &mut |_| Some(100));
        // The running run has 80s left, and only the queued `shipcheck run` goes first
        assert_eq!(
            wait,
            Wait {
                wait_s: 180,
                ahead: 1,
                known: true
            }
        );
        assert_eq!(
            column_of(&rows, "wait_s"),
            [json!(null), json!(80), json!(180)]
        );
        assert_eq!(rows[0]["estimate_s"], 100);
        assert!(table(&rows).lines().nth(2).unwrap().contains("  1m20s "));
        assert!(!forecast(&mut rows, 1, true, &mut |_| None).known);
        let typical = Typical {
            median_s: 367,
            slowest_s: 552,
            runs: 10,
        };
        assert_eq!(
            estimate_line(&typical, &wait),
            "estimate: about 6m07s, the median of the last 10 full runs here, the slowest took 9m12s, after about 3m00s in the queue with 1 ahead"
        );
        let none = Wait {
            wait_s: 0,
            ahead: 0,
            known: true,
        };
        assert!(estimate_line(&typical, &none).ends_with("took 9m12s"));

        drop(going);
        assert_eq!(list(dir.path(), 130)[0]["jobs"], json!([]));
        drop(note);
        drop(slot);
        assert_eq!(list(dir.path(), 130).len(), 2);
    }
}
