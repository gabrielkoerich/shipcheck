use super::{Entry, HAS_SLOT, Plan, QUEUED_S, WAITING_FOR_SLOT, lookup};
use crate::cache;
use crate::clock::unix_now;
use crate::git::repo_id;
use crate::paths::home;
use crate::queue;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(super) fn nonce(key: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let h = Sha256::digest(format!("{key}{}{}", now.as_nanos(), std::process::id()));
    cache::hex(&h[..8])
}

pub(super) fn claim_slot(slots: &Path) -> Result<(PathBuf, fs::File), String> {
    for n in 0..64 {
        let lock = fs::File::create(slots.join(format!("{n}.lock"))).map_err(|e| e.to_string())?;

        if lock.try_lock().is_ok() {
            return Ok((slots.join(n.to_string()), lock));
        }
    }

    Err("all 64 run slots of this repository are busy".into())
}

/// Held while one tree runs, so a second request for it waits and then reads the cached result
pub fn lock_key(key: &str) -> Result<fs::File, String> {
    let dir = home().join("locks");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let lock = fs::File::create(dir.join(format!("{key}.lock"))).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    Ok(lock)
}

pub fn lock_tree(key: &str) -> Result<(fs::File, Option<u64>), String> {
    let waited = running(key).then(unix_now);
    Ok((lock_key(key)?, waited))
}

/// A stored pass, or any result a run stored while this one waited for it, so a failure that just
/// ran is not run again at once. A failure found later runs again, so a flaky one cannot stick
pub fn reusable(plan: &Plan, waited: Option<u64>) -> Option<Entry> {
    lookup(plan).filter(|e| e.passed || waited.is_some_and(|t| e.finished_unix >= t))
}

/// The one key under `home` that starts with `id`, a run's id, among the trees that ran or run
pub fn find_key(home: &Path, id: &str) -> Option<String> {
    if id.len() < 6 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }

    let mut keys = BTreeSet::new();

    for (dir, ext) in [("locks", ".lock"), ("cache", ".json")] {
        for entry in fs::read_dir(home.join(dir)).into_iter().flatten().flatten() {
            let name = entry.file_name();

            if let Some(key) = name.to_str().and_then(|n| n.strip_suffix(ext))
                && key.starts_with(id)
            {
                keys.insert(key.to_string());
            }
        }
    }

    // An id that names two trees names none
    (keys.len() == 1).then(|| keys.pop_first()).flatten()
}

/// Whether a run of this key is going right now on this machine, without waiting for it
pub fn running(key: &str) -> bool {
    let path = home().join("locks").join(format!("{key}.lock"));

    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .is_ok_and(|lock| lock.try_lock().is_err())
}

/// The machine's run slots: `SHIPCHECK_MAX_RUNS`, or a sixth of the CPUs, since three brain runs on
/// 14 CPUs left none idle and failed its timing tests
pub fn max_runs() -> usize {
    std::env::var("SHIPCHECK_MAX_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get() / 6))
        .max(1)
}

/// A run's place in the queue for a machine run slot, a file it holds locked and removes when it goes
pub(super) struct Ticket {
    pub(super) path: PathBuf,
    _lock: fs::File,
}

impl Ticket {
    /**
     * Named `<priority>-<arrival>-<pid>`, so the names sort into the queue's order. It is locked under
     * a temporary name and renamed into the queue, so no other run sees it unlocked and removes it.
     * It holds what `shipcheck queue` prints of the run
     **/
    pub(super) fn take(queue: &Path, priority: u8, run: &queue::Run) -> Result<Ticket, String> {
        fs::create_dir_all(queue).map_err(|e| format!("{}: {e}", queue.display()))?;
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let name = format!("{priority}-{at:024}-{}", std::process::id());
        let temp = queue.join(format!(".{name}"));
        let lock = fs::File::create(&temp).map_err(|e| format!("{}: {e}", temp.display()))?;
        lock.try_lock()
            .map_err(|e| format!("{}: {e}", temp.display()))?;
        fs::write(&temp, serde_json::json!(run).to_string())
            .map_err(|e| format!("{}: {e}", temp.display()))?;
        let path = queue.join(name);
        fs::rename(&temp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Ticket { path, _lock: lock })
    }

    fn name(&self) -> &std::ffi::OsStr {
        self.path.file_name().unwrap_or_default()
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// What one look at the queue found: a slot to hold, or how many live runs are ahead
pub(super) enum QueueStep {
    Slot(fs::File, usize),
    Ahead(usize),
}

pub(super) fn queue_step(dir: &Path, ticket: &Ticket, slots: usize) -> Result<QueueStep, String> {
    let queue = dir.join("queue");
    let mine = ticket.name();
    let mut names: Vec<_> = fs::read_dir(&queue)
        .map_err(|e| format!("{}: {e}", queue.display()))?
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .filter(|n| !n.to_string_lossy().starts_with('.'))
        .collect();
    names.sort();
    let mut ahead = 0;

    for name in names.iter().take_while(|n| n.as_os_str() < mine) {
        let path = queue.join(name);

        // A ticket nobody holds belongs to a run that is gone
        match fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) if f.try_lock().is_ok() => {
                let _ = fs::remove_file(&path);
            }
            Ok(_) => ahead += 1,
            Err(_) => {}
        }
    }

    if ahead > 0 {
        return Ok(QueueStep::Ahead(ahead));
    }

    for n in 0..slots {
        let lock = fs::File::create(dir.join(format!("{n}.lock"))).map_err(|e| e.to_string())?;

        if lock.try_lock().is_ok() {
            return Ok(QueueStep::Slot(lock, n));
        }
    }

    Ok(QueueStep::Ahead(0))
}

pub(super) fn claim_runner(
    plan: &Plan,
    say: &mut dyn FnMut(&str),
) -> Result<(fs::File, queue::Note), String> {
    let dir = home().join("runners");
    let trigger = std::env::var("SHIPCHECK_TRIGGER").ok();
    let priority = u8::from(trigger.is_some());
    let run = queue::Run {
        repo: repo_id(&plan.repo).unwrap_or_default(),
        key: plan.key.clone(),
        path: plan.repo.display().to_string(),
        branch: plan.branch.clone(),
        sha: plan.sha.clone(),
        trigger: trigger.unwrap_or_else(|| "cli".into()),
        pid: std::process::id(),
        queued_unix: unix_now(),
        ..queue::Run::default()
    };
    let ticket = Ticket::take(&dir.join("queue"), priority, &run)?;
    let mut told = None;

    loop {
        match queue_step(&dir, &ticket, max_runs())? {
            QueueStep::Slot(lock, n) => {
                WAITING_FOR_SLOT.store(false, Ordering::Relaxed);
                HAS_SLOT.store(true, Ordering::Relaxed);
                let now = unix_now();
                QUEUED_S.store(now.saturating_sub(run.queued_unix), Ordering::Relaxed);

                if told.is_some() {
                    say(&format!(
                        "started after {}s in the queue",
                        now.saturating_sub(run.queued_unix)
                    ));
                }

                return Ok((lock, queue::Note::start(&dir, n, run, now)));
            }
            QueueStep::Ahead(ahead) => {
                if told != Some(ahead) {
                    say(&match ahead {
                        0 => "queued for a run slot, every slot is busy (position 1)".to_string(),
                        1 => "queued behind 1 run (position 2)".to_string(),
                        n => format!("queued behind {n} runs (position {})", n + 1),
                    });
                    WAITING_FOR_SLOT.store(true, Ordering::Relaxed);
                    told = Some(ahead);
                }
            }
        }

        std::thread::sleep(Duration::from_millis(500));
    }
}
