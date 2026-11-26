use super::{Entry, Plan};
use crate::cache;
use crate::git::{git_raw, repo_id};
use crate::history;
use crate::paths::{home, log_dir};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn last_run(
    home: &Path,
    repo: &str,
    tree: &str,
    has: impl Fn(&str) -> bool,
) -> Option<(String, String)> {
    history::runs(home, repo, usize::MAX)
        .into_iter()
        .rev()
        .find(|r| r["tree"] == tree && r["key"].as_str().is_some_and(&has))
        .map(|r| {
            let field = |k: &str| r[k].as_str().unwrap_or_default().to_string();
            (field("key"), field("base"))
        })
}

pub fn cached(key: &str) -> Option<Entry> {
    let text = fs::read_to_string(home().join("cache").join(format!("{key}.json"))).ok()?;
    let entry: Entry = serde_json::from_str(&text).ok()?;
    // A result copied to another key's file is not that key's result
    (entry.key == key).then_some(entry)
}

/// The result for a plan without running it: a pass when `paths` excludes the change, else the cache
pub fn lookup(plan: &Plan) -> Option<Entry> {
    let mut entry = skipped(plan).or_else(|| cached(&plan.key))?;
    if !holds(plan, &entry) {
        return None;
    }

    // A cached result can come from another commit with the same tree, the notes must be this one's
    entry.ci_changes.clone_from(&plan.ci_changes);
    Some(entry)
}

/// Fails a result whose pass does not hold, every workflow too, so each job check fails beside `shipcheck`
pub fn fail_closed(entry: &mut Entry) {
    entry.passed = false;

    for wf in &mut entry.workflows {
        wf.passed = false;
    }
}

pub fn holds(plan: &Plan, entry: &Entry) -> bool {
    let on_base = Command::new("git")
        .arg("-C")
        .arg(&plan.repo)
        .args(["merge-base", "--is-ancestor", &entry.base, &plan.base])
        .status()
        .is_ok_and(|s| s.success());
    entry.tree == plan.tree && on_base && logs_hold(entry)
}

/// Whether the logs a result names are still the ones it hashed
pub(super) fn logs_hold(entry: &Entry) -> bool {
    entry.workflows.iter().all(|wf| {
        fs::read(log_dir(&entry.key).join(format!("{}.log", wf.file)))
            .is_ok_and(|log| cache::hex(&Sha256::digest(&log)) == wf.log_sha256)
    })
}

/// A failed result whose passed checks and jobs a full run keeps, and where its logs are
pub(super) struct Previous {
    pub(super) entry: Entry,
    pub(super) logs: PathBuf,
}

/// Where a branch's last full result is named, so its next tree knows what already passed
pub(super) fn branch_link(plan: &Plan) -> Option<PathBuf> {
    let branch = plan.branch.as_ref()?;
    let id = repo_id(&plan.repo).ok()?;
    let name = cache::hex(&Sha256::digest(branch.as_bytes()));
    Some(home().join("branches").join(id).join(name))
}

/// Whose failed result a run keeps the passes of: this tree's own, or also the branch's last one
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reuse {
    Tree,
    Branch,
}

pub(super) fn previous(plan: &Plan, reuse: Reuse) -> Option<Previous> {
    if let Some(entry) = cached(&plan.key).filter(|e| !e.passed && holds(plan, e)) {
        return Some(Previous {
            entry,
            logs: log_dir(&plan.key),
        });
    }

    if reuse == Reuse::Tree {
        return None;
    }

    let link: serde_json::Value =
        serde_json::from_slice(&fs::read(branch_link(plan)?).ok()?).ok()?;
    let key = link["key"].as_str()?;

    if link["inputs"].as_str() != Some(plan.inputs.as_str()) || key == plan.key {
        return None;
    }

    let entry = cached(key).filter(|e| !e.passed && e.skipped.is_none() && logs_hold(e))?;
    Some(Previous {
        entry,
        logs: log_dir(key),
    })
}

/// Names this result as the branch's last, for the next tree's run
pub(super) fn link_branch(plan: &Plan) {
    if let Some(path) = branch_link(plan) {
        let body = serde_json::json!({"key": plan.key, "inputs": plan.inputs, "tree": plan.tree});
        let _ = path.parent().map(fs::create_dir_all);
        let _ = fs::write(path, body.to_string());
    }
}

pub fn skipped(plan: &Plan) -> Option<Entry> {
    let filter = plan.config.path_filter().ok()??;
    let range = format!("{}...{}", plan.base, plan.sha);
    // Raw NUL-separated names, and both sides of a rename, so no quoted or moved path slips past
    let changed = git_raw(
        &plan.repo,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            &range,
        ],
    )
    .ok()?;

    for name in changed.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let name = std::str::from_utf8(name).ok()?;

        if filter.is_match(name) {
            return None;
        }
    }

    Some(Entry {
        key: plan.key.clone(),
        sha: plan.sha.clone(),
        tree: plan.tree.clone(),
        base: plan.base.clone(),
        passed: true,
        workflows: Vec::new(),
        finished_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        skipped: Some(format!(
            "no change under paths ({})",
            plan.config.paths.join(", ")
        )),
        ci_changes: plan.ci_changes.clone(),
    })
}
