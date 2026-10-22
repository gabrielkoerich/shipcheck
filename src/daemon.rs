use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::github::{self, App};
use crate::{check, run};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    #[serde(default = "ten")]
    pub interval_seconds: u64,
    #[serde(default)]
    pub run_misses: bool,
    pub repos: BTreeMap<String, RepoEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoEntry {
    pub path: PathBuf,
    #[serde(default = "origin_main")]
    pub base: String,
}

fn ten() -> u64 {
    10
}

fn origin_main() -> String {
    "origin/main".into()
}

impl DaemonConfig {
    pub fn load() -> Result<Self, String> {
        let path = run::home().join("daemon.toml");
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

struct Token {
    value: String,
    minted: Instant,
}

/// Polls GitHub for open PR heads with no `shipcheck` check, and posts one from the cache
pub fn serve(config: &DaemonConfig, app: &App) -> ! {
    let mut tokens: BTreeMap<String, Token> = BTreeMap::new();
    let mut done: HashSet<(String, String)> = HashSet::new();
    let mut waiting: HashSet<(String, String)> = HashSet::new();

    loop {
        for (repo, entry) in &config.repos {
            let token = match token(app, repo, &mut tokens) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("{repo}: {e}");
                    continue;
                }
            };

            if let Err(e) = cycle(config, app, repo, entry, &token, &mut done, &mut waiting) {
                eprintln!("{repo}: {e}");
            }
        }

        std::thread::sleep(Duration::from_secs(config.interval_seconds));
    }
}

// Installation tokens last an hour, so mint a new one after fifty minutes
fn token(app: &App, repo: &str, tokens: &mut BTreeMap<String, Token>) -> Result<String, String> {
    if let Some(t) = tokens.get(repo)
        && t.minted.elapsed() < Duration::from_secs(50 * 60)
    {
        return Ok(t.value.clone());
    }

    let value = app.installation_token(repo)?;
    tokens.insert(
        repo.to_string(),
        Token {
            value: value.clone(),
            minted: Instant::now(),
        },
    );
    Ok(value)
}

fn git(entry: &RepoEntry, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(&entry.path)
        .args(args)
        .status()
        .is_ok_and(|s| s.success())
}

fn cycle(
    config: &DaemonConfig,
    app: &App,
    repo: &str,
    entry: &RepoEntry,
    token: &str,
    done: &mut HashSet<(String, String)>,
    waiting: &mut HashSet<(String, String)>,
) -> Result<(), String> {
    let base_branch = entry.base.strip_prefix("origin/").unwrap_or(&entry.base);
    // shipcheck.toml on the configured branch only says what a PR into that branch must pass
    let prs: Vec<_> = github::open_prs(token, repo)?
        .into_iter()
        .filter(|pr| pr.base == base_branch)
        .collect();

    if prs
        .iter()
        .all(|pr| done.contains(&(repo.to_string(), pr.sha.clone())))
    {
        return Ok(());
    }

    if !git(entry, &["fetch", "-q", "origin", base_branch]) {
        return Err(format!(
            "could not fetch {base_branch}, so its config cannot be trusted"
        ));
    }

    for pr in prs {
        let id = (repo.to_string(), pr.sha.clone());

        if done.contains(&id) {
            continue;
        }

        // One PR that cannot be checked must not hold back the others
        match handle(config, app, repo, entry, token, &pr) {
            Ok(Some(verdict)) => {
                println!("{repo}#{}: {verdict} on {}", pr.number, &pr.sha[..12]);
                waiting.remove(&id);
                done.insert(id);
            }
            Ok(None) => {
                if waiting.insert(id) {
                    println!(
                        "{repo}#{}: no result for {}, waiting for a local run",
                        pr.number,
                        &pr.sha[..12]
                    );
                }
            }
            Err(e) => eprintln!("{repo}#{}: {e}", pr.number),
        }
    }

    Ok(())
}

/// Posts the check for one PR, or returns None while it waits for a local run
fn handle(
    config: &DaemonConfig,
    app: &App,
    repo: &str,
    entry: &RepoEntry,
    token: &str,
    pr: &github::OpenPr,
) -> Result<Option<&'static str>, String> {
    let (number, sha) = (pr.number, &pr.sha);
    let existing = github::find_check(token, repo, sha, app.id())?;

    if existing
        .as_ref()
        .is_some_and(|(_, status)| status == "completed")
    {
        return Ok(Some("already checked"));
    }

    if !git(entry, &["cat-file", "-e", &format!("{sha}^{{commit}}")]) {
        git(
            entry,
            &["fetch", "-q", "origin", &format!("pull/{number}/head")],
        );
    }

    let plan = run::plan(&entry.path, sha, &entry.base)?;

    // The check reflects the owner's rules, so a repository with no shipcheck.toml on main gets none
    if plan.defaults {
        return Ok(None);
    }

    // Host mode runs a commit's code as this user outside any container, so never a fork's, not even from the cache
    if plan.config.runner == crate::config::RunnerKind::Host && pr.head_repo != repo {
        return Ok(Some("not checked, host mode never checks a fork's PR"));
    }

    // Shows the check without a result: in_progress while a run goes, queued while none does
    let mark = |running: bool| -> Result<u64, String> {
        let wanted = if running { "in_progress" } else { "queued" };

        match &existing {
            Some((id, status)) if status == wanted => Ok(*id),
            Some((id, _)) => {
                github::update_check(token, repo, *id, &check::waiting(sha, running)).map(|_| *id)
            }
            None => {
                github::post_check(token, repo, &check::waiting(sha, running)).map(|(id, _)| id)
            }
        }
    };

    // A run of this tree on this machine holds its lock, the daemon checks again next cycle
    if run::running(&plan.key) {
        mark(true)?;
        return Ok(None);
    }

    let (result, id) = match run::lookup(&plan) {
        Some(result) => (result, existing.as_ref().map(|(id, _)| *id)),
        // A fork's code would run on this machine with its secrets, so only its own author runs it
        None if config.run_misses && pr.head_repo == repo => {
            let id = mark(true)?;
            let _key = run::lock_key(&plan.key)?;
            println!("{repo}#{number}: no result for {}, running", &sha[..12]);
            let result = run::run(&plan, &mut |line| println!("{repo}#{number}: {line}"))?;
            (result, Some(id))
        }
        None => {
            mark(false)?;
            return Ok(None);
        }
    };

    let logs = run::log_dir(&plan.key);
    // Keyed as the runner keys it, and no log at all when the secrets cannot be read
    let secrets = run::repo_id(&plan.repo).and_then(|id| run::secret_values(&id));
    // A fork's log could show what its own code printed of this machine, so only same-repo PRs get one
    let with_logs =
        plan.config.logs == crate::config::Logs::Failed && pr.head_repo == repo && secrets.is_ok();
    let secrets = secrets.unwrap_or_default();
    let body = check::body(
        sha,
        &result,
        with_logs.then_some((logs.as_path(), secrets.as_slice())),
    );

    match id {
        Some(id) => github::update_check(token, repo, id, &body)?,
        None => github::post_check(token, repo, &body)?.1,
    };

    Ok(Some(if result.passed {
        "posted success"
    } else {
        "posted failure"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg: DaemonConfig =
            toml::from_str("[repos]\n\"acme/app\" = { path = \"/src/app\" }\n").unwrap();
        assert_eq!(cfg.interval_seconds, 10);
        assert!(!cfg.run_misses);
        assert_eq!(cfg.repos["acme/app"].base, "origin/main");
    }

    #[test]
    fn config_rejects_unknown_fields() {
        assert!(toml::from_str::<DaemonConfig>("repos = {}\npoll = 5\n").is_err());
    }
}
