use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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

/// `shipcheck queue`: one row per run, the running ones first
pub fn table(rows: &[Value]) -> String {
    let mut out = format!(
        "{:<8} {:>3} {:<28} {:<24} {:<12} {:<7} {:>8}  {:<24} {}\n",
        "STATE", "POS", "REPOSITORY", "BRANCH", "COMMIT", "BY", "ELAPSED", "JOBS", "PATH"
    );

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
            "{:<8} {:>3} {:<28} {:<24} {:<12} {:<7} {:>7}s  {:<24} {}\n",
            text("state"),
            r["position"]
                .as_u64()
                .map_or("-".to_string(), |p| p.to_string()),
            text("repo"),
            text("branch"),
            &sha[..sha.len().min(12)],
            text("trigger"),
            r["elapsed_s"].as_u64().unwrap_or_default(),
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
        let _hook = lock(&queue.join("1-001-3"), &run(3, "third"));
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

        drop(going);
        assert_eq!(list(dir.path(), 130)[0]["jobs"], json!([]));
        drop(note);
        drop(slot);
        assert_eq!(list(dir.path(), 130).len(), 2);
    }
}
