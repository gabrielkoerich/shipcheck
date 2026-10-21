use std::fs;
use std::path::Path;
use std::process::Command;

use crate::config::{Network, RepoConfig};
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
                "`shipcheck post` goes through {} with this machine's token",
                crate::service::api()
            ),
            Poster::None => "not signed in. Optional: `shipcheck login` lets `shipcheck post` put the check on PRs".into(),
        },
    });

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
            Err(format!("{base} is missing: `git fetch origin main`"))
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

                if p.defaults {
                    format!(
                        "not on {base}, so runs use the workflows that run for branches: {list}. For the PR check, `shipcheck init` and merge shipcheck.toml to main"
                    )
                } else {
                    format!("runs {list} on {base}")
                }
            })
            .map_err(Clone::clone),
    ));
    let planned: Result<RepoConfig, String> = plan.map(|p| p.config);

    if let Ok(config) = &planned {
        found.push(Check {
            name: "network",
            ok: true,
            detail: match config.network {
                Network::None => "none: jobs reach nothing. A job that downloads packages or tools fails, then the owner sets `network = \"internet\"` in shipcheck.toml".into(),
                Network::Internet => "internet: jobs can download, and can still reach this machine and the local network (docs/SECURITY.md)".into(),
            },
        });
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

    found
}
