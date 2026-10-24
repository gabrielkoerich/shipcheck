use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const MARK: &str = "# Installed by `shipcheck install`";

pub const PRE_PUSH: &str = r#"#!/bin/sh
# Installed by `shipcheck install`. Starts a background run of each pushed commit, never blocks the push
if ! command -v shipcheck >/dev/null 2>&1; then
  exit 0
fi

zero=0000000000000000000000000000000000000000

while read -r local_ref local_sha remote_ref remote_sha; do
  if [ "$local_sha" != "$zero" ]; then
    shipcheck on-push "$local_ref" "$local_sha"
  fi
done

exit 0
"#;

const HOOK_LINE: &str = "shipcheck on-push \"$local_ref\" \"$local_sha\"";

pub const POST_COMMIT: &str = r#"#!/bin/sh
# Installed by `shipcheck install`. Starts a background run of each commit when shipcheck.toml lists commit in on
if command -v shipcheck >/dev/null 2>&1; then
  shipcheck on-commit
fi
"#;

const COMMIT_LINE: &str = "shipcheck on-commit";

pub fn config_for(workflows: &[String]) -> String {
    let list = workflows
        .iter()
        .map(|w| format!("\"{w}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "# Workflows that must pass before a PR can merge. All others run only on GitHub, on main.\n\
         # Remove deploys and anything else that must not run on a laptop\n\
         workflows = [{list}]\n"
    )
}

/// A host check `init` found, and where it came from
pub struct Found {
    pub name: String,
    pub command: String,
    pub from: String,
}

/// A job that keeps running on GitHub for PRs, since shipcheck cannot run it here
pub struct Kept {
    pub file: String,
    pub job: String,
    pub why: String,
    /// GitHub's names for its check runs, None when `init` cannot tell them
    pub checks: Option<Vec<String>>,
}

#[derive(Default)]
pub struct Detected {
    pub checks: Vec<Found>,
    pub versions: Vec<String>,
    /// What was left out, and why
    pub skipped: Vec<String>,
    pub github: Vec<Kept>,
    /// Why the repository runs in act, when a job runs a matrix act can run
    pub act: Option<String>,
}

/// What `init` needs to know of the machine it runs on
#[derive(Default)]
pub struct Machine {
    pub docker: bool,
    pub mac: bool,
    /// The rustup toolchains installed, such as `1.94.1` or `nightly`
    pub toolchains: Vec<String>,
}

impl Machine {
    #[cfg(not(test))]
    pub fn this() -> Self {
        Machine {
            docker: responds("docker", &["info"]),
            mac: cfg!(target_os = "macos"),
            toolchains: toolchains(),
        }
    }

    // Tests see the same result on every machine
    #[cfg(test)]
    pub fn this() -> Self {
        Machine::default()
    }
}

/// The names `rustup toolchain list` prints, without this machine's host triple
#[cfg(not(test))]
fn toolchains() -> Vec<String> {
    let out = |cmd: &str, args: &[&str]| {
        Command::new(cmd)
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    let host = out("rustc", &["-vV"])
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(str::to_string))
        .unwrap_or_default();
    let suffix = format!("-{host}");
    out("rustup", &["toolchain", "list"])
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(|t| t.strip_suffix(&suffix).unwrap_or(t).to_string())
        .collect()
}

impl Detected {
    /// One line per check and per thing left out, for the person who runs `init`
    pub fn explain(&self) -> String {
        let mut out: Vec<String> = self
            .checks
            .iter()
            .map(|c| format!("  {} = {}  (from {})", c.name, c.command, c.from))
            .collect();

        if !self.skipped.is_empty() {
            out.push("left out:".into());
            out.extend(self.skipped.iter().map(|s| format!("  {s}")));
        }

        if !self.github.is_empty() {
            out.push("stay on GitHub for PRs, and use Actions minutes:".into());
            out.extend(
                self.github
                    .iter()
                    .map(|k| format!("  {}, job {}: {}", k.file, k.job, k.why)),
            );
            out.push(REQUIRE.into());
        }

        out.join("\n")
    }
}

/**
 * For the repository owner, once: writes shipcheck.toml, to be committed, with every check
 * `detect` finds. With none, act with every workflow. Returns what it wrote and why
 **/
pub fn init(root: &Path) -> Result<Option<String>, String> {
    init_on(root, &Machine::this())
}

pub fn init_on(root: &Path, machine: &Machine) -> Result<Option<String>, String> {
    let config = root.join("shipcheck.toml");

    if config.exists() {
        return Ok(None);
    }

    let found = detect_on(root, machine);

    if let Some(why) = &found.act {
        // Only what GitHub runs for a PR, so no release or deploy runs on a laptop
        let workflows: Vec<String> = workflow_files(root)
            .unwrap_or_default()
            .into_iter()
            .filter(|f| {
                fs::read_to_string(root.join(".github/workflows").join(f))
                    .is_ok_and(|s| crate::workflow::runs_for_branches(&s))
            })
            .collect();
        // A job that runs only off the base branch would be a check that never runs on GitHub's main
        let (pr_only, workflows): (Vec<String>, Vec<String>) = workflows
            .into_iter()
            .partition(|f| found.github.iter().any(|k| k.file == *f));

        if workflows.is_empty() {
            return Ok(Some(NOTHING_LOCAL.into()));
        }

        crate::agents::write_in_repo(root, &config, &act_config(&workflows))?;
        let mut said = format!("wrote shipcheck.toml with runner = \"act\": {why}");

        for file in pr_only {
            said.push_str(&format!(
                "\n{file} stays on GitHub as it is: a job's `if:` depends on the PR or the default branch"
            ));
        }

        return Ok(Some(said));
    }

    if found.checks.is_empty() && !found.github.is_empty() {
        // No step 4 follows
        let said = found.explain().replace(&format!("\n{REQUIRE}"), "");
        return Ok(Some(format!("{NOTHING_LOCAL}\n{said}")));
    }

    if !found.checks.is_empty() {
        crate::agents::write_in_repo(root, &config, &host_config(&found.checks, &found.versions))?;
        return Ok(Some(format!(
            "wrote shipcheck.toml with these host checks:\n{}",
            found.explain()
        )));
    }

    let workflows = workflow_files(root).ok_or_else(|| {
        "no .github/workflows and no package.json scripts, Cargo.toml, pyproject.toml or go.mod in this repository".to_string()
    })?;
    crate::agents::write_in_repo(root, &config, &act_config(&workflows))?;
    Ok(Some(
        "wrote shipcheck.toml with runner = \"act\": no check found in the workflows' run steps or in a package.json test or lint script, Cargo.toml, pyproject.toml or go.mod, so the GitHub workflows run in containers".into(),
    ))
}

const REQUIRE: &str = "  Require their GitHub checks too, step 4 adds them";

/// What `init` says when it writes no shipcheck.toml because every job stays on GitHub
pub const NOTHING_LOCAL: &str = "nothing to run locally: every job stays on GitHub, so init wrote no shipcheck.toml and set nothing up (https://shipcheck.run/docs/how-it-works/#which-jobs-run-where)";

fn act_config(workflows: &[String]) -> String {
    format!(
        "runner = \"act\"\n{}\n# Splits each one-line pytest step into parallel shards. A test that fails only after another test's leftovers can pass\nshard_tests = true\n",
        config_for(workflows),
    )
}

pub fn host_config(checks: &[Found], versions: &[String]) -> String {
    let list = |items: Vec<String>| items.join(", ");
    let mut pinned: Vec<&str> = checks
        .iter()
        .flat_map(|c| c.command.split_whitespace())
        .filter_map(|w| w.split_once('=').map(|(name, _)| name))
        .filter(|n| n.ends_with("_VERSION") && n.len() > "_VERSION".len())
        .collect();
    pinned.sort();
    pinned.dedup();
    let note = if pinned.is_empty() {
        String::new()
    } else {
        format!(
            "# These variables tell GitHub's setup steps what to install. Here they pin nothing, each\n\
             # check uses this machine's tools, which `versions` records: {}\n",
            pinned.join(", ")
        )
    };
    format!(
        "# Checks run natively on this machine in an OS sandbox, each in its own clone of the tree\n\
         # (https://shipcheck.run/docs/config/#modes). Detected from the workflows and project files:\n\
         # keep the ones CI needs. `runner = \"act\"` with `workflows` runs the GitHub workflows instead\n\
         runner = \"host\"\n\n[host]\nversions = [{}]\n\n{note}[host.checks]\n{}\n",
        list(
            versions
                .iter()
                .map(|v| toml::Value::String(v.clone()).to_string())
                .collect()
        ),
        checks
            .iter()
            .map(|c| format!("{} = {}", c.name, toml::Value::String(c.command.clone())))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// The workflow files, sorted, or None without `.github/workflows`
pub fn workflow_files(root: &Path) -> Option<Vec<String>> {
    let mut files: Vec<String> = fs::read_dir(root.join(".github/workflows"))
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| (n.ends_with(".yml") || n.ends_with(".yaml")) && !n.contains(char::is_control))
        .collect();
    files.sort();
    Some(files)
}

/**
 * Host checks, one per workflow job GitHub runs for a feature branch and named by its id, after
 * the commands of the jobs it needs. Project files give checks only when no job gives one, since
 * a check CI does not run can fail a PR GitHub passes. `versions` come from project files
 **/
pub fn detect(root: &Path) -> Detected {
    detect_on(root, &Machine::this())
}

/// Whether a job has a `run` step, which is what host mode or act would run of it
fn runs_something(job: &crate::workflow::Job) -> bool {
    job.steps.iter().any(|s| !s.run.is_empty())
}

const PR_ONLY: &str =
    "its `if:` depends on the PR or the default branch, so it never runs on the base branch";

/// Why a job stays on GitHub for PRs in host mode, or None when shipcheck may run it here
fn stays(job: &crate::workflow::Job, machine: &Machine, others_off_linux: bool) -> Option<String> {
    let os = job.os;

    if job.pr_only {
        return Some(PR_ONLY.into());
    }

    if let Some(axis) = job.matrix.as_ref().filter(|_| runs_something(job)) {
        let entries = match (os.mac, os.windows) {
            (true, true) => "macOS and Windows",
            (true, false) => "macOS",
            (false, true) => "Windows",
            _ if !machine.docker => {
                return Some(format!(
                    "runs a matrix ({axis}) and Docker is not running, so act cannot run it"
                ));
            }
            _ if others_off_linux => {
                return Some(format!(
                    "runs a matrix ({axis}), and act cannot run this repository's macOS or Windows jobs"
                ));
            }
            _ => return None,
        };
        return Some(format!(
            "runs a matrix ({axis}) with {entries} entries, which act cannot run"
        ));
    }

    if os.windows {
        return Some("runs on Windows, which shipcheck does not support yet".into());
    }

    if os.mac && !os.linux && !machine.mac {
        return Some("runs on macOS, and this machine is not a Mac".into());
    }

    None
}

/**
 * As `detect`, for a machine with or without Docker and macOS. A job with a matrix makes it act
 * when act can run every job, since host mode tests only this machine's toolchain. Otherwise such
 * jobs, and jobs on a platform this machine cannot run, stay on GitHub
 **/
pub fn detect_on(root: &Path, machine: &Machine) -> Detected {
    let mut d = Detected::default();
    let mut files = Vec::new();

    for file in workflow_files(root).unwrap_or_default() {
        let source =
            fs::read_to_string(root.join(".github/workflows").join(&file)).unwrap_or_default();

        if !crate::workflow::runs_for_branches(&source) {
            d.skipped.push(format!(
                "{file}: GitHub does not run it for feature branches"
            ));
            continue;
        }

        files.push((file, crate::workflow::job_runs(&source)));
    }

    let all = || {
        files
            .iter()
            .flat_map(|(f, jobs)| jobs.iter().map(move |j| (f, j)))
    };
    let off_linux = all().any(|(_, j)| j.os.mac || j.os.windows);
    let matrix =
        all().find(|(_, j)| j.matrix.is_some() && runs_something(j) && !ships(&j.id, &['-', '_']));

    if let Some((file, job)) = matrix.filter(|_| machine.docker && !off_linux) {
        d.act = Some(format!(
            "{file}, job {} runs a matrix ({}): host mode tests only this machine's toolchain, so this repository runs in act",
            job.id.escape_debug(),
            job.matrix.as_deref().unwrap_or_default()
        ));

        // Their workflows stay out of `workflows` and as they are on GitHub
        for (file, jobs) in &files {
            for j in jobs.iter().filter(|j| j.pr_only) {
                d.github.push(Kept {
                    file: file.clone(),
                    job: j.id.escape_debug().to_string(),
                    why: PR_ONLY.into(),
                    checks: j.check_names.clone(),
                });
            }
        }

        return d;
    }

    for (file, jobs) in &files {
        let mut kept: Vec<(String, String)> = jobs
            .iter()
            .filter_map(|j| Some((j.id.clone(), stays(j, machine, off_linux)?)))
            .collect();

        // GitHub skips a job whose needs it skipped, so what a kept job needs stays too
        for (id, _) in kept.clone() {
            let job = jobs
                .iter()
                .find(|j| j.id == id)
                .expect("a job of this file");
            let mut before = Vec::new();
            needed(jobs, &job.needs, &mut before, &mut vec![id.clone()]);

            for need in before {
                if !kept.iter().any(|(k, _)| *k == need) {
                    kept.push((need, format!("job {id} stays on GitHub and needs it")));
                }
            }
        }

        for (id, why) in &kept {
            d.github.push(Kept {
                file: file.clone(),
                job: id.escape_debug().to_string(),
                why: why.clone(),
                checks: jobs
                    .iter()
                    .find(|j| j.id == *id)
                    .and_then(|j| j.check_names.clone()),
            });
        }

        let stays_here = |id: &str| kept.iter().any(|(k, _)| k == id);
        let own: Vec<(&str, Option<JobCommand>)> = jobs
            .iter()
            .map(|job| {
                if stays_here(&job.id) {
                    return (job.id.as_str(), None);
                }

                match job_check(job, &machine.toolchains) {
                    Ok((command, exports, notes)) => {
                        d.skipped
                            .extend(notes.iter().map(|n| format!("{file}, job {}: {n}", job.id)));
                        (job.id.as_str(), Some((command, exports)))
                    }
                    Err(why) => {
                        d.skipped
                            .push(format!("{file}, job {}: {why}", job.id.escape_debug()));
                        (job.id.as_str(), None)
                    }
                }
            })
            .collect();

        for (job, (id, command)) in jobs.iter().zip(&own) {
            let Some((command, exports)) = command else {
                continue;
            };
            let mut before = Vec::new();
            needed(jobs, &job.needs, &mut before, &mut vec![job.id.clone()]);
            let mut ran = Vec::new();
            let mut inlined = Vec::new();
            let mut missing = Vec::new();

            for need in &before {
                match own.iter().find(|(i, _)| i == need) {
                    Some((_, Some(c))) => {
                        inlined.push(c);
                        ran.push(need.as_str());
                    }
                    _ => missing.push(need.as_str()),
                }
            }

            // Variables every inlined job exports alike, such as the workflow's `env`, go once in front
            let common: Vec<String> = exports
                .iter()
                .filter(|e| inlined.iter().all(|(_, es)| es.contains(e)))
                .cloned()
                .collect();
            let rest = |es: &[String]| -> Vec<String> {
                es.iter().filter(|e| !common.contains(e)).cloned().collect()
            };
            let mut parts: Vec<String> = inlined
                .iter()
                .map(|(c, es)| format!("({})", with_exports(&rest(es), c)))
                .collect();

            // Its artifacts come from a job it needs that no check rebuilds, so it would find no files
            let unbuilt: Vec<&str> = job
                .needs
                .iter()
                .map(String::as_str)
                .filter(|n| missing.contains(n))
                .collect();

            if job.downloads && !unbuilt.is_empty() {
                d.skipped.push(format!(
                    "{file}, job {id}: downloads artifacts from jobs it needs that have no check: {}",
                    unbuilt.join(", ")
                ));
                continue;
            }

            d.skipped.extend(missing.iter().map(|need| {
                let why = if stays_here(need) {
                    "stays on GitHub"
                } else {
                    "has no check"
                };
                format!("{file}, job {id}: needs job {need}, which {why}")
            }));

            let after = if parts.is_empty() {
                String::new()
            } else {
                format!(", after job {}", ran.join(", "))
            };
            parts.push(with_exports(&rest(exports), command));
            let command = with_exports(&common, &parts.join(" && "));

            if let Some(same) = d.checks.iter().find(|c| c.command == command) {
                d.skipped
                    .push(format!("{file}, job {id}: runs the same as {}", same.name));
                continue;
            }

            let mut name = id.to_string();
            let mut n = 1;

            while d.checks.iter().any(|c| c.name == name) {
                n += 1;
                name = format!("{id}-{n}");
            }

            d.checks.push(Found {
                name,
                command,
                from: format!("{file}, job {id}{after}"),
            });
        }
    }

    let (project, versions) = project_checks(root);
    d.versions = versions;
    // A job that stays on GitHub is CI's check of what project files would guess
    let from_workflows = !d.checks.is_empty() || !d.github.is_empty();

    for (name, tool, command, from) in project {
        if from_workflows {
            let runs_it = all().find(|(_, j)| j.steps.iter().any(|s| s.run.contains(&command)));
            let why = if d.checks.iter().any(|c| c.command.contains(&command)) {
                "already in a workflow check".to_string()
            } else if let Some((file, job)) = runs_it {
                format!("{file}, job {} runs it on GitHub", job.id.escape_debug())
            } else {
                "no workflow runs it".to_string()
            };
            d.skipped.push(format!("{from}: {why}: `{command}`"));
            continue;
        }

        let name = if d.checks.iter().any(|c| c.name == name) {
            format!("{name}-{tool}")
        } else {
            name
        };
        d.checks.push(Found {
            name,
            command,
            from,
        });
    }

    let tools = tool_versions(&d.checks, &d.versions, answers);
    d.versions.extend(tools);

    // A pinned toolchain's own version, since `rustc --version` names the default one
    for check in &d.checks {
        for word in check.command.split_whitespace() {
            if let Some(t) = word.strip_prefix("RUSTUP_TOOLCHAIN=") {
                let version = format!("rustc +{t} --version");

                if !d.versions.contains(&version) {
                    d.versions.push(version);
                }
            }
        }
    }

    d
}

/// A job's command and the `NAME=value` exports it needs in front
type JobCommand = (String, Vec<String>);

/// `export <exports> && <command>`, or the command alone
fn with_exports(exports: &[String], command: &str) -> String {
    if exports.is_empty() {
        return command.to_string();
    }

    format!("export {} && {command}", exports.join(" "))
}

/// Shell builtins, coreutils, git and tools that set the machine up, whose version never changes what a check finds
const PLAIN: &[&str] = &[
    "cd", "test", "[", "true", "false", "echo", "printf", "mkdir", "cp", "mv", "rm", "ls", "cat",
    "diff", "sh", "bash", "env", "time", "export", "exit", "sleep", "xargs", "grep", "sed", "awk",
    "find", "tee", "head", "tail", "sort", "uniq", "wc", "tr", "cut", "touch", "chmod", "ln",
    "pwd", "command", "git", "sudo", "apt", "apt-get", "brew", "curl", "wget", "rustup",
    "truncate", "mount", "umount", "chown",
];

/**
 * `<tool> --version` for each tool the checks start that no project file already names, so a new
 * gitleaks or shellcheck runs the checks again. Only tools that answer, since a `versions` command
 * that fails makes every run fail
 **/
fn tool_versions(checks: &[Found], known: &[String], answers: fn(&str) -> bool) -> Vec<String> {
    let covered: Vec<&str> = known
        .iter()
        .filter_map(|v| v.split_whitespace().next())
        .collect();
    let mut tools: Vec<String> = Vec::new();

    for check in checks {
        let words = check
            .command
            .replace(['(', ')', '{', '}', ';', '|', '&'], "\n");

        for segment in words.lines() {
            // The tool `xargs` starts is the one that runs, after its flags
            let mut after_xargs = false;
            let tool = segment
                .split_whitespace()
                .find(|w| {
                    after_xargs |= *w == "xargs";
                    !(["time", "xargs"].contains(w)
                        || w.contains('=')
                        || after_xargs && w.starts_with('-'))
                })
                .unwrap_or_default();
            let named = !tool.is_empty()
                && tool
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c));

            if named
                && !PLAIN.contains(&tool)
                && !tool.starts_with("mkfs")
                && !covered.contains(&tool)
                && !tools.iter().any(|t| t == tool)
            {
                tools.push(tool.to_string());
            }
        }
    }

    tools
        .into_iter()
        .filter(|t| answers(t))
        .map(|t| format!("{t} --version"))
        .collect()
}

/// Whether `<tool> --version` exits 0 within a few seconds on this machine
#[cfg(not(test))]
fn answers(tool: &str) -> bool {
    responds(tool, &["--version"])
}

#[cfg(not(test))]
fn responds(tool: &str, args: &[&str]) -> bool {
    let Ok(mut child) = std::process::Command::new(tool)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };

    for _ in 0..60 {
        if let Ok(Some(status)) = child.try_wait() {
            return status.success();
        }

        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let _ = child.kill();
    let _ = child.wait();
    false
}

// Tests see the same file on every machine, whatever is installed
#[cfg(test)]
fn answers(_: &str) -> bool {
    false
}

/// The jobs `needs` names, and the jobs those need, each once, every job after the ones it needs
fn needed(
    jobs: &[crate::workflow::Job],
    needs: &[String],
    out: &mut Vec<String>,
    path: &mut Vec<String>,
) {
    for need in needs {
        // A cycle is GitHub's error, and this stops at it
        if out.contains(need) || path.contains(need) {
            continue;
        }

        if let Some(job) = jobs.iter().find(|j| &j.id == need) {
            path.push(need.clone());
            needed(jobs, &job.needs, out, path);
            path.pop();
        }

        out.push(need.clone());
    }
}

/// Commands that set a machine up rather than check the code, left out of a job's check. Package
/// installs into the checkout stay, since a warm clone's `node_modules` can be older than the lockfile
const SETUP: &[&str] = &[
    "pip install",
    "pip3 install",
    "python -m pip install",
    "python3 -m pip install",
    "go install",
    "npm config",
    "git config",
    "gh ",
    "apt",
    "sudo",
    "curl",
    "wget",
    "brew",
    "rustup",
    "cargo install",
    "echo",
];

/// Whether a command or a job id has deploy, publish or release as a word of its own
fn ships(text: &str, separators: &[char]) -> bool {
    text.to_lowercase()
        .split(|c: char| c.is_whitespace() || separators.contains(&c))
        .any(|w| ["deploy", "publish", "release"].contains(&w))
}

/// Builtins that change the shell they run in, so a step that uses them runs in a subshell
const STATEFUL: &[&str] = &[
    "cd", "export", "set", "unset", "source", ".", "alias", "pushd", "popd", "shopt", "ulimit",
    "umask",
];

/// Commands that move files or talk to git, which pass whatever the code is
const HOUSEKEEPING: &[&str] = &[
    "cd", "export", "mkdir", "touch", "cp", "mv", "rm", "ln", "chmod", "find", "ls", "cat", "echo",
    "printf", "tar", "unzip", "install", "git", "sleep", "true", "pwd", "which",
];

/// The tool and subcommand of package installs and lockfile updates that `install` does not name
const INSTALLS: &[[&str; 2]] = &[
    ["npm", "ci"],
    ["uv", "sync"],
    ["poetry", "lock"],
    ["cargo", "fetch"],
    ["cargo", "update"],
    ["cargo", "generate-lockfile"],
    ["go", "mod"],
];

/**
 * Whether a check only installs packages, moves files or talks to git, so it would pass whatever
 * the code is. That happens when the job's real work is a `uses:` step or a step left out
 **/
fn checks_nothing(command: &str) -> bool {
    command.split(['&', '|', ';', '(', ')', '{', '}']).all(|c| {
        // `VAR=1` before the command and `+nightly` after cargo do not change what runs
        let words: Vec<&str> = c
            .split_whitespace()
            .filter(|w| !w.contains('=') && !w.starts_with('+'))
            .collect();
        words.is_empty()
            || HOUSEKEEPING.contains(&words[0])
            || words.contains(&"install")
            || INSTALLS.iter().any(|i| words.starts_with(i))
    })
}

/// `NAME=value` for the shell, or None for a name a shell cannot export or a value with a control character
fn assignment(name: &str, value: &str) -> Option<String> {
    let name_ok = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');

    if !name_ok || value.chars().any(char::is_control) {
        return None;
    }

    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:,@+%".contains(c))
    {
        return Some(format!("{name}={value}"));
    }

    Some(format!("{name}='{}'", value.replace('\'', "'\\''")))
}

/// Commands that need root, which a check never has
const ROOT: &[&str] = &[
    "sudo", "mount", "umount", "losetup", "apt", "apt-get", "dnf", "yum", "modprobe",
];

fn needs_root(line: &str) -> bool {
    line.split(|c: char| c.is_whitespace() || "&|;(){}".contains(c))
        .any(|w| ROOT.contains(&w) || w.starts_with("mkfs"))
}

/// Variables the sandbox sets for each check, which a workflow's value would point elsewhere
const SANDBOX_VARS: &[&str] = &["TMPDIR", "HOME"];

const CONTROL: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "select", "{", "}",
];

/**
 * One step as a command: its lines that are not blank, comments or setup, joined with ` && `,
 * which is how GitHub's `bash -e` stops at the first failure. A line with `|` or `&` is grouped
 * so `&&` cannot bind into it. Ok(None) for nothing to run, Err(fatal, why) to leave it out
 **/
fn step_command(step: &crate::workflow::RunStep) -> Result<Option<String>, (bool, String)> {
    let leave = |why: &str| Err((false, why.to_string()));

    if step.conditional {
        return leave("a step with `if:` or `continue-on-error`");
    }

    if step.run.chars().any(|c| c.is_control() && c != '\n') {
        return leave("a step with a control character");
    }

    if step
        .shell
        .as_deref()
        .is_some_and(|s| s != "bash" && s != "sh")
    {
        return leave("a step in another shell");
    }

    let mut lines = Vec::new();
    let mut root = false;

    for line in step.run.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let first = line.split_whitespace().next().unwrap_or_default();

        if ["$RUNNER_", "${RUNNER_", "$GITHUB_", "${GITHUB_"]
            .iter()
            .any(|v| line.contains(v))
        {
            return leave("a step with a variable only GitHub's runner sets");
        }

        if line.contains("${{") {
            return leave("a step with an expression");
        }

        if line.ends_with('\\') {
            return leave("a step with a `\\` continuation");
        }

        if line.contains("<<") {
            return leave("a step with a heredoc");
        }

        if CONTROL.contains(&first) || line.contains("()") {
            return leave("a step with shell control flow or a function");
        }

        if line.contains(';') {
            return leave("a step with `;`, which GitHub's `bash -e` stops at");
        }

        root |= needs_root(line);

        if SETUP.iter().any(|s| line.starts_with(s)) {
            continue;
        }

        if ships(line, &[]) {
            return Err((true, "a step deploys, publishes or releases".into()));
        }

        lines.push(line.to_string());
    }

    // The rest of such a step works on what the root commands made, such as a volume they mount
    if root && !lines.is_empty() {
        return leave(
            "a step that sets up the machine as root, with `sudo`, `mount`, `mkfs` or `apt`",
        );
    }

    // A step that only changes its own shell, such as a lone `cd` after its install was left out, does nothing
    if lines.iter().all(|l| {
        !l.contains(['&', '|'])
            && STATEFUL.contains(&l.split_whitespace().next().unwrap_or_default())
    }) {
        return Ok(None);
    }

    // GitHub sets a step's `env` for its process only
    let mut exports = Vec::new();

    for (name, value) in &step.env {
        match value.as_deref().and_then(|v| assignment(name, v)) {
            Some(a) => exports.push(a),
            None => return leave("a step with an expression"),
        }
    }

    if !exports.is_empty() {
        lines.insert(0, format!("export {}", exports.join(" ")));
    }

    Ok(match lines.as_slice() {
        [] => None,
        [one] => Some(one.to_string()),
        many => Some(
            many.iter()
                .map(|l| {
                    if l.contains(['|', '&']) {
                        format!("{{ {l}; }}")
                    } else {
                        l.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(" && "),
        ),
    })
}

/// A `uses` step's input, when it is not an expression
fn input(step: &crate::workflow::RunStep, name: &str) -> Option<String> {
    step.with
        .iter()
        .find(|(k, _)| k == name)
        .and_then(|(_, v)| v.clone())
}

/**
 * The command a check runs for an action: the install a setup action runs into the checkout, which
 * a check needs since the action is left out, `pnpm/setup` unless `install: false`,
 * `pnpm/action-setup` with `run_install: true` and `ruby/setup-ruby` with `bundler-cache: true`,
 * and `gitleaks dir` for `gitleaks/gitleaks-action`. `setup-node`'s and `setup-uv`'s caches install nothing
 **/
fn action_install(step: &crate::workflow::RunStep) -> Option<String> {
    let action = step.uses.as_deref()?.split('@').next()?;
    let set = |name: &str| {
        step.with
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_deref())
    };

    match action {
        "pnpm/setup" if set("install").is_none_or(|v| v.is_some_and(|v| v != "false")) => {
            Some("pnpm install".into())
        }
        "pnpm/action-setup" if set("run_install") == Some(Some("true")) => {
            Some("pnpm install".into())
        }
        "ruby/setup-ruby" if set("bundler-cache") == Some(Some("true")) => {
            Some("bundle install".into())
        }
        // The tree, as the cache key is, so history scans stay on GitHub. gitleaks reads `.gitleaks.toml` itself
        "gitleaks/gitleaks-action" => {
            let config = step
                .env
                .iter()
                .find(|(k, _)| k == "GITLEAKS_CONFIG")
                .map(|(_, v)| v.as_deref());

            match config {
                None => Some("gitleaks dir --redact .".into()),
                Some(Some(c)) if assignment("C", c).is_some_and(|a| a == format!("C={c}")) => {
                    Some(format!("gitleaks dir --redact --config {c} ."))
                }
                Some(_) => None,
            }
        }
        _ => None,
    }
}

/**
 * Why a job needs a Rust toolchain or target this machine may not have: one a step adds with
 * `rustup`, or a `dtolnay/rust-toolchain` or `actions-rust-lang/setup-rust-toolchain` other than
 * stable or with targets. A check without it fails on a machine that lacks it, where GitHub passes
 **/
fn rust_need(step: &crate::workflow::RunStep) -> Option<String> {
    for line in step.run.lines() {
        let words: Vec<&str> = line
            .split_whitespace()
            .filter(|w| !w.starts_with('-'))
            .collect();
        let named = |n: usize| (words.len() > n).then(|| words[n..].join(" "));

        match words.as_slice() {
            ["rustup", "target", "add", ..] => {
                return named(3).map(|t| format!("needs target {t}"));
            }
            ["rustup", "toolchain", "install" | "add", ..] | ["rustup", "install", ..] => {
                let at = if words[1] == "install" { 2 } else { 3 };
                let toolchains: Vec<&str> = words[at..]
                    .iter()
                    .copied()
                    .filter(|t| *t != "stable")
                    .collect();

                if !toolchains.is_empty() {
                    return Some(format!("needs toolchain {}", toolchains.join(" ")));
                }
            }
            _ => {}
        }
    }

    let (action, at) = step.uses.as_deref()?.split_once('@')?;
    let default = match action {
        "dtolnay/rust-toolchain" => at,
        // Without `toolchain` it reads rust-toolchain.toml, which rustup reads here too
        "actions-rust-lang/setup-rust-toolchain" => "stable",
        _ => return None,
    };
    let set = |name: &str| {
        step.with
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_deref())
    };
    let toolchain = match set("toolchain") {
        Some(Some(t)) => t.trim().to_string(),
        Some(None) => return Some("needs a toolchain from an expression".into()),
        None => default.to_string(),
    };

    if toolchain != "stable" {
        return Some(format!("needs toolchain {toolchain}"));
    }

    match set("targets").or(set("target")) {
        Some(Some(t)) if !t.trim().is_empty() => Some(format!("needs target {}", t.trim())),
        Some(None) => Some("needs a target from an expression".into()),
        _ => None,
    }
}

/**
 * One job's check: its steps' commands joined with ` && `. Returns the command, the job's
 * `NAME=value` exports to put in front, and the steps it left out. Each GitHub step starts in its
 * own shell, so a step of several commands, with `|` or `&`, or with its own directory runs in a
 * subshell. A toolchain from `toolchains` is pinned with `RUSTUP_TOOLCHAIN`
 **/
#[allow(clippy::type_complexity)]
fn job_check(
    job: &crate::workflow::Job,
    toolchains: &[String],
) -> Result<(String, Vec<String>, Vec<String>), String> {
    // A name becomes a TOML key, a directory and a check name
    let id_ok = job
        .id
        .starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && job
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c));

    if !id_ok {
        return Err("the job id is not letters, digits, `_` and `-`".into());
    }

    if ships(&job.id, &['-', '_']) {
        return Err("it looks like a deploy, publish or release".into());
    }

    if job.conditional {
        return Err("a job with `if:` or `continue-on-error`, which GitHub may skip".into());
    }

    let mut pin: Option<&str> = None;

    for need in job.steps.iter().filter_map(rust_need) {
        let Some(t) = need.strip_prefix("needs toolchain ") else {
            return Err(need);
        };

        if !toolchains.iter().any(|i| i == t) {
            return Err(format!("{need}, which rustup does not have here"));
        }

        if pin.is_some_and(|p| p != t) {
            return Err(format!(
                "needs toolchains {} and {t}",
                pin.unwrap_or_default()
            ));
        }

        pin = toolchains.iter().find(|i| *i == t).map(String::as_str);
    }

    let mut parts: Vec<(Option<&str>, String)> = Vec::new();
    let mut notes = Vec::new();
    let mut exports: Vec<String> = pin
        .and_then(|t| assignment("RUSTUP_TOOLCHAIN", t))
        .into_iter()
        .collect();

    for (name, value) in &job.env {
        if SANDBOX_VARS.contains(&name.as_str()) {
            notes.push(format!("left out variable {name}, which the sandbox sets"));
            continue;
        }

        match value.as_deref().and_then(|v| assignment(name, v)) {
            Some(a) => exports.push(a),
            None => notes.push(format!("left out variable {name}, an expression")),
        }
    }

    // A setup action that installs becomes its install command
    let mut steps: Vec<crate::workflow::RunStep> = job
        .steps
        .iter()
        .filter_map(|step| {
            if !step.run.is_empty() {
                return Some(step.clone());
            }

            Some(crate::workflow::RunStep {
                run: action_install(step)?,
                dir: input(step, "working-directory"),
                shell: None,
                // An action's `env` is for the action, not for the command that replaces it
                env: Vec::new(),
                ..step.clone()
            })
        })
        .collect();

    for step in &mut steps {
        step.env.retain(|(name, _)| {
            let sandbox = SANDBOX_VARS.contains(&name.as_str());

            if sandbox {
                notes.push(format!("left out variable {name}, which the sandbox sets"));
            }

            !sandbox
        });
    }

    for step in &steps {
        let dir = step.dir.as_deref().filter(|d| *d != ".");

        if dir.is_some_and(|d| {
            d.starts_with('/')
                || d.contains("..")
                || !d
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
        }) {
            notes.push("left out a step with an unusual working directory".to_string());
            continue;
        }

        match step_command(step) {
            Ok(Some(command)) => parts.push((dir, command)),
            Ok(None) => {}
            Err((true, why)) => return Err(why),
            Err((false, why)) => notes.push(format!("left out {why}")),
        }
    }

    let mut seen = BTreeSet::new();
    notes.retain(|n| seen.insert(n.clone()));

    if parts.is_empty() && notes.is_empty() {
        return Err("no run step to check with".into());
    }

    if parts.is_empty() {
        return Err(format!(
            "nothing left to check with, it {}",
            notes.join(", ")
        ));
    }

    if parts.iter().all(|(_, run)| checks_nothing(run)) {
        let mut why =
            "nothing left to check with, it only installs, moves files or runs git".to_string();

        if !notes.is_empty() {
            why = format!("{why}, it {}", notes.join(", "));
        }

        return Err(why);
    }

    let shared = parts[0]
        .0
        .filter(|d| parts.iter().all(|(p, _)| *p == Some(*d)));
    let one = parts.len() == 1;
    let joined = parts
        .iter()
        .map(|(d, run)| match (d, shared) {
            (Some(d), None) => format!("(cd {d} && {run})"),
            _ if !one
                && (run.contains(['|', '&'])
                    || STATEFUL.contains(&run.split_whitespace().next().unwrap_or_default())) =>
            {
                format!("({run})")
            }
            _ => run.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" && ");
    let command = match shared {
        Some(d) => format!("cd {d} && {joined}"),
        None => joined,
    };

    Ok((command, exports, notes))
}

/**
 * Host checks and `versions` from the project files at the root: package.json scripts with the
 * package manager its lockfile names, Cargo.toml, pyproject.toml and go.mod. Each check is its
 * name, its tool, which names it when the name is taken, its command and its file
 **/
#[allow(clippy::type_complexity)]
fn project_checks(root: &Path) -> (Vec<(String, String, String, String)>, Vec<String>) {
    let has = |f: &str| root.join(f).exists();
    let mut checks = Vec::new();
    let mut versions = Vec::new();
    let mut add = |name: &str, tool: &str, command: String, from: &str| {
        checks.push((
            name.to_string(),
            tool.to_string(),
            command,
            from.to_string(),
        ));
    };

    let scripts = fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .map(|v| v["scripts"].clone())
        .unwrap_or_default();
    // npm init's placeholder fails on purpose
    let script = |name: &str| {
        scripts[name]
            .as_str()
            .is_some_and(|s| !s.contains("no test specified"))
    };

    if script("test") || script("lint") {
        let pm = if has("bun.lock") || has("bun.lockb") {
            "bun"
        } else if has("pnpm-lock.yaml") {
            "pnpm"
        } else if has("yarn.lock") {
            "yarn"
        } else {
            "npm"
        };
        // `bun test` is bun's own test runner, not the script
        let run = |name: &str| match (pm, name) {
            ("npm", "test") => "npm test".to_string(),
            ("npm", _) | ("bun", _) => format!("{pm} run {name}"),
            _ => format!("{pm} {name}"),
        };

        for name in ["test", "lint"].into_iter().filter(|n| script(n)) {
            add(name, pm, run(name), "package.json");
        }

        if pm != "bun" {
            versions.push("node --version".to_string());
        }

        versions.push(format!("{pm} --version"));
    }

    if has("Cargo.toml") {
        add("test", "cargo", "cargo test".into(), "Cargo.toml");
        add(
            "clippy",
            "cargo",
            "cargo clippy --all-targets -- -D warnings".into(),
            "Cargo.toml",
        );
        versions.extend(["cargo --version".into(), "rustc --version".into()]);
    }

    if has("pyproject.toml") {
        if has("uv.lock") {
            add("test", "pytest", "uv run pytest".into(), "pyproject.toml");
            versions.push("uv --version".into());
        } else {
            add("test", "pytest", "pytest".into(), "pyproject.toml");
            versions.push("python3 --version".into());
        }
    }

    if has("go.mod") {
        add("test", "go", "go test ./...".into(), "go.mod");
        add("vet", "go", "go vet ./...".into(), "go.mod");
        versions.push("go version".into());
    }

    (checks, versions)
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// For every clone, once: writes the pre-push hook from this binary into the repository's own
/// hooks directory, which no checkout can change, unless another hook manager owns the hooks
pub fn install(root: &Path) -> Result<String, String> {
    let hooks_path = git(root, &["config", "--get", "core.hooksPath"])?;

    if !hooks_path.is_empty() {
        return Err(format!(
            "core.hooksPath is {hooks_path}, another hook manager owns the hooks. Add this line to its pre-push hook: {HOOK_LINE}"
        ));
    }

    let dir = PathBuf::from(git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
    )?);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut done = vec![write_hook(&dir.join("pre-push"), PRE_PUSH, HOOK_LINE)?];

    // The commit hook only helps, so a foreign one there is a note, not a failure
    match write_hook(&dir.join("post-commit"), POST_COMMIT, COMMIT_LINE) {
        Ok(line) | Err(line) => done.push(line),
    }

    Ok(done.join("\n"))
}

fn write_hook(hook: &Path, script: &str, line: &str) -> Result<String, String> {
    if let Ok(existing) = fs::read_to_string(hook)
        && !existing.contains(MARK)
    {
        return Err(format!(
            "{} already exists. Add this line to it: {line}",
            hook.display()
        ));
    }

    fs::write(hook, script).map_err(|e| e.to_string())?;
    make_executable(hook)?;
    Ok(format!("installed {}", hook.display()))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::config::RepoConfig;

    #[test]
    fn generated_config_parses() {
        let text = config_for(&["ci.yml".into(), "deploy.yml".into()]);
        assert_eq!(
            RepoConfig::parse(&text).unwrap().workflows,
            ["ci.yml", "deploy.yml"]
        );
    }

    #[test]
    fn init_turns_sharding_on() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
        fs::write(dir.path().join(".github/workflows/ci.yml"), "on: push").unwrap();
        init(dir.path()).unwrap();
        let text = fs::read_to_string(dir.path().join("shipcheck.toml")).unwrap();
        assert!(RepoConfig::parse(&text).unwrap().shard_tests.enabled());
    }

    #[test]
    fn init_turns_workflow_jobs_into_host_checks() {
        let ci = r#"on: [push, pull_request]
defaults:
  run:
    shell: bash
jobs:
  test:
    steps:
      - uses: actions/checkout@v4
      - run: pip install -r r.txt
      - run: cargo fmt --check
      - run: cargo build --release
      - run: make ${{ x }}
      - run: cat "$GITHUB_OUTPUT"
      - run: cargo test | tee out || true
      - run: make lint
        continue-on-error: true
      - run: make coverage
        if: github.event_name == 'push'
  service:
    defaults:
      run:
        working-directory: service
    steps:
      - run: bun install
      - run: bun run test
      - run: bunx tsc --noEmit
  mixed:
    steps:
      - run: make
      - run: make check
        working-directory: sub
  clippy:
    steps:
      - run: cargo clippy --all-targets -- -D warnings
  again:
    steps:
      - run: cargo clippy --all-targets -- -D warnings
  deploy-docs:
    steps:
      - run: make docs
  predeploy:
    steps:
      - run: |
          # Seed the keypairs
          mkdir -p target/deploy

          cp key.json target/deploy/key.json
          a || b
      - run: anchor build
  ship:
    steps:
      - run: npm publish --dry-run
  gated:
    if: github.repository == 'a/b'
    steps:
      - run: make
  "a b":
    steps:
      - run: "true"
  setup-only:
    steps:
      - run: pip install -r r.txt
"#;
        let tags =
            "on:\n  push:\n    tags: [v*]\njobs:\n  release:\n    steps:\n      - run: make\n";
        let (cfg, text, said) = init_with(&[
            (".github/workflows/ci.yml", ci),
            (".github/workflows/tag.yml", tags),
            ("Cargo.toml", ""),
            ("package.json", r#"{"scripts": {"lint": "eslint ."}}"#),
        ]);
        assert_eq!(cfg.runner, crate::config::RunnerKind::Host);
        assert_eq!(
            checks(&cfg),
            [
                ("clippy", "cargo clippy --all-targets -- -D warnings"),
                ("mixed", "make && (cd sub && make check)"),
                (
                    "predeploy",
                    "(mkdir -p target/deploy && cp key.json target/deploy/key.json && { a || b; }) && anchor build"
                ),
                (
                    "service",
                    "cd service && bun install && bun run test && bunx tsc --noEmit"
                ),
                (
                    "test",
                    "cargo fmt --check && cargo build --release && (cargo test | tee out || true)"
                ),
            ],
            "{text}"
        );
        // Written in the order found
        assert!(
            text.contains("[host.checks]\ntest = \"cargo fmt --check"),
            "{text}"
        );
        // Project files give versions, and no check while a workflow gives some
        assert_eq!(
            cfg.host.versions,
            [
                "node --version",
                "npm --version",
                "cargo --version",
                "rustc --version"
            ]
        );
        for line in [
            "  test = cargo fmt --check && cargo build --release && (cargo test | tee out || true)  (from ci.yml, job test)",
            "  ci.yml, job test: left out a step with an expression",
            "  ci.yml, job test: left out a step with a variable only GitHub's runner sets",
            "  ci.yml, job test: left out a step with `if:` or `continue-on-error`",
            "  ci.yml, job again: runs the same as clippy",
            "  ci.yml, job deploy-docs: it looks like a deploy, publish or release",
            "  ci.yml, job ship: a step deploys, publishes or releases",
            "  ci.yml, job gated: a job with `if:` or `continue-on-error`, which GitHub may skip",
            "  ci.yml, job a b: the job id is not letters, digits, `_` and `-`",
            "  ci.yml, job setup-only: no run step to check with",
            "  tag.yml: GitHub does not run it for feature branches",
            "  Cargo.toml: already in a workflow check: `cargo clippy --all-targets -- -D warnings`",
            "  Cargo.toml: already in a workflow check: `cargo test`",
            "  package.json: no workflow runs it: `npm run lint`",
        ] {
            assert!(said.contains(line), "{line}\n{said}");
        }
        assert_eq!(
            said.matches("job test: left out a step with `if:`").count(),
            1,
            "{said}"
        );
    }

    #[test]
    fn multi_line_steps_take_only_simple_commands() {
        let ci = r#"on: push
jobs:
  ok:
    steps:
      - run: |
          cd tests
          FUZZ=1 cargo run --release
  cont:
    steps:
      - run: |
          cargo test \
            --locked
  heredoc:
    steps:
      - run: |
          cat <<EOF > a
          x
          EOF
  flow:
    steps:
      - run: |
          if [ -f a ]
          then make
          fi
  func:
    steps:
      - run: |
          f() { make; }
          f
"#;
        let (cfg, _, said) = init_with(&[(".github/workflows/ci.yml", ci)]);
        assert_eq!(
            checks(&cfg),
            [("ok", "cd tests && FUZZ=1 cargo run --release")]
        );
        for line in [
            "job cont: nothing left to check with, it left out a step with a `\\` continuation",
            "job heredoc: nothing left to check with, it left out a step with a heredoc",
            "job flow: nothing left to check with, it left out a step with shell control flow or a function",
            "job func: nothing left to check with, it left out a step with shell control flow or a function",
        ] {
            assert!(said.contains(line), "{line}\n{said}");
        }
    }

    #[test]
    fn a_job_runs_the_jobs_it_needs_first() {
        let ci = r#"on: push
jobs:
  e2e:
    needs: [integration, build]
    steps:
      - uses: actions/download-artifact@v4
      - run: make e2e
  integration:
    needs: build
    steps:
      - run: make integration
  build:
    needs: setup
    steps:
      - run: make build
  setup:
    steps:
      - run: pip install -r r.txt
"#;
        let (cfg, _, said) = init_with(&[(".github/workflows/ci.yml", ci)]);
        assert_eq!(
            cfg.host.checks["e2e"],
            "(make build) && (make integration) && make e2e"
        );
        assert_eq!(
            cfg.host.checks["integration"],
            "(make build) && make integration"
        );
        assert_eq!(cfg.host.checks["build"], "make build");
        assert!(
            said.contains("(from ci.yml, job e2e, after job build, integration)"),
            "{said}"
        );
        assert!(
            said.contains("job build: needs job setup, which has no check"),
            "{said}"
        );
    }

    #[test]
    fn project_files_give_checks_only_without_workflow_checks() {
        let (cfg, _, _) = init_with(&[
            (
                ".github/workflows/ci.yml",
                "on: push\njobs:\n  a:\n    steps:\n      - run: pip install -r r.txt\n",
            ),
            ("Cargo.toml", ""),
        ]);
        assert_eq!(cfg.host.checks["test"], "cargo test");
    }

    #[test]
    fn workflow_checks_cannot_inject_toml() {
        let ci = r#"on: push
jobs:
  test:
    steps:
      - run: "x\r\u007f"
      - run: "x\n[host]\nchecks = { t = \"true\" }"
  lint:
    steps:
      - run: 'cargo clippy "a" ''b'''
"#;
        let (cfg, text, _) = init_with(&[(".github/workflows/ci.yml", ci)]);
        // The lines of a multi-line step are commands inside one TOML string, never TOML lines
        assert_eq!(
            checks(&cfg),
            [
                ("lint", "cargo clippy \"a\" 'b'"),
                ("test", "x && [host] && checks = { t = \"true\" }")
            ],
            "{text}"
        );
    }

    #[test]
    fn workflow_env_goes_into_the_check() {
        // axum's CI.yml, trimmed
        let ci = r#"on: [push, pull_request]
env:
  CARGO_TERM_COLOR: always
jobs:
  cargo-hack:
    runs-on: ubuntu-24.04
    env:
      # Fail the build if there are any warnings
      RUSTFLAGS: "-D warnings"
    steps:
      - uses: taiki-e/install-action@cargo-hack
      - name: cargo hack check
        run: cargo hack check --each-feature --no-dev-deps --all
  armv5te-unknown-linux-musleabi:
    runs-on: ubuntu-24.04
    steps:
      - name: Check
        env:
          CC: clang
        run: cargo check --all-targets --all-features --target armv5te-unknown-linux-musleabi
"#;
        // rails' check-markdown-api.yml and gin's gin.yml, trimmed
        let rails = r#"on: pull_request
jobs:
  markdown_api:
    steps:
      - run: ruby .github/workflows/scripts/check-markdown-api.rb $BASE_SHA $HEAD_SHA
        env:
          BASE_SHA: ${{ github.event.pull_request.base.sha }}
          HEAD_SHA: ${{ github.event.pull_request.head.sha }}
  test:
    env:
      GO111MODULE: on
      TESTTAGS: ${{ matrix.test-tags }}
    steps:
      - run: make test
"#;
        let (cfg, _, said) = init_with(&[
            (".github/workflows/ci.yml", ci),
            (".github/workflows/rails.yml", rails),
        ]);
        assert_eq!(
            checks(&cfg),
            [
                (
                    "armv5te-unknown-linux-musleabi",
                    "export CARGO_TERM_COLOR=always && export CC=clang && cargo check --all-targets --all-features --target armv5te-unknown-linux-musleabi"
                ),
                (
                    "cargo-hack",
                    "export CARGO_TERM_COLOR=always RUSTFLAGS='-D warnings' && cargo hack check --each-feature --no-dev-deps --all"
                ),
                ("test", "export GO111MODULE=on && make test"),
            ]
        );
        for line in [
            "rails.yml, job markdown_api: nothing left to check with, it left out a step with an expression",
            "rails.yml, job test: left out variable TESTTAGS, an expression",
        ] {
            assert!(said.contains(line), "{line}\n{said}");
        }
    }

    #[test]
    fn a_job_that_checks_nothing_is_left_out() {
        // fastapi, axum, ruff, cobra and express workflows, trimmed
        let ci = r#"on: pull_request
jobs:
  pre-commit:
    steps:
      - run: uv sync --locked --extra all
      - run: uv run prek run --from-ref origin/${GITHUB_BASE_REF} --to-ref HEAD
        continue-on-error: true
  test-msrv:
    steps:
      - run: cargo +nightly update -Z minimal-versions
      - run: cargo +${{ env.MSRV }} test --locked --all-features
  docker-build:
    steps:
      - uses: docker/build-push-action@v6
      - run: |
          mkdir -p /tmp/digests
          touch "/tmp/digests/x"
  test-win:
    runs-on: windows-latest
    steps:
      - run: git config --global core.autocrlf input
      - run: go test ./...
  test:
    steps:
      - run: npm config set loglevel error
      - run: python -m pip install pre-commit==4.6.0
      - run: npm install
      - run: npm run test-ci
  rail_inspector:
    steps:
      - run: rm -f Gemfile.lock
      - run: cd tools/rail_inspector && bundle exec rake
"#;
        let (cfg, _, said) = init_with(&[(".github/workflows/ci.yml", ci)]);
        assert_eq!(
            checks(&cfg),
            [
                (
                    "rail_inspector",
                    "rm -f Gemfile.lock && (cd tools/rail_inspector && bundle exec rake)"
                ),
                ("test", "npm install && npm run test-ci"),
            ]
        );
        for line in [
            "job pre-commit: nothing left to check with, it only installs, moves files or runs git, it left out a step with `if:` or `continue-on-error`",
            "job test-msrv: nothing left to check with, it only installs, moves files or runs git, it left out a step with an expression",
            "job docker-build: nothing left to check with, it only installs, moves files or runs git",
            "job test-win: runs on Windows, which shipcheck does not support yet",
        ] {
            assert!(said.contains(line), "{line}\n{said}");
        }
    }

    #[test]
    fn a_step_cd_stays_in_its_step() {
        // fastapi's test-redistribute.yml, trimmed: the install after the `cd` is left out
        let ci = r#"on: pull_request
jobs:
  test-redistribute:
    steps:
      - run: python -m build --sdist
      - run: |
          cd dist/fastapi*/
          pip install --group tests --editable .[all]
      - run: |
          cd dist/fastapi*/
          bash scripts/test.sh
      - run: cd dist
      - run: pip wheel --no-deps fastapi*.tar.gz
"#;
        let (cfg, _, _) = init_with(&[(".github/workflows/ci.yml", ci)]);
        assert_eq!(
            checks(&cfg),
            [(
                "test-redistribute",
                "python -m build --sdist && (cd dist/fastapi*/ && bash scripts/test.sh) && pip wheel --no-deps fastapi*.tar.gz"
            )]
        );
    }

    #[test]
    fn a_job_on_artifacts_no_check_builds_is_left_out() {
        // fastapi's test.yml, trimmed
        let ci = r#"on: pull_request
jobs:
  test:
    strategy:
      matrix:
        python-version: ["3.10", "3.14"]
    steps:
      - run: uv run --no-sync bash scripts/test-cov.sh ${{ matrix.python-version }}
  coverage-combine:
    needs: [test]
    steps:
      - uses: actions/download-artifact@v8
      - run: uv sync --locked --no-dev --group tests --extra all
      - run: uv run coverage combine coverage
      - run: uv run coverage report --fail-under=100
"#;
        let found = detect_with(
            &[(".github/workflows/ci.yml", ci), ("Cargo.toml", "")],
            false,
        );
        assert!(found.act.is_none());
        let said = found.explain();
        assert!(
            said.contains("job coverage-combine: downloads artifacts from jobs it needs that have no check: test"),
            "{said}"
        );
        // The matrix stays on GitHub, so Cargo.toml's guesses are no checks either
        assert!(found.checks.is_empty(), "{said}");
    }

    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();

        for (name, text) in files {
            fs::create_dir_all(dir.path().join(name).parent().unwrap()).unwrap();
            fs::write(dir.path().join(name), text).unwrap();
        }

        dir
    }

    // axum's CI.yml, trimmed
    const AXUM: &str = r#"on:
  push:
    branches: [main]
  pull_request: {}
jobs:
  check:
    runs-on: ubuntu-24.04
    strategy:
      matrix:
        workspace: [".", examples]
    steps:
      - uses: dtolnay/rust-toolchain@beta
      - run: cargo clippy --manifest-path ${{ matrix.workspace }}/Cargo.toml
      - run: cargo fmt --all --check --manifest-path ${{ matrix.workspace }}/Cargo.toml
  check-docs:
    runs-on: ubuntu-24.04
    steps:
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo doc --package axum --all-features --no-deps
  external-types:
    runs-on: ubuntu-24.04
    steps:
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: nightly-2026-03-20
      - run: cargo hack --no-private check-external-types --all-features
  test-nightly:
    runs-on: ubuntu-24.04
    steps:
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: ${{ steps.rust-toolchain.outputs.version }}
      - run: cargo test
        working-directory: axum-macros
  armv5te-unknown-linux-musleabi:
    runs-on: ubuntu-24.04
    steps:
      - uses: dtolnay/rust-toolchain@stable
        with:
          target: armv5te-unknown-linux-musleabi
      - run: cargo check --all-targets --target armv5te-unknown-linux-musleabi
"#;

    #[test]
    fn a_linux_matrix_runs_the_repository_in_act_with_docker() {
        let release = "on:\n  push:\n    branches: [main]\njobs:\n  release:\n    steps:\n      - run: make\n";
        let files = [
            (".github/workflows/CI.yml", AXUM),
            (".github/workflows/release-plz.yml", release),
        ];
        let found = detect_with(&files, true);
        assert_eq!(
            found.act.as_deref(),
            Some(
                "CI.yml, job check runs a matrix (workspace: ., examples): host mode tests only this machine's toolchain, so this repository runs in act"
            )
        );
        assert!(found.checks.is_empty() && found.github.is_empty());

        let dir = repo(&files);
        let said = init_on(
            dir.path(),
            &Machine {
                docker: true,
                ..Machine::default()
            },
        )
        .unwrap()
        .unwrap();
        let cfg =
            RepoConfig::parse(&fs::read_to_string(dir.path().join("shipcheck.toml")).unwrap())
                .unwrap();
        assert_eq!(cfg.runner, crate::config::RunnerKind::Act);
        assert_eq!(cfg.workflows, ["CI.yml"]);
        assert!(
            said.contains("runs a matrix (workspace: ., examples)"),
            "{said}"
        );
    }

    #[test]
    fn without_docker_a_matrix_stays_on_github_and_toolchains_are_left_out() {
        let found = detect_with(&[(".github/workflows/CI.yml", AXUM)], false);
        assert!(found.act.is_none());
        let checks: Vec<&str> = found.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(checks, ["check-docs"]);
        let said = found.explain();
        for line in [
            "  CI.yml, job check: runs a matrix (workspace: ., examples) and Docker is not running, so act cannot run it",
            "  CI.yml, job external-types: needs toolchain nightly-2026-03-20",
            "  CI.yml, job test-nightly: needs a toolchain from an expression",
            "  CI.yml, job armv5te-unknown-linux-musleabi: needs target armv5te-unknown-linux-musleabi",
        ] {
            assert!(said.contains(line), "{line}\n{said}");
        }
        assert_eq!(
            found.github[0].checks.as_deref(),
            Some(&["check (.)".to_string(), "check (examples)".to_string()][..])
        );
    }

    #[test]
    fn a_matrix_with_macos_or_windows_entries_stays_on_github() {
        // flask's tests.yaml and express's ci.yml, trimmed
        let flask = r#"on:
  pull_request:
jobs:
  tests:
    name: ${{ matrix.name || matrix.python }}
    runs-on: ${{ matrix.os || 'ubuntu-latest' }}
    strategy:
      matrix:
        include:
          - {python: '3.15'}
          - {python: '3.14'}
          - {name: Windows, python: '3.14', os: windows-latest}
          - {name: Mac, python: '3.14', os: macos-latest}
          - {python: '3.13'}
          - {python: '3.12'}
          - {python: '3.11'}
    steps:
      - uses: astral-sh/setup-uv@v9
        with:
          enable-cache: true
      - run: uv run --locked --no-default-groups --group dev tox run
        env:
          TOX_ENV: ${{ matrix.tox || format('py{0}', matrix.python) }}
  typing:
    runs-on: ubuntu-latest
    steps:
      - uses: astral-sh/setup-uv@v9
      - run: uv run --locked --no-default-groups --group dev tox run -e typing
"#;
        let express = r#"on: [push, pull_request]
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/setup-node@v7
        with:
          cache: npm
      - run: npm install --ignore-scripts --include=dev
      - run: npm run lint
  test:
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, windows-latest]
        node-version: [18, 26]
    name: Node.js ${{ matrix.node-version }} - ${{matrix.os}}
    steps:
      - uses: actions/setup-node@v7
        with:
          node-version: ${{ matrix.node-version }}
      - run: npm install
      - run: npm run test-ci
"#;
        // Docker cannot help: act runs no macOS or Windows entry
        let found = detect_with(
            &[
                (".github/workflows/tests.yaml", flask),
                (".github/workflows/ci.yml", express),
            ],
            true,
        );
        assert!(found.act.is_none());
        let checks: Vec<(&str, &str)> = found
            .checks
            .iter()
            .map(|c| (c.name.as_str(), c.command.as_str()))
            .collect();
        assert_eq!(
            checks,
            [
                (
                    "lint",
                    "npm install --ignore-scripts --include=dev && npm run lint"
                ),
                (
                    "typing",
                    "uv run --locked --no-default-groups --group dev tox run -e typing"
                ),
            ]
        );
        let said = found.explain();
        for line in [
            "  ci.yml, job test: runs a matrix (node-version: 18, 26) with Windows entries, which act cannot run",
            "  tests.yaml, job tests: runs a matrix (python: 3.15, 3.14, 3.13, 3.12, ...) with macOS and Windows entries, which act cannot run",
        ] {
            assert!(said.contains(line), "{line}\n{said}");
        }
        assert_eq!(
            found.github[0].checks.as_deref(),
            Some(
                &[
                    "Node.js 18 - ubuntu-latest".to_string(),
                    "Node.js 26 - ubuntu-latest".to_string(),
                    "Node.js 18 - windows-latest".to_string(),
                    "Node.js 26 - windows-latest".to_string(),
                ][..]
            )
        );
        // A matrix of `include` entries has names init cannot tell
        assert!(found.github[1].checks.is_none());
    }

    #[test]
    fn windows_and_macos_jobs_stay_on_github() {
        // cobra's test.yml and ripgrep's ci.yml, trimmed
        let cobra = r#"on:
  push:
  pull_request:
jobs:
  test-unix:
    strategy:
      matrix:
        platform: [ubuntu, macOS]
        go: [17, 24]
    name: '${{ matrix.platform }} | 1.${{ matrix.go }}.x'
    runs-on: ${{ matrix.platform }}-latest
    steps:
      - run: make richtest
  test-win:
    name: MINGW64
    defaults:
      run:
        shell: msys2 {0}
    runs-on: windows-latest
    steps:
      - shell: bash
        run: git config --global core.autocrlf input
      - run: make test
  mac-only:
    runs-on: macos-14
    steps:
      - run: make test
"#;
        let ripgrep = r#"on:
  pull_request:
jobs:
  wasm:
    runs-on: ubuntu-latest
    steps:
    - uses: dtolnay/rust-toolchain@master
      with:
        toolchain: stable
    - run: rustup target add wasm32-wasip1
    - run: cargo build --verbose
  rustfmt:
    runs-on: ubuntu-latest
    steps:
    - uses: dtolnay/rust-toolchain@master
      with:
        toolchain: stable
        components: rustfmt
    - run: cargo fmt --all --check
"#;
        let files = [
            (".github/workflows/test.yml", cobra),
            (".github/workflows/ci.yml", ripgrep),
        ];
        let found = detect_with(&files, false);
        let kept: Vec<(&str, &str)> = found
            .github
            .iter()
            .map(|k| (k.job.as_str(), k.why.as_str()))
            .collect();
        assert_eq!(
            kept,
            [
                (
                    "test-unix",
                    "runs a matrix (go: 17, 24) with macOS entries, which act cannot run"
                ),
                (
                    "test-win",
                    "runs on Windows, which shipcheck does not support yet"
                ),
                ("mac-only", "runs on macOS, and this machine is not a Mac"),
            ]
        );
        assert_eq!(
            found.github[1].checks.as_deref(),
            Some(&["MINGW64".to_string()][..])
        );
        assert_eq!(
            found.github[0].checks.as_ref().unwrap()[1],
            "ubuntu | 1.24.x"
        );
        let checks: Vec<&str> = found.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(checks, ["rustfmt"]);
        assert!(
            found
                .explain()
                .contains("  ci.yml, job wasm: needs target wasm32-wasip1"),
            "{}",
            found.explain()
        );

        // On a Mac, host mode runs the macOS job
        let dir = repo(&files);
        let mac = detect_on(
            dir.path(),
            &Machine {
                docker: false,
                mac: true,
                ..Machine::default()
            },
        );
        assert!(mac.checks.iter().any(|c| c.name == "mac-only"));
        assert!(!mac.github.iter().any(|k| k.job == "mac-only"));
    }

    #[test]
    fn what_a_job_on_github_needs_stays_there_too() {
        let ci = r#"on: pull_request
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: make lint
  tests:
    needs: [lint]
    runs-on: ubuntu-latest
    strategy:
      matrix:
        python: ["3.12", "3.13"]
    steps:
      - run: make test
  docs:
    runs-on: ubuntu-latest
    steps:
      - run: make docs
"#;
        let found = detect_with(&[(".github/workflows/ci.yml", ci)], false);
        let kept: Vec<(&str, &str)> = found
            .github
            .iter()
            .map(|k| (k.job.as_str(), k.why.as_str()))
            .collect();
        assert_eq!(
            kept,
            [
                (
                    "tests",
                    "runs a matrix (python: 3.12, 3.13) and Docker is not running, so act cannot run it"
                ),
                ("lint", "job tests stays on GitHub and needs it"),
            ]
        );
        let checks: Vec<&str> = found.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(checks, ["docs"]);
    }

    #[test]
    fn a_matrix_with_no_run_step_does_not_make_it_act() {
        // express's codeql.yml, trimmed
        let codeql = "on: pull_request\njobs:\n  analyze:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        language: [javascript, actions]\n    steps:\n      - uses: github/codeql-action/init@v4\n";
        let ci = "on: pull_request\njobs:\n  lint:\n    steps:\n      - run: make lint\n";
        let found = detect_with(
            &[
                (".github/workflows/codeql.yml", codeql),
                (".github/workflows/ci.yml", ci),
            ],
            true,
        );
        assert!(found.act.is_none() && found.github.is_empty());
        assert_eq!(found.checks[0].command, "make lint");
    }

    #[test]
    fn setup_actions_that_install_keep_their_install() {
        // vite's ci.yml lint and rails' ci.yml, trimmed
        let ci = r#"on: pull_request
jobs:
  lint:
    steps:
      - uses: pnpm/setup@a0881977cd8ea73b1ed212236c99ca10dcc80cf4 # v3.1.1
        with:
          runtime: node@24
          cache: true
      - run: pnpm run build
  no-install:
    steps:
      - uses: pnpm/setup@v3
        with:
          install: false
      - run: pnpm run build2
  action-setup:
    steps:
      - uses: pnpm/action-setup@v4
        with:
          run_install: true
      - run: pnpm test
  ruby:
    steps:
      - uses: ruby/setup-ruby@v1
        with:
          bundler-cache: true
      - run: bundle exec rake
  node:
    steps:
      - uses: actions/setup-node@v7
        with:
          cache: npm
      - uses: astral-sh/setup-uv@v9
        with:
          enable-cache: true
      - run: npm test
"#;
        let (cfg, _, _) = init_with(&[(".github/workflows/ci.yml", ci)]);
        assert_eq!(
            checks(&cfg),
            [
                ("action-setup", "pnpm install && pnpm test"),
                ("lint", "pnpm install && pnpm run build"),
                ("no-install", "pnpm run build2"),
                ("node", "npm test"),
                ("ruby", "bundle install && bundle exec rake"),
            ]
        );
    }

    /// What `init` finds in these files on a Linux machine with or without Docker
    fn detect_with(files: &[(&str, &str)], docker: bool) -> Detected {
        detect_on(
            repo(files).path(),
            &Machine {
                docker,
                ..Machine::default()
            },
        )
    }

    /// The config `init` writes for a root holding these files, parsed and validated
    fn init_with(files: &[(&str, &str)]) -> (RepoConfig, String, String) {
        let dir = tempfile::tempdir().unwrap();

        for (name, text) in files {
            fs::create_dir_all(dir.path().join(name).parent().unwrap()).unwrap();
            fs::write(dir.path().join(name), text).unwrap();
        }

        let said = init(dir.path()).unwrap().unwrap();
        let text = fs::read_to_string(dir.path().join("shipcheck.toml")).unwrap();
        let cfg = RepoConfig::parse(&text).unwrap();
        let workflows: BTreeSet<String> = cfg.workflows.iter().cloned().collect();
        assert_eq!(cfg.validate(&workflows), Ok(()), "{text}");
        (cfg, text, said)
    }

    fn checks(cfg: &RepoConfig) -> Vec<(&str, &str)> {
        cfg.host
            .checks
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    #[test]
    fn versions_name_each_tool_a_check_starts() {
        let found = |command: &str| Found {
            name: "c".into(),
            command: command.into(),
            from: String::new(),
        };
        let checks = [
            found("git rev-parse HEAD >/dev/null && gitleaks git --redact ."),
            found("{ git ls-files -z '*.sh' | xargs -0 shellcheck -S warning; } && make lint"),
            found("time cargo check && (FUZZ=1 uv run pytest || ./run.sh)"),
        ];
        let known = vec!["cargo --version".to_string()];
        assert_eq!(
            tool_versions(&checks, &known, |t| t != "make"),
            ["gitleaks --version", "shellcheck --version", "uv --version"]
        );
    }

    #[test]
    fn init_detects_package_json_scripts_with_the_lockfiles_manager() {
        let pkg = r#"{"scripts": {"test": "jest", "lint": "eslint ."}}"#;
        for (lock, test, lint, version) in [
            (
                "package-lock.json",
                "npm test",
                "npm run lint",
                "npm --version",
            ),
            ("yarn.lock", "yarn test", "yarn lint", "yarn --version"),
            ("pnpm-lock.yaml", "pnpm test", "pnpm lint", "pnpm --version"),
            ("bun.lock", "bun run test", "bun run lint", "bun --version"),
        ] {
            let (cfg, text, said) = init_with(&[("package.json", pkg), (lock, "")]);
            assert_eq!(cfg.runner, crate::config::RunnerKind::Host);
            assert!(text.contains("runner = \"host\""));
            assert_eq!(checks(&cfg), [("lint", lint), ("test", test)], "{lock}");
            assert!(cfg.host.versions.contains(&version.to_string()));
            assert_eq!(
                cfg.host.versions.contains(&"node --version".to_string()),
                !lock.starts_with("bun")
            );
            assert!(
                said.contains(&format!("  test = {test}  (from package.json)")),
                "{said}"
            );
        }

        // npm init's placeholder test is no check
        let placeholder = r#"{"scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#;
        let (cfg, _, _) = init_with(&[("package.json", placeholder), ("Cargo.toml", "")]);
        assert_eq!(checks(&cfg).len(), 2);
    }

    #[test]
    fn init_detects_cargo_python_and_go() {
        let (cfg, _, _) = init_with(&[("Cargo.toml", "[package]\nname = \"a\"\n")]);
        assert_eq!(
            checks(&cfg),
            [
                ("clippy", "cargo clippy --all-targets -- -D warnings"),
                ("test", "cargo test")
            ]
        );
        assert_eq!(cfg.host.versions, ["cargo --version", "rustc --version"]);

        let (cfg, _, _) = init_with(&[("pyproject.toml", ""), ("uv.lock", "")]);
        assert_eq!(checks(&cfg), [("test", "uv run pytest")]);
        assert_eq!(cfg.host.versions, ["uv --version"]);
        let (cfg, _, _) = init_with(&[("pyproject.toml", "")]);
        assert_eq!(checks(&cfg), [("test", "pytest")]);

        let (cfg, _, _) = init_with(&[("go.mod", "module a\n")]);
        assert_eq!(
            checks(&cfg),
            [("test", "go test ./..."), ("vet", "go vet ./...")]
        );
        assert_eq!(cfg.host.versions, ["go version"]);

        // Two ecosystems, so the second test is named after its tool
        let (cfg, _, _) = init_with(&[("Cargo.toml", ""), ("go.mod", "")]);
        assert_eq!(cfg.host.checks["test"], "cargo test");
        assert_eq!(cfg.host.checks["test-go"], "go test ./...");
    }

    #[test]
    fn init_falls_back_to_act_and_says_why() {
        let (cfg, text, said) = init_with(&[
            (".github/workflows/ci.yml", "on: push"),
            ("package.json", r#"{"scripts": {"build": "tsc"}}"#),
        ]);
        assert_eq!(cfg.runner, crate::config::RunnerKind::Act);
        assert!(text.starts_with("runner = \"act\"\n"));
        assert_eq!(cfg.workflows, ["ci.yml"]);
        assert!(said.contains("runner = \"act\": no check found"), "{said}");

        let dir = tempfile::tempdir().unwrap();
        assert!(
            init(dir.path())
                .unwrap_err()
                .contains("no .github/workflows")
        );
    }

    #[test]
    fn a_step_that_needs_root_and_sandbox_variables_are_left_out() {
        // orch's release.yml test job, trimmed
        let ci = r#"on: push
jobs:
  test:
    runs-on: ubuntu-latest
    env:
      HOME: /home/runner
    steps:
      - name: Create XFS reflink volume
        run: |
          command -v mkfs.xfs >/dev/null || sudo apt-get install -y xfsprogs
          truncate -s 8G /tmp/xfs.img
          mkfs.xfs -m reflink=1 /tmp/xfs.img
          sudo mkdir /mnt/xfs
          sudo mount -o loop /tmp/xfs.img /mnt/xfs
      - name: Run tests with coverage
        env:
          ORCH_COW_EXPECT_CLONE: "1"
          TMPDIR: /mnt/xfs/tmp
        run: cargo llvm-cov nextest --lcov --output-path lcov.info
  setup:
    steps:
      - run: |
          sudo apt-get update
          sudo apt-get install -y lcov
      - run: make check
"#;
        let (cfg, _, said) = init_with(&[(".github/workflows/ci.yml", ci)]);
        assert_eq!(
            checks(&cfg),
            [
                ("setup", "make check"),
                (
                    "test",
                    "export ORCH_COW_EXPECT_CLONE=1 && cargo llvm-cov nextest --lcov --output-path lcov.info"
                ),
            ]
        );
        for line in [
            "ci.yml, job test: left out a step that sets up the machine as root, with `sudo`, `mount`, `mkfs` or `apt`",
            "ci.yml, job test: left out variable TMPDIR, which the sandbox sets",
            "ci.yml, job test: left out variable HOME, which the sandbox sets",
        ] {
            assert!(said.contains(line), "{line}\n{said}");
        }
        // A step of only setup commands is dropped without a note
        assert!(!said.contains("job setup:"), "{said}");
    }

    #[test]
    fn versions_leave_out_setup_tools() {
        let checks = [Found {
            name: "c".into(),
            command: "({ command -v mkfs.xfs >/dev/null || sudo apt-get install -y xfsprogs; } && truncate -s 8G x && mkfs.xfs x && sudo mount x y) && cargo test".into(),
            from: String::new(),
        }];
        assert_eq!(tool_versions(&checks, &[], |_| true), ["cargo --version"]);
    }

    #[test]
    fn a_pinned_toolchain_is_pinned_when_rustup_has_it() {
        // orch's release.yml and oblivion's ci.yml, trimmed
        let ci = r#"on: push
env:
  RUST_VERSION: 1.93.1
jobs:
  check:
    steps:
      - uses: actions-rust-lang/setup-rust-toolchain@v1
        with:
          toolchain: "1.94.1"
          components: clippy, rustfmt
      - run: cargo clippy --all-targets -- -D warnings
  prop:
    steps:
      - uses: actions-rust-lang/setup-rust-toolchain@v1
        with:
          toolchain: ${{ env.RUST_VERSION }}
      - run: cargo test
  nightly:
    steps:
      - uses: actions-rust-lang/setup-rust-toolchain@v1
        with:
          toolchain: nightly-2026-01-01
      - run: cargo build
  file:
    steps:
      - uses: actions-rust-lang/setup-rust-toolchain@v1
      - run: cargo doc
  target:
    steps:
      - uses: actions-rust-lang/setup-rust-toolchain@v1
        with:
          target: wasm32-wasip1
      - run: cargo check
"#;
        let found = detect_on(
            repo(&[(".github/workflows/ci.yml", ci)]).path(),
            &Machine {
                toolchains: vec!["stable".into(), "1.93.1".into(), "1.94.1".into()],
                ..Machine::default()
            },
        );
        let checks: Vec<(&str, &str)> = found
            .checks
            .iter()
            .map(|c| (c.name.as_str(), c.command.as_str()))
            .collect();
        assert_eq!(
            checks,
            [
                (
                    "check",
                    "export RUSTUP_TOOLCHAIN=1.94.1 RUST_VERSION=1.93.1 && cargo clippy --all-targets -- -D warnings"
                ),
                (
                    "prop",
                    "export RUSTUP_TOOLCHAIN=1.93.1 RUST_VERSION=1.93.1 && cargo test"
                ),
                ("file", "export RUST_VERSION=1.93.1 && cargo doc"),
            ]
        );
        assert_eq!(
            found.versions,
            ["rustc +1.94.1 --version", "rustc +1.93.1 --version"]
        );
        let said = found.explain();
        for line in [
            "ci.yml, job nightly: needs toolchain nightly-2026-01-01, which rustup does not have here",
            "ci.yml, job target: needs target wasm32-wasip1",
        ] {
            assert!(said.contains(line), "{line}\n{said}");
        }

        // Without the toolchain the job is left out
        let found = detect_with(&[(".github/workflows/ci.yml", ci)], false);
        assert!(
            found.explain().contains(
                "ci.yml, job check: needs toolchain 1.94.1, which rustup does not have here"
            ),
            "{}",
            found.explain()
        );
    }

    #[test]
    fn a_matrix_of_one_combination_is_a_plain_job() {
        // bean's ci.yml and git-cow's, trimmed
        let ci = r#"on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        python-version: [3.13]
    steps:
      - uses: ./.github/actions/setup
        with:
          python-version: ${{ matrix.python-version }}
      - run: uv run pytest -v --python ${{ matrix.python-version }}
  one:
    strategy:
      matrix:
        include:
          - {os: ubuntu-latest, fs: ext4}
    runs-on: ${{ matrix.os }}
    steps:
      - run: make test FS=${{matrix.fs}}
"#;
        let found = detect_with(&[(".github/workflows/ci.yml", ci)], true);
        assert!(found.act.is_none() && found.github.is_empty());
        let checks: Vec<(&str, &str)> = found
            .checks
            .iter()
            .map(|c| (c.name.as_str(), c.command.as_str()))
            .collect();
        assert_eq!(
            checks,
            [
                ("test", "uv run pytest -v --python 3.13"),
                ("one", "make test FS=ext4")
            ]
        );
        let jobs = crate::workflow::job_runs(ci);
        assert_eq!(jobs[0].check_names, Some(vec!["test (3.13)".to_string()]));

        // Two values are a matrix, and the reason shows them
        let two = ci.replace("[3.13]", "[3.12, 3.13]");
        let found = detect_with(&[(".github/workflows/ci.yml", &two)], true);
        assert!(
            found
                .act
                .as_deref()
                .is_some_and(|w| w.contains("runs a matrix (python-version: 3.12, 3.13)")),
            "{:?}",
            found.act
        );
    }

    // orch's orch-review.yml, trimmed
    const REVIEW: &str = r#"on:
  push:
jobs:
  review-gate:
    runs-on: ubuntu-latest
    if: github.ref != format('refs/heads/{0}', github.event.repository.default_branch)
    steps:
      - run: ./scripts/lint-frontmatter.sh
"#;

    #[test]
    fn a_job_that_runs_only_off_the_base_branch_stays_on_github() {
        let ci = "on: push\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make lint\n";
        let found = detect_with(
            &[
                (".github/workflows/ci.yml", ci),
                (".github/workflows/orch-review.yml", REVIEW),
            ],
            false,
        );
        assert_eq!(found.checks[0].command, "make lint");
        assert_eq!(found.github.len(), 1);
        assert_eq!(found.github[0].job, "review-gate");
        assert_eq!(found.github[0].why, PR_ONLY);

        // In act mode its workflow is not one of the checks
        let matrix = "on: push\njobs:\n  test:\n    strategy:\n      matrix:\n        py: ['3.12', '3.13']\n    steps:\n      - run: make test\n";
        let dir = repo(&[
            (".github/workflows/ci.yml", matrix),
            (".github/workflows/orch-review.yml", REVIEW),
        ]);
        let said = init_on(
            dir.path(),
            &Machine {
                docker: true,
                ..Machine::default()
            },
        )
        .unwrap()
        .unwrap();
        let cfg =
            RepoConfig::parse(&fs::read_to_string(dir.path().join("shipcheck.toml")).unwrap())
                .unwrap();
        assert_eq!(cfg.workflows, ["ci.yml"]);
        assert!(
            said.contains("orch-review.yml stays on GitHub as it is"),
            "{said}"
        );
    }

    #[test]
    fn a_project_check_a_job_on_github_runs_says_so() {
        // git-cow's ci.yml, trimmed
        let ci = r#"on: [push, pull_request]
jobs:
  test:
    strategy:
      matrix:
        include:
          - {os: macos-latest, fs: apfs}
          - {os: ubuntu-latest, fs: xfs}
    runs-on: ${{ matrix.os }}
    steps:
      - run: cargo test
"#;
        let found = detect_with(
            &[(".github/workflows/ci.yml", ci), ("Cargo.toml", "")],
            false,
        );
        let said = found.explain();
        assert!(
            said.contains("Cargo.toml: ci.yml, job test runs it on GitHub: `cargo test`"),
            "{said}"
        );
        assert!(
            said.contains("Cargo.toml: no workflow runs it: `cargo clippy"),
            "{said}"
        );
        let dir = repo(&[(".github/workflows/ci.yml", ci)]);
        let said = init(dir.path()).unwrap().unwrap();
        assert!(said.starts_with(NOTHING_LOCAL), "{said}");
        assert!(!said.contains("step 4"), "{said}");
    }

    #[test]
    fn gitleaks_action_becomes_gitleaks_dir() {
        // orch's release.yml secrets job, trimmed
        let ci = r#"on: push
jobs:
  secrets:
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: gitleaks/gitleaks-action@v2
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GITLEAKS_CONFIG: .gitleaks.toml
  leaks:
    steps:
      - uses: gitleaks/gitleaks-action@v2
"#;
        let (cfg, _, _) = init_with(&[(".github/workflows/ci.yml", ci)]);
        assert_eq!(
            checks(&cfg),
            [
                ("leaks", "gitleaks dir --redact ."),
                ("secrets", "gitleaks dir --redact --config .gitleaks.toml ."),
            ]
        );
        let found = |command: &str| Found {
            name: "c".into(),
            command: command.into(),
            from: String::new(),
        };
        assert_eq!(
            tool_versions(&[found("gitleaks dir --redact .")], &[], |_| true),
            ["gitleaks --version"]
        );
    }

    #[test]
    fn workflow_env_is_exported_once_per_check() {
        // oblivion's ci.yml, trimmed
        let ci = r#"on: push
env:
  RUST_VERSION: 1.93.1
jobs:
  build:
    steps:
      - run: anchor build
  integration:
    needs: build
    env:
      EXTRA: "1"
    steps:
      - run: anchor test
"#;
        let (cfg, text, _) = init_with(&[(".github/workflows/ci.yml", ci)]);
        assert_eq!(
            cfg.host.checks["integration"],
            "export RUST_VERSION=1.93.1 && (anchor build) && export EXTRA=1 && anchor test"
        );
        assert!(
            text.contains("# check uses this machine's tools, which `versions` records: RUST_VERSION\n[host.checks]"),
            "{text}"
        );
    }

    #[test]
    fn hook_is_marked_and_hands_off_to_shipcheck() {
        assert!(PRE_PUSH.starts_with("#!/bin/sh\n"));
        assert!(PRE_PUSH.contains(MARK));
        assert!(PRE_PUSH.contains(HOOK_LINE));
    }
}
