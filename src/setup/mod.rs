use crate::agents;
use crate::autostart;
use crate::base;
use crate::config::Act;
use crate::git;
use crate::java;
use crate::java::Jdk;
use crate::paths;
use crate::ruby;
use crate::ruby::Ruby;
use crate::runner::act;
use crate::workflow;
use crate::workflow::Job;
use crate::workflow::RunStep;
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Engine;

const MARK: &str = "# Installed by `shipcheck install`";

pub const PRE_PUSH: &str = r#"#!/bin/sh
# Installed by `shipcheck install`. Starts a background run of each commit pushed to a branch other than the default, never blocks the push
if ! command -v shipcheck >/dev/null 2>&1; then
  exit 0
fi

zero=0000000000000000000000000000000000000000

while read -r local_ref local_sha remote_ref remote_sha; do
  if [ "$local_sha" != "$zero" ]; then
    shipcheck on-push "$local_ref" "$local_sha" "$remote_ref"
  fi
done

exit 0
"#;

pub const HOOK_LINE: &str = "shipcheck on-push \"$local_ref\" \"$local_sha\" \"$remote_ref\"";

pub const POST_COMMIT: &str = r"#!/bin/sh
# Installed by `shipcheck install`. Starts a background run of each commit when shipcheck.toml lists commit in on
if command -v shipcheck >/dev/null 2>&1; then
  shipcheck on-commit
fi
";

pub const COMMIT_LINE: &str = "shipcheck on-commit";

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
#[derive(Clone, Debug)]
pub struct Found {
    pub name: String,
    pub command: String,
    pub from: String,
}

/// A job that keeps running on GitHub for PRs, since shipcheck cannot run it here
#[derive(Clone, Debug)]
pub struct Kept {
    pub file: String,
    pub job: String,
    pub why: String,
    /// GitHub's names for its check runs, None when `init` cannot tell them
    pub checks: Option<Vec<String>>,
    /// Why act cannot run it here either, on a machine with Docker
    pub act: Option<String>,
}

#[derive(Default, Clone, Debug)]
pub struct Detected {
    pub checks: Vec<Found>,
    pub versions: Vec<String>,
    /// What was left out, and why
    pub skipped: Vec<String>,
    pub github: Vec<Kept>,
    /// Jobs no host check runs as GitHub does, which act runs on an engine this machine has
    pub acts: Vec<(Kept, Engine)>,
    /// Why the repository runs in act, when a job runs a matrix act can run
    pub act: Option<String>,
    /// Each check and a tool it starts that is not on this machine's PATH
    pub missing: Vec<String>,
    /// Checks that run on this machine's version of a tool, not the one the workflow sets up
    pub warnings: Vec<String>,
}

/// What `init` needs to know of the machine it runs on
#[derive(Default, Debug)]
pub struct Machine {
    pub docker: bool,
    /// colima answers, so a job that runs Docker can get a VM of its own
    pub colima: bool,
    pub mac: bool,
    /// The rustup toolchains installed, such as `1.94.1` or `nightly`
    pub toolchains: Vec<String>,
    pub jdks: Vec<Jdk>,
    /// The `CRuby` version `ruby` runs, such as `4.0.9`
    pub ruby: Option<String>,
    pub rubies: Vec<Ruby>,
    /// The Xcode apps installed, such as `/Applications/Xcode_26.6.app`
    pub xcodes: Vec<String>,
}

impl Machine {
    #[cfg(not(test))]
    pub fn this() -> Self {
        Machine {
            docker: responds("docker", &["info"]),
            colima: responds("colima", &["version"]),
            mac: cfg!(target_os = "macos"),
            toolchains: toolchains(),
            jdks: java::installed(),
            ruby: ruby(),
            rubies: ruby::installed(),
            xcodes: xcodes(),
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
            .map_or_default(|o| String::from_utf8_lossy(&o.stdout).into_owned())
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

/// `RUBY_VERSION` when `ruby` is `CRuby`
#[cfg(not(test))]
fn ruby() -> Option<String> {
    let out = Command::new("ruby")
        .args(["-e", "print RUBY_VERSION if RUBY_ENGINE == 'ruby'"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !version.is_empty()).then_some(version)
}

#[cfg(not(test))]
fn xcodes() -> Vec<String> {
    fs::read_dir("/Applications")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| {
            n.starts_with("Xcode")
                && std::path::Path::new(n)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("app"))
        })
        .map(|n| format!("/Applications/{n}"))
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

        if !self.warnings.is_empty() {
            out.push("run on this machine's version, not the workflow's:".into());
            out.extend(self.warnings.iter().map(|w| format!("  {w}")));
        }

        if !self.missing.is_empty() {
            out.push("not installed here, so these checks fail until it is:".into());
            out.extend(self.missing.iter().map(|m| format!("  {m}")));
        }

        if !self.acts.is_empty() {
            out.push("run with act, since no host check runs them as GitHub does:".into());
            out.extend(self.acts.iter().map(|(k, engine)| {
                let on = match engine {
                    Engine::Colima => " on a colima VM of its own, which reaches this machine's loopback and the internet, so it is for trusted code only",
                    Engine::Docker => "",
                };
                format!("  {}, job {}{on}: {}", k.file, k.job, k.why)
            }));
        }

        if !self.github.is_empty() {
            out.push("stay on GitHub for PRs, and use Actions minutes:".into());
            out.extend(self.github.iter().map(|k| match &k.act {
                Some(act) => format!(
                    "  {}, job {}: {}, and act cannot run it either: {act}",
                    k.file, k.job, k.why
                ),
                None => format!("  {}, job {}: {}", k.file, k.job, k.why),
            }));
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

    let (text, said) = propose(root, machine)?;

    let Some(text) = text else {
        return Ok(Some(said));
    };

    agents::write_in_repo(root, &config, &text)?;
    Ok(Some(format!("wrote shipcheck.toml with {said}")))
}

/**
 * What `init` would write as shipcheck.toml, writing nothing: the text, and what it holds for
 * "wrote shipcheck.toml with ...". No text, and why, when there is nothing to run here
 **/
pub fn propose(root: &Path, machine: &Machine) -> Result<(Option<String>, String), String> {
    let found = detect_on(root, machine);

    if let Some(why) = &found.act {
        // Only what GitHub runs for a PR, so no release or deploy runs on a laptop
        let (workflows, unrun): (Vec<String>, Vec<String>) = workflow_files(root)
            .unwrap_or_default()
            .into_iter()
            .partition(|f| {
                fs::read_to_string(root.join(".github/workflows").join(f))
                    .is_ok_and(|s| workflow::runs_for_branches(&s))
            });
        // A job that runs only off the base branch would be a check that never runs on GitHub's main
        let (pr_only, workflows): (Vec<String>, Vec<String>) = workflows
            .into_iter()
            .partition(|f| found.github.iter().any(|k| k.file == *f));

        if workflows.is_empty() {
            let mut said = NOTHING_LOCAL.to_string();

            for k in &found.github {
                let _ = write!(said, "\n  {}, job {}: {}", k.file, k.job, k.why);
            }

            for file in unrun {
                let _ = write!(said, "\n  {file}: it runs on no PR or branch push");
            }

            return Ok((None, said));
        }

        let mut said = format!("runner = \"act\": {why}");

        for file in pr_only {
            for k in found.github.iter().filter(|k| k.file == file) {
                let _ = write!(
                    said,
                    "\n{file} stays on GitHub as it is, since act runs a workflow whole: job {}: {}",
                    k.job, k.why
                );
            }
        }

        return Ok((Some(act_config(&workflows)), said));
    }

    if found.checks.is_empty() && found.acts.is_empty() && !found.github.is_empty() {
        // No step 4 follows
        let said = found.explain().replace(&format!("\n{REQUIRE}"), "");
        return Ok((None, format!("{NOTHING_LOCAL}\n{said}")));
    }

    if !found.checks.is_empty() || !found.acts.is_empty() {
        return Ok((Some(config_text(&found)), said_for(&found)));
    }

    let workflows = workflow_files(root).ok_or_else(|| {
        "no .github/workflows and no package.json scripts, Cargo.toml, pyproject.toml or go.mod in this repository".to_string()
    })?;
    Ok((
        Some(act_config(&workflows)),
        "runner = \"act\": no check found in the workflows' run steps or in a package.json test or lint script, Cargo.toml, pyproject.toml or go.mod, so the GitHub workflows run in containers".into(),
    ))
}

const REQUIRE: &str =
    "  Require their GitHub checks too, the branch protection step of init adds them";

/// What `init` says when it writes no shipcheck.toml because every job stays on GitHub
pub const NOTHING_LOCAL: &str = "nothing to run locally: every job stays on GitHub, so init wrote no shipcheck.toml and set nothing up (https://shipcheck.run/docs/how-it-works/#which-jobs-run-where)";

fn act_config(workflows: &[String]) -> String {
    format!(
        "runner = \"act\"\n# act's jobs reach the internet, to download what their actions and steps set up\nnetwork = \"internet\"\n{}\n# Splits each one-line pytest step into parallel shards. A test that fails only after another test's leftovers can pass\nshard_tests = true\n",
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
    pinned.sort_unstable();
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

/// What "init writes shipcheck.toml with ..." says of a config `config_text` writes
pub fn said_for(d: &Detected) -> String {
    let what = if d.checks.is_empty() {
        "jobs on act only"
    } else {
        "these host checks"
    };
    format!("{what}:\n{}", d.explain())
}

/// The host checks, then a `[jobs]` entry for each job act runs
pub fn config_text(d: &Detected) -> String {
    let mut text = if d.checks.is_empty() {
        String::new()
    } else {
        host_config(&d.checks, &d.versions)
    };

    // A job on Docker downloads its actions' tools, and `network` covers act's jobs only
    if d.acts.iter().any(|(_, e)| *e == Engine::Docker) {
        let line = "# act's jobs reach the internet, to download what their steps set up\nnetwork = \"internet\"\n";
        let at = text.find("\n[").map_or(0, |i| i + 1);
        let gap = if text.is_empty() { "" } else { "\n" };
        text.insert_str(at, &format!("{line}{gap}"));
    }

    if !d.acts.is_empty() {
        text.push_str(
            "\n# No host check runs these jobs as GitHub does, so act runs them, each in its own checkout\n",
        );
    }

    for (k, engine) in &d.acts {
        let name = toml::Value::String(format!("{}/{}", k.file, k.job));
        let _ = write!(text, "\n[jobs.{name}]\nrunner = \"act\"\n");

        if *engine == Engine::Colima {
            text.push_str("# A VM of its own, with its own Docker. It reaches this machine's loopback, for trusted code only\nengine = \"colima\"\n");
        }
    }

    text
}

/// Marks a note of `job_check` that the check runs with another version than the workflow's
const WARNING: &str = "warning: ";

/// Actions that report to GitHub or publish, which only GitHub's run of the job may do
const REPORTERS: &[&str] = &[
    "github/codeql-action/",
    "actions/dependency-review-action",
    "actions/upload-pages-artifact",
    "actions/deploy-pages",
    "actions/attest",
    "codecov/",
    "coverallsapp/",
    "peaceiris/actions-gh-pages",
    "jamesives/github-pages-deploy-action",
    "softprops/action-gh-release",
];

/// Actions that read the PR, such as its changed files, which act's run has none of
const PR_AWARE: &[&str] = &[
    "tj-actions/changed-files",
    "dorny/paths-filter",
    "reviewdog/",
    "danger/",
];

/// Commands that read the PR's event or its review themselves, and under act or here find none and may pass
const PR_AWARE_COMMANDS: &[&str] = &["danger", "danger-swift", "danger-kotlin", "reviewdog"];

/// The flags of each launcher that take the next word as their value. pnpm's `-w` is `--workspace-root` and takes none
fn value_flags(launcher: &str) -> &'static [&'static str] {
    match launcher {
        "npm" => &["-w", "--workspace", "--prefix"],
        "pnpm" => &["--filter", "-F", "--dir", "-C"],
        "yarn" => &["--cwd"],
        "bun" => &["--cwd", "--filter", "-F"],
        "npx" | "bunx" | "pnpx" => &["-p", "--package"],
        _ => &[],
    }
}

/// `words` past their leading flags, and the value a flag in `values` takes, such as `--filter web`
fn past_flags<'a>(words: &'a [String], values: &[&str]) -> &'a [String] {
    let mut at = 0;

    while let Some(w) = words.get(at).filter(|w| w.starts_with('-')) {
        at += if values.contains(&w.as_str()) { 2 } else { 1 };
    }

    &words[at.min(words.len())..]
}

/// `words` past a package manager, its flags and `yarn workspace <name>`, so at its subcommand or script
fn past_manager(words: &[String]) -> &[String] {
    let Some(manager) = words.first() else {
        return words;
    };
    let values = value_flags(manager);
    let after = past_flags(&words[1..], values);

    if manager == "yarn" && after.first().is_some_and(|w| w == "workspace") {
        return past_flags(after.get(2..).unwrap_or_default(), values);
    }

    after
}

/// The command `words` runs, past launchers such as `npx`, `yarn dlx`, `pnpm --filter web exec`, `bundle exec` or `go run`
fn launched(words: &[String]) -> &[String] {
    let mut rest = words;

    loop {
        let first = rest.first().map_or_default(String::as_str);
        let values = value_flags(first);
        rest = match first {
            "npx" | "bunx" | "pnpx" => past_flags(&rest[1..], values),
            "yarn" | "pnpm" | "bun" => {
                let after = past_manager(rest);

                if matches!(
                    after.first().map(String::as_str),
                    Some("run" | "dlx" | "exec" | "x")
                ) {
                    past_flags(&after[1..], values)
                } else {
                    after
                }
            }
            "npm" => {
                let after = past_manager(rest);

                if after.first().is_some_and(|w| w == "exec") {
                    past_flags(&after[1..], values)
                } else {
                    return rest;
                }
            }
            "bundle" if rest.get(1).is_some_and(|w| w == "exec") => &rest[2..],
            // `mint run danger/swift danger-swift ci` names the package, then its command
            "mint" if rest.get(1).is_some_and(|w| w == "run") => {
                let after = past_flags(&rest[2..], values);
                if after.len() > 1 && after[0].contains('/') {
                    &after[1..]
                } else {
                    after
                }
            }
            "swift" | "go" if rest.get(1).is_some_and(|w| w == "run") => {
                past_flags(&rest[2..], values)
            }
            _ => return rest,
        };
    }
}

/// The command of `words` that reads the PR's event itself, past a launcher. `danger local` and reviewdog's local reporter read none
fn reads_the_event(words: &[String]) -> Option<String> {
    let run = launched(words);
    let first = run.first()?;
    // `npx danger@11 ci` and `go run github.com/reviewdog/reviewdog/cmd/reviewdog@latest` name a version
    let path = first
        .split_once('@')
        .filter(|(p, _)| !p.is_empty())
        .map_or(first.as_str(), |(p, _)| p);
    let name = path.rsplit('/').next()?;
    let args = &run[1..];
    let local = match name {
        "danger" | "danger-swift" | "danger-kotlin" => args
            .iter()
            .find(|a| !a.starts_with('-'))
            .is_some_and(|a| a == "local"),
        "reviewdog" => {
            args.iter().any(|a| a == "-reporter=local")
                || args
                    .windows(2)
                    .any(|w| w[0] == "-reporter" && w[1] == "local")
        }
        _ => true,
    };

    (PR_AWARE_COMMANDS.contains(&name) && !local).then(|| name.to_string())
}

/**
 * The lines a task runner runs for `words`: the `package.json` script npm, yarn, pnpm or bun names,
 * or the recipe of the make target or just recipe, the first one when none is named
 **/
fn task_lines(dir: &Path, words: &[String]) -> Vec<String> {
    // The same cap as a file a step names
    let read = |name: &str| {
        let file = dir.join(name);
        let small = fs::metadata(&file).is_ok_and(|m| m.is_file() && m.len() < 1 << 20);
        small.then(|| fs::read_to_string(&file).ok()).flatten()
    };
    let named = |rest: &[String]| {
        rest.iter()
            .find(|w| !w.starts_with('-') && !w.contains('='))
            .cloned()
    };

    match words.first().map(String::as_str) {
        Some(manager @ ("npm" | "yarn" | "pnpm" | "bun")) => {
            let values = value_flags(manager);
            let after = past_manager(words);
            let script = match (manager, after.first().map(String::as_str)) {
                ("npm", Some("run" | "run-script")) => past_flags(&after[1..], values).first(),
                ("npm", Some("test" | "start")) => after.first(),
                ("npm", _) => None,
                (_, Some("run")) => past_flags(&after[1..], values).first(),
                _ => after.first(),
            };
            let json: Option<serde_json::Value> =
                read("package.json").and_then(|t| serde_json::from_str(&t).ok());
            script
                .and_then(|s| json?["scripts"][s.as_str()].as_str().map(String::from))
                .into_iter()
                .collect()
        }
        Some("make" | "gmake") => {
            let text = ["GNUmakefile", "makefile", "Makefile"]
                .iter()
                .find_map(|f| read(f))
                .unwrap_or_default();
            recipe(&text, named(&words[1..]).as_deref(), |l| {
                l.starts_with('\t')
            })
        }
        Some("just") => {
            let text = ["justfile", "Justfile", ".justfile"]
                .iter()
                .find_map(|f| read(f))
                .unwrap_or_default();
            recipe(&text, named(&words[1..]).as_deref(), |l| {
                l.starts_with(' ') || l.starts_with('\t')
            })
        }
        _ => Vec::new(),
    }
}

/// The tool that reads the PR's event in the scripts or recipes `words` runs, three task runners deep
fn event_in_tasks(dir: &Path, words: &[String], depth: u8) -> Option<String> {
    if depth == 3 {
        return None;
    }

    task_lines(dir, words)
        .iter()
        .flat_map(|l| started(l))
        .find_map(|w| reads_the_event(&w).or_else(|| event_in_tasks(dir, &w, depth + 1)))
}

/// The body lines of the rule `target` of a Makefile or justfile, the first rule when None
fn recipe(text: &str, target: Option<&str>, body: impl Fn(&str) -> bool) -> Vec<String> {
    let mut lines = text.lines().peekable();
    // A special target such as `.PHONY`, an assignment such as `X := 1`, or just's `set` and `export` heads no rule
    let heads = |l: &str| {
        let assigns = [":=", "::=", "?=", "+=", "!="]
            .iter()
            .any(|a| l.contains(a));
        !body(l)
            && !l.starts_with(['#', '.'])
            && !l.starts_with("set ")
            && !l.starts_with("export ")
            && !assigns
            && l.split_once(':').is_some_and(|(h, _)| !h.contains('='))
    };

    while let Some(line) = lines.next() {
        if !heads(line) {
            continue;
        }

        let names = line.split_once(':').map_or("", |(h, _)| h);
        let hit = target.is_none_or(|t| names.split_whitespace().next() == Some(t));

        if hit {
            let mut out = Vec::new();

            while let Some(l) = lines.next_if(|l| body(l) || l.trim().is_empty()) {
                out.push(l.trim().trim_start_matches(['@', '-']).to_string());
            }

            return out;
        }
    }

    Vec::new()
}

/// Commands that run containers, whose daemon a job gets only on a VM of its own
const CONTAINERS: &[&str] = &[
    "act",
    "buildah",
    "docker",
    "docker-compose",
    "kind",
    "minikube",
    "nerdctl",
    "podman",
];

/// Commands that change the kernel, its namespaces or the machine's services, which a container never may
const KERNEL: &[&str] = &[
    "apparmor_parser",
    "bwrap",
    "chroot",
    "insmod",
    "iptables",
    "losetup",
    "modprobe",
    "mount",
    "nsenter",
    "service",
    "setcap",
    "sysctl",
    "systemctl",
    "unshare",
];

/// Commands that start the command after them, with flags of their own
const WRAPPERS: &[&str] = &["sudo", "env", "time", "exec", "nohup"];

/**
 * The words of each command a `run` starts, wrappers such as `sudo -u x`, `env` and `time` and
 * leading `VAR=value` taken off, and the commands a `bash -c` or `sh -c` string starts
 **/
fn started(run: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();

    for part in run
        .replace("&&", "\n")
        .replace("||", "\n")
        .split(['\n', ';', '|', '&', '(', ')', '`'])
    {
        let words = words(part);
        let mut at = 0;

        while let Some(w) = words.get(at) {
            if WRAPPERS.contains(&w.as_str()) {
                at += 1;

                while let Some(flag) = words.get(at).filter(|f| f.starts_with('-')) {
                    // `sudo -u <user>` and `-g <group>` take the word after them
                    at += if ["-u", "-g"].contains(&flag.as_str()) {
                        2
                    } else {
                        1
                    };
                }
            } else if w.contains('=') && !w.starts_with('-') {
                at += 1;
            } else {
                break;
            }
        }

        let words = words[at.min(words.len())..].to_vec();

        if words.is_empty() {
            continue;
        }

        let shell = ["sh", "bash", "zsh", "dash"].contains(&words[0].as_str());

        match words.iter().position(|w| w == "-c").filter(|_| shell) {
            Some(c) => out.extend(words.get(c + 1).map_or_default(|s| started(s))),
            None => out.push(words),
        }
    }

    out
}

/// A command's name for the lists above: a bare name, or the basename of one in a system directory
fn command_word(word: &str) -> Option<&str> {
    let system = [
        "/bin/",
        "/sbin/",
        "/usr/bin/",
        "/usr/sbin/",
        "/usr/local/bin/",
    ];

    match word.rsplit_once('/') {
        None => Some(word),
        Some((dir, name)) if system.contains(&format!("{dir}/").as_str()) => Some(name),
        Some(_) => None,
    }
}

/// What a job runs that only a VM gives it: a container runtime, or a change to the kernel
fn needs_vm(job: &Job) -> Option<String> {
    if job.runs_docker {
        return Some("docker".into());
    }

    for step in &job.steps {
        if step.uses.as_deref().is_some_and(|u| {
            u.to_lowercase()
                .starts_with("crazy-max/ghaction-setup-docker")
        }) {
            return Some("crazy-max/ghaction-setup-docker".into());
        }

        for words in started(&step.run) {
            let Some(name) = command_word(&words[0]).map(str::to_string) else {
                continue;
            };
            let netns = name == "ip" && words.get(1).is_some_and(|w| w == "netns");

            if CONTAINERS.contains(&name.as_str()) || KERNEL.contains(&name.as_str()) || netns {
                return Some(if netns { "ip netns".into() } else { name });
            }
        }

        if ["/proc/sys/", "/sys/fs/cgroup", "/dev/kvm"]
            .iter()
            .any(|p| step.run.contains(p))
        {
            return Some("the kernel's /proc/sys, cgroups or KVM".into());
        }
    }

    None
}

/**
 * Why a run here, by act or a host check, would see the PR otherwise than GitHub. A file in the
 * repository a step's command names, or the Makefile, justfile or package.json its task runner
 * reads, that reads the event or the refs. Or HEAD in a workflow that runs on PRs, which there is
 * the merge commit, with HEAD^ the base, and here the branch's own commit
 **/
fn reads_the_pr(job: &Job, source: &str, root: &Path) -> Option<String> {
    let on_prs = workflow::runs_on_prs(source);
    // A path that stays in the repository, else None
    let inside = |dir: &Path, word: &str| -> Option<PathBuf> {
        let word = word.strip_prefix("./").unwrap_or(word);
        let ok = !word.is_empty() && !word.starts_with('/') && !word.split('/').any(|p| p == "..");
        ok.then(|| dir.join(word))
    };
    let reads = |file: &Path| {
        let small = fs::metadata(file).is_ok_and(|m| m.is_file() && m.len() < 1 << 20);
        small
            .then(|| fs::read_to_string(file).ok())
            .flatten()
            .and_then(|text| workflow::reads_what_act_lacks(&text))
    };

    for step in &job.steps {
        let head = step
            .run
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|w| w == "HEAD");

        if on_prs && (head || step.run.contains("git log -1")) {
            return Some("it reads HEAD, which on a PR is GitHub's merge commit and HEAD^ its base, and here the branch's own commit".into());
        }

        let Some(mut dir) = inside(root, step.dir.as_deref().unwrap_or(".")) else {
            continue;
        };

        for words in started(&step.run) {
            // The command itself, or a line of the script or recipe its task runner runs
            let tool = reads_the_event(&words).or_else(|| event_in_tasks(&dir, &words, 0));

            if let Some(tool) = tool {
                return Some(format!(
                    "it runs {tool}, which reads the PR's event itself, and only GitHub's run of a PR has it"
                ));
            }

            if words[0] == "cd" {
                match words.get(1).and_then(|d| inside(&dir, d)) {
                    Some(d) => dir = d,
                    None => break,
                }

                continue;
            }

            let runner: &[&str] = match words[0].as_str() {
                "make" | "gmake" => &["Makefile", "makefile", "GNUmakefile"],
                "just" => &["justfile", "Justfile", ".justfile"],
                "npm" | "yarn" | "pnpm" | "bun" | "npx" => &["package.json"],
                _ => &[],
            };
            let named = words
                .iter()
                .filter(|w| !w.starts_with('-'))
                .map(String::as_str)
                .chain(runner.iter().copied());

            for word in named {
                let Some(file) = inside(&dir, word) else {
                    continue;
                };

                if let Some(what) = reads(&file) {
                    let shown = file
                        .strip_prefix(root)
                        .unwrap_or(&file)
                        .display()
                        .to_string();
                    return Some(format!(
                        "its file {shown} reads `{what}`, which only GitHub's run of a PR has"
                    ));
                }
            }
        }
    }

    None
}

/// Why act would run a job otherwise than GitHub does: what it reads, reports or publishes
fn act_differs(job: &Job, source: &str, root: &Path) -> Option<String> {
    if let Some(why) = workflow::act_lacks_context(source, &job.id) {
        return Some(why);
    }

    let reports = job
        .steps
        .iter()
        .filter_map(|s| s.uses.as_deref())
        .find(|u| {
            let lower = u.to_lowercase();
            REPORTERS.iter().any(|r| lower.starts_with(r))
                || ships(u.split('@').next().unwrap_or(u), &['-', '_', '/'])
        });

    if let Some(u) = reports {
        return Some(format!("it reports to GitHub or publishes with {u}"));
    }

    let pr_aware = job.steps.iter().find(|s| {
        let lower = s.uses.as_deref().unwrap_or_default().to_lowercase();
        let only_new = lower.starts_with("golangci/golangci-lint-action")
            && s.with
                .iter()
                .any(|(k, v)| k == "only-new-issues" && v.as_deref() != Some("false"));
        PR_AWARE.iter().any(|p| lower.starts_with(p)) || only_new
    });

    if let Some(s) = pr_aware {
        return Some(format!(
            "{} reads the PR, which act's run has none of",
            s.uses.as_deref().unwrap_or_default()
        ));
    }

    reads_the_pr(job, source, root)
}

/**
 * Why act cannot run a job here as GitHub does, or the engine it would use. A job off Linux, on a
 * label act has no image for, one whose `if:` or values read the PR, one that reports to GitHub, or
 * one that needs a VM, since colima reaches this machine's loopback and is for its owner to choose
 **/
fn act_engine(job: &Job, machine: &Machine, source: &str, root: &Path) -> Result<Engine, String> {
    // act checks Docker before any job, a colima job's too
    if !machine.docker {
        return Err("Docker is not running".into());
    }

    let labels = act::platforms(&Act::default());

    if job.runs_on.is_empty() || job.runs_on.iter().any(|l| l.contains("${{")) {
        return Err("its runs-on is not a label act has an image for".into());
    }

    if let Some(l) = job.runs_on.iter().find(|l| !labels.contains_key(*l)) {
        return Err(format!("act has no image for its label {l}"));
    }

    if let Some(why) = act_differs(job, source, root) {
        return Err(why);
    }

    if let Some(what) = needs_vm(job) {
        let vm = if machine.colima {
            "colima could run it, but colima reaches this machine's loopback, so it is opt-in"
        } else {
            "only a VM runs it"
        };
        return Err(format!("it runs {what}, and {vm}"));
    }

    Ok(Engine::Docker)
}

/**
 * Moves each job kept on GitHub that act can run here as GitHub does, with every job it needs, to
 * `acts`. act runs the jobs it needs too. A job kept because a job left on GitHub needs it moves
 * only with that job. A job act cannot run keeps its reason, with why act cannot either
 **/
pub fn place(root: &Path, machine: &Machine, d: &mut Detected) {
    type File = (String, Vec<Job>, OnceCell<Result<(), String>>);
    let mut files: BTreeMap<String, File> = BTreeMap::new();

    for k in &d.github {
        files.entry(k.file.clone()).or_insert_with(|| {
            let source = fs::read_to_string(root.join(".github/workflows").join(&k.file))
                .unwrap_or_default();
            let jobs = workflow::job_runs(&source);
            (source, jobs, OnceCell::default())
        });
    }

    let engine = |k: &Kept| -> Result<Engine, String> {
        let (source, jobs, parsed) = files.get(&k.file).ok_or("no workflow")?;
        let engine = engine_of(root, source, jobs, &k.job, machine)?;
        parsed.get_or_init(|| act_parses(root, &k.file)).clone()?;
        Ok(engine)
    };
    let kept = std::mem::take(&mut d.github);
    let (needs, own): (Vec<Kept>, Vec<Kept>) =
        kept.into_iter().partition(|k| needed_by(k).is_some());

    for k in own {
        match engine(&k) {
            Ok(e) => d.acts.push((k, e)),
            // Docker not running changes nothing, what stays says why as before
            Err(why) if !machine.docker => {
                let _ = why;
                d.github.push(k);
            }
            // A reason that already names act needs no second one
            Err(_) if k.why.contains("act cannot run") => d.github.push(k),
            Err(why) => d.github.push(Kept {
                act: Some(why),
                ..k
            }),
        }
    }

    for k in needs {
        let moved = needed_by(&k)
            .is_some_and(|by| d.acts.iter().any(|(a, _)| a.file == k.file && a.job == by));

        match engine(&k) {
            Ok(e) if moved => d.acts.push((k, e)),
            _ => d.github.push(k),
        }
    }
}

/// The engine act runs a job and every job it needs on here as GitHub does, or why it cannot
fn engine_of(
    root: &Path,
    source: &str,
    jobs: &[Job],
    id: &str,
    machine: &Machine,
) -> Result<Engine, String> {
    let job = jobs.iter().find(|j| j.id == id).ok_or("no job")?;
    let mut before = Vec::new();
    needed(jobs, &job.needs, &mut before, &mut vec![job.id.clone()]);

    for n in &before {
        let need = jobs.iter().find(|j| j.id == *n).ok_or("no job")?;
        act_engine(need, machine, source, root)
            .map_err(|why| format!("job {n} it needs: {why}"))?;
    }

    act_engine(job, machine, source, root)
}

/// `engine_of` for a job of a workflow file in `root`
pub fn act_engine_for(
    root: &Path,
    machine: &Machine,
    file: &str,
    id: &str,
) -> Result<Engine, String> {
    let source =
        fs::read_to_string(root.join(".github/workflows").join(file)).map_err(|e| e.to_string())?;
    let engine = engine_of(root, &source, &workflow::job_runs(&source), id, machine)?;
    act_parses(root, file)?;
    Ok(engine)
}

/**
 * Whether act parses a workflow of `root`, as every run of it would first. act checks a workflow
 * against its own schema, which can lag GitHub's, and fails every run of one it rejects
 **/
#[cfg(not(test))]
pub fn act_parses(root: &Path, file: &str) -> Result<(), String> {
    let out = Command::new("act")
        .arg("--list")
        .arg("-W")
        .arg(root.join(".github/workflows").join(file))
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("act did not start: {e}"))?;

    if out.status.success() {
        return Ok(());
    }

    let err = String::from_utf8_lossy(&out.stderr);
    let why = err
        .lines()
        .find_map(|l| l.strip_prefix("Error: "))
        .unwrap_or("act --list failed");
    Err(format!("act cannot parse its workflow: {why}"))
}

// Unit tests start no act, tests/act_parse_e2e.rs runs the real one
#[cfg(test)]
pub fn act_parses(_: &Path, _: &str) -> Result<(), String> {
    Ok(())
}

/// The job a job was kept on GitHub for, since that job stays there and needs it
fn needed_by(k: &Kept) -> Option<&str> {
    k.why
        .strip_prefix("job ")?
        .strip_suffix(" stays on GitHub and needs it")
}

/// The workflow files, sorted, or None without `.github/workflows`
pub fn workflow_files(root: &Path) -> Option<Vec<String>> {
    let mut files: Vec<String> = fs::read_dir(root.join(".github/workflows"))
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| {
            (std::path::Path::new(n)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("yml"))
                || std::path::Path::new(n)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml")))
                && !n.contains(char::is_control)
        })
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
fn runs_something(job: &Job) -> bool {
    job.steps.iter().any(|s| !s.run.is_empty())
}

const DOCKER: &str = "runs Docker, which shipcheck never gives a job";

const BASE_ONLY: &str = "runs only on the base branch, so it is no PR check and stays as it is";

/// The jobs of a file GitHub runs only on the base branch: an `if:` that requires it, or a need that does
pub fn base_only(jobs: &[Job]) -> BTreeSet<String> {
    let mut only: BTreeSet<String> = jobs
        .iter()
        .filter(|j| j.base_only)
        .map(|j| j.id.clone())
        .collect();

    // GitHub skips a job whose need it skipped, so the set grows until no job joins it
    loop {
        let more: Vec<String> = jobs
            .iter()
            .filter(|j| !only.contains(&j.id) && j.needs.iter().any(|n| only.contains(n)))
            .map(|j| j.id.clone())
            .collect();

        if more.is_empty() {
            return only;
        }

        only.extend(more);
    }
}

const PR_ONLY: &str = "its `if:` reads the PR, such as its labels, or the default branch, so only GitHub can tell if it runs";

/// Whether any job of the workflows runs on macOS or Windows
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Jobs {
    AllOnLinux,
    SomeOffLinux,
}

/// Why a job stays on GitHub for PRs in host mode, or None when shipcheck may run it here
fn stays(job: &Job, machine: &Machine, jobs: Jobs) -> Option<String> {
    let os = job.os;

    if job.pr_only {
        return Some(PR_ONLY.into());
    }

    if job.runs_docker {
        return Some(DOCKER.into());
    }

    if let Some(what) = needs_vm(job) {
        return Some(format!(
            "runs {what}, which only a VM of its own gives a job"
        ));
    }

    // Host mode runs on this machine, not in the job's image
    if let Some(image) = &job.container {
        return Some(format!("runs in container {image}"));
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
            // act would run it, but not beside the other jobs, which stay
            _ if jobs == Jobs::SomeOffLinux => {
                return Some(format!(
                    "runs a matrix ({axis}), and host mode runs only this machine's toolchain"
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

    // A job with `if:` is left out anyway, and says so
    if !job.conditional
        && let Err(why) = java::pick(job, &machine.jdks)
    {
        return Some(why);
    }

    // A missing Xcode keeps the job here, a different one only changes what the check runs with
    let needs_xcode = match xcode(job) {
        Some(Err(why)) => return Some(why),
        Some(Ok(_)) => true,
        None => job.steps.iter().any(|s| s.run.contains("xcodebuild")),
    };

    if needs_xcode && machine.xcodes.is_empty() {
        return Some("needs Xcode, which this machine does not have".into());
    }

    None
}

fn sets_up_xcode(step: &RunStep) -> bool {
    step.uses
        .as_deref()
        .is_some_and(|u| u.split('@').next() == Some("maxim-lobanov/setup-xcode"))
}

/// A line split into words as a shell does, quote marks removed and what they hold kept
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;

    for c in line.chars() {
        match (quote, c) {
            _ if escaped => {
                word.push(c);
                escaped = false;
            }
            (Some('"') | None, '\\') => escaped = true,
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, _) if c.is_whitespace() => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(c),
        }
    }

    if !word.is_empty() {
        out.push(word);
    }

    out
}

/// The command a line's words start, after `sudo` and its flags
fn command_name(words: &[String]) -> Option<&str> {
    let mut sudo = false;
    words
        .iter()
        .find(|w| {
            let skip = *w == "sudo" || sudo && w.starts_with('-');
            sudo |= *w == "sudo";
            !skip
        })
        .map(String::as_str)
}

/// The path `xcode-select -s`, `--switch` or `--switch=` sets, in a line's words
fn xcode_select(words: &[String]) -> Option<&str> {
    let at = words.iter().position(|w| w == "xcode-select")?;

    for (i, w) in words.iter().enumerate().skip(at + 1) {
        if ["&&", "||", ";", "|"].contains(&w.as_str()) {
            break;
        }

        if let Some(path) = w.strip_prefix("--switch=") {
            return Some(path);
        }

        if ["-s", "--switch", "-switch"].contains(&w.as_str()) {
            return words.get(i + 1).map(String::as_str);
        }
    }

    None
}

/**
 * The Xcode app a job picks with `DEVELOPER_DIR`, `xcode-select -s` or `setup-xcode`, such as
 * `/Applications/Xcode_26.6.app`, empty for any Xcode. Err, why it stays, for a pick init cannot read
 **/
fn xcode(job: &Job) -> Option<Result<String, String>> {
    let expression = || Some(Err("picks its Xcode with an expression".to_string()));
    // None for a path that is not an Xcode, such as the Command Line Tools
    let pick = |path: &str| {
        if path.contains('$') {
            return expression();
        }

        path.find(".app").map(|i| Ok(path[..i + 4].to_string()))
    };
    let from_env = |env: &[(String, Option<String>)]| {
        env.iter()
            .find(|(k, _)| k == "DEVELOPER_DIR")
            .and_then(|(_, v)| v.as_deref().map_or_else(expression, pick))
    };
    let mut picks: Vec<Result<String, String>> = from_env(&job.env).into_iter().collect();

    for step in &job.steps {
        picks.extend(from_env(&step.env));

        for line in step.run.lines() {
            let words = words(line);
            // `export DEVELOPER_DIR=` and `echo "DEVELOPER_DIR=..." >> $GITHUB_ENV` pick one too
            let mut set = words
                .iter()
                .filter_map(|w| w.strip_prefix("DEVELOPER_DIR="))
                .chain(xcode_select(&words));
            picks.extend(set.find_map(pick));
        }

        if sets_up_xcode(step) {
            picks.push(match input(step, "xcode-version") {
                Some(v) if v.starts_with("latest") => Ok(String::new()),
                Some(v) if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.') => {
                    Ok(format!("/Applications/Xcode_{v}.app"))
                }
                _ => Err("picks its Xcode with a version init cannot match".into()),
            });
        }
    }

    // One check has one DEVELOPER_DIR, so a job that switches Xcode between steps stays whole
    let first = picks.first()?.clone();

    if first.is_ok() && picks.iter().any(|p| *p != first) {
        return Some(Err("picks more than one Xcode".into()));
    }

    Some(first)
}

/// A `run` whose every line only picks an Xcode, `|| true` or not, or hands `DEVELOPER_DIR` to later steps
fn picks_xcode_only(run: &str) -> bool {
    let picks = |line: &str| {
        let words = words(line);
        let to_env = words.len() == 4
            && words[0] == "echo"
            && words[1].starts_with("DEVELOPER_DIR=")
            && words[2] == ">>"
            && words[3] == "$GITHUB_ENV";
        to_env
            || without_xcode_pick(line.trim().trim_end_matches("|| true").trim())
                .is_ok_and(|l| l.is_empty())
    };
    let mut lines = run.lines().filter(|l| !l.trim().is_empty()).peekable();
    lines.peek().is_some() && lines.all(picks)
}

/// A job's or step's `DEVELOPER_DIR` pick, which becomes the check's own, or none when this machine lacks it
fn picked(name: &str, value: Option<&String>) -> bool {
    name == "DEVELOPER_DIR" && value.is_none_or(|v| v.contains(".app"))
}

/**
 * A line without the parts that pick an Xcode, `xcode-select -s` and `export DEVELOPER_DIR=`,
 * which `xcode` makes the check's `DEVELOPER_DIR`. Err when `&&` does not set them apart
 **/
fn without_xcode_pick(line: &str) -> Result<String, String> {
    if !line.contains("xcode-select") && !line.contains("DEVELOPER_DIR=") {
        return Ok(line.to_string());
    }

    // A pick joined by `||`, `|` or `&` is left with the line, which then says so
    let picks = |part: &str| {
        let words = words(part);
        !unquoted(part).contains(['|', '&']) && command_name(&words) == Some("xcode-select")
            || words.len() == 2 && words[0] == "export" && words[1].starts_with("DEVELOPER_DIR=")
    };
    let kept: Vec<&str> = line
        .split("&&")
        .map(str::trim)
        .filter(|p| !picks(p))
        .collect();

    if kept.iter().any(|p| p.contains("xcode-select")) {
        return Err("a step that runs xcode-select beside other commands".into());
    }

    Ok(kept.join(" && "))
}

/**
 * As `detect`, for a machine with or without Docker and macOS. A job with a matrix makes it act
 * when act can run every job, since host mode tests only this machine's toolchain. Otherwise such
 * jobs, and jobs on a platform this machine cannot run, stay on GitHub
 **/
#[expect(
    clippy::too_many_lines,
    reason = "one rule per kind of job, read as one table"
)]
pub fn detect_on(root: &Path, machine: &Machine) -> Detected {
    let mut d = Detected::default();
    let ruby_file = fs::read_to_string(root.join(".ruby-version")).ok();
    let mut files = Vec::new();
    let mut sources: BTreeMap<String, String> = BTreeMap::new();

    for file in workflow_files(root).unwrap_or_default() {
        let source =
            fs::read_to_string(root.join(".github/workflows").join(&file)).unwrap_or_default();

        if !workflow::runs_for_branches(&source) {
            d.skipped.push(if workflow::on_pull_request_target(&source) {
                format!("{file}: it runs on pull_request_target, from the base branch with its secrets, so it stays on GitHub")
            } else {
                format!("{file}: GitHub does not run it for feature branches")
            });
            continue;
        }

        files.push((file.clone(), workflow::job_runs(&source)));
        sources.insert(file, source);
    }

    let all = || {
        files
            .iter()
            .flat_map(|(f, jobs)| jobs.iter().map(move |j| (f, j)))
    };
    let platforms = if all().any(|(_, j)| j.os.mac || j.os.windows) {
        Jobs::SomeOffLinux
    } else {
        Jobs::AllOnLinux
    };
    let matrix =
        all().find(|(_, j)| j.matrix.is_some() && runs_something(j) && !ships(&j.id, &['-', '_']));
    // act runs every job of a listed workflow, and a job that runs Docker fails there every time
    let docker_job = all().any(|(_, j)| j.runs_docker);

    let in_act = |_: &_| {
        machine.docker
            && platforms == Jobs::AllOnLinux
            && !docker_job
            && files.iter().all(|(f, _)| act_parses(root, f).is_ok())
    };

    if let Some((file, job)) = matrix.filter(in_act) {
        d.act = Some(format!(
            "{file}, job {} runs a matrix ({}): host mode tests only this machine's toolchain, so this repository runs in act",
            job.id.escape_debug(),
            job.matrix.as_deref().unwrap_or_default()
        ));

        // act runs a listed workflow whole, so one job act would run otherwise keeps its workflow as it is on GitHub
        for (file, jobs) in &files {
            for j in jobs {
                let why = if j.pr_only {
                    Some(PR_ONLY.to_string())
                } else {
                    act_differs(j, &sources[file], root)
                };

                if let Some(why) = why {
                    d.github.push(Kept {
                        file: file.clone(),
                        job: j.id.escape_debug().to_string(),
                        why,
                        checks: j.check_names.clone(),
                        act: None,
                    });
                }
            }
        }

        return d;
    }

    for (file, jobs) in &files {
        let only = base_only(jobs);
        let mut kept: Vec<(String, String)> = jobs
            .iter()
            .filter(|j| !only.contains(&j.id))
            .filter_map(|j| {
                // A check without one of its steps would test less than the job, so it stays whole
                let why = stays(j, machine, platforms)
                    .or_else(|| reads_the_pr(j, &sources[file], root))
                    .or_else(|| {
                        dropped_step(j).map(|s| format!("a check would leave out its {s}"))
                    })?;
                Some((j.id.clone(), why))
            })
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
                act: None,
            });
        }

        let stays_here = |id: &str| kept.iter().any(|(k, _)| k == id);
        let own: Vec<(&str, Option<JobCommand>)> = jobs
            .iter()
            .map(|job| {
                if stays_here(&job.id) {
                    return (job.id.as_str(), None);
                }

                if only.contains(&job.id) {
                    d.skipped.push(format!(
                        "{file}, job {}: {BASE_ONLY}",
                        job.id.escape_debug()
                    ));
                    return (job.id.as_str(), None);
                }

                match job_check(job, machine, ruby_file.as_deref()) {
                    Ok((command, exports, notes)) => {
                        for n in notes {
                            match n.strip_prefix(WARNING) {
                                Some(w) => d.warnings.push(format!("{file}, job {}: {w}", job.id)),
                                None => d.skipped.push(format!("{file}, job {}: {n}", job.id)),
                            }
                        }

                        (job.id.as_str(), Some((command, exports)))
                    }
                    // A toolchain or target this machine lacks, act sets up as GitHub does
                    Err(why) if machine.docker && why.starts_with("needs ") => {
                        d.github.push(Kept {
                            file: file.clone(),
                            job: job.id.escape_debug().to_string(),
                            why,
                            checks: job.check_names.clone(),
                            act: None,
                        });
                        (job.id.as_str(), None)
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

    place(root, machine, &mut d);
    let (project, versions) = project_checks(root);
    d.versions = versions;
    // A job that stays on GitHub is CI's check of what project files would guess
    let from_workflows = !d.checks.is_empty() || !d.github.is_empty();

    for (name, tool, command, from) in project {
        if from_workflows {
            let runs_it =
                all().find(|(_, j)| j.steps.iter().any(|s| runs_command(&s.run, &command)));
            let why = if d.checks.iter().any(|c| runs_command(&c.command, &command)) {
                "already in a workflow check".to_string()
            } else if let Some((file, job)) = runs_it {
                let on_act = d
                    .acts
                    .iter()
                    .any(|(k, _)| k.file == *file && k.job == job.id);
                let how = if on_act { "with act" } else { "on GitHub" };
                format!("{file}, job {} runs it {how}", job.id.escape_debug())
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
    d.missing = missing_tools(&d.checks, on_path);

    // A pinned toolchain's own version, since `rustc --version` names the default one
    for check in &d.checks {
        for word in check.command.split_whitespace() {
            let version = if let Some(t) = word.strip_prefix("RUSTUP_TOOLCHAIN=") {
                format!("rustc +{t} --version")
            } else if let Some(v) = word.strip_prefix("UV_PYTHON=") {
                // uv downloads a missing one on first use, which then changes the key once
                format!("uv python find {v} --show-version || echo missing")
            } else if let Some(root) = word.strip_prefix("RUBY_ROOT=") {
                format!("{root}/bin/ruby --version")
            } else if let Some(home) = word.strip_prefix("JAVA_HOME=") {
                // It prints to stderr, and `versions` keep stdout
                format!("{home}/bin/java -version 2>&1")
            } else {
                continue;
            };

            if !d.versions.contains(&version) {
                d.versions.push(version);
            }
        }
    }

    // The Xcode a check's swift or xcodebuild runs with, so an upgrade runs the checks again
    for check in d
        .checks
        .iter()
        .filter(|_| machine.mac && !machine.xcodes.is_empty())
    {
        if !tools_in(&check.command)
            .iter()
            .any(|t| ["xcodebuild", "swift", "xcrun"].contains(&t.as_str()))
        {
            continue;
        }

        let version = match assigned(&check.command, "DEVELOPER_DIR") {
            Some(dir) => format!("{dir} xcodebuild -version"),
            None => "xcodebuild -version".to_string(),
        };

        if !d.versions.contains(&version) {
            d.versions.push(version);
        }
    }

    // The Python uv picks, which a check without `UV_PYTHON` runs on and which this machine can change
    let unpinned = d.checks.iter().any(|c| {
        c.command
            .split_whitespace()
            .any(|w| w == "uv" || w == "uvx")
            && !c.command.contains("UV_PYTHON=")
    });
    let found = "uv python find --show-version || echo missing".to_string();

    if unpinned && !d.versions.contains(&found) {
        d.versions.push(found);
    }

    // The Ruby on the `PATH`, which a check without `RUBY_ROOT` runs on
    let unpinned = d.checks.iter().any(|c| {
        c.command
            .split_whitespace()
            .any(|w| ["ruby", "bundle", "gem", "rake"].contains(&w))
            && !c.command.contains("RUBY_ROOT=")
    });
    let found = "ruby --version".to_string();

    if unpinned && !d.versions.contains(&found) {
        d.versions.push(found);
    }

    d
}

/**
 * The literal `python-version` of the job's `setup-python` step, such as `3.13` or `3.14t`, when a
 * step runs uv, which takes it from `UV_PYTHON`. None for `3.x`, a list, `PyPy` or an expression
 **/
fn setup_python(job: &Job) -> Option<String> {
    let runs_uv = job.steps.iter().any(|s| {
        s.run
            .split(|c: char| c.is_whitespace() || c == ';' || c == '&' || c == '|')
            .any(|w| w == "uv" || w == "uvx")
    });
    let version = job
        .steps
        .iter()
        .filter(|s| {
            s.uses
                .as_deref()
                .and_then(|u| u.split('@').next())
                .is_some_and(|a| a == "actions/setup-python")
        })
        .find_map(|s| input(s, "python-version"))?;
    let number = version.strip_suffix('t').unwrap_or(&version);
    let plain = !number.is_empty()
        && number.split('.').count() <= 3
        && number
            .split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));

    (runs_uv && plain).then_some(version)
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
        .filter_map(|v| v.split_whitespace().find(|w| !w.contains('=')))
        .collect();
    let mut tools: Vec<String> = Vec::new();

    for tool in checks.iter().flat_map(|c| tools_in(&c.command)) {
        if !covered.contains(&tool.as_str()) && !tools.contains(&tool) {
            tools.push(tool);
        }
    }

    tools
        .into_iter()
        .filter(|t| answers(t))
        .map(|t| format!("{t} --version"))
        .collect()
}

/// `<check> calls <tool>` for each tool a check starts that `installed` does not find
fn missing_tools(checks: &[Found], installed: fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();

    for check in checks {
        // What the check runs after an install or a venv's `activate` can come from it
        let command = &check.command;
        let cut = [" install", "activate"]
            .iter()
            .filter_map(|w| command.find(w))
            .min()
            .unwrap_or(command.len());

        for tool in tools_in(&command[..cut]) {
            let line = format!("{} calls {tool}", check.name);

            if !installed(&tool) && !out.contains(&line) {
                out.push(line);
            }
        }
    }

    out
}

/// Shell builtins that `PLAIN` and `STATEFUL` do not name, which no PATH holds
const BUILTINS: &[&str] = &[
    "eval", "exec", "read", "type", "wait", "trap", "shift", "local", "declare", "return", "hash",
];

/// The tools a command starts, each once, without builtins, coreutils and setup tools
fn tools_in(command: &str) -> Vec<String> {
    let words = command.replace(['(', ')', '{', '}', ';', '|', '&'], "\n");
    let mut tools: Vec<String> = Vec::new();

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
        let named = tool.starts_with(|c: char| c.is_ascii_alphanumeric())
            && tool
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c));

        if named
            && !PLAIN.contains(&tool)
            && !STATEFUL.contains(&tool)
            && !BUILTINS.contains(&tool)
            && !tool.starts_with("mkfs")
            && !tools.iter().any(|t| t == tool)
        {
            tools.push(tool.to_string());
        }
    }

    tools
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

/// Whether an executable file of that name is in a directory of `PATH`
#[cfg(not(test))]
fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| {
            fs::metadata(d.join(tool))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

#[cfg(test)]
fn on_path(_: &str) -> bool {
    true
}

/// The jobs `needs` names, and the jobs those need, each once, every job after the ones it needs
fn needed(jobs: &[Job], needs: &[String], out: &mut Vec<String>, path: &mut Vec<String>) {
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

/**
 * Whether one command between `&&`, `|` or `;` sets the machine up rather than checks the code, by
 * its first words, `sudo` taken off: a package or tool install, `git config` or `rustup`. Package
 * installs into the checkout stay, since a warm clone's `node_modules` can be older than the lockfile
 **/
fn sets_up(words: &[&str]) -> bool {
    let words = match words {
        ["sudo", rest @ ..] => {
            let flags = rest.iter().take_while(|w| w.starts_with('-')).count();
            &rest[flags..]
        }
        _ => words,
    };
    let package = [
        "install",
        "update",
        "upgrade",
        "purge",
        "remove",
        "autoremove",
    ];

    match words {
        ["pip" | "pip3" | "go" | "cargo", "install", ..]
        | ["python" | "python3", "-m", "pip", "install", ..]
        | ["npm" | "git", "config", ..]
        | ["rustup" | "which", ..]
        | [
            "brew",
            "install" | "update" | "upgrade" | "tap" | "bundle",
            ..,
        ]
        | ["command", "-v", ..] => true,
        ["apt" | "apt-get" | "dnf" | "yum", rest @ ..] => rest.iter().any(|w| package.contains(w)),
        _ => false,
    }
}

/// A line's commands between `&&`, `||`, `|`, `;` and parentheses, each as its words
fn line_commands(line: &str) -> Vec<Vec<&str>> {
    line.split(|c: char| "&|;()".contains(c))
        .map(|c| c.split_whitespace().collect::<Vec<_>>())
        .filter(|c| !c.is_empty())
        .collect()
}

/// Whether a line only sets the machine up: installs, or a `curl` or `wget` piped into a shell
fn setup_line(line: &str) -> bool {
    let commands = line_commands(line);
    let shell = |c: &Vec<&str>| {
        matches!(
            c.as_slice(),
            ["sh" | "bash", ..] | ["sudo", "sh" | "bash", ..]
        )
    };
    let piped = matches!(commands.first().map(|c| c[0]), Some("curl" | "wget"))
        && commands.len() > 1
        && commands[1..].iter().all(shell);

    piped || commands.iter().all(|c| sets_up(c))
}

/// Whether a line sends its output to a file, such as `echo x >> gradle.properties`, which the build may read
fn writes_a_file(line: &str) -> bool {
    line.split('>')
        .skip(1)
        .map(str::trim_start)
        .filter(|rest| !rest.is_empty() && !rest.starts_with('&'))
        .any(|rest| rest.split_whitespace().next() != Some("/dev/null"))
}

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
    "printf", "tar", "unzip", "install", "git", "sleep", "true", "pwd", "which", "sed",
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
    unquoted(command)
        .split(['&', '|', ';', '(', ')', '{', '}'])
        .all(|c| {
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

/// Commands that need root, which a check never has, as a command's first word
const ROOT: &[&str] = &[
    "sudo", "mount", "umount", "losetup", "apt", "apt-get", "dnf", "yum", "modprobe",
];

fn needs_root(line: &str) -> bool {
    line_commands(line)
        .iter()
        .any(|c| ROOT.contains(&c[0]) || c[0].starts_with("mkfs"))
}

/// Commands on a runner that reach the network or GitHub, which a check cannot, as a command's first word
const RUNNER_ONLY: &[&str] = &["curl", "wget", "gh", "brew"];

/// Variables the sandbox sets for each check, which a workflow's value would point elsewhere
const SANDBOX_VARS: &[&str] = &["TMPDIR", "HOME"];

const CONTROL: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "select", "{", "}", "exit", "return",
];

/// What separates commands, unless a quote or a `\` makes it literal
const SEPARATORS: &str = "&|;(){}";

/**
 * A line without quote marks and without the separators that quotes or a `\` make literal, so a
 * quoted `;` is not a command separator while a quoted word stays. `$'...'` takes `\'` inside it
 **/
fn unquoted(line: &str) -> String {
    let mut out = String::new();
    // `'`, `"`, or `$` for `$'...'`, where `\` escapes as it does in `"`
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if escaped {
            escaped = false;

            if !SEPARATORS.contains(c) {
                out.push(c);
            }

            continue;
        }

        match (quote, c) {
            (None | Some('"' | '$'), '\\') => escaped = true,
            (None, '$') if chars.peek() == Some(&'\'') => {
                chars.next();
                quote = Some('$');
            }
            (None, '\'' | '"') => quote = Some(c),
            (Some('$'), '\'') => quote = None,
            (Some(q), _) if q == c => quote = None,
            (Some(_), _) if SEPARATORS.contains(c) => {}
            _ => out.push(c),
        }
    }

    out
}

/// The `NAME=value` a command exports, as `assignment` wrote it
fn assigned<'a>(command: &'a str, name: &str) -> Option<&'a str> {
    let rest = &command[command.find(&format!("{name}="))?..];
    let value = &rest[name.len() + 1..];
    let end = match value.strip_prefix('\'') {
        Some(quoted) => name.len() + 3 + quoted.find('\'')?,
        None => rest.find(char::is_whitespace).unwrap_or(rest.len()),
    };
    Some(&rest[..end])
}

/**
 * One step as a command: its lines that are not blank, comments or setup, joined with ` && `,
 * which is how GitHub's `bash -e` stops at the first failure. A line with `|` or `&` is grouped
 * so `&&` cannot bind into it. Ok(None) for nothing to run, Err(fatal, why) to leave it out
 **/
fn step_command(step: &RunStep) -> Result<Option<String>, (bool, String)> {
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

        if unquoted(line).contains(';') {
            return leave("a step with `;`, which GitHub's `bash -e` stops at");
        }

        // The job's Xcode becomes its `DEVELOPER_DIR`, rather than a switch for the whole machine
        let line = &match without_xcode_pick(line) {
            Ok(line) => line,
            Err(why) => return leave(&why),
        };

        if line.is_empty() {
            continue;
        }

        if setup_line(line) {
            continue;
        }

        // `echo` only prints, unless it writes a file the build reads
        if first == "echo" && !writes_a_file(line) {
            continue;
        }

        if needs_root(line) {
            return leave("a step that needs root, with `sudo`, `mount`, `mkfs` or `apt`");
        }

        if line_commands(line)
            .iter()
            .any(|c| RUNNER_ONLY.contains(&c[0]))
        {
            return leave("a step with `curl`, `wget`, `gh` or `brew` that is not an install");
        }

        if ships(line, &[]) {
            return Err((true, "a step deploys, publishes or releases".into()));
        }

        lines.push(line.clone());
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
        [one] => Some(one.clone()),
        many => Some(
            many.iter()
                .map(|l| {
                    if l.contains(['|', '&']) {
                        format!("{{ {l}; }}")
                    } else {
                        l.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(" && "),
        ),
    })
}

/**
 * The commands of a job that check something, each with its step's directory, for `init` to
 * compare with the checks of a shipcheck.toml. Steps a check could not run are left out
 **/
pub fn job_commands(job: &Job) -> Vec<(Option<String>, String)> {
    let Ok((parts, _, _)) = steps_of(job) else {
        return Vec::new();
    };

    parts
        .into_iter()
        .flat_map(|(dir, command)| {
            commands(&command)
                .into_iter()
                .map(move |c| (dir.clone(), c))
        })
        .collect()
}

/**
 * The first step of a job that its check would leave out, named, and why: a step that is not a
 * command, or an action that is not checkout, a cache, an artifact or a setup. Without that step
 * the check would test less than the job does, so it does not stand for the job
 **/
pub fn dropped_step(job: &Job) -> Option<String> {
    let (_, dropped, _) = steps_of(job).ok()?;
    let (step, why) = dropped.into_iter().next()?;

    Some(format!("step {step}, {why}"))
}

/// Actions a check can do without, since they fetch the tree, cache, move artifacts or install a tool the machine has
const KNOWN_ACTIONS: &[&str] = &[
    "actions/checkout",
    "actions/cache",
    "actions/cache/restore",
    "actions/cache/save",
    "actions/upload-artifact",
    "actions/download-artifact",
    "dtolnay/rust-toolchain",
    "actions-rs/toolchain",
    "swatinem/rust-cache",
    "mozilla-actions/sccache-action",
    "taiki-e/install-action",
    "jdx/mise-action",
    "pnpm/action-setup",
    "pnpm/setup",
];

/// Owners whose `setup-*` actions only install a toolchain, such as `gradle/actions/setup-gradle`
const SETUP_OWNERS: &[&str] = &[
    "actions",
    "actions-rust-lang",
    "astral-sh",
    "denoland",
    "erlef",
    "goto-bus-stop",
    "gradle",
    "graalvm",
    "haskell-actions",
    "homebrew",
    "julia-actions",
    "oven-sh",
    "ruby",
    "shivammathur",
];

fn drops_nothing(uses: &str) -> bool {
    let action = uses
        .split('@')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let owner = action.split('/').next().unwrap_or_default();
    let last = action.rsplit('/').next().unwrap_or_default();

    KNOWN_ACTIONS.contains(&action.as_str())
        || (action.contains('/') && SETUP_OWNERS.contains(&owner) && last.starts_with("setup-"))
}

/**
 * A job's steps as commands, each with its directory, and the steps left out as their name and
 * why. Err for a step that makes the whole job no check, such as a deploy
 **/
#[expect(
    clippy::type_complexity,
    reason = "a tuple read once by its only caller"
)]
fn steps_of(
    job: &Job,
) -> Result<
    (
        Vec<(Option<String>, String)>,
        Vec<(String, String)>,
        Vec<String>,
    ),
    String,
> {
    let mut parts = Vec::new();
    let mut dropped = Vec::new();
    let mut notes = Vec::new();
    // A pick init reads becomes the check's `DEVELOPER_DIR`, so its step is done, not dropped
    let xcode_read = matches!(xcode(job), Some(Ok(_)));

    for step in &job.steps {
        if xcode_read && (sets_up_xcode(step) || picks_xcode_only(&step.run)) {
            continue;
        }

        let label = step
            .name
            .clone()
            .or_else(|| step.uses.clone())
            .or_else(|| step.run.lines().next().map(str::to_string))
            .unwrap_or_default();
        let label = format!("`{}`", label.trim());

        // A setup action that installs becomes its install command
        let mut step = if !step.run.is_empty() {
            step.clone()
        } else if let Some(run) = action_install(step) {
            RunStep {
                run,
                dir: input(step, "working-directory"),
                shell: None,
                // An action's `env` is for the action, not for the command that replaces it
                env: Vec::new(),
                ..step.clone()
            }
        } else if step.uses.as_deref().is_some_and(drops_nothing) {
            continue;
        } else {
            dropped.push((
                label,
                "a step that uses an action no command stands for".into(),
            ));
            continue;
        };

        step.env
            .retain(|(name, value)| !picked(name, value.as_ref()));
        step.env.retain(|(name, _)| {
            let sandbox = SANDBOX_VARS.contains(&name.as_str());

            if sandbox {
                notes.push(format!("left out variable {name}, which the sandbox sets"));
            }

            !sandbox
        });

        let dir = step.dir.clone().filter(|d| d != ".");

        if dir.as_deref().is_some_and(|d| {
            d.starts_with('/')
                || d.contains("..")
                || !d
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
        }) {
            dropped.push((label, "a step with an unusual working directory".into()));
            continue;
        }

        match step_command(&step) {
            Ok(Some(command)) => parts.push((dir, command)),
            Ok(None) => {}
            Err((true, why)) => return Err(why),
            Err((false, why)) => dropped.push((label, why)),
        }
    }

    Ok((parts, dropped, notes))
}

/// A command's parts between `&&`, whitespace collapsed and grouping cut, without the ones that check nothing
pub fn commands(command: &str) -> Vec<String> {
    command
        .split("&&")
        .map(|c| {
            c.trim_matches(|ch: char| ch.is_whitespace() || "(){};".contains(ch))
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|c| !c.is_empty() && !checks_nothing(c))
        .collect()
}

/// A `uses` step's input, when it is not an expression
fn input(step: &RunStep, name: &str) -> Option<String> {
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
fn action_install(step: &RunStep) -> Option<String> {
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

            // A warm clone also holds untracked build output, which is not the commit, so only the
            // tracked files of HEAD are scanned, in the check's own home, the one place it may write
            let scan = |config: &str| {
                format!(
                    r#"d=$(mktemp -d "$HOME/scan.XXXXXX") && git archive HEAD | tar -x -C "$d" && gitleaks dir --redact{config} "$d""#
                )
            };

            match config {
                None => Some(scan("")),
                Some(Some(c)) if assignment("C", c).is_some_and(|a| a == format!("C={c}")) => {
                    Some(scan(&format!(" --config {c}")))
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
fn rust_need(step: &RunStep) -> Option<String> {
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
    let at = step.ref_name.as_deref().unwrap_or(at);
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
 * subshell. A rustup toolchain this machine has is pinned with `RUSTUP_TOOLCHAIN`, a JDK with `JAVA_HOME`
 **/
#[expect(
    clippy::too_many_lines,
    reason = "one rule per kind of step, read as one table"
)]
fn job_check(
    job: &Job,
    machine: &Machine,
    ruby_file: Option<&str>,
) -> Result<(String, Vec<String>, Vec<String>), String> {
    let toolchains = &machine.toolchains;
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
    let mut notes = Vec::new();

    for need in job.steps.iter().filter_map(rust_need) {
        let Some(t) = need.strip_prefix("needs toolchain ") else {
            return Err(need);
        };

        // The workflow's version when this machine has it, else this machine's, said so
        if !toolchains.iter().any(|i| i == t) {
            notes.push(format!(
                "{WARNING}{need}, which rustup does not have here, so the check runs on the default toolchain until `rustup toolchain install {t}`"
            ));
            continue;
        }

        if pin.is_some_and(|p| p != t) {
            return Err(format!(
                "needs toolchains {} and {t}",
                pin.unwrap_or_default()
            ));
        }

        pin = toolchains.iter().find(|i| *i == t).map(String::as_str);
    }

    let developer_dir = match xcode(job) {
        Some(Ok(app)) if machine.xcodes.contains(&app) => {
            assignment("DEVELOPER_DIR", &format!("{app}/Contents/Developer"))
        }
        Some(Ok(app)) if !app.is_empty() => {
            notes.push(format!(
                "left out {app}, which this machine does not have, so it runs with the default Xcode"
            ));
            None
        }
        _ => None,
    };
    let mut exports: Vec<String> = pin
        .and_then(|t| assignment("RUSTUP_TOOLCHAIN", t))
        .into_iter()
        .chain(developer_dir)
        .collect();

    // As `setup-java` leaves it, so gradlew and mvnw find the JDK through `JAVA_HOME`
    if let Some(jdk) = java::pick(job, &machine.jdks)? {
        let home = jdk.home.display();
        exports.push(format!("JAVA_HOME={home}"));
        exports.push(format!("PATH={home}/bin:\"$PATH\""));
    }

    // As chruby leaves it, so `ruby`, `gem` and `bundle` are the workflow's Ruby
    let ruby = ruby::pick(job, ruby_file, machine.ruby.as_deref(), &machine.rubies)?;
    // The Ruby this machine has runs it, the workflow's version only when this machine has that one
    notes.extend(ruby.note.map(|n| format!("{WARNING}{n}")));

    if let Some(ruby) = ruby.ruby {
        let root = ruby.root.display();
        exports.push(format!("RUBY_ROOT={root}"));
        exports.push(format!("PATH={root}/bin:\"$PATH\""));
    }

    // As `setup-python` leaves it for uv. A version file or a matrix leaves uv to choose
    if let Some(version) = setup_python(job) {
        exports.push(format!("UV_PYTHON={version}"));
    }

    for (name, value) in &job.env {
        if picked(name, value.as_ref()) {
            continue;
        }

        if SANDBOX_VARS.contains(&name.as_str()) {
            notes.push(format!("left out variable {name}, which the sandbox sets"));
            continue;
        }

        match value.as_deref().and_then(|v| assignment(name, v)) {
            Some(a) => exports.push(a),
            None => notes.push(format!("left out variable {name}, an expression")),
        }
    }

    let (parts, dropped, step_notes) = steps_of(job)?;
    notes.extend(step_notes);
    notes.extend(dropped.iter().map(|(_, why)| format!("left out {why}")));

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
        .as_deref()
        .filter(|d| parts.iter().all(|(p, _)| p.as_deref() == Some(*d)));
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
            _ => run.clone(),
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
 * Whether a shell command runs `command`, word by word, as `pnpm lint` in `pnpm run lint`. Flags
 * between words count as the same command, so `go test -race ./...` runs `go test ./...`
 **/
fn runs_command(shell: &str, command: &str) -> bool {
    let words = |s: &str| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();

        for w in s.split_whitespace() {
            let w = w.trim_start_matches('(').trim_end_matches([')', ';']);
            // `pnpm run lint`, `yarn run lint` and `npm run test` are `pnpm lint`, `yarn lint` and `npm test`
            let script = out
                .last()
                .is_some_and(|l| matches!(l.as_str(), "npm" | "pnpm" | "yarn"));

            if w == "run" && script {
                continue;
            }

            out.push(w.to_string());
        }

        out
    };
    let (hay, want) = (words(shell), words(command));
    let Some((first, rest)) = want.split_first() else {
        return false;
    };

    (0..hay.len()).filter(|&i| &hay[i] == first).any(|i| {
        let mut at = i + 1;

        rest.iter().enumerate().all(|(k, w)| {
            // Flags may come after the tool and its subcommand, never inside the tool's name
            while k > 0 && at < hay.len() && hay[at] != *w && hay[at].starts_with('-') {
                at += 1;
            }

            let found = hay.get(at) == Some(w);
            at += 1;
            found
        })
    })
}

/**
 * Host checks and `versions` from the project files at the root: package.json scripts with the
 * package manager its lockfile names, Cargo.toml, pyproject.toml and go.mod. Each check is its
 * name, its tool, which names it when the name is taken, its command and its file
 **/
#[expect(
    clippy::type_complexity,
    reason = "a tuple read once by its only caller"
)]
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
        .map_or_default(|v| v["scripts"].clone());
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
            ("npm" | "bun", _) => format!("{pm} run {name}"),
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

/**
 * What `shipcheck install` would fix in this clone, which `run`, `commit` and `doctor` say: no daemon
 * entry, so a head pushed from elsewhere, such as a rebase on GitHub, gets no run, or hooks an older
 * install wrote. Nothing under a moved home or `SHIPCHECK_NO_DAEMON`, where install leaves the daemon
 **/
pub fn install_gap(root: &Path) -> Option<String> {
    if paths::home_overridden() || std::env::var_os("SHIPCHECK_NO_DAEMON").is_some_and(|v| v == "1")
    {
        return None;
    }

    let entry = if !base::registered(root) && autostart::daemon_on_main(root) {
        Entry::Missing
    } else {
        Entry::Registered
    };
    let hooks = if stale_hooks(root) {
        Hooks::Stale
    } else {
        Hooks::Current
    };
    let name = git::repo_id(root)
        .ok()
        .and_then(|id| id.rsplit('/').next().map(String::from))
        .unwrap_or_else(|| "this repository".into());
    gap_line(&name, entry, hooks)
}

/// Whether the clone has the daemon entry it needs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    Registered,
    Missing,
}

/// Whether the hooks shipcheck wrote match the ones this version writes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hooks {
    Current,
    Stale,
}

fn gap_line(name: &str, entry: Entry, hooks: Hooks) -> Option<String> {
    let what = match (entry, hooks) {
        (Entry::Missing, Hooks::Stale) => {
            "is not registered with the daemon and has outdated hooks"
        }
        (Entry::Missing, Hooks::Current) => {
            "is not registered with the daemon, so a PR pushed from elsewhere gets no run"
        }
        (Entry::Registered, Hooks::Stale) => "has outdated hooks",
        (Entry::Registered, Hooks::Current) => return None,
    };
    Some(format!("{name} {what}, run `shipcheck install` to fix"))
}

/// Whether a hook shipcheck wrote differs from the one this version writes, a foreign hook never does
fn stale_hooks(root: &Path) -> bool {
    let dir = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
    )
    .unwrap_or_default();

    if dir.is_empty() {
        return false;
    }

    [("pre-push", PRE_PUSH), ("post-commit", POST_COMMIT)]
        .iter()
        .any(|(file, script)| {
            fs::read_to_string(Path::new(&dir).join(file))
                .is_ok_and(|hook| hook.contains(MARK) && hook != *script)
        })
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
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
