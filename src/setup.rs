use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Engine;

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

pub const HOOK_LINE: &str = "shipcheck on-push \"$local_ref\" \"$local_sha\"";

pub const POST_COMMIT: &str = r#"#!/bin/sh
# Installed by `shipcheck install`. Starts a background run of each commit when shipcheck.toml lists commit in on
if command -v shipcheck >/dev/null 2>&1; then
  shipcheck on-commit
fi
"#;

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
    /// Why act cannot run it here either, on a machine with Docker
    pub act: Option<String>,
}

#[derive(Default)]
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
#[derive(Default)]
pub struct Machine {
    pub docker: bool,
    /// colima answers, so a job that runs Docker can get a VM of its own
    pub colima: bool,
    pub mac: bool,
    /// The rustup toolchains installed, such as `1.94.1` or `nightly`
    pub toolchains: Vec<String>,
    pub jdks: Vec<crate::java::Jdk>,
    /// The CRuby version `ruby` runs, such as `4.0.9`
    pub ruby: Option<String>,
    pub rubies: Vec<crate::ruby::Ruby>,
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
            jdks: crate::java::installed(),
            ruby: ruby(),
            rubies: crate::ruby::installed(),
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

/// `RUBY_VERSION` when `ruby` is CRuby
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
        .filter(|n| n.starts_with("Xcode") && n.ends_with(".app"))
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
                    Engine::Colima => " on a colima VM of its own",
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

    crate::agents::write_in_repo(root, &config, &text)?;
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
            return Ok((None, NOTHING_LOCAL.into()));
        }

        let mut said = format!("runner = \"act\": {why}");

        for file in pr_only {
            said.push_str(&format!(
                "\n{file} stays on GitHub as it is: a job's `if:` depends on the PR or the default branch"
            ));
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
        text.push_str(&format!("\n[jobs.{name}]\nrunner = \"act\"\n"));

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
            Some(c) => out.extend(words.get(c + 1).map(|s| started(s)).unwrap_or_default()),
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
fn needs_vm(job: &crate::workflow::Job) -> Option<String> {
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
 * Why act cannot run a job here as GitHub does, or the engine it would use. A job off Linux, on a
 * label act has no image for, one whose `if:` or values read the PR, one that reports to GitHub, or
 * one that needs a VM, since colima reaches this machine's loopback and is for its owner to choose
 **/
fn act_engine(
    job: &crate::workflow::Job,
    machine: &Machine,
    source: &str,
) -> Result<Engine, String> {
    // act checks Docker before any job, a colima job's too
    if !machine.docker {
        return Err("Docker is not running".into());
    }

    let labels = crate::runner::act::platforms(&crate::config::Act::default());

    if job.runs_on.is_empty() || job.runs_on.iter().any(|l| l.contains("${{")) {
        return Err("its runs-on is not a label act has an image for".into());
    }

    if let Some(l) = job.runs_on.iter().find(|l| !labels.contains_key(*l)) {
        return Err(format!("act has no image for its label {l}"));
    }

    if let Some(why) = crate::workflow::act_lacks_context(source, &job.id) {
        return Err(why);
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
        return Err(format!("it reports to GitHub or publishes with {u}"));
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
        return Err(format!(
            "{} reads the PR, which act's run has none of",
            s.uses.as_deref().unwrap_or_default()
        ));
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
    let mut files: BTreeMap<String, (String, Vec<crate::workflow::Job>)> = BTreeMap::new();

    for k in &d.github {
        files.entry(k.file.clone()).or_insert_with(|| {
            let source = fs::read_to_string(root.join(".github/workflows").join(&k.file))
                .unwrap_or_default();
            let jobs = crate::workflow::job_runs(&source);
            (source, jobs)
        });
    }

    let engine = |k: &Kept| -> Result<Engine, String> {
        let (source, jobs) = files.get(&k.file).ok_or("no workflow")?;
        let job = jobs.iter().find(|j| j.id == k.job).ok_or("no job")?;
        let mut before = Vec::new();
        needed(jobs, &job.needs, &mut before, &mut vec![job.id.clone()]);

        for n in &before {
            let need = jobs.iter().find(|j| j.id == *n).ok_or("no job")?;
            act_engine(need, machine, source).map_err(|why| format!("job {n} it needs: {why}"))?;
        }

        act_engine(job, machine, source)
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

const DOCKER: &str = "runs Docker, which shipcheck never gives a job";

const BASE_ONLY: &str = "runs only on the base branch, so it is no PR check and stays as it is";

/// The jobs of a file GitHub runs only on the base branch: an `if:` that requires it, or a need that does
pub fn base_only(jobs: &[crate::workflow::Job]) -> BTreeSet<String> {
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

/// Why a job stays on GitHub for PRs in host mode, or None when shipcheck may run it here
fn stays(job: &crate::workflow::Job, machine: &Machine, others_off_linux: bool) -> Option<String> {
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
            _ if others_off_linux => {
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
        && let Err(why) = crate::java::pick(job, &machine.jdks)
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

fn sets_up_xcode(step: &crate::workflow::RunStep) -> bool {
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
fn xcode(job: &crate::workflow::Job) -> Option<Result<String, String>> {
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
            let set = words
                .iter()
                .filter_map(|w| w.strip_prefix("DEVELOPER_DIR="))
                .chain(xcode_select(&words));
            picks.extend(set.filter_map(pick).next());
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
fn picked(name: &str, value: &Option<String>) -> bool {
    name == "DEVELOPER_DIR" && value.as_deref().is_none_or(|v| v.contains(".app"))
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
pub fn detect_on(root: &Path, machine: &Machine) -> Detected {
    let mut d = Detected::default();
    let ruby_file = fs::read_to_string(root.join(".ruby-version")).ok();
    let mut files = Vec::new();

    for file in workflow_files(root).unwrap_or_default() {
        let source =
            fs::read_to_string(root.join(".github/workflows").join(&file)).unwrap_or_default();

        if !crate::workflow::runs_for_branches(&source) {
            d.skipped.push(if crate::workflow::on_pull_request_target(&source) {
                format!("{file}: it runs on pull_request_target, from the base branch with its secrets, so it stays on GitHub")
            } else {
                format!("{file}: GitHub does not run it for feature branches")
            });
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
    // act runs every job of a listed workflow, and a job that runs Docker fails there every time
    let docker_job = all().any(|(_, j)| j.runs_docker);

    if let Some((file, job)) = matrix.filter(|_| machine.docker && !off_linux && !docker_job) {
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
                    act: None,
                });
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
                let why = stays(j, machine, off_linux).or_else(|| {
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
 * step runs uv, which takes it from `UV_PYTHON`. None for `3.x`, a list, PyPy or an expression
 **/
fn setup_python(job: &crate::workflow::Job) -> Option<String> {
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
    use std::os::unix::fs::PermissionsExt;

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
        ["pip" | "pip3", "install", ..]
        | ["python" | "python3", "-m", "pip", "install", ..]
        | ["go", "install", ..]
        | ["npm", "config", ..]
        | ["git", "config", ..]
        | ["rustup", ..]
        | ["cargo", "install", ..]
        | ["brew", "install" | "update" | "upgrade" | "tap" | "bundle", ..]
        // A guard before an install, as in `command -v jq || apt-get install jq`
        | ["command", "-v", ..]
        | ["which", ..] => true,
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

        lines.push(line.to_string());
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

/**
 * The commands of a job that check something, each with its step's directory, for `init` to
 * compare with the checks of a shipcheck.toml. Steps a check could not run are left out
 **/
pub fn job_commands(job: &crate::workflow::Job) -> Vec<(Option<String>, String)> {
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
pub fn dropped_step(job: &crate::workflow::Job) -> Option<String> {
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
#[allow(clippy::type_complexity)]
fn steps_of(
    job: &crate::workflow::Job,
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
            crate::workflow::RunStep {
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

        step.env.retain(|(name, value)| !picked(name, value));
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
#[allow(clippy::type_complexity)]
fn job_check(
    job: &crate::workflow::Job,
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
    if let Some(jdk) = crate::java::pick(job, &machine.jdks)? {
        let home = jdk.home.display();
        exports.push(format!("JAVA_HOME={home}"));
        exports.push(format!("PATH={home}/bin:\"$PATH\""));
    }

    // As chruby leaves it, so `ruby`, `gem` and `bundle` are the workflow's Ruby
    let ruby = crate::ruby::pick(job, ruby_file, machine.ruby.as_deref(), &machine.rubies)?;
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
        if picked(name, value) {
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
pub(crate) mod tests {
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
            (
                ".github/workflows/title.yml",
                "on:\n  pull_request_target:\njobs:\n  title:\n    steps:\n      - run: make\n",
            ),
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
            ],
            "{text}"
        );
        // Written in the order found
        assert!(
            text.contains("[host.checks]\nservice = \"cd service"),
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
            // A check without its other steps would pass where the job fails
            "  ci.yml, job test: a check would leave out its step `make ${{ x }}`, a step with an expression",
            "  ci.yml, job again: runs the same as clippy",
            "  ci.yml, job deploy-docs: it looks like a deploy, publish or release",
            "  ci.yml, job ship: a step deploys, publishes or releases",
            "  ci.yml, job gated: a job with `if:` or `continue-on-error`, which GitHub may skip",
            "  ci.yml, job a b: the job id is not letters, digits, `_` and `-`",
            "  ci.yml, job setup-only: no run step to check with",
            "  tag.yml: GitHub does not run it for feature branches",
            "  title.yml: it runs on pull_request_target, from the base branch with its secrets, so it stays on GitHub",
            "  Cargo.toml: already in a workflow check: `cargo clippy --all-targets -- -D warnings`",
            "  Cargo.toml: ci.yml, job test runs it on GitHub: `cargo test`",
            "  package.json: no workflow runs it: `npm run lint`",
        ] {
            assert!(said.contains(line), "{line}\n{said}");
        }
    }

    #[test]
    fn a_script_runs_in_its_package_managers_forms_and_with_flags() {
        for (shell, command) in [
            ("pnpm run lint", "pnpm lint"),
            ("pnpm format && pnpm run lint", "pnpm lint"),
            ("yarn run lint", "yarn lint"),
            ("npm run test", "npm test"),
            ("npm test", "npm test"),
            ("go test -race -tags=integration ./...", "go test ./..."),
            ("(cargo test --workspace)", "cargo test"),
        ] {
            assert!(runs_command(shell, command), "{shell} runs {command}");
        }

        for (shell, command) in [
            ("pnpm run test-unit", "pnpm test"),
            ("npm run test:ci", "npm test"),
            ("bun test", "bun run test"),
            ("go vet ./...", "go test ./..."),
            ("go -C sub test ./...", "go test ./..."),
            ("go test ./cmd/...", "go test ./..."),
        ] {
            assert!(
                !runs_command(shell, command),
                "{shell} does not run {command}"
            );
        }
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
            "job cont: a check would leave out its step `cargo test \\`, a step with a `\\` continuation",
            "job heredoc: a check would leave out its step `cat <<EOF > a`, a step with a heredoc",
            "job flow: a check would leave out its step `if [ -f a ]`, a step with shell control flow or a function",
            "job func: a check would leave out its step `f() { make; }`, a step with shell control flow or a function",
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
  control:
    steps:
      - run: "x\r\u007f"
  test:
    steps:
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
            "rails.yml, job markdown_api: a check would leave out its step `ruby .github/workflows/scripts/check-markdown-api.rb $BASE_SHA $HEAD_SHA`, a step with an expression",
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
            // A job made of steps a check leaves out stays on GitHub, so init never offers to cut its trigger
            "job pre-commit: a check would leave out its step `uv run prek run --from-ref origin/${GITHUB_BASE_REF} --to-ref HEAD`, a step with `if:` or `continue-on-error`",
            "job test-msrv: a check would leave out its step `cargo +${{ env.MSRV }} test --locked --all-features`, a step with an expression",
            "job docker-build: runs Docker, which shipcheck never gives a job",
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
        assert_eq!(checks, ["check-docs", "external-types"]);
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
        assert!(found.act.is_none());
        // Its only step is an action no command stands for
        assert_eq!(not_host(&found)[0].job, "analyze");
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

    #[test]
    fn a_job_runs_on_the_workflows_ruby_or_stays_without_one() {
        let ci = r#"
on: pull_request
jobs:
  latest:
    steps:
      - uses: ruby/setup-ruby@v1
        with:
          ruby-version: ruby
          bundler-cache: true
      - run: bundle exec rake
  same:
    steps:
      - uses: ruby/setup-ruby@v1
        with:
          ruby-version: '4.0'
      - run: bundle exec rake a
  old:
    steps:
      - uses: ruby/setup-ruby@v1
        with:
          ruby-version: 2.7
      - run: bundle exec rake b
  jruby:
    steps:
      - uses: ruby/setup-ruby@v1
        with:
          ruby-version: jruby
      - run: bundle exec rake c
  file:
    steps:
      - uses: ruby/setup-ruby@v1
      - run: bundle exec rake d
"#;
        let rubies = [crate::ruby::Ruby {
            engine: "ruby".into(),
            version: "2.7.8".into(),
            root: "/rubies/2.7.8".into(),
        }];
        let d = detect_on(
            repo(&[
                (".github/workflows/ci.yml", ci),
                (".ruby-version", "ruby-3.4.1\n"),
            ])
            .path(),
            &Machine {
                ruby: Some("4.0.9".into()),
                rubies: rubies.to_vec(),
                ..Machine::default()
            },
        );
        let checks: Vec<(&str, &str)> = d
            .checks
            .iter()
            .map(|c| (c.name.as_str(), c.command.as_str()))
            .collect();
        assert_eq!(
            checks,
            [
                ("latest", "bundle install && bundle exec rake"),
                ("same", "bundle exec rake a"),
                (
                    "old",
                    "export RUBY_ROOT=/rubies/2.7.8 PATH=/rubies/2.7.8/bin:\"$PATH\" && bundle exec rake b"
                ),
                ("file", "bundle exec rake d"),
            ]
        );
        assert!(
            d.skipped
                .iter()
                .any(|s| s == "ci.yml, job jruby: needs jruby, which this machine does not have"),
            "{:?}",
            d.skipped
        );
        // A check on this machine's Ruby is a warning, not a job left out
        assert_eq!(
            d.warnings,
            [
                "ci.yml, job file: runs on Ruby 4.0.9, which differs from the Ruby 3.4.1 its workflow sets up"
            ]
        );

        // Each Ruby a check runs on is in the key
        for version in ["/rubies/2.7.8/bin/ruby --version", "ruby --version"] {
            assert!(
                d.versions.iter().any(|v| v == version),
                "{version}: {:?}",
                d.versions
            );
        }
    }

    /// What `init` finds in these files on a Linux machine with or without Docker
    #[test]
    fn a_job_no_host_check_runs_as_github_does_goes_to_act_unless_act_differs() {
        let job = |id: &str, extra: &str, steps: &str| {
            format!("  {id}:\n    runs-on: ubuntu-latest\n{extra}    steps:\n{steps}")
        };
        let heredoc = "      - run: |\n          cat <<EOF > a\n          x\n          EOF\n          make test\n";
        let ci = [
            "on: pull_request\njobs:\n".to_string(),
            job("lint", "", "      - run: make lint\n"),
            job("heredoc", "", heredoc),
            job("e2e", "", "      - run: docker build .\n"),
            job("podman", "", "      - run: sudo podman build .\n"),
            job("userns", "", "      - run: sudo apparmor_parser -r /etc/apparmor.d/bwrap\n      - run: make test\n"),
            job("words", "", "      - run: echo mount the disk && make check\n      - uses: some/action@v1\n"),
            job("wasm", "", "      - uses: dtolnay/rust-toolchain@stable\n        with:\n          targets: wasm32-wasip1\n      - run: cargo check\n"),
            job("pr", "    if: github.event_name == 'pull_request'\n", heredoc),
            job("owner", "    if: github.repository_owner == 'acme'\n", heredoc),
            job("changed", "", "      - uses: some/lint@v1\n        with:\n          base: ${{ github.event.pull_request.base.sha }}\n"),
            job("codeql", "", "      - uses: github/codeql-action/analyze@v4\n"),
            job("newer", "", heredoc).replace("ubuntu-latest", "ubuntu-26.04-arm"),
            job("matrix", "", heredoc).replace("ubuntu-latest", "${{ matrix.os }}"),
            "  mac:\n    runs-on: macos-latest\n    steps:\n      - uses: some/action@v1\n".into(),
            "  after:\n    needs: mac\n    runs-on: ubuntu-latest\n    steps:\n      - uses: some/action@v1\n".into(),
            // A host check of its own, kept on GitHub only because a macOS job needs it
            "  prep:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make prep\n".into(),
            "  mac2:\n    needs: prep\n    runs-on: macos-latest\n    steps:\n      - run: make mac\n".into(),
            job("evname", "", "      - run: if [ \"$GITHUB_EVENT_NAME\" = pull_request ]; then make lint-changed; fi\n"),
            job("baseref", "", "      - run: git diff origin/$GITHUB_BASE_REF\n      - uses: some/action@v1\n"),
            job("evpath", "", "      - run: jq .pull_request \"$GITHUB_EVENT_PATH\"\n      - uses: some/action@v1\n"),
            job("secret", "", "      - if: secrets.TOKEN != ''\n        run: make upload\n      - uses: some/action@v1\n"),
            job("var", "", "      - if: vars.RUN_SLOW == 'true'\n        run: make slow\n      - uses: some/action@v1\n"),
            job("ref", "", "      - if: github.ref != 'refs/heads/main'\n        run: make pr\n      - uses: some/action@v1\n"),
            job("changes", "", "      - uses: tj-actions/changed-files@v46\n"),
            job("newissues", "", "      - uses: golangci/golangci-lint-action@v8\n        with:\n          only-new-issues: true\n"),
            job("envwrap", "", "      - run: env X=1 podman build .\n"),
            job("shellc", "", "      - run: bash -c 'docker build .'\n"),
            job("own", "", "      - run: ./scripts/service start && make check\n      - uses: some/action@v1\n"),
        ]
        .concat();
        let place = |docker: bool, colima: bool| {
            detect_on(
                repo(&[(".github/workflows/ci.yml", &ci)]).path(),
                &Machine {
                    docker,
                    colima,
                    ..Machine::default()
                },
            )
        };
        let why = |d: &Detected, id: &str| {
            d.github
                .iter()
                .find(|k| k.job == id)
                .and_then(|k| k.act.clone())
                .unwrap_or_default()
        };

        // Without Docker nothing changes: what no host check runs stays on GitHub, as it said before
        let found = place(false, false);
        assert!(found.acts.is_empty());
        assert!(found.github.iter().all(|k| k.act.is_none()));
        assert_eq!(found.checks[0].name, "lint");

        let found = place(true, true);
        let acts: Vec<(&str, Engine)> = found
            .acts
            .iter()
            .map(|(k, e)| (k.job.as_str(), *e))
            .collect();
        assert_eq!(
            acts,
            // A target this machine lacks is no host check, act sets it up as GitHub does
            [
                ("heredoc", Engine::Docker),
                ("words", Engine::Docker),
                // `./scripts/service` is the repository's own script, not the system's `service`
                ("own", Engine::Docker),
                ("wasm", Engine::Docker)
            ]
        );
        for (id, reason) in [
            (
                "e2e",
                "it runs docker, and colima could run it, but colima reaches this machine's loopback, so it is opt-in",
            ),
            ("podman", "it runs podman, and colima could run it"),
            ("userns", "it runs apparmor_parser"),
            (
                "pr",
                "its `if: github.event_name == 'pull_request'` can run otherwise under act",
            ),
            ("owner", "its `if: github.repository_owner == 'acme'`"),
            ("changed", "it reads `github.event`"),
            (
                "codeql",
                "it reports to GitHub or publishes with github/codeql-action/analyze@v4",
            ),
            ("newer", "act has no image for its label ubuntu-26.04-arm"),
            ("matrix", "its runs-on is not a label act has an image for"),
            ("after", "job mac it needs:"),
            ("evname", "it reads `GITHUB_EVENT_NAME`"),
            ("baseref", "it reads `GITHUB_BASE_REF`"),
            ("evpath", "it reads `GITHUB_EVENT_PATH`"),
            (
                "secret",
                "a step's `if: secrets.TOKEN != ''` reads `secrets.`",
            ),
            ("var", "reads `vars.`"),
            ("ref", "reads `github.ref`"),
            ("changes", "tj-actions/changed-files@v46 reads the PR"),
            ("newissues", "golangci/golangci-lint-action@v8 reads the PR"),
            ("envwrap", "it runs podman"),
            ("shellc", "it runs docker"),
        ] {
            assert!(
                why(&found, id).contains(reason),
                "{id}: {}",
                why(&found, id)
            );
        }

        // A job a macOS job needs stays with it, though act could run it alone
        let prep = found
            .github
            .iter()
            .find(|k| k.job == "prep")
            .expect("prep stays");
        assert_eq!(prep.why, "job mac2 stays on GitHub and needs it");

        // Without colima a VM job says only a VM runs it, and init never writes colima unasked
        assert!(why(&place(true, false), "e2e").ends_with("only a VM runs it"));
        let text = config_text(&found);
        assert!(
            text.contains("\n[jobs.\"ci.yml/heredoc\"]\nrunner = \"act\"\n"),
            "{text}"
        );
        assert!(!text.contains("colima"), "{text}");
        // Before any table, so it is the config's own `network`
        assert!(text.contains("network = \"internet\"\n\n[host]"), "{text}");
        assert!(
            found
                .explain()
                .contains("  ci.yml, job heredoc: a check would leave out its step"),
            "{}",
            found.explain()
        );
        assert!(
            found
                .explain()
                .contains(", and act cannot run it either: it runs docker"),
            "{}",
            found.explain()
        );

        // What init writes runs those jobs with act, beside the host check
        let cfg = crate::config::RepoConfig::parse(&text).unwrap();
        cfg.validate(&["ci.yml".to_string()].into()).unwrap();
        let jobs = crate::run::expected_jobs(&cfg, |_| Some(ci.clone())).unwrap();
        assert_eq!(
            jobs["ci.yml"],
            ["heredoc", "own", "wasm", "words"].map(String::from).into()
        );
        assert_eq!(
            jobs[crate::runner::host::HOST],
            ["lint"].map(String::from).into()
        );

        // A colima job the owner chose needs no network line, which covers Docker's jobs only
        let mut vm = place(true, true);
        vm.acts = vec![(vm.acts.remove(0).0, Engine::Colima)];
        let text = config_text(&vm);
        assert!(
            text.contains("engine = \"colima\"") && !text.contains("network"),
            "{text}"
        );
    }

    /// The jobs no host check runs, on GitHub or on act, in that order
    fn not_host(found: &Detected) -> Vec<&Kept> {
        found
            .github
            .iter()
            .chain(found.acts.iter().map(|(k, _)| k))
            .collect()
    }

    fn detect_with(files: &[(&str, &str)], docker: bool) -> Detected {
        detect_on(
            repo(files).path(),
            &Machine {
                docker,
                ..Machine::default()
            },
        )
    }

    #[test]
    fn a_literal_setup_python_version_pins_uv_and_goes_into_the_key() {
        let ci = r#"on: pull_request
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/setup-python@v6
        with:
          python-version: '3.13'
      - run: uv run pytest
  typing:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/setup-python@v6
        with:
          python-version-file: pyproject.toml
      - run: uv run mypy
"#;
        let found = detect_with(&[(".github/workflows/ci.yml", ci)], false);
        let command = |name: &str| {
            let check = found.checks.iter().find(|c| c.name == name).unwrap();
            check.command.clone()
        };
        assert_eq!(command("test"), "export UV_PYTHON=3.13 && uv run pytest");
        assert_eq!(command("typing"), "uv run mypy");
        assert!(
            found
                .versions
                .contains(&"uv python find 3.13 --show-version || echo missing".to_string()),
            "{:?}",
            found.versions
        );
        assert!(
            found
                .versions
                .contains(&"uv python find --show-version || echo missing".to_string()),
            "{:?}",
            found.versions
        );

        let job = |version: &str, run: &str| {
            let ci = format!(
                "on: push\njobs:\n  t:\n    steps:\n      - uses: actions/setup-python@v6\n        with:\n          python-version: {version}\n      - run: {run}\n"
            );
            let jobs = crate::workflow::job_runs(&ci);
            setup_python(&jobs[0])
        };
        assert_eq!(job("'3.14t'", "uv run pytest").as_deref(), Some("3.14t"));
        assert_eq!(
            job("3.12.4", "cd x && uvx ruff check").as_deref(),
            Some("3.12.4")
        );
        assert_eq!(job("'3.x'", "uv run pytest"), None);
        assert_eq!(job("pypy-3.11", "uv run pytest"), None);
        assert_eq!(job("${{ matrix.python }}", "uv run pytest"), None);
        assert_eq!(job("'3.13'", "pytest"), None);
    }

    #[test]
    fn a_jdk_this_machine_has_is_pinned_and_a_missing_one_keeps_the_job_on_github() {
        let ci = r#"on: pull_request
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/setup-java@v5
        with:
          distribution: temurin
          java-version: |
            11
            21
      - run: ./gradlew test
  native:
    runs-on: ubuntu-latest
    steps:
      - uses: graalvm/setup-graalvm@v1
        with:
          java-version: 25
      - run: ./gradlew nativeTest
"#;
        let jdk = |version: &str, home: &str, native| crate::java::Jdk {
            version: version.into(),
            home: home.into(),
            native,
        };
        let found = detect_on(
            repo(&[(".github/workflows/ci.yml", ci)]).path(),
            &Machine {
                jdks: vec![
                    jdk("11.0.2", "/jvm/11", false),
                    jdk("21.0.5", "/jvm/21", false),
                ],
                ..Machine::default()
            },
        );
        assert_eq!(found.checks.len(), 1);
        assert_eq!(
            found.checks[0].command,
            "export JAVA_HOME=/jvm/21 PATH=/jvm/21/bin:\"$PATH\" && ./gradlew test"
        );
        assert_eq!(found.versions, ["/jvm/21/bin/java -version 2>&1"]);
        assert_eq!(found.github.len(), 1);
        assert_eq!(found.github[0].job, "native");
        assert_eq!(
            found.github[0].why,
            "needs GraalVM 25 with native-image, which this machine does not have"
        );

        // Without a JDK both stay on GitHub
        let found = detect_with(&[(".github/workflows/ci.yml", ci)], false);
        assert!(found.checks.is_empty());
        assert_eq!(
            found.github[0].why,
            "needs JDK 11, which this machine does not have"
        );
    }

    #[test]
    fn an_echo_into_a_file_of_the_tree_stays_in_the_check() {
        let ci = r#"on: pull_request
jobs:
  latest:
    runs-on: ubuntu-latest
    steps:
      - run: echo 'kotlin.jvm.target.validation.mode=ignore' >> ./gradle.properties
      - run: echo building 2>&1 > /dev/null
      - run: make test
"#;
        let found = detect_with(&[(".github/workflows/ci.yml", ci)], false);
        assert_eq!(
            found.checks[0].command,
            "echo 'kotlin.jvm.target.validation.mode=ignore' >> ./gradle.properties && make test"
        );
    }

    #[test]
    fn a_label_gated_job_that_also_runs_on_main_is_not_said_to_never_run_there() {
        let ci = r#"on: pull_request
jobs:
  providers:
    runs-on: ubuntu-latest
    if: github.ref == 'refs/heads/main' || contains(github.event.pull_request.labels.*.name, 'providers')
    steps:
      - run: make test
"#;
        let found = detect_with(&[(".github/workflows/ci.yml", ci)], false);
        assert_eq!(found.github[0].why, PR_ONLY);
        assert!(!PR_ONLY.contains("never runs"), "{PR_ONLY}");
    }

    /// The config `init` writes for a root holding these files, parsed and validated
    #[test]
    fn a_docker_job_stays_on_github_and_a_base_only_chain_holds_nothing_back() {
        let (cfg, _, said) = init_with(&[(".github/workflows/ci.yml", CI_WITH_RELEASE)]);
        assert_eq!(checks(&cfg), [("test", "cargo test")]);
        for line in [
            "job e2e: runs Docker, which shipcheck never gives a job",
            "job check-release: runs only on the base branch",
            "job build: runs only on the base branch",
        ] {
            assert!(said.contains(line), "{line}\n{said}");
        }

        // Without the comment the SHA names no toolchain, so the check runs on this machine's, said so
        let bare = CI_WITH_RELEASE.replace(" # stable", "");
        let d = detect_with(&[(".github/workflows/ci.yml", &bare)], true);
        assert!(d.checks.iter().any(|c| c.name == "test"));
        assert!(
            d.explain().contains("job test: needs toolchain"),
            "{}",
            d.explain()
        );
    }

    pub(crate) const CI_WITH_RELEASE: &str = r#"on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: dtolnay/rust-toolchain@b725eda525362e7cf34af9a7243cac1e3f01a7ec # stable
      - run: cargo test
  e2e:
    runs-on: ubuntu-latest
    steps:
      - run: docker version
      - run: cargo test --test e2e -- --ignored
  check-release:
    needs: [test, e2e]
    if: github.ref == 'refs/heads/main'
    runs-on: ubuntu-latest
    steps:
      - run: ./next-version.sh
  build:
    needs: check-release
    strategy:
      matrix:
        os: [macos-latest, ubuntu-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - run: cargo build --release
"#;

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
        assert_eq!(
            cfg.host.versions,
            [
                "uv --version",
                "uv python find --show-version || echo missing"
            ]
        );
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
    fn a_step_that_needs_root_keeps_its_job_on_github() {
        // orch's release.yml test job, trimmed, and an install beside the check in one step
        let ci = r#"on: push
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - name: Create XFS reflink volume
        run: |
          command -v mkfs.xfs >/dev/null || sudo apt-get install -y xfsprogs
          truncate -s 8G /tmp/xfs.img
          mkfs.xfs -m reflink=1 /tmp/xfs.img
          sudo mkdir /mnt/xfs
          sudo mount -o loop /tmp/xfs.img /mnt/xfs
      - name: Run tests with coverage
        run: cargo llvm-cov nextest --lcov --output-path lcov.info
  setup:
    steps:
      - run: |
          sudo apt-get update
          sudo apt-get install -y lcov
      - run: make check
  together:
    steps:
      - run: |
          sudo apt-get install -y libfoo
          make check-all
  feature:
    steps:
      - run: cargo test --features mount
  vars:
    env:
      HOME: /home/runner
    steps:
      - env:
          ORCH_COW_EXPECT_CLONE: "1"
          TMPDIR: /mnt/xfs/tmp
        run: cargo nextest run
"#;
        let (cfg, _, said) = init_with(&[(".github/workflows/ci.yml", ci)]);
        assert_eq!(
            checks(&cfg),
            [
                ("feature", "cargo test --features mount"),
                ("setup", "make check"),
                ("together", "make check-all"),
                (
                    "vars",
                    "export ORCH_COW_EXPECT_CLONE=1 && cargo nextest run"
                ),
            ]
        );
        for line in [
            "ci.yml, job test: runs mount, which only a VM of its own gives a job",
            "ci.yml, job vars: left out variable TMPDIR, which the sandbox sets",
            "ci.yml, job vars: left out variable HOME, which the sandbox sets",
        ] {
            assert!(said.contains(line), "{line}\n{said}");
        }
        // A step of only setup commands is dropped without a note
        assert!(!said.contains("job setup:"), "{said}");
    }

    #[test]
    fn only_install_lines_are_setup() {
        let ci = r#"on: push
jobs:
  root-script:
    steps:
      - run: sudo ./scripts/test.sh
  aptos:
    steps:
      - run: aptos move test
  health:
    steps:
      - run: curl -f localhost:8080/health
  gh:
    steps:
      - run: gh workflow view ci.yml
  installer:
    steps:
      - run: curl -fsSL https://example.com/install.sh | sh
      - run: make test
  exits:
    steps:
      - run: |
          exit 0
          make test
  semicolon:
    steps:
      - run: make a; make b
  gated:
    steps:
      - run: make lint
        if: matrix.os == 'linux'
      - run: make test-gated
  echoed:
    steps:
      - run: echo "::group::tests"
      - run: brew install jq
      - run: make echoed
"#;
        let found = detect_with(&[(".github/workflows/ci.yml", ci)], true);
        let checks: Vec<(&str, &str)> = found
            .checks
            .iter()
            .map(|c| (c.name.as_str(), c.command.as_str()))
            .collect();
        assert_eq!(
            checks,
            [
                ("aptos", "aptos move test"),
                ("installer", "make test"),
                ("echoed", "make echoed"),
            ],
            "{}",
            found.explain()
        );
        let kept: Vec<(&str, &str)> = not_host(&found)
            .iter()
            .map(|k| (k.job.as_str(), k.why.as_str()))
            .collect();
        assert_eq!(
            kept,
            [
                (
                    "root-script",
                    "a check would leave out its step `sudo ./scripts/test.sh`, a step that needs root, with `sudo`, `mount`, `mkfs` or `apt`"
                ),
                (
                    "health",
                    "a check would leave out its step `curl -f localhost:8080/health`, a step with `curl`, `wget`, `gh` or `brew` that is not an install"
                ),
                (
                    "gh",
                    "a check would leave out its step `gh workflow view ci.yml`, a step with `curl`, `wget`, `gh` or `brew` that is not an install"
                ),
                (
                    "exits",
                    "a check would leave out its step `exit 0`, a step with shell control flow or a function"
                ),
                (
                    "semicolon",
                    "a check would leave out its step `make a; make b`, a step with `;`, which GitHub's `bash -e` stops at"
                ),
                (
                    "gated",
                    "a check would leave out its step `make lint`, a step with `if:` or `continue-on-error`"
                ),
            ]
        );
    }

    #[test]
    fn only_known_actions_are_left_out_without_a_trace() {
        for (uses, nothing) in [
            ("actions/checkout@v6", true),
            ("actions/cache/restore@v4", true),
            ("gradle/actions/setup-gradle@v4", true),
            ("Homebrew/actions/setup-homebrew@master", true),
            ("actions-rs/toolchain@v1", true),
            ("jdx/mise-action@v2", true),
            ("pnpm/action-setup@v4", true),
            ("Swatinem/rust-cache@v2", true),
            ("acme/setup-and-run-e2e@v1", false),
            ("acme/cache@v1", false),
            ("acme/action-setup@v1", false),
            ("./.github/actions/setup-node", false),
            ("docker://alpine:3", false),
            ("EmbarkStudios/cargo-deny-action@v2", false),
        ] {
            assert_eq!(drops_nothing(uses), nothing, "{uses}");
        }
    }

    #[test]
    fn a_step_in_an_unusual_directory_keeps_its_job_on_github() {
        let ci = "on: push\njobs:\n  up:\n    steps:\n      - run: make\n        working-directory: ../other\n      - run: make test\n";
        let found = detect_with(&[(".github/workflows/ci.yml", ci)], true);
        assert!(found.checks.is_empty());
        assert_eq!(
            not_host(&found)[0].why,
            "a check would leave out its step `make`, a step with an unusual working directory"
        );
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
        // orch's release.yml and an Anchor project's ci.yml, trimmed
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
                ("nightly", "export RUST_VERSION=1.93.1 && cargo build"),
                ("file", "export RUST_VERSION=1.93.1 && cargo doc"),
            ]
        );
        assert_eq!(
            found.versions,
            ["rustc +1.94.1 --version", "rustc +1.93.1 --version"]
        );
        let said = found.explain();
        for line in [
            "run on this machine's version, not the workflow's:\n  ci.yml, job nightly",
            "ci.yml, job nightly: needs toolchain nightly-2026-01-01, which rustup does not have here, so the check runs on the default toolchain",
            "ci.yml, job target: needs target wasm32-wasip1",
        ] {
            assert!(said.contains(line), "{line}\n{said}");
        }

        // Without the toolchain the check runs on the default one, with a warning
        let found = detect_with(&[(".github/workflows/ci.yml", ci)], false);
        assert!(
            found.warnings.contains(
                &"ci.yml, job check: needs toolchain 1.94.1, which rustup does not have here, so the check runs on the default toolchain until `rustup toolchain install 1.94.1`".to_string()
            ),
            "{}",
            found.explain()
        );
        assert!(found.checks.iter().any(|c| c.name == "check"));
    }

    #[test]
    fn a_job_its_check_would_run_only_part_of_stays_on_github() {
        // curl's checkdocs.yml, checksrc.yml and linux-old.yml, trimmed
        let ci = r#"on: pull_request
jobs:
  proselint:
    steps:
      - uses: actions/checkout@v6
      - run: git ls-files '*.md' -z | xargs -0 -n1 .github/scripts/trimmarkdownheader.pl
      - name: check prose
        run: |
          cat <<JSON > ~/.proselintrc.json
          {}
          JSON
          proselint check --config ~/.proselintrc.json docs
  deny:
    steps:
      - run: cargo build
      - uses: EmbarkStudios/cargo-deny-action@v2
  pytype:
    steps:
      - name: install prereqs
        uses: ./.github/actions/pkg-install
        with:
          pip: pytype
      - run: |
          source ~/venv/bin/activate
          pytype -j auto .
  cached:
    steps:
      - uses: ./.github/actions/cache
      - run: make test
  old:
    runs-on: ubuntu-latest
    container: debian:stretch-slim
    steps:
      - run: make
  boxed:
    container:
      image: alpine:3.24
    steps:
      - run: make check
  plain:
    steps:
      - uses: actions/checkout@v6
      - run: make lint
"#;
        let found = detect_with(&[(".github/workflows/ci.yml", ci)], true);
        let checks: Vec<&str> = found.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(checks, ["plain"], "{}", found.explain());
        let kept: Vec<(&str, &str)> = not_host(&found)
            .iter()
            .map(|k| (k.job.as_str(), k.why.as_str()))
            .collect();
        assert_eq!(
            kept,
            [
                (
                    "proselint",
                    "a check would leave out its step `check prose`, a step with a heredoc"
                ),
                (
                    "deny",
                    "a check would leave out its step `EmbarkStudios/cargo-deny-action@v2`, a step that uses an action no command stands for"
                ),
                (
                    "pytype",
                    "a check would leave out its step `install prereqs`, a step that uses an action no command stands for"
                ),
                // A local action is the repository's own code, whatever its name
                (
                    "cached",
                    "a check would leave out its step `./.github/actions/cache`, a step that uses an action no command stands for"
                ),
                ("boxed", "runs in container alpine:3.24"),
                ("old", "runs in container debian:stretch-slim"),
            ]
        );
        // Only `old` names a runner act has an image for, the rest stay on GitHub and say why
        let acts: Vec<&str> = found.acts.iter().map(|(k, _)| k.job.as_str()).collect();
        assert_eq!(acts, ["old"]);
        assert!(
            found.github.iter().all(
                |k| k.act.as_deref() == Some("its runs-on is not a label act has an image for")
            )
        );
    }

    #[test]
    fn a_matrix_of_one_combination_is_a_plain_job() {
        // A Python app's ci.yml and git-cow's, trimmed
        let ci = r#"on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        python-version: [3.13]
    steps:
      - uses: actions/checkout@v6
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
                (
                    "leaks",
                    r#"d=$(mktemp -d "$HOME/scan.XXXXXX") && git archive HEAD | tar -x -C "$d" && gitleaks dir --redact "$d""#
                ),
                (
                    "secrets",
                    r#"d=$(mktemp -d "$HOME/scan.XXXXXX") && git archive HEAD | tar -x -C "$d" && gitleaks dir --redact --config .gitleaks.toml "$d""#
                ),
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
        // An Anchor project's ci.yml, trimmed
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

    // Alamofire's ci.yml, and the other ways a workflow picks an Xcode
    const XCODE: &str = r#"on: pull_request
jobs:
  env:
    runs-on: macOS-26
    env:
      DEVELOPER_DIR: "/Applications/Xcode 26.6.app/Contents/Developer"
    steps:
      - run: xcodebuild -scheme "Alamofire macOS" build
  select:
    runs-on: macos-15
    steps:
      - run: sudo -E xcode-select -s /Applications/Xcode_26.6.app && swift test
  switch:
    runs-on: macos-15
    steps:
      - run: xcode-select --switch=/Applications/Xcode_26.6.app
      - run: swift build
  githubenv:
    runs-on: macos-15
    steps:
      - run: echo "DEVELOPER_DIR=/Applications/Xcode_26.6.app/Contents/Developer" >> $GITHUB_ENV
      - run: swift build -v
  action:
    runs-on: macos-15
    steps:
      - uses: maxim-lobanov/setup-xcode@v1
        with:
          xcode-version: '26.6'
      - run: swift test -c debug
  beside:
    runs-on: macos-15
    steps:
      - run: sudo xcode-select -s /Applications/Xcode_26.6.app || true
      - run: swift build --build-tests
  latest:
    runs-on: macos-15
    steps:
      - uses: maxim-lobanov/setup-xcode@v1
        with:
          xcode-version: latest-stable
      - run: xcrun swift build
  tools:
    runs-on: macos-15
    steps:
      - run: sudo xcode-select -s /Library/Developer/CommandLineTools
      - run: swift build
"#;

    fn xcode_on(xcodes: &[&str]) -> Detected {
        detect_on(
            repo(&[(".github/workflows/ci.yml", XCODE)]).path(),
            &Machine {
                mac: true,
                xcodes: xcodes.iter().map(|x| x.to_string()).collect(),
                ..Machine::default()
            },
        )
    }

    fn commands_of(found: &Detected) -> Vec<(&str, &str)> {
        found
            .checks
            .iter()
            .map(|c| (c.name.as_str(), c.command.as_str()))
            .collect()
    }

    #[test]
    fn a_job_that_picks_two_xcodes_stays_whole() {
        let ci = "on: pull_request\njobs:\n  two:\n    runs-on: macos-latest\n    steps:\n      - run: sudo xcode-select -s /Applications/Xcode_26.6.app\n      - run: swift build\n      - run: sudo xcode-select -s /Applications/Xcode_16.4.app\n      - run: swift test\n";
        let found = detect_on(
            repo(&[(".github/workflows/ci.yml", ci)]).path(),
            &Machine {
                mac: true,
                xcodes: vec![
                    "/Applications/Xcode_26.6.app".into(),
                    "/Applications/Xcode_16.4.app".into(),
                ],
                ..Machine::default()
            },
        );
        assert!(found.checks.is_empty(), "{}", found.explain());
        assert_eq!(found.github[0].why, "picks more than one Xcode");
    }

    #[test]
    fn a_job_that_needs_xcode_stays_on_github_on_a_machine_without_one() {
        let found = xcode_on(&[]);
        let kept: Vec<(&str, &str)> = found
            .github
            .iter()
            .map(|k| (k.job.as_str(), k.why.as_str()))
            .collect();
        let why = "needs Xcode, which this machine does not have";
        assert_eq!(
            kept,
            [
                "env",
                "select",
                "switch",
                "githubenv",
                "action",
                "beside",
                "latest"
            ]
            .map(|j| (j, why))
        );
        // The Command Line Tools are not an Xcode
        assert_eq!(commands_of(&found), [("tools", "swift build")]);
    }

    #[test]
    fn a_job_that_picks_an_installed_xcode_runs_with_it() {
        let found = xcode_on(&[
            "/Applications/Xcode 26.6.app",
            "/Applications/Xcode_26.6.app",
        ]);
        let spaced = "DEVELOPER_DIR='/Applications/Xcode 26.6.app/Contents/Developer'";
        let dir = "DEVELOPER_DIR=/Applications/Xcode_26.6.app/Contents/Developer";
        assert_eq!(
            commands_of(&found),
            [
                (
                    "env",
                    format!("export {spaced} && xcodebuild -scheme \"Alamofire macOS\" build")
                        .as_str()
                ),
                ("select", &format!("export {dir} && swift test")),
                ("switch", &format!("export {dir} && swift build")),
                ("githubenv", &format!("export {dir} && swift build -v")),
                ("action", &format!("export {dir} && swift test -c debug")),
                (
                    "beside",
                    &format!("export {dir} && swift build --build-tests")
                ),
                ("latest", "xcrun swift build"),
                ("tools", "swift build"),
            ]
        );
        // `xcode-select -s <app> || true` only picks the Xcode the check's DEVELOPER_DIR names
        assert!(
            found.github.is_empty(),
            "{:?}",
            found.github.iter().map(|k| &k.why).collect::<Vec<_>>()
        );
        for version in [
            format!("{spaced} xcodebuild -version"),
            format!("{dir} xcodebuild -version"),
            "xcodebuild -version".to_string(),
        ] {
            assert!(
                found.versions.contains(&version),
                "{version}\n{:?}",
                found.versions
            );
        }
    }

    #[test]
    fn a_job_that_picks_another_xcode_runs_with_the_default_one() {
        let found = xcode_on(&["/Applications/Xcode_16.4.app"]);
        assert!(found.github.is_empty());
        let checks = commands_of(&found);
        assert_eq!(
            checks[0],
            ("env", "xcodebuild -scheme \"Alamofire macOS\" build")
        );
        assert_eq!(checks[1], ("select", "swift test"));
        assert!(
            found.skipped.contains(
                &"ci.yml, job env: left out /Applications/Xcode 26.6.app, which this machine does not have, so it runs with the default Xcode"
                    .to_string()
            ),
            "{:?}",
            found.skipped
        );
    }

    #[test]
    fn a_quoted_semicolon_is_part_of_its_word() {
        let ci = "on: pull_request\njobs:\n  quoted:\n    steps:\n      - run: sed -i'' 's;@MainActor;;g' Tests/*.swift\n      - run: swift build\n  bare:\n    steps:\n      - run: make; make test\n  android:\n    steps:\n      - run: sed -i'' 's;@MainActor;;g' Tests/*.swift\n      - uses: skiptools/swift-android-action@v2\n";
        let found = detect_with(&[(".github/workflows/ci.yml", ci)], false);
        assert_eq!(
            commands_of(&found),
            [(
                "quoted",
                "sed -i'' 's;@MainActor;;g' Tests/*.swift && swift build"
            )]
        );
        // Its action does the work, so a check of the `sed` alone would pass whatever the code is
        assert!(
            found
                .github
                .iter()
                .any(|k| k.job == "android"
                    && k.why.contains("uses an action no command stands for")),
            "{:?}",
            found.github.iter().map(|k| &k.why).collect::<Vec<_>>()
        );
        assert_eq!(unquoted(r#"a "b;c" 'd;e' f\;g ;"#), "a bc de fg ;");
        assert!(unquoted(r"echo $'\'' ; make test").contains(';'));
        assert!(!checks_nothing("\"./ci/test.sh\""));
    }

    #[test]
    fn a_stays_reason_is_about_its_own_job() {
        let ci = r#"on: pull_request
jobs:
  visionOS:
    runs-on: ${{ matrix.runsOn }}
    strategy:
      matrix:
        include:
          - runsOn: self-xcode266
          - runsOn: self-xcode264
    steps:
      - run: xcodebuild test
  linux:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        image: [swift:6.2, swift:6.3]
    steps:
      - run: swift build
"#;
        let found = detect_with(&[(".github/workflows/ci.yml", ci)], true);
        let why: Vec<&str> = not_host(&found).iter().map(|k| k.why.as_str()).collect();
        assert_eq!(
            why,
            [
                "runs a matrix (runsOn: self-xcode266, self-xcode264) with macOS entries, which act cannot run",
                "runs a matrix (image: swift:6.2, swift:6.3), and host mode runs only this machine's toolchain",
            ]
        );
    }

    #[test]
    fn a_tool_not_on_path_is_named() {
        let checks = [Found {
            name: "build".into(),
            command: "export A=1 && set -o pipefail && eval \"$X\" && exec foo && find . -exec ls {} + && xcodebuild build | xcbeautify && pip install ruff && ruff check".into(),
            from: String::new(),
        }];
        assert_eq!(
            missing_tools(&checks, |t| ["xcodebuild", "pip"].contains(&t)),
            ["build calls xcbeautify"]
        );
    }
}
