use std::fs;
use std::path::Path;
use std::process::Command;

use crate::config::{Network, RepoConfig, RunnerKind};
use crate::run;
use crate::runner::{self, Runner};
use crate::service::Poster;

/// One thing shipcheck needs, and the command that fixes it when it is missing
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

fn check(name: &'static str, result: Result<String, String>) -> Check {
    match result {
        Ok(detail) => Check {
            name,
            ok: true,
            detail,
        },
        Err(detail) => Check {
            name,
            ok: false,
            detail,
        },
    }
}

fn output(cmd: &str, args: &[&str], cwd: &Path) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;

    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }

    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Without the hardened runtime, `DYLD_INSERT_LIBRARIES` loads any program's code into shipcheck
fn hardened_note() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return String::new();
    };
    let flags = Command::new("codesign")
        .arg("-dv")
        .arg(&exe)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
        .unwrap_or_default();

    if flags.contains("runtime") {
        return String::new();
    }

    format!(
        ". This binary lacks the hardened runtime, so another program can load code into it and read them: `codesign -f -s - -o runtime {}`",
        exe.display()
    )
}

/**
 * Everything a run needs on this machine and in this repository, each with what to do when it is
 * missing, so an agent can set shipcheck up from the output alone
 **/
pub fn checks(cwd: &Path, base: &str) -> Vec<Check> {
    let mut found = vec![check(
        "git",
        output("git", &["--version"], cwd).map_err(|_| "install git".to_string()),
    )];

    let act = runner::act::Act::default();
    found.push(check(
        "act",
        act.identity().map_err(|_| {
            "install act 0.2.86 or later: `brew install act`, or https://github.com/nektos/act/releases".to_string()
        }),
    ));
    found.push(check(
        "docker",
        act.preflight()
            .and_then(|_| {
                output(
                    "docker",
                    &["version", "--format", "{{.Server.Platform.Name}} {{.Server.Version}}"],
                    cwd,
                )
            })
            .map_err(|e| {
                format!("{e}. Start a Docker engine: Docker Desktop (`open -a Docker`), or `colima start`")
            }),
    ));

    let home = run::home();

    if let Ok(usage) = runner::gc::usage(&home, &runner::act::home_prefix()) {
        found.push(Check {
            name: "disk",
            ok: true,
            detail: usage,
        });
    }

    let (templates, runs) = (home.join("templates"), home.join("runs"));
    let cow = fs::create_dir_all(&templates)
        .and_then(|_| fs::create_dir_all(&runs))
        .is_ok_and(|_| git_cow::can_clone(&templates, &runs));
    found.push(Check {
        name: "copy-on-write",
        ok: true,
        detail: if cow {
            format!("{} clones files, checkouts cost no disk", home.display())
        } else {
            format!(
                "{} cannot clone files, so each job gets a full copy. Works, slower. APFS, btrfs or XFS clone",
                home.display()
            )
        },
    });

    found.push(Check {
        name: "post",
        ok: true,
        detail: match crate::service::poster() {
            Poster::Key => "`shipcheck post` uses this machine's App key".into(),
            Poster::Service(_) => format!(
                "`shipcheck post` goes through {} with {}",
                crate::service::api(),
                crate::service::identity().unwrap_or_default()
            ),
            Poster::None => "not signed in. Optional: `shipcheck login`, or SHIPCHECK_TOKEN from `shipcheck token`, lets `shipcheck post` put the check on PRs".into(),
        },
    });

    let mut secrets = vec![home.join("token"), home.join("app.pem")];
    secrets.extend(run::repo_id(cwd).ok().map(|id| run::secrets_file(&id)));
    let secrets: Vec<_> = secrets
        .into_iter()
        .filter(|p| crate::secret::exists(p))
        .collect();

    if !secrets.is_empty() {
        // Reads each one, so after an upgrade the keychain asks here, where someone can answer
        let failed: Vec<_> = secrets
            .iter()
            .filter_map(|p| crate::secret::read(p).err())
            .collect();
        let names: Vec<_> = secrets
            .iter()
            .map(|p| p.strip_prefix(&home).unwrap_or(p).display().to_string())
            .collect();
        found.push(check(
            "secrets",
            if !failed.is_empty() {
                Err(failed.join(". "))
            } else if cfg!(target_os = "macos") {
                Ok(format!(
                    "{} in the login keychain, readable only by this shipcheck binary{}",
                    names.join(", "),
                    hardened_note()
                ))
            } else {
                Ok(format!(
                    "{} in files of mode 0600 in {}, readable by every process of this user",
                    names.join(", "),
                    home.display()
                ))
            },
        ));
    }

    let host = output("sysctl", &["-n", "hw.memsize"], cwd)
        .ok()
        .and_then(|m| m.parse().ok());

    if let (true, Some((_, docker)), Some(host)) = (
        cfg!(target_os = "macos"),
        runner::act::engine_resources(),
        host,
    ) && let Some(note) = memory_note(docker, host)
    {
        found.push(Check {
            name: "docker memory",
            ok: true,
            detail: note,
        });
    }

    let Ok(top) = output("git", &["rev-parse", "--show-toplevel"], cwd) else {
        found.push(check(
            "repository",
            Err("not inside a git repository, run shipcheck in a clone".into()),
        ));
        return found;
    };

    let has_base = output("git", &["rev-parse", "--verify", "-q", base], cwd).is_ok();
    found.push(check(
        "base",
        if has_base {
            Ok(format!("{base} found"))
        } else {
            Err(format!(
                "{base} is missing: `git fetch origin {}`",
                base.strip_prefix("origin/").unwrap_or(base)
            ))
        },
    ));

    let plan = if has_base {
        run::plan(cwd, "HEAD", base)
    } else {
        Err(format!("needs {base}"))
    };
    found.push(check(
        "shipcheck.toml",
        plan.as_ref()
            .map(|p| {
                let list = p.config.workflows.join(", ");
                let checks: Vec<&str> = p.config.host.checks.keys().map(String::as_str).collect();
                let host = format!("runs host checks {} on {base}, natively as this user in a sandbox", checks.join(", "));

                if p.config.acts() && p.config.hosts() {
                    let acts = jobs_line(&p.config);
                    format!("{host}, and {acts} with act")
                } else if p.config.hosts() {
                    host
                } else if p.defaults {
                    format!(
                        "not on {base}, so runs use the workflows that run for branches: {list}. For the PR check, `shipcheck init` (`--yes` from a script or an agent) and merge shipcheck.toml to main"
                    )
                } else if !p.config.jobs.is_empty() {
                    format!("runs {} with act on {base}", jobs_line(&p.config))
                } else {
                    format!("runs {list} on {base}")
                }
            })
            .map_err(Clone::clone),
    ));
    if let Ok(p) = &plan
        && !p.config.shard_tests.enabled()
    {
        let shardable: Vec<String> = p
            .workflows
            .iter()
            .filter(|(path, body)| {
                let file = path.rsplit('/').next().unwrap_or(path);
                p.config.act_files().iter().any(|w| w == file)
                    && runner::shard::shardable(&String::from_utf8_lossy(body))
            })
            .map(|(path, _)| path.rsplit('/').next().unwrap_or(path).to_string())
            .collect();

        if !shardable.is_empty() {
            found.push(Check {
                name: "shard_tests",
                ok: true,
                detail: format!(
                    "off. {} has a pytest step that `shard_tests = true` in shipcheck.toml would split into parallel shards",
                    shardable.join(", ")
                ),
            });
        }
    }

    if let Ok(p) = &plan
        && !p.defaults
        && let Some(required) = required_checks(cwd, base)
    {
        let jobs = run::jobs(p).unwrap_or_default();
        let names: Vec<String> = crate::check::job_names(&jobs)
            .into_iter()
            .map(|(_, _, n)| n)
            .collect();
        found.push(match ruleset_note(&required, &names) {
            _ if !required.iter().any(|r| r.starts_with("shipcheck")) => Check {
                name: "required checks",
                ok: true,
                detail: "no branch rule requires shipcheck's checks. Optional: `shipcheck init` sets one up (https://shipcheck.run/docs/branch-protection/)".into(),
            },
            None => Check {
                name: "required checks",
                ok: true,
                detail: format!(
                    "the branch rules require every job check: {}",
                    names.join(", ")
                ),
            },
            Some(note) => Check {
                name: "required checks",
                ok: false,
                detail: note,
            },
        });
    }

    let planned: Result<RepoConfig, String> = plan.map(|p| p.config);

    // Host checks run no container
    if planned.as_ref().is_ok_and(|c| !c.acts()) {
        for c in found
            .iter_mut()
            .filter(|c| !c.ok && ["act", "docker"].contains(&c.name))
        {
            c.ok = true;
            c.detail = format!("not needed, runner is host ({})", c.detail);
        }
    }

    if planned.as_ref().is_ok_and(|c| c.hosts()) {
        found.push(check(
            "sandbox",
            runner::host::sandbox().map(|s| {
                format!("{s}: host checks write only to their clone, a new home and the repository's tool caches")
            }),
        ));
    }

    if let Ok(config) = &planned {
        found.push(Check {
            name: "network",
            ok: true,
            detail: match config.network {
                _ if !config.acts() => "as this machine has it: host checks are sandboxed for files, not for the network, and `network` does not apply".into(),
                Network::None => "none: jobs reach nothing. A job that downloads packages or tools fails, then the owner sets `network = \"internet\"` in shipcheck.toml".into(),
                Network::Internet => "internet: jobs can download, and can still reach this machine and the local network".into(),
            },
        });
    }

    if let Ok(config) = &planned {
        let on_vm: Vec<String> = config
            .engines()
            .into_iter()
            .filter(|(_, e)| *e == crate::config::Engine::Colima)
            .map(|(job, _)| job)
            .collect();

        if !on_vm.is_empty() {
            found.push(check(
                "colima",
                runner::colima::version().map(|v| {
                    format!(
                        "{v}, a VM of its own for each of {}. For trusted repositories only: the VM reaches this machine's 127.0.0.1 through 192.168.5.2 and the internet whatever `network` says, so also the artifact servers of docker runs going at the same time, and act runs beside the job's code. The cache key holds colima's and lima's versions, not the VM image's digest, so an image colima swaps under the same version is not seen",
                        on_vm.join(", ")
                    )
                }),
            ));
        }
    }

    let hook = Path::new(&top).join(".git/hooks/pre-push");
    let hooked = fs::read_to_string(&hook).is_ok_and(|h| h.contains("shipcheck on-push"));
    found.push(Check {
        name: "hooks",
        ok: true,
        detail: if hooked {
            "installed, pushes start runs in the background".into()
        } else {
            "not installed. Optional: `shipcheck install` starts runs on push, or run `shipcheck run` yourself".into()
        },
    });

    let missing = crate::agents::notes(Path::new(&top));
    found.push(Check {
        name: "instructions",
        ok: true,
        detail: if missing.is_empty() {
            "the shipcheck block is in AGENTS.md and the other instruction files".into()
        } else {
            missing.join(", ")
        },
    });

    let legacy = run::legacy_home();

    if cfg!(target_os = "macos") && legacy.is_dir() && legacy != run::home() {
        found.push(Check {
            name: "old home",
            ok: false,
            detail: format!(
                "{} is still there next to {}, so shipcheck did not move it. Move what you need from it, then trash it",
                legacy.display(),
                run::home().display()
            ),
        });
    }

    if cfg!(target_os = "macos") && crate::autostart::logs_to_legacy_home() {
        found.push(Check {
            name: "daemon",
            ok: false,
            detail: format!(
                "the LaunchAgent logs to {}, run `shipcheck install` to point it at {}",
                legacy.display(),
                run::home().display()
            ),
        });
    }

    if run::home_overridden() {
        found.push(Check {
            name: "home",
            ok: true,
            detail: format!(
                "SHIPCHECK_HOME moves results to {}. `post` and the daemon refuse it without --home-ok",
                run::home().display()
            ),
        });
    }

    found
}

/// The check names the base branch's rules require, read with the person's `gh`. None when gh cannot read them
/// What act runs: each listed workflow whole, then each act `[jobs]` entry
fn jobs_line(config: &RepoConfig) -> String {
    let mut parts = config.workflows.clone();
    let engines = config.engines();
    parts.extend(
        config
            .jobs_on(RunnerKind::Act)
            .into_iter()
            .map(|j| match engines.get(&j) {
                Some(crate::config::Engine::Colima) => format!("job {j} on colima"),
                _ => format!("job {j}"),
            }),
    );
    parts.join(", ")
}

fn required_checks(cwd: &Path, base: &str) -> Option<Vec<String>> {
    let branch = base.strip_prefix("origin/").unwrap_or(base);
    let rules = output(
        "gh",
        &[
            "api",
            &format!("repos/{{owner}}/{{repo}}/rules/branches/{branch}"),
        ],
        cwd,
    )
    .ok()?;
    let rules: serde_json::Value = serde_json::from_str(&rules).ok()?;
    Some(
        rules
            .as_array()?
            .iter()
            .filter(|r| r["type"] == "required_status_checks")
            .flat_map(|r| {
                r["parameters"]["required_status_checks"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|c| c["context"].as_str().map(String::from))
            .collect(),
    )
}

/// What to change when the required `shipcheck/` names differ from the current jobs' check names
fn ruleset_note(required: &[String], jobs: &[String]) -> Option<String> {
    let required: Vec<&String> = required
        .iter()
        .filter(|n| n.starts_with("shipcheck/"))
        .collect();
    let add: Vec<&str> = jobs
        .iter()
        .filter(|j| !required.contains(j))
        .map(String::as_str)
        .collect();
    let stale: Vec<&str> = required
        .iter()
        .filter(|r| !jobs.contains(r))
        .map(|r| r.as_str())
        .collect();

    if add.is_empty() && stale.is_empty() {
        return None;
    }

    let mut note = Vec::new();

    if !add.is_empty() {
        note.push(format!("not required yet: {}", add.join(", ")));
    }

    if !stale.is_empty() {
        note.push(format!(
            "required but no job posts it, so every PR waits: {}",
            stale.join(", ")
        ));
    }

    Some(format!(
        "{}. Update the ruleset in Settings, Rules (https://shipcheck.run/docs/branch-protection/)",
        note.join(". ")
    ))
}

const GIB: u64 = 1 << 30;

/**
 * A note when Docker's VM has under 8 GiB or under a third of the Mac's memory. Test shards and
 * parallel jobs run out of memory before CPUs. Linux engines share the host's memory, so it is macOS only
 **/
fn memory_note(docker: u64, host: u64) -> Option<String> {
    (docker < 8 * GIB || docker < host / 3).then(|| {
        format!(
            "Docker has {:.1} GiB of this Mac's {:.0} GiB. Test shards and parallel jobs need more: Docker Desktop, Settings, Resources, Memory, such as 16 GB on a 32 GB Mac. https://shipcheck.run/docs/speed/",
            docker as f64 / GIB as f64,
            host as f64 / GIB as f64
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_ruleset_drift() {
        let v = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let jobs = v(&["shipcheck/lint", "shipcheck/test"]);
        assert!(
            ruleset_note(
                &v(&["shipcheck", "shipcheck/lint", "shipcheck/test"]),
                &jobs
            )
            .is_none()
        );
        let note = ruleset_note(&v(&["shipcheck/lint", "shipcheck/old"]), &jobs).unwrap();
        assert!(note.contains("not required yet: shipcheck/test"), "{note}");
        assert!(note.contains("every PR waits: shipcheck/old"), "{note}");
    }

    #[test]
    fn notes_a_small_docker_vm() {
        // This Mac: 36 GB, Docker Desktop's default 8 GB VM
        let note = memory_note(8_318_976_000, 36 * GIB).unwrap();
        assert!(
            note.starts_with("Docker has 7.7 GiB of this Mac's 36 GiB"),
            "{note}"
        );
        assert!(memory_note(16 * GIB, 36 * GIB).is_none());
        assert!(memory_note(10 * GIB, 36 * GIB).is_some(), "under a third");
        assert!(memory_note(8 * GIB, 16 * GIB).is_none());
        assert!(memory_note(6 * GIB, 8 * GIB).is_some(), "under 8 GiB");
    }
}
