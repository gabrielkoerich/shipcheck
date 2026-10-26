use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{RepoConfig, RunnerKind};
use crate::setup;
use crate::ui::Style;

/// The hosted App's slug and ID
const APP_SLUG: &str = "shipcheck-run";
const APP_ID: &str = "5400635";

/// The GitHub Actions App, which posts the checks of jobs that stay on GitHub
const ACTIONS_APP_ID: u64 = 15368;

const PROTECTION_DOCS: &str = "https://shipcheck.run/docs/branch-protection/";

/// The App that posts the checks
#[derive(Clone, Debug, PartialEq)]
pub enum App {
    /// SHIPCHECK_APP_ID names an App of the person's own
    Own(String),
    Hosted,
    /// The hosted App is not public yet
    NotPublic,
}

impl App {
    /// SHIPCHECK_APP_ID, else whether GitHub shows the hosted App
    pub fn find() -> App {
        if let Ok(id) = std::env::var("SHIPCHECK_APP_ID") {
            return App::Own(id);
        }

        let public = ureq::get(format!("https://api.github.com/apps/{APP_SLUG}"))
            .header("User-Agent", "shipcheck")
            .config()
            .timeout_global(Some(std::time::Duration::from_secs(10)))
            .build()
            .call()
            .is_ok();

        if public { App::Hosted } else { App::NotPublic }
    }

    fn id(&self) -> Option<String> {
        match self {
            App::Own(id) => Some(id.clone()),
            App::Hosted => Some(APP_ID.into()),
            App::NotPublic => None,
        }
    }
}

fn install_url() -> String {
    format!("https://github.com/apps/{APP_SLUG}/installations/new")
}

/// One workflow edit init offers
pub struct Change {
    pub file: String,
    edited: String,
    pub diff: String,
    /// What yes and no do, in a sentence and in a few words for the review
    pub yes: String,
    pub no: String,
    short_yes: String,
    short_no: String,
}

/// The required checks init can turn on
pub struct Ruleset {
    app: String,
    pub names: Vec<String>,
    github: Vec<String>,
    notes: Vec<String>,
}

/// What `init` would do, read from the repository and the machine, with nothing written
pub struct Plan {
    pub base: String,
    /// The shipcheck.toml to write and what it holds, None when one exists
    pub config: Option<(String, String)>,
    /// What the existing shipcheck.toml checks, when there is one
    pub existing: Option<String>,
    pub workflows: Vec<Change>,
    /// Workflows init leaves alone, and why
    pub notes: Vec<String>,
    pub app: App,
    pub ruleset: Option<Ruleset>,
    /// Why init has nothing to set up, which ends it
    pub stop: Option<String>,
    /// The file, job and reason of each job whose check failed when init tried it
    failed: Vec<(String, String, String)>,
    /// The file, job and GitHub's check names of each job that stays on GitHub for PRs
    github: Vec<(String, String, Option<Vec<String>>)>,
    /// Where each job runs, for a shipcheck.toml of host checks init derived, which the person can change
    pub detected: Option<setup::Detected>,
    /// The checks the default branch requires now, which the ruleset keeps for a job whose names init cannot tell. None when GitHub did not say
    required: Option<Vec<String>>,
}

/// The person's choice for each step of a plan
#[derive(Clone, Debug, PartialEq)]
pub struct Answers {
    pub config: bool,
    pub workflows: Vec<bool>,
    pub open_app: bool,
    pub ruleset: bool,
}

/// A step the person can choose for
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Config,
    Workflow(usize),
    App,
    Ruleset,
}

/**
 * Builds the plan, and tries each host check it would write. Writes nothing in the repository, so
 * quitting before apply leaves everything as it was
 **/
pub fn plan(root: &Path, app: App, r: &mut dyn Report) -> Result<Plan, String> {
    plan_at(root, &base_branch(root)?, app, r)
}

/// Why init makes no ruleset when GitHub would not say what the branch requires now
const KEPT_RULES: &str = "branch rules: GitHub did not say which checks the default branch requires now, so init leaves its rules as they are. Run `shipcheck init` again when `gh` is signed in and online";

pub(crate) fn plan_at(
    root: &Path,
    base: &str,
    app: App,
    r: &mut dyn Report,
) -> Result<Plan, String> {
    // Only a plan that can make a ruleset asks GitHub
    let required = if app.id().is_some() {
        required_now(root, base)
    } else {
        Some(Vec::new())
    };
    plan_with(root, base, app, required, r)
}

/// `plan_at` with the checks the branch requires now already read
fn plan_with(
    root: &Path,
    base: &str,
    app: App,
    required: Option<Vec<String>>,
    r: &mut dyn Report,
) -> Result<Plan, String> {
    let path = root.join("shipcheck.toml");
    let mut plan = Plan {
        base: base.to_string(),
        config: None,
        existing: None,
        workflows: Vec::new(),
        notes: Vec::new(),
        app,
        ruleset: None,
        stop: None,
        failed: Vec::new(),
        github: Vec::new(),
        detected: None,
        required,
    };
    if plan.required.is_none() {
        plan.notes.push(KEPT_RULES.into());
    }

    let text = if path.exists() {
        fs::read_to_string(&path).map_err(|e| format!("shipcheck.toml: {e}"))?
    } else {
        let machine = setup::Machine::this();

        match setup::propose(root, &machine)? {
            (Some(text), said) => {
                let Some((text, said)) = tried(root, &machine, text, said, r, &mut plan)? else {
                    return Ok(plan);
                };
                plan.config = Some((text.clone(), said));
                text
            }
            (None, said) => {
                plan.stop = Some(said);
                return Ok(plan);
            }
        }
    };
    let cfg = RepoConfig::parse(&text)?;

    if plan.config.is_none() {
        let mut existing = match cfg.runner {
            RunnerKind::Host => format!(
                "checks {}",
                cfg.host
                    .checks
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => format!("workflows {}", cfg.workflows.join(", ")),
        };
        let acts = cfg.jobs_on(RunnerKind::Act);

        if !acts.is_empty() {
            let jobs = format!("act jobs {}", acts.join(", "));
            existing = if cfg.host.checks.is_empty() && cfg.workflows.is_empty() {
                jobs
            } else {
                format!("{existing}, and {jobs}")
            };
        }

        plan.existing = Some(existing);
    }

    workflows(root, &cfg, &mut plan)?;
    plan.ruleset = ruleset_plan(
        root,
        &text,
        &plan.app,
        &plan.github,
        plan.required.as_deref(),
    );
    Ok(plan)
}

/**
 * Keeps the host checks of a proposed shipcheck.toml that pass when init tries them, some with
 * color turned off. A job whose check fails stays on GitHub with the end of its log as the reason.
 * None, with the plan stopped, when no check passes
 **/
fn tried(
    root: &Path,
    machine: &setup::Machine,
    text: String,
    said: String,
    r: &mut dyn Report,
    plan: &mut Plan,
) -> Result<Option<(String, String)>, String> {
    if RepoConfig::parse(&text)?.runner != RunnerKind::Host {
        // Only act jobs and no check to try, still each job has a place the person may change
        let d = setup::detect_on(root, machine);
        plan.detected = d.act.is_none().then_some(d);
        return Ok(Some((text, said)));
    }

    let (results, versions) = try_checks(root, &text, r)?;
    let mut d = setup::detect_on(root, machine);
    let mut failed = Vec::new();
    // try_checks already said which versions it left out
    d.versions = versions;
    d.checks.retain_mut(|c| match results.get(&c.name) {
        Some(Ok(command)) => {
            c.command = command.clone();
            true
        }
        other => {
            let why = other.and_then(|t| t.as_ref().err()).cloned();
            failed.push((
                c.from.clone(),
                why.unwrap_or_else(|| "init did not try it".into()),
            ));
            false
        }
    });

    let mut ids = Vec::new();

    for (from, why) in failed {
        // A check from project files has no job, so nothing stays on GitHub for it
        let Some((file, job)) = from.split_once(", job ") else {
            d.skipped.push(format!("{from}: {why}"));
            continue;
        };
        let job = job.split(", after job ").next().unwrap_or(job);
        ids.push((file.to_string(), job.to_string()));
        d.github.push(setup::Kept {
            file: file.into(),
            job: job.into(),
            why,
            checks: None,
            act: None,
        });
    }

    plan.failed.extend(
        d.github
            .iter()
            .filter(|k| ids.contains(&(k.file.clone(), k.job.clone())))
            .map(|k| (k.file.clone(), k.job.clone(), k.why.clone())),
    );

    if d.checks.is_empty() && d.acts.is_empty() {
        plan.stop = Some(format!(
            "no check passed when init tried it, so init wrote no shipcheck.toml and set nothing up:\n{}",
            d.explain()
        ));
        return Ok(None);
    }

    let out = (setup::config_text(&d), setup::said_for(&d));
    plan.detected = Some(d);
    Ok(Some(out))
}

/**
 * The plan again after the person moved jobs between host checks, act and GitHub: a new
 * shipcheck.toml and the workflow edits and required checks that go with it. Nothing is written
 **/
pub fn replan(root: &Path, plan: &mut Plan, d: setup::Detected) -> Result<(), String> {
    let nothing = d.checks.is_empty() && d.acts.is_empty();
    let text = setup::config_text(&d);
    let cfg = RepoConfig::parse(&text)?;
    plan.config = Some((text.clone(), setup::said_for(&d)));
    plan.detected = Some(d);
    plan.workflows.clear();
    plan.notes.clear();
    plan.github.clear();
    workflows(root, &cfg, plan)?;
    plan.ruleset = ruleset_plan(
        root,
        &text,
        &plan.app,
        &plan.github,
        plan.required.as_deref(),
    );

    // As init does when every job stays on GitHub, which an empty shipcheck.toml would not say
    if nothing {
        return Err("every job runs on GitHub, so there is no shipcheck.toml to write: move a job here, or quit".into());
    }

    Ok(())
}

/// Each check's command once it passed, or why it failed
type Tried = BTreeMap<String, Result<String, String>>;

/**
 * Tries each `versions` command, then each check, then each safe variant of a check that failed.
 * A version that fails would fail every run, so it is left out with a warning, and only the checks
 * that start its tool fail with it. Returns the results and the versions that answered
 **/
fn try_checks(root: &Path, text: &str, r: &mut dyn Report) -> Result<(Tried, Vec<String>), String> {
    let cfg = RepoConfig::parse(text)?;
    let mut tried = Tried::new();
    let mut versions = Vec::new();

    for version in &cfg.host.versions {
        let Err(e) = probe(root, &cfg.host, version) else {
            versions.push(version.clone());
            continue;
        };
        let tool = version.split_whitespace().next().unwrap_or_default();
        r.done(
            false,
            &format!("`{version}` does not run, so shipcheck.toml leaves it out of versions"),
        );

        for (name, command) in &cfg.host.checks {
            let starts = command
                .split(|c: char| c.is_whitespace() || "();|&".contains(c))
                .any(|w| w == tool);

            if starts && !tried.contains_key(name) {
                r.done(
                    false,
                    &format!("{name} ... failed, `{version}` does not run"),
                );
                tried.insert(
                    name.clone(),
                    Err(format!(
                        "failed when init tried it: {e}\n    fix: install {tool}"
                    )),
                );
            }
        }
    }

    let mut todo: Vec<(String, Vec<String>)> = cfg
        .host
        .checks
        .iter()
        .filter(|(name, _)| !tried.contains_key(*name))
        .map(|(name, command)| {
            let mut commands = vec![command.clone()];
            commands.extend(variants(command));
            (name.clone(), commands)
        })
        .collect();
    let mut first = true;

    while !todo.is_empty() {
        let what = if first {
            "trying"
        } else {
            "trying again, with color off,"
        };
        r.start(&format!(
            "{what} {} in the sandbox, as `shipcheck run` would",
            plural(todo.len(), "check")
        ));
        first = false;
        let mut round = RepoConfig::parse(text)?;
        round.host.versions = versions.clone();
        round.host.checks = todo
            .iter()
            .map(|(n, c)| (n.clone(), c[0].clone()))
            .collect();
        let results = run_trial(root, round)?;
        let mut next = Vec::new();

        for (name, mut commands) in todo {
            let command = commands.remove(0);
            let t = results
                .get(&name)
                .ok_or("a check init tried has no result")?;

            if t.passed {
                let how = if command == cfg.host.checks[&name] {
                    ""
                } else {
                    ", with color off"
                };
                r.done(true, &format!("{name} ... passed {}s{how}", t.secs));
                tried.insert(name, Ok(command));
            } else if commands.is_empty() || deterministic(&t.log) {
                r.done(false, &format!("{name} ... failed {}s", t.secs));
                tried.insert(name, Err(reason(&t.log)));
            } else {
                next.push((name, commands));
            }
        }

        todo = next;
    }

    Ok((tried, versions))
}

fn plural(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

#[cfg(not(test))]
fn run_trial(root: &Path, cfg: RepoConfig) -> Result<BTreeMap<String, crate::run::Trial>, String> {
    crate::run::trial(root, cfg, &mut |_| {})
}

// Unit tests start no jail, every check passes. tests/init_tries.rs runs the real ones
#[cfg(test)]
fn run_trial(_: &Path, cfg: RepoConfig) -> Result<BTreeMap<String, crate::run::Trial>, String> {
    Ok(cfg
        .host
        .checks
        .into_iter()
        .map(|(name, command)| {
            let t = crate::run::Trial {
                passed: !command.contains("fails-in-trial"),
                log: String::new(),
                secs: 0,
            };
            (name, t)
        })
        .collect())
}

/// Whether one `versions` command answers in the sandbox, as every plan runs it
#[cfg(not(test))]
fn probe(root: &Path, host: &crate::config::Host, version: &str) -> Result<(), String> {
    use crate::runner::Runner;
    let host = crate::config::Host {
        versions: vec![version.to_string()],
        ..host.clone()
    };
    crate::runner::host::Host::new(host, root)
        .identity()
        .map(|_| ())
}

// A toolchain named `missing` is one this machine lacks
#[cfg(test)]
fn probe(_: &Path, _: &crate::config::Host, version: &str) -> Result<(), String> {
    if version.contains("+missing") {
        return Err("error: toolchain 'missing' is not installed".into());
    }

    Ok(())
}

/// Turns color off for tools that read these, so a check that fails on color codes can pass
const NO_COLOR: &str = "NO_COLOR=1 FORCE_COLOR=0 PY_COLORS=0 TERM=dumb";

/// Flags that force color, which some tools refuse without a terminal
const COLOR_FLAGS: &[&str] = &[
    "--color=always",
    "--color=yes",
    "--color always",
    "-c always",
];

/// The safe variants of a check, in the order init tries them: without forced color, then with color off too
fn variants(command: &str) -> Vec<String> {
    let plain = without_color(command);
    let mut out = Vec::new();

    if plain != command {
        out.push(plain.clone());
    }

    out.push(format!("export {NO_COLOR} && {plain}"));
    out
}

fn without_color(command: &str) -> String {
    COLOR_FLAGS
        .iter()
        .fold(command.to_string(), |c, flag| without(&c, flag))
}

/// The command with each ` <flag>` that ends a word removed
fn without(command: &str, flag: &str) -> String {
    let needle = format!(" {flag}");
    let mut out = String::new();
    let mut rest = command;

    while let Some(at) = rest.find(&needle) {
        let after = &rest[at + needle.len()..];
        let ends = after
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || ";&|)".contains(c));
        out.push_str(&rest[..at]);

        if !ends {
            out.push_str(&needle);
        }

        rest = after;
    }

    out.push_str(rest);
    out
}

/// A failure no variant changes: a missing command or a refused permission
fn deterministic(log: &str) -> bool {
    [
        "command not found",
        ": not found",
        "not installed",
        "Operation not permitted",
        "EPERM",
    ]
    .iter()
    .any(|w| log.contains(w))
}

/// The command a failure names as missing, as bash, zsh and dash print it
fn missing_tool(log: &str) -> Option<String> {
    log.lines()
        .find_map(|line| {
            let line = line.trim();

            if let Some((_, tool)) = line.split_once("command not found: ") {
                return tool.split_whitespace().next();
            }

            let before = line
                .strip_suffix(": command not found")
                .or_else(|| line.strip_suffix(": not found"))?;
            before.rsplit(": ").next()
        })
        .filter(|t| {
            !t.is_empty()
                && t.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._+-/".contains(c))
        })
        .map(str::to_string)
}

/// Why a job stays on GitHub after its check failed: the end of the log, and the fix when the log names one
fn reason(log: &str) -> String {
    let lines: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(6)..]
        .iter()
        .map(|l| format!("\n      {}", l.trim_end()))
        .collect::<String>();
    let fix = missing_tool(log)
        .map(|t| format!("\n    fix: install {t}"))
        .unwrap_or_default();
    format!("failed when init tried it:{tail}{fix}")
}

/// The jobs that stay on GitHub for PRs: the ones shipcheck cannot run here, and the ones whose check failed when init tried it
fn kept(root: &Path, failed: &[(String, String, String)]) -> Vec<setup::Kept> {
    let mut kept = setup::detect(root).github;

    for (file, job, why) in failed {
        let source =
            fs::read_to_string(root.join(".github/workflows").join(file)).unwrap_or_default();
        let checks = crate::workflow::job_runs(&source)
            .into_iter()
            .find(|j| j.id == *job)
            .and_then(|j| j.check_names);
        kept.push(setup::Kept {
            file: file.clone(),
            job: job.clone(),
            why: why.clone(),
            checks,
            act: None,
        });
    }

    kept
}

impl Plan {
    /// The choices init makes without asking
    pub fn recommended(&self) -> Answers {
        Answers {
            config: true,
            workflows: vec![true; self.workflows.len()],
            open_app: self.app == App::Hosted,
            ruleset: self.ruleset.is_some(),
        }
    }

    pub fn steps(&self) -> Vec<Step> {
        let mut steps = Vec::new();

        if self.config.is_some() {
            steps.push(Step::Config);
        }

        steps.extend((0..self.workflows.len()).map(Step::Workflow));

        if self.app == App::Hosted {
            steps.push(Step::App);
        }

        if self.ruleset.is_some() {
            steps.push(Step::Ruleset);
        }

        steps
    }

    pub fn title(&self, step: Step) -> String {
        match step {
            Step::Config => "shipcheck.toml".into(),
            Step::Workflow(i) => self.workflows[i].file.clone(),
            Step::App => "GitHub App".into(),
            Step::Ruleset => "Branch protection".into(),
        }
    }

    /// The step's effect with these answers, in a few words
    pub fn effect(&self, step: Step, a: &Answers) -> String {
        match step {
            Step::Config if a.config => "write it, with the checks above".into(),
            Step::Config => "do not write it, and set nothing else up".into(),
            Step::Workflow(i) if a.workflows[i] => self.workflows[i].short_yes.clone(),
            Step::Workflow(i) => self.workflows[i].short_no.clone(),
            Step::App if a.open_app => "open the install page in the browser".into(),
            Step::App => "print the install link only".into(),
            Step::Ruleset if a.ruleset => "PRs only, and they need the checks above".into(),
            Step::Ruleset => "left unprotected".into(),
        }
    }

    /// The numbered review, one line per step, then what init always does
    pub fn review(&self, a: &Answers) -> Vec<String> {
        let steps = self.steps();
        let width = steps
            .iter()
            .map(|s| self.title(*s).len())
            .max()
            .unwrap_or(0);
        let mut lines: Vec<String> = steps
            .iter()
            .enumerate()
            .map(|(n, s)| {
                format!(
                    "{}  {:width$}  {}",
                    n + 1,
                    self.title(*s),
                    self.effect(*s, a)
                )
            })
            .collect();

        if let Some(checks) = &self.existing {
            lines.insert(0, format!("shipcheck.toml exists, init keeps it: {checks}"));
        }

        lines.extend(self.notes.iter().cloned());
        lines.push("Always: git hooks, the daemon, AGENTS.md, and this machine signs in".into());
        lines
    }

    /// What a step shows before its question, with diffs indented
    pub fn details(&self, step: Step, s: Style) -> Vec<String> {
        match step {
            Step::Config => {
                let said = self.config.as_ref().map_or("", |(_, said)| said);
                vec![format!("init writes shipcheck.toml with {said}")]
            }
            Step::Workflow(i) => {
                let w = &self.workflows[i];
                let mut out = vec![format!("{}: {}", w.file, w.yes)];
                out.push(s.dim(&format!("Leave it as is and {}", w.no)));
                out.extend(w.diff.lines().map(|l| format!("  {}", s.diff(l))));
                out
            }
            Step::App => vec![
                "The shipcheck App posts the checks. Install it on this repository:".into(),
                s.dim(&install_url()),
            ],
            Step::Ruleset => {
                let Some(r) = &self.ruleset else {
                    return Vec::new();
                };
                let mut out = vec![
                    "A ruleset on the default branch: no direct pushes, only PRs, and a PR merges once its branch is up to date and these checks pass:".to_string(),
                ];
                out.extend(r.names.iter().map(|n| format!("  {n}")));

                if !r.github.is_empty() {
                    out.push("And from GitHub Actions, for the jobs that stay on GitHub:".into());
                    out.extend(r.github.iter().map(|n| format!("  {n}")));
                }

                out.extend(r.notes.iter().map(|n| format!("{} {n}", s.warn())));
                out.push(s.yellow(
                    "Install the App before the first PR, since a required check that nobody posts blocks it",
                ));
                out.push(s.dim(PROTECTION_DOCS));
                out
            }
        }
    }

    pub fn question(&self, step: Step) -> String {
        match step {
            Step::Config => "Write shipcheck.toml?".into(),
            Step::Workflow(i) => {
                let w = &self.workflows[i];
                format!("Change {}?", w.file)
            }
            Step::App => "Open the install page in the browser when init applies?".into(),
            Step::Ruleset => "Protect the default branch, using your `gh` login?".into(),
        }
    }

    pub fn default(&self, step: Step) -> bool {
        let a = self.recommended();
        self.answer(step, &a)
    }

    pub fn answer(&self, step: Step, a: &Answers) -> bool {
        match step {
            Step::Config => a.config,
            Step::Workflow(i) => a.workflows[i],
            Step::App => a.open_app,
            Step::Ruleset => a.ruleset,
        }
    }

    pub fn set(&self, step: Step, a: &mut Answers, yes: bool) {
        match step {
            Step::Config => a.config = yes,
            Step::Workflow(i) => a.workflows[i] = yes,
            Step::App => a.open_app = yes,
            Step::Ruleset => a.ruleset = yes,
        }
    }
}

/// Where apply reports: a spinner at a terminal, lines otherwise
pub trait Report {
    fn start(&mut self, what: &str);
    fn done(&mut self, ok: bool, line: &str);
}

/// What `init` does after the repository's files, run by the caller: instructions, hooks, daemon, sign-in
pub type Finish<'a> = &'a mut dyn FnMut(&mut dyn FnMut(&str)) -> Result<(), String>;

/**
 * Writes the answered plan in order and returns the next steps for the person. Without a
 * shipcheck.toml it stops after the first step, since the rest is for checks the repository lacks
 **/
pub fn apply(
    root: &Path,
    plan: &Plan,
    a: &Answers,
    finish: Finish,
    r: &mut dyn Report,
) -> Result<Vec<String>, String> {
    let mut next = Vec::new();
    let mut wrote = false;

    if let Some((text, _)) = &plan.config {
        if !a.config {
            r.done(false, "no shipcheck.toml written, so init set nothing up");
            return Ok(next);
        }

        r.start("writing shipcheck.toml");
        crate::agents::write_in_repo(root, &root.join("shipcheck.toml"), text)?;
        r.done(true, "wrote shipcheck.toml");
        wrote = true;
    }

    for (w, yes) in plan.workflows.iter().zip(&a.workflows) {
        if !yes {
            continue;
        }

        let path = format!(".github/workflows/{}", w.file);
        r.start(&format!("writing {path}"));

        match crate::agents::write_in_repo(root, &root.join(&path), &w.edited) {
            Ok(()) => {
                r.done(true, &format!("wrote {path}"));
                wrote = true;
            }
            Err(e) => r.done(false, &e),
        }
    }

    finish(&mut |lines| {
        for line in lines.lines() {
            let attention = line.starts_with("not ")
                || line.starts_with("could not")
                || line.contains(" not installed");
            r.done(!attention, line);
        }
    })?;

    match &plan.app {
        App::Hosted if a.open_app => {
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            let opened = Command::new(opener)
                .arg(install_url())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            r.done(
                opened,
                &format!(
                    "{} the App install page: {}",
                    if opened { "opened" } else { "could not open" },
                    install_url()
                ),
            );
            next.push(format!(
                "Install the shipcheck App on this repository, if the page did not finish it: {}",
                install_url()
            ));
        }
        App::Hosted => next.push(format!(
            "Install the shipcheck App on this repository: {}",
            install_url()
        )),
        App::Own(id) => next.push(format!(
            "SHIPCHECK_APP_ID is {id}: install that App on this repository from its settings on GitHub"
        )),
        App::NotPublic => next.push("The hosted shipcheck App is not public yet. Until it is, the check needs an App of your own: set SHIPCHECK_APP_ID and put its key at app.pem in shipcheck's home (https://shipcheck.run/docs/install/#what-works-without-the-github-app)".into()),
    }

    if let Some(rule) = &plan.ruleset {
        if a.ruleset {
            r.start("creating the ruleset");

            match create_ruleset(root, rule) {
                Ok(()) => r.done(
                    true,
                    "protected the default branch with the ruleset `shipcheck`",
                ),
                Err(e) => {
                    r.done(false, &e);
                    next.push(format!(
                        "Protect the default branch by hand: {PROTECTION_DOCS}"
                    ));
                }
            }
        } else {
            next.push(format!(
                "Protect the default branch: run `shipcheck init` again, or see {PROTECTION_DOCS}"
            ));
        }
    }

    if wrote {
        next.insert(
            0,
            format!(
                "Review shipcheck.toml, the workflow changes and AGENTS.md, commit them, and merge to {}",
                plan.base
            ),
        );
    } else {
        next.insert(0, "Commit AGENTS.md if init changed it".into());
    }

    next.push("On another machine, `shipcheck install` sets it up and signs it in".into());
    Ok(next)
}

/// Report lines, with marks, to a writer
pub struct Lines<'a> {
    pub out: &'a mut dyn Write,
    pub style: Style,
}

impl Report for Lines<'_> {
    fn start(&mut self, _: &str) {}

    fn done(&mut self, ok: bool, line: &str) {
        let mark = if ok {
            self.style.ok()
        } else {
            self.style.warn()
        };
        let _ = writeln!(self.out, "  {mark} {line}");
    }
}

/// `init --yes`: no questions, the plan in plain text and the recommended choices applied
pub fn plain(root: &Path, app: App, out: &mut dyn Write, finish: Finish) -> Result<bool, String> {
    let s = Style::stdout();
    let Some((plan, a)) = show(
        root,
        app,
        out,
        "--yes takes the recommended choice of each step",
    )?
    else {
        return Ok(false);
    };
    let _ = writeln!(out, "\n{}", s.bold("Applying"));
    let next = apply(root, &plan, &a, finish, &mut Lines { out, style: s })?;
    next_steps(out, s, &next);
    Ok(true)
}

/// Set in init's preview process, whose trials then leave the repository's worktree entries alone
pub const PREVIEW: &str = "SHIPCHECK_INIT_PREVIEW";

/// The child's pid while `preview_apart` waits on it, for the signal handler
static PREVIEW_CHILD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn forward(sig: libc::c_int) {
    let pid = PREVIEW_CHILD.load(std::sync::atomic::Ordering::SeqCst);

    if pid > 0 {
        // Safe: kill only sends a signal
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/**
 * `init` with no terminal and no `--yes`: the plan `--yes` would apply, and nothing written. The
 * preview runs as its own process with a scratch home, since a trial leaves templates and slots
 * in the home it uses and git worktree entries in the repository. This process waits, passing on
 * SIGINT, SIGTERM and SIGHUP, then removes those entries and the scratch home however the preview
 * ended. The exit code is the preview's, or 128 plus the signal that ended it
 **/
pub fn preview_apart(root: &Path) -> Result<u8, String> {
    use std::os::unix::process::ExitStatusExt;

    let scratch = tempfile::tempdir().map_err(|e| e.to_string())?;
    let had = worktrees(root).is_some_and(|w| w.exists());
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = std::process::Command::new(exe)
        .arg("init")
        .current_dir(root)
        .env("SHIPCHECK_HOME", scratch.path())
        .env(PREVIEW, "1")
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("shipcheck: {e}"))?;
    PREVIEW_CHILD.store(child.id() as i32, std::sync::atomic::Ordering::SeqCst);
    let signals = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

    // Safe: the handler calls only kill and reads an atomic
    for sig in signals {
        unsafe {
            libc::signal(sig, forward as *const () as libc::sighandler_t);
        }
    }

    let status = child.wait();

    for sig in signals {
        // Safe: back to the default action
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
        }
    }

    PREVIEW_CHILD.store(0, std::sync::atomic::Ordering::SeqCst);
    forget_worktrees(root, scratch.path(), had);
    drop(scratch);
    let status = status.map_err(|e| e.to_string())?;
    Ok(match (status.code(), status.signal()) {
        (Some(code), _) => code.clamp(0, 255) as u8,
        (None, Some(sig)) => (128 + sig).clamp(0, 255) as u8,
        _ => 1,
    })
}

/// The preview itself, in the process `preview_apart` starts
pub fn preview(root: &Path, app: App, out: &mut dyn Write) -> Result<(), String> {
    if show(
        root,
        app,
        out,
        "the recommended choice of each step, which --yes applies",
    )?
    .is_some()
    {
        let _ = writeln!(out, "\nnothing written: rerun with --yes to apply");
    }

    Ok(())
}

/// The repository's folder of git worktree entries
fn worktrees(root: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).join("worktrees"))
}

/// Removes the repository's git worktree entries for checkouts under `dir`, and no other, and the folder when it had none
fn forget_worktrees(root: &Path, dir: &Path, had: bool) {
    let Some(worktrees) = worktrees(root) else {
        return;
    };
    let dirs = [dir.to_path_buf(), dir.canonicalize().unwrap_or_default()];

    for entry in fs::read_dir(&worktrees).into_iter().flatten().flatten() {
        let gitdir = fs::read_to_string(entry.path().join("gitdir")).unwrap_or_default();
        let under =
            |d: &PathBuf| !d.as_os_str().is_empty() && Path::new(gitdir.trim()).starts_with(d);

        if dirs.iter().any(under) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }

    // remove_dir removes only an empty folder
    if !had {
        let _ = fs::remove_dir(&worktrees);
    }
}

/// Prints the plan, each step and the review of its recommended choices. None when init stops
fn show(
    root: &Path,
    app: App,
    out: &mut dyn Write,
    intro: &str,
) -> Result<Option<(Plan, Answers)>, String> {
    let s = Style::stdout();
    let _ = writeln!(out, "{}", s.bold("shipcheck init"));
    let plan = plan(root, app, &mut Lines { out, style: s })?;

    if let Some(why) = &plan.stop {
        let _ = writeln!(out, "\n{why}");
        return Ok(None);
    }

    let _ = writeln!(out, "{}", s.dim(intro));

    for (n, step) in plan.steps().into_iter().enumerate() {
        let _ = writeln!(
            out,
            "\n{}",
            s.bold(&format!("{}. {}", n + 1, plan.title(step)))
        );

        for line in plan.details(step, s) {
            let _ = writeln!(out, "  {line}");
        }
    }

    // A ruleset changes who can push and a browser needs a person, so only the wizard does them
    let a = Answers {
        ruleset: false,
        open_app: false,
        ..plan.recommended()
    };
    let _ = writeln!(out, "\n{}", s.bold("Review"));

    for line in plan.review(&a) {
        let _ = writeln!(out, "  {line}");
    }

    Ok(Some((plan, a)))
}

fn next_steps(out: &mut dyn Write, s: Style, next: &[String]) {
    let _ = writeln!(out, "\n{}", s.bold("Next"));

    for (n, line) in next.iter().enumerate() {
        let _ = writeln!(out, "  {}. {line}", n + 1);
    }
}

/// A spinner per change, then a marked line, at a terminal
struct Spinners {
    spinner: Option<cliclack::ProgressBar>,
    style: Style,
}

impl Report for Spinners {
    fn start(&mut self, what: &str) {
        if let Some(old) = self.spinner.take() {
            old.clear();
        }

        let spinner = cliclack::spinner();
        spinner.start(what);
        self.spinner = Some(spinner);
    }

    fn done(&mut self, ok: bool, line: &str) {
        if let Some(spinner) = self.spinner.take() {
            spinner.clear();
        }

        let mark = if ok {
            self.style.ok()
        } else {
            self.style.warn()
        };
        let _ = cliclack::log::remark(format!("{mark} {line}"));
    }
}

/// What quitting leaves: the trials' clones stay in shipcheck's cache, where they speed up later runs
const QUIT: &str =
    "Quit, nothing written to the repository. Trial clones stay in shipcheck's cache";

/**
 * The terminal wizard: QuickStart or every step, a review that can reopen a step, then apply.
 * Nothing is written before apply, and Ctrl-C or quit leaves everything as it was
 **/
pub fn tui(root: &Path, find: fn() -> App, finish: Finish) -> Result<bool, String> {
    match wizard(root, find, finish) {
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
            let _ = cliclack::outro_cancel(QUIT);
            Ok(false)
        }
        Err(e) => Err(e.to_string()),
        Ok(result) => result,
    }
}

fn wizard(root: &Path, find: fn() -> App, finish: Finish) -> std::io::Result<Result<bool, String>> {
    let s = Style::stderr();
    cliclack::intro(s.bold(" shipcheck init "))?;
    cliclack::log::remark(s.dim(
        "Sets this repository up so its PRs get their checks from this machine. Nothing changes until you apply",
    ))?;
    let mut report = Spinners {
        spinner: None,
        style: s,
    };
    report.start("Reading the workflows");
    let plan = plan(root, find(), &mut report);

    if let Some(spinner) = report.spinner.take() {
        match &plan {
            Ok(_) => spinner.stop("Read the workflows"),
            Err(e) => spinner.error(e),
        }
    }

    let plan = match plan {
        Ok(plan) => plan,
        Err(e) => return Ok(Err(e)),
    };

    if let Some(why) = &plan.stop {
        cliclack::outro(why)?;
        return Ok(Ok(false));
    }

    // Every choice is made in the tabs, and nothing is written before Apply on the Review tab
    let Some((plan, a)) = crate::tabs::run(root, plan)? else {
        cliclack::outro_cancel(QUIT)?;
        return Ok(Ok(false));
    };

    let next = match apply(root, &plan, &a, finish, &mut report) {
        Ok(next) => next,
        Err(e) => {
            cliclack::outro_cancel(format!("{} {e}", s.fail()))?;
            return Ok(Err(e));
        }
    };
    let next: Vec<String> = next
        .iter()
        .enumerate()
        .map(|(n, l)| format!("{}. {l}", n + 1))
        .collect();
    cliclack::outro_note(s.bold("Next"), next.join("\n"))?;
    Ok(Ok(true))
}

/// The base the daemon, the hooks and `shipcheck run` use, as a branch name. Offline it refuses to guess
fn base_branch(root: &Path) -> Result<String, String> {
    let base = crate::daemon::known_base(root).ok_or(
        "origin did not say its default branch, so init cannot tell which branch to gate. Run `shipcheck init` again when online",
    )?;
    Ok(base.strip_prefix("origin/").unwrap_or(&base).to_string())
}

/// One job of a workflow: the shipcheck checks that run its commands, and the commands none runs
struct Cover {
    job: String,
    by: Vec<String>,
    missing: Vec<String>,
}

/**
 * Which checks of `cfg` run each job of a workflow. In act mode a listed workflow runs whole. In
 * host mode a job is covered when every command it checks with is a part of some check's command,
 * in the job's directory when it has one. Check names do not count, since a person names them
 **/
fn coverage(
    root: &Path,
    cfg: &RepoConfig,
    file: &str,
    jobs: &[crate::workflow::Job],
) -> Vec<Cover> {
    let names = crate::run::expected_jobs(cfg, |f| {
        fs::read_to_string(root.join(".github/workflows").join(f)).ok()
    })
    .map(|jobs| crate::check::job_names(&jobs))
    .unwrap_or_default();
    let name = |f: &str, job: &str| {
        names
            .iter()
            .find(|(nf, nj, _)| nf == f && nj == job)
            .map_or_else(
                || format!("{}/{job}", crate::check::NAME),
                |(_, _, n)| n.clone(),
            )
    };

    // act runs it whole, as a listed workflow's job or one `[jobs]` names
    let on_act = |id: &str| {
        names
            .iter()
            .find(|(f, j, _)| f == file && j == id)
            .map(|(_, _, n)| n.clone())
    };

    if cfg.runner != RunnerKind::Host {
        return jobs
            .iter()
            .map(|j| {
                let by: Vec<String> = on_act(&j.id).into_iter().collect();
                Cover {
                    job: j.id.clone(),
                    missing: if by.is_empty() {
                        vec!["shipcheck.toml does not list the workflow".into()]
                    } else {
                        Vec::new()
                    },
                    by,
                }
            })
            .collect();
    }

    let checks: Vec<(String, &String, Vec<String>)> = cfg
        .host
        .checks
        .iter()
        .map(|(key, command)| {
            (
                name(crate::runner::host::HOST, key),
                command,
                setup::commands(command),
            )
        })
        .collect();

    jobs.iter()
        .map(|job| {
            if let Some(n) = on_act(&job.id) {
                return Cover {
                    job: job.id.clone(),
                    by: vec![n],
                    missing: Vec::new(),
                };
            }

            let mut cover = Cover {
                job: job.id.clone(),
                by: Vec::new(),
                missing: Vec::new(),
            };
            let commands = setup::job_commands(job);

            if commands.is_empty() {
                cover
                    .missing
                    .push("init found no command in it to compare".into());
            } else if let Some(step) = setup::dropped_step(job) {
                // No check runs the step, so no check stands for the job
                cover.missing.push(format!("no check runs its {step}"));
            }

            for (dir, command) in commands {
                let hits: Vec<&String> = checks
                    .iter()
                    .filter(|(_, whole, parts)| {
                        // init drops a forced color flag when only that makes the check pass
                        (parts.contains(&command) || parts.contains(&without_color(&command)))
                            && dir
                                .as_ref()
                                .is_none_or(|d| whole.contains(&format!("cd {d}")))
                    })
                    .map(|(n, _, _)| n)
                    .collect();

                if hits.is_empty() {
                    cover.missing.push(match &dir {
                        Some(d) => format!("`{command}` in {d}"),
                        None => format!("`{command}`"),
                    });
                }

                for n in hits {
                    if !cover.by.contains(n) {
                        cover.by.push(n.clone());
                    }
                }
            }

            cover
        })
        .collect()
}

/// The workflow edits for the jobs shipcheck checks, and notes for the workflows left alone
fn workflows(root: &Path, cfg: &RepoConfig, plan: &mut Plan) -> Result<(), String> {
    let base = plan.base.clone();
    // What the person placed, else what this machine can run
    let kept = match &plan.detected {
        Some(d) => d.github.clone(),
        None => kept(root, &plan.failed),
    };
    // A job the config runs with act is shipcheck's, whatever this machine could run
    let on_act = crate::run::expected_jobs(cfg, |f| {
        fs::read_to_string(root.join(".github/workflows").join(f)).ok()
    })
    .unwrap_or_default();
    let mut offered = false;

    for file in setup::workflow_files(root).unwrap_or_default() {
        let path = root.join(".github/workflows").join(&file);
        let source = fs::read_to_string(&path).map_err(|e| format!("{file}: {e}"))?;
        let jobs = crate::workflow::job_runs(&source);
        let covers = coverage(root, cfg, &file, &jobs);
        let staying: Vec<&str> = kept
            .iter()
            .filter(|k| k.file == file)
            .filter(|k| !on_act.get(&file).is_some_and(|jobs| jobs.contains(&k.job)))
            .map(|k| k.job.as_str())
            .collect();

        if !crate::workflow::runs_for_branches(&source) {
            continue;
        }

        offered = true;
        gate(&file, &source, &jobs, &covers, &staying, &base, plan);
    }

    if !offered {
        plan.notes
            .push("no workflow runs on feature branches".into());
    }

    Ok(())
}

/**
 * A workflow keeps its triggers, and the jobs shipcheck checks run on GitHub only on the base
 * branch and for a fork's PR, which the daemon never runs. Cutting `on:` instead would leave a
 * fork's PR with no check. A job that a job left on GitHub needs is not gated, or GitHub would skip
 * that job too
 **/
fn gate(
    file: &str,
    source: &str,
    jobs: &[crate::workflow::Job],
    covers: &[Cover],
    staying: &[&str],
    base: &str,
    plan: &mut Plan,
) {
    let local = |id: &str| covers.iter().any(|c| c.job == id && c.missing.is_empty());
    let mut ungated: BTreeSet<String> = jobs
        .iter()
        .map(|j| j.id.clone())
        .filter(|id| !local(id) || staying.contains(&id.as_str()))
        .collect();

    // A gated job still runs on the base branch, so a job that runs only there may need it
    let base_only = crate::setup::base_only(jobs);

    // Until no job left on GitHub for PRs needs a gated one
    while let Some(need) = jobs
        .iter()
        .filter(|j| ungated.contains(&j.id) && !base_only.contains(&j.id))
        .flat_map(|j| j.needs.iter())
        .find(|n| !ungated.contains(*n))
        .cloned()
    {
        ungated.insert(need);
    }

    let gated: Vec<String> = jobs
        .iter()
        .map(|j| j.id.clone())
        .filter(|id| !ungated.contains(id))
        .collect();
    let stays: Vec<&crate::workflow::Job> = jobs
        .iter()
        .filter(|j| ungated.contains(&j.id) && !base_only.contains(&j.id))
        .collect();
    plan.github.extend(
        stays
            .iter()
            .map(|j| (file.to_string(), j.id.clone(), j.check_names.clone())),
    );
    let stay = stays
        .iter()
        .map(|j| j.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    let on_prs = crate::workflow::runs_on_prs(source);

    if gated.is_empty() {
        plan.notes.push(if on_prs {
            format!(
                "{file}: GitHub keeps running it on PRs, for {stay}, and no job of it runs only in shipcheck"
            )
        } else {
            // Its branch pushes are no PR check, and no shipcheck check takes their place
            format!(
                "{file}: GitHub keeps running it on pushes to every branch, and no shipcheck check runs its jobs"
            )
        });
        return;
    }

    // init is init: a workflow an earlier init gated is the person's now, and doctor reports drift
    let before = crate::workflow::gated_jobs(source);

    if !before.is_empty() {
        plan.notes.push(format!(
            "{file}: an earlier init gated {}, so init leaves it as it is",
            before.join(", ")
        ));
        return;
    }

    let Some(edited) = crate::workflow::gate_jobs(source, &gated, base) else {
        plan.notes.push(format!(
            "{file}: a job has a form shipcheck does not edit. To run {} on GitHub only where shipcheck does not, give each `if: {gate}` by hand, or `if: (<its own>) && {gate}`",
            gated.join(", "),
            gate = crate::workflow::gate_condition(source, base)
        ));
        return;
    };
    let by: Vec<String> = covers
        .iter()
        .filter(|c| gated.contains(&c.job))
        .flat_map(|c| c.by.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    plan.workflows.push(Change {
        file: file.to_string(),
        diff: diff(file, source, &edited),
        yes: if stay.is_empty() {
            format!(
                "on PRs shipcheck runs {}, GitHub keeps running {file} on pushes to {base}{}",
                by.join(", "),
                elsewhere(source, base)
            )
        } else {
            format!(
                "on PRs shipcheck runs {}, GitHub keeps running {stay} on PRs and all of {file} on pushes to {base}{}",
                by.join(", "),
                elsewhere(source, base)
            )
        },
        no: if on_prs {
            format!("GitHub keeps running all of {file} on PRs, beside shipcheck's checks")
        } else {
            format!("GitHub keeps running {file} on pushes to every branch")
        },
        short_yes: if stay.is_empty() {
            format!("PRs: {}, {base} and forks: GitHub", by.join(", "))
        } else {
            format!("PRs: {}, GitHub {stay}, {base} and forks: GitHub", by.join(", "))
        },
        short_no: format!("PRs and {base}: GitHub, as now"),
        edited,
    });
}

/// Where else GitHub keeps running the gated jobs, since shipcheck runs only PRs into the base
fn elsewhere(source: &str, base: &str) -> String {
    let (pushes, prs) = crate::workflow::other_branches(source, base);
    let (kept, dropped): (Vec<String>, Vec<String>) = pushes
        .into_iter()
        .partition(|p| crate::workflow::push_condition(p).is_some());
    let mut more: Vec<String> = Vec::new();

    if !kept.is_empty() {
        more.push(format!("on pushes to {}", kept.join(", ")));
    }

    if prs {
        more.push("on PRs into other branches".to_string());
    }

    more.push("on fork PRs".to_string());
    let lost = if dropped.is_empty() {
        String::new()
    } else {
        format!(
            ". Not on pushes to {}, since no `if:` can match that pattern",
            dropped.join(", ")
        )
    };
    format!(", and {}{lost}", more.join(" and "))
}

/// The changed lines between the first and the last that differ
pub fn diff(file: &str, old: &str, new: &str) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    // Longest common subsequence from the end, so each edit shows as its own hunk
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];

    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut out = vec![
        format!("--- a/.github/workflows/{file}"),
        format!("+++ b/.github/workflows/{file}"),
    ];
    let (mut i, mut j) = (0, 0);

    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
            continue;
        }

        out.push(format!("@@ -{} +{} @@", i + 1, j + 1));
        let mut removed = Vec::new();

        while i < a.len() || j < b.len() {
            if i < a.len() && j < b.len() && a[i] == b[j] {
                break;
            }

            if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
                out.push(format!("+{}", b[j]));
                j += 1;
            } else {
                removed.push(format!("-{}", a[i]));
                i += 1;
            }
        }

        // Removed lines before added ones, as diff prints them
        let at = out.len() - out.iter().rev().take_while(|l| l.starts_with('+')).count();
        let added = out.split_off(at);
        out.extend(removed);
        out.extend(added);
    }

    out.join("\n")
}

/// The job check names of a shipcheck.toml, the checks `init` requires
pub fn job_checks(root: &Path, text: &str) -> Result<Vec<String>, String> {
    let cfg = RepoConfig::parse(text)?;
    let jobs = crate::run::expected_jobs(&cfg, |file| {
        fs::read_to_string(root.join(".github/workflows").join(file)).ok()
    })?;
    Ok(crate::check::job_names(&jobs)
        .into_iter()
        .map(|(_, _, name)| name)
        .collect())
}

/// The ruleset for the App's job checks and the checks of `stays`, the jobs gate left on GitHub for PRs
fn ruleset_plan(
    root: &Path,
    text: &str,
    app: &App,
    stays: &[(String, String, Option<Vec<String>>)],
    required: Option<&[String]>,
) -> Option<Ruleset> {
    let app = app.id().filter(|a| a.parse::<u64>().is_ok())?;
    // Without what the branch requires now, a new ruleset could leave a check out, so there is none
    let required = required?;
    let names = job_checks(root, text).ok().filter(|n| !n.is_empty())?;
    let mut github = Vec::new();
    let mut notes = Vec::new();

    for (file, job, checks) in stays {
        if let Some(checks) = checks {
            github.extend(checks.iter().cloned());
            continue;
        }

        // Names init cannot tell, from a matrix or an expression, stay required as the branch requires them now
        let source =
            fs::read_to_string(root.join(".github/workflows").join(file)).unwrap_or_default();
        let now: Vec<String> = required
            .iter()
            .filter(|c| crate::workflow::may_name(&source, job, c))
            .cloned()
            .collect();

        if now.is_empty() {
            notes.push(format!(
                "{file}, job {job}: its check names depend on its matrix or an expression, require them by hand once a PR shows them"
            ));
        }

        github.extend(now);
    }

    github.sort();
    github.dedup();
    Some(Ruleset {
        app,
        names,
        github,
        notes,
    })
}

/**
 * The checks `base` requires now, by rulesets and by branch protection, with the person's `gh`
 * login. None when GitHub did not answer, so the caller keeps the branch rules as they are. A
 * branch with no protection answers 404 "Branch not protected", which is no requirement
 **/
fn required_now(root: &Path, base: &str) -> Option<Vec<String>> {
    let get = |path: String| -> Option<Option<serde_json::Value>> {
        let out = Command::new("gh")
            .args(["api", &path])
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
            .ok()?;

        if out.status.success() {
            return serde_json::from_slice(&out.stdout).ok().map(Some);
        }

        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        said.contains("Branch not protected").then_some(None)
    };
    let mut out = Vec::new();

    if let Some(rules) = get(format!("repos/{{owner}}/{{repo}}/rules/branches/{base}"))? {
        out.extend(
            rules
                .as_array()
                .into_iter()
                .flatten()
                .filter(|r| r["type"] == "required_status_checks")
                .flat_map(|r| {
                    r["parameters"]["required_status_checks"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .filter_map(|c| c["context"].as_str().map(String::from)),
        );
    }

    if let Some(p) = get(format!(
        "repos/{{owner}}/{{repo}}/branches/{base}/protection/required_status_checks"
    ))? {
        out.extend(
            p["contexts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c.as_str().map(String::from)),
        );
    }

    Some(out)
}

/**
 * The ruleset branch-protection.md describes: each job check from the App, each check of a job
 * that stays on GitHub from GitHub Actions, branches up to date
 **/
pub fn ruleset(app_id: &str, names: &[String], github: &[String]) -> serde_json::Value {
    let id = app_id.parse::<u64>().unwrap_or_default();
    let checks: Vec<_> =
        names
            .iter()
            .map(|name| serde_json::json!({ "context": name, "integration_id": id }))
            .chain(github.iter().map(
                |name| serde_json::json!({ "context": name, "integration_id": ACTIONS_APP_ID }),
            ))
            .collect();
    serde_json::json!({
        "name": "shipcheck",
        "target": "branch",
        "enforcement": "active",
        "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
        "rules": [{
            "type": "required_status_checks",
            "parameters": {
                "strict_required_status_checks_policy": true,
                "do_not_enforce_on_create": false,
                "required_status_checks": checks
            }
        }, {
            "type": "pull_request",
            "parameters": {
                "required_approving_review_count": 0,
                "dismiss_stale_reviews_on_push": false,
                "require_code_owner_review": false,
                "require_last_push_approval": false,
                "required_review_thread_resolution": false
            }
        }]
    })
}

/// The repository's page for a new ruleset, else the docs that describe it
fn rules_url(root: &Path) -> String {
    crate::run::repo_id(root).map_or_else(
        |_| PROTECTION_DOCS.to_string(),
        |id| format!("https://github.com/{id}/settings/rules/new?target=branch"),
    )
}

/// Creates the ruleset with the person's own `gh` login, which shipcheck never stores
fn create_ruleset(root: &Path, rule: &Ruleset) -> Result<(), String> {
    let child = Command::new("gh")
        .args([
            "api",
            "--method",
            "POST",
            "repos/{owner}/{repo}/rulesets",
            "--input",
            "-",
        ])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let result = child.and_then(|mut c| {
        if let Some(mut stdin) = c.stdin.take() {
            stdin.write_all(
                ruleset(&rule.app, &rule.names, &rule.github)
                    .to_string()
                    .as_bytes(),
            )?;
        }

        c.wait_with_output()
    });

    match result {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let why = if ["403", "404", "Not Found", "Resource not accessible"]
                .iter()
                .any(|w| err.contains(w))
            {
                "your `gh` login has no admin rights on this repository".to_string()
            } else {
                format!("gh: {}", err.trim())
            };
            Err(format!(
                "could not protect the default branch, {why}. Set it up by hand at {}",
                rules_url(root)
            ))
        }
        Err(e) => Err(format!(
            "could not run gh: {e}. Set the ruleset up by hand at {}",
            rules_url(root)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();

        for (name, text) in files {
            fs::create_dir_all(dir.path().join(name).parent().unwrap()).unwrap();
            fs::write(dir.path().join(name), text).unwrap();
        }

        dir
    }

    /// Collects what apply reports
    #[derive(Default)]
    struct Said(Vec<String>);

    impl Report for Said {
        fn start(&mut self, _: &str) {}

        fn done(&mut self, ok: bool, line: &str) {
            self.0
                .push(format!("{} {line}", if ok { "ok" } else { "!" }));
        }
    }

    fn finish_nothing(say: &mut dyn FnMut(&str)) -> Result<(), String> {
        say("wrote hooks");
        Ok(())
    }

    const CI: &str =
        "on:\n  pull_request:\n  push:\njobs:\n  test:\n    steps:\n      - run: cargo test\n";
    const LINT: &str =
        "on: [push, pull_request]\njobs:\n  lint:\n    steps:\n      - run: cargo clippy\n";

    // This repository's own files, from the init its author ran
    const SHIPCHECK_TOML: &str = include_str!("../tests/fixtures/own/shipcheck.toml");
    const OWN_CI: &str = include_str!("../tests/fixtures/own/ci.yml");
    const OWN_E2E: &str = include_str!("../tests/fixtures/own/e2e.yml");
    const OWN_LEAKS: &str = include_str!("../tests/fixtures/own/leaks.yml");

    /// OWN_CI without the step that sets AppArmor up with `sudo`, which keeps the test job on GitHub
    fn own_ci() -> String {
        let start = OWN_CI.find("      - timeout-minutes: 5").unwrap();
        let end = OWN_CI.find("      - run: cargo test").unwrap();
        format!("{}{}", &OWN_CI[..start], &OWN_CI[end..])
    }

    #[test]
    fn variants_drop_forced_color_then_turn_color_off() {
        assert_eq!(
            variants("cargo test --color=always && npx jest --color always"),
            [
                "cargo test && npx jest",
                "export NO_COLOR=1 FORCE_COLOR=0 PY_COLORS=0 TERM=dumb && cargo test && npx jest",
            ]
        );
        assert_eq!(
            variants("pytest -c always.ini"),
            ["export NO_COLOR=1 FORCE_COLOR=0 PY_COLORS=0 TERM=dumb && pytest -c always.ini"]
        );
        assert_eq!(variants("ls --color=yes; make -c always")[0], "ls; make");
    }

    #[test]
    fn a_missing_command_is_no_flake_and_names_its_fix() {
        for (log, tool) in [
            (
                "$ x\nbash: line 1: autoreconf: command not found\n",
                "autoreconf",
            ),
            ("zsh: command not found: pnpm", "pnpm"),
            ("sh: 1: pmccabe: not found", "pmccabe"),
        ] {
            assert!(deterministic(log), "{log}");
            assert_eq!(missing_tool(log).as_deref(), Some(tool), "{log}");
            assert!(
                reason(log).ends_with(&format!("fix: install {tool}")),
                "{log}"
            );
        }

        assert!(deterministic("open /dev/ptmx: Operation not permitted"));
        assert!(!deterministic("error: 2 tests failed"));
        assert_eq!(missing_tool("error: 2 tests failed"), None);
        let log = (1..=20).map(|n| format!("line {n}\n")).collect::<String>();
        let why = reason(&log);
        assert!(why.starts_with("failed when init tried it:"), "{why}");
        assert!(why.contains("line 15") && why.contains("line 20") && !why.contains("line 14"));
        assert!(!why.contains("fix:"));
    }

    #[test]
    fn coverage_compares_commands_not_names() {
        // ci.yml's test job, without its service job: fmt, clippy and test run its three commands
        let own = own_ci();
        let ci = own.split("\n  service:").next().unwrap();
        let dir = repo(&[
            (".github/workflows/ci.yml", ci),
            ("shipcheck.toml", SHIPCHECK_TOML),
        ]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        let ci = &plan.workflows[0];
        assert_eq!(
            ci.yes,
            "on PRs shipcheck runs shipcheck/clippy, shipcheck/fmt, shipcheck/test, GitHub keeps running ci.yml on pushes to main, and on PRs into other branches and on fork PRs"
        );
        // `on:` stays, so a fork's PR, which the daemon never runs, still gets the checks
        assert!(!ci.diff.contains("-  pull_request:"), "{}", ci.diff);
        assert!(ci.diff.contains("&& (github.ref == 'refs/heads/main' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\""), "{}", ci.diff);
    }

    #[test]
    fn a_check_of_part_of_a_job_does_not_cover_it() {
        // curl's checkdocs.yml proselint job: a check of its first step alone passed where proselint never ran
        let ci = "on:\n  pull_request:\n  push:\njobs:\n  proselint:\n    steps:\n      - run: trim.pl\n      - name: check prose\n        run: |\n          cat <<JSON > rc.json\n          {}\n          JSON\n          proselint check --config rc.json docs\n";
        let dir = repo(&[
            (".github/workflows/ci.yml", ci),
            (
                "shipcheck.toml",
                "runner = \"host\"\n[host]\nchecks = { proselint = \"trim.pl\" }\n",
            ),
        ]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        // Not gated: GitHub keeps running it on PRs
        assert!(plan.workflows.is_empty());
        assert_eq!(
            plan.notes,
            [
                "ci.yml: GitHub keeps running it on PRs, for proselint, and no job of it runs only in shipcheck"
            ]
        );
    }

    #[test]
    fn a_job_the_config_runs_with_act_is_gated_like_a_host_check() {
        let ci = "on:\n  pull_request:\n  push:\njobs:\n  lint:\n    steps:\n      - run: make lint\n  proselint:\n    steps:\n      - run: |\n          cat <<JSON > rc.json\n          {}\n          JSON\n          proselint check --config rc.json docs\n";
        let dir = repo(&[
            (".github/workflows/ci.yml", ci),
            (
                "shipcheck.toml",
                "runner = \"host\"\n[host]\nchecks = { lint = \"make lint\" }\n\n[jobs.\"ci.yml/proselint\"]\nrunner = \"act\"\n",
            ),
        ]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        let ci = &plan.workflows[0];
        assert!(
            ci.yes.starts_with("on PRs shipcheck runs shipcheck/lint, shipcheck/proselint, GitHub keeps running ci.yml on pushes to main"),
            "{}",
            ci.yes
        );
        // Both jobs keep the fork clause, so a fork's PR still runs them on GitHub
        assert_eq!(
            ci.diff
                .matches("github.event.pull_request.head.repo.fork")
                .count(),
            2,
            "{}",
            ci.diff
        );
        assert!(plan.github.is_empty(), "{:?}", plan.github);
    }

    #[test]
    fn a_job_whose_check_fails_its_trial_stays_on_github_untried_by_act() {
        let ci = "on: pull_request\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make lint\n  broken:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make fails-in-trial\n";
        let dir = repo(&[(".github/workflows/ci.yml", ci)]);
        let machine = setup::Machine {
            docker: true,
            ..setup::Machine::default()
        };
        let (text, said) = setup::propose(dir.path(), &machine).unwrap();
        let mut plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        plan.failed.clear();
        let (text, said) = super::tried(
            dir.path(),
            &machine,
            text.unwrap(),
            said,
            &mut Said::default(),
            &mut plan,
        )
        .unwrap()
        .unwrap();
        // act would run it without a trial, so it is written nowhere and GitHub keeps it
        assert!(!text.contains("broken"), "{text}");
        assert!(text.contains("lint = \"make lint\""), "{text}");
        assert_eq!(plan.failed.len(), 1, "{:?}", plan.failed);
        assert!(
            said.contains("ci.yml, job broken: failed when init tried it"),
            "{said}"
        );
    }

    #[test]
    fn coverage_names_the_jobs_no_check_runs() {
        let dir = repo(&[
            (".github/workflows/ci.yml", &own_ci()),
            (".github/workflows/e2e.yml", OWN_E2E),
            (".github/workflows/leaks.yml", OWN_LEAKS),
            ("shipcheck.toml", SHIPCHECK_TOML),
        ]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        let files: Vec<&str> = plan.workflows.iter().map(|w| w.file.as_str()).collect();
        // Without Docker, e2e.yml's matrix stays on GitHub, which tests see on every machine
        // leaks.yml's job has no command a check could run, so it stays on GitHub too
        assert_eq!(files, ["ci.yml"]);
        assert!(
            plan.notes[0].starts_with("e2e.yml: GitHub keeps running it on PRs"),
            "{:?}",
            plan.notes
        );
        assert_eq!(
            plan.notes[1],
            "leaks.yml: GitHub keeps running it on PRs, for gitleaks, and no job of it runs only in shipcheck"
        );
        // A job no check runs stays on GitHub ungated, so a PR still runs it there
        assert_eq!(
            plan.workflows[0].yes,
            "on PRs shipcheck runs shipcheck/clippy, shipcheck/fmt, shipcheck/test, GitHub keeps running service on PRs and all of ci.yml on pushes to main, and on PRs into other branches and on fork PRs"
        );
        assert!(plan.workflows[0].diff.contains("&& (github.ref == 'refs/heads/main' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\""));
    }

    #[test]
    fn apply_writes_only_what_is_answered() {
        let dir = repo(&[
            (".github/workflows/ci.yml", CI),
            (".github/workflows/lint.yml", LINT),
            (
                "shipcheck.toml",
                "runner = \"host\"\n[host.checks]\ntest = \"cargo test\"\nclippy = \"cargo clippy\"\n",
            ),
        ]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        let mut a = plan.recommended();
        assert_eq!(a.workflows, [true, true]);

        // Changing step 2, lint.yml, and back again, as the review's "Change a step" does
        plan.set(Step::Workflow(1), &mut a, false);
        assert!(
            plan.review(&a)[2].contains("PRs and main: GitHub, as now"),
            "{:?}",
            plan.review(&a)
        );
        plan.set(Step::Workflow(1), &mut a, true);
        assert!(plan.review(&a)[2].contains("PRs: shipcheck/clippy, main and forks: GitHub"));
        plan.set(Step::Workflow(1), &mut a, false);

        let mut said = Said::default();
        let next = apply(dir.path(), &plan, &a, &mut finish_nothing, &mut said).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
            CI.replace("  test:\n", "  test:\n    if: (github.ref == 'refs/heads/main' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\n")
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/lint.yml")).unwrap(),
            LINT
        );
        assert_eq!(
            said.0,
            ["ok wrote .github/workflows/ci.yml", "ok wrote hooks"]
        );
        assert!(next[0].starts_with("Review shipcheck.toml"), "{next:?}");
    }

    #[test]
    fn a_push_workflow_is_offered_only_when_a_check_runs_its_jobs() {
        const SETUP: &str =
            "on:\n  push:\n    paths: [x]\njobs:\n  setup:\n    steps:\n      - uses: a/b@v1\n";
        const PUSH: &str = "on: push\njobs:\n  test:\n    steps:\n      - run: cargo test\n";
        let dir = repo(&[
            (".github/workflows/ci.yml", PUSH),
            (".github/workflows/setup.yml", SETUP),
        ]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        let files: Vec<&str> = plan.workflows.iter().map(|w| w.file.as_str()).collect();
        assert_eq!(files, ["ci.yml"]);
        assert_eq!(
            plan.workflows[0].no,
            "GitHub keeps running ci.yml on pushes to every branch"
        );
        assert!(
            plan.notes.contains(&"setup.yml: GitHub keeps running it on pushes to every branch, and no shipcheck check runs its jobs".to_string()),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn plan_writes_nothing_and_a_declined_config_sets_nothing_up() {
        let dir = repo(&[(".github/workflows/ci.yml", CI)]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        assert!(!dir.path().join("shipcheck.toml").exists());
        assert_eq!(plan.steps()[0], Step::Config);
        assert_eq!(plan.recommended().workflows, [true]);

        let mut a = plan.recommended();
        a.config = false;
        let mut said = Said::default();
        let mut finished = false;
        apply(
            dir.path(),
            &plan,
            &a,
            &mut |_| {
                finished = true;
                Ok(())
            },
            &mut said,
        )
        .unwrap();
        assert!(!finished);
        assert!(!dir.path().join("shipcheck.toml").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
            CI
        );

        let mut said = Said::default();
        apply(
            dir.path(),
            &plan,
            &plan.recommended(),
            &mut finish_nothing,
            &mut said,
        )
        .unwrap();
        assert_eq!(said.0[0], "ok wrote shipcheck.toml");
        assert!(dir.path().join("shipcheck.toml").exists());
    }

    #[test]
    fn a_job_that_runs_only_on_main_keeps_no_pr_job_ungated() {
        let ci = crate::setup::tests::CI_WITH_RELEASE;
        let config = "runner = \"host\"\n[host.checks]\ntest = \"cargo test\"\n";
        let dir = repo(&[(".github/workflows/ci.yml", ci), ("shipcheck.toml", config)]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        let mut said = Said::default();
        apply(
            dir.path(),
            &plan,
            &plan.recommended(),
            &mut finish_nothing,
            &mut said,
        )
        .unwrap();
        // release needs test only on main, where the gate still runs it, and e2e stays on PRs
        assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
            ci.replace(
                "  test:\n",
                "  test:\n    if: (github.ref == 'refs/heads/main' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\n"
            ),
        );
        assert!(
            plan.workflows[0]
                .yes
                .contains("GitHub keeps running e2e on PRs"),
            "{}",
            plan.workflows[0].yes
        );
    }

    #[test]
    fn workflows_keeps_a_matrix_on_github_and_gates_the_rest() {
        let ci = "on:\n  pull_request:\n  push:\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make lint\n  tests:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        python: [\"3.12\", \"3.13\"]\n    steps:\n      - run: make test\n";
        let config = "runner = \"host\"\n[host.checks]\nlint = \"make lint\"\n";
        let dir = repo(&[(".github/workflows/ci.yml", ci), ("shipcheck.toml", config)]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        let mut said = Said::default();
        apply(
            dir.path(),
            &plan,
            &plan.recommended(),
            &mut finish_nothing,
            &mut said,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
            ci.replace(
                "  lint:\n",
                "  lint:\n    if: (github.ref == 'refs/heads/main' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\n"
            ),
        );
        assert_eq!(
            plan.workflows[0].yes,
            "on PRs shipcheck runs shipcheck/lint, GitHub keeps running tests on PRs and all of ci.yml on pushes to main, and on PRs into other branches and on fork PRs"
        );

        // The matrix needs lint, so lint stays on GitHub too and nothing is gated
        let needs = ci.replace("  tests:\n", "  tests:\n    needs: [lint]\n");
        let dir = repo(&[
            (".github/workflows/ci.yml", &needs),
            ("shipcheck.toml", config),
        ]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        assert!(plan.workflows.is_empty());
        assert!(
            plan.notes[0].contains("ci.yml: GitHub keeps running it on PRs, for lint, tests"),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn a_job_with_no_command_stays_and_the_rest_is_gated() {
        // rubocop's linting.yml, trimmed: yamllint is an action, so no host check can run it
        let lint = "on:\n  push:\n    branches: [main]\n  pull_request:\njobs:\n  ruby:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make lint\n  yaml:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: karancode/yamllint-github-action@v3.0.0\n";
        let config = "runner = \"host\"\n[host.checks]\nlint = \"make lint\"\n";
        let dir = repo(&[
            (".github/workflows/lint.yml", lint),
            ("shipcheck.toml", config),
        ]);
        let plan = super::plan_with(
            dir.path(),
            "main",
            App::Own("1".into()),
            Some(Vec::new()),
            &mut Said::default(),
        )
        .unwrap();
        let change = &plan.workflows[0];
        assert_eq!(
            Some(change.edited.clone()),
            crate::workflow::gate_jobs(lint, &["ruby".to_string()], "main")
        );
        // yaml stays on GitHub for PRs, so the ruleset requires its check from GitHub Actions
        assert_eq!(plan.ruleset.unwrap().github, ["yaml"]);
    }

    #[test]
    fn the_ruleset_requires_every_job_gate_leaves_on_github() {
        // A shipcheck.toml from an init whose trial failed test: test stays on GitHub, and release
        // runs only on main, so no PR waits for it
        let ci = "on: [push, pull_request]\njobs:\n  lint:\n    steps:\n      - run: make lint\n  test:\n    steps:\n      - run: make test\n  release:\n    if: github.ref == 'refs/heads/main'\n    steps:\n      - run: make release\n";
        let config = "runner = \"host\"\n[host.checks]\nlint = \"make lint\"\n";
        let dir = repo(&[(".github/workflows/ci.yml", ci), ("shipcheck.toml", config)]);
        let plan = super::plan_with(
            dir.path(),
            "main",
            App::Own("1".into()),
            Some(Vec::new()),
            &mut Said::default(),
        )
        .unwrap();
        let rule = plan.ruleset.unwrap();
        assert_eq!(rule.names, ["shipcheck/lint"]);
        assert_eq!(rule.github, ["test"]);
    }

    #[test]
    fn the_ruleset_keeps_the_required_checks_of_a_job_whose_names_init_cannot_tell() {
        // hynek/structlog, trimmed: the test matrix's names depend on its include, so init cannot tell them
        let ci = "on: [push, pull_request]\njobs:\n  colors:\n    steps:\n      - run: make colors\n  tests:\n    strategy:\n      matrix:\n        include:\n          - python: '3.12'\n    name: tests on ${{ matrix.python }}\n    steps:\n      - run: tox\n";
        let config = "runner = \"host\"\n[host.checks]\ncolors = \"make colors\"\n";
        let dir = repo(&[(".github/workflows/ci.yml", ci), ("shipcheck.toml", config)]);
        let required =
            ["tests on 3.12", "tests on 3.13", "shipcheck/colors", "docs"].map(String::from);
        let stays = [("ci.yml".to_string(), "tests".to_string(), None)];
        let rule = ruleset_plan(
            dir.path(),
            config,
            &App::Own("1".into()),
            &stays,
            Some(&required),
        )
        .unwrap();
        assert_eq!(rule.names, ["shipcheck/colors"]);
        // Only the checks of the job whose names init cannot tell, docs is no job of these
        assert_eq!(rule.github, ["tests on 3.12", "tests on 3.13"]);
        assert!(rule.notes.is_empty(), "{:?}", rule.notes);
        // GitHub did not say, so there is no ruleset to write and the branch keeps its rules
        let unread = ruleset_plan(dir.path(), config, &App::Own("1".into()), &stays, None);
        assert!(unread.is_none());
        let plan = super::plan_with(
            dir.path(),
            "main",
            App::Own("1".into()),
            None,
            &mut Said::default(),
        )
        .unwrap();
        assert!(plan.ruleset.is_none());
        let review = plan.review(&plan.recommended());
        assert!(review.iter().any(|l| l == KEPT_RULES), "{review:?}");
    }

    #[test]
    fn a_second_init_leaves_the_workflows_it_gated() {
        let config =
            "runner = \"host\"\n[host.checks]\ntest = \"cargo test\"\nclippy = \"cargo clippy\"\n";
        let dir = repo(&[
            (".github/workflows/ci.yml", CI),
            (".github/workflows/lint.yml", LINT),
            ("shipcheck.toml", config),
        ]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        let mut a = plan.recommended();
        a.workflows = vec![true, false];
        apply(
            dir.path(),
            &plan,
            &a,
            &mut finish_nothing,
            &mut Said::default(),
        )
        .unwrap();
        let gated = fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap();

        // lint.yml, declined the first time, is offered again, ci.yml is not touched
        let again =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        let files: Vec<&str> = again.workflows.iter().map(|w| w.file.as_str()).collect();
        assert_eq!(files, ["lint.yml"]);
        assert!(
            again
                .notes
                .contains(&"ci.yml: an earlier init gated test, so init leaves it as it is".into()),
            "{:?}",
            again.notes
        );
        apply(
            dir.path(),
            &again,
            &again.recommended(),
            &mut finish_nothing,
            &mut Said::default(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
            gated
        );
    }

    #[test]
    fn the_hand_edit_note_gives_the_gate_init_would_write() {
        let flow =
            "on: [push, pull_request]\njobs:\n  test: {runs-on: x, steps: [{run: cargo test}]}\n";
        let config = "runner = \"host\"\n[host.checks]\ntest = \"cargo test\"\n";
        let dir = repo(&[
            (".github/workflows/ci.yml", flow),
            ("shipcheck.toml", config),
        ]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        let gate = crate::workflow::gate_condition(flow, "main");
        let note = plan.notes.iter().find(|n| n.contains("by hand")).unwrap();
        assert!(note.contains(&format!("`if: {gate}`")), "{note}");
        assert_eq!(crate::workflow::ungate(&gate).as_deref(), Some(""));
    }

    #[test]
    fn a_version_that_does_not_run_is_left_out_with_a_warning() {
        let dir = repo(&[]);
        let text = "runner = \"host\"\n[host]\nversions = [\"rustc +missing --version\", \"cargo --version\"]\n[host.checks]\ntest = \"cargo test\"\n";
        let mut said = Said::default();
        let (tried, versions) = try_checks(dir.path(), text, &mut said).unwrap();
        assert_eq!(versions, ["cargo --version"]);
        assert!(tried["test"].is_ok());
        assert!(
            said.0.contains(
                &"! `rustc +missing --version` does not run, so shipcheck.toml leaves it out of versions".into()
            ),
            "{:?}",
            said.0
        );
    }

    #[test]
    fn init_gates_to_the_branch_runs_and_the_daemon_use() {
        let dir = repo(&[(".github/workflows/ci.yml", CI)]);
        let origin = tempfile::tempdir().unwrap();
        let git = |at: &Path, args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(at)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(origin.path(), &["init", "-q", "--bare", "-b", "master"]);
        git(dir.path(), &["init", "-q", "-b", "master"]);
        git(
            dir.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "a",
            ],
        );
        git(
            dir.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        );
        git(
            dir.path(),
            &["push", "-q", "origin", "master", "master:evil"],
        );
        git(dir.path(), &["fetch", "-q", "origin"]);

        // A local origin/HEAD at another branch does not pick the base, the remote does
        git(
            dir.path(),
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/evil",
            ],
        );
        assert_eq!(
            crate::run::remote_base(dir.path()).as_deref(),
            Some("origin/master")
        );
        let plan = super::plan(dir.path(), App::NotPublic, &mut Said::default()).unwrap();
        assert_eq!(plan.base, "master");
        assert!(plan.workflows[0].edited.contains("refs/heads/master"));

        // A remote that does not answer gives no base, never the local origin/HEAD, and init refuses
        git(dir.path(), &["remote", "set-url", "origin", "/nonexistent"]);
        assert_eq!(crate::run::remote_base(dir.path()), None);
        assert!(
            base_branch(dir.path())
                .unwrap_err()
                .contains("again when online")
        );
    }

    #[test]
    fn a_workflow_of_jobs_only_github_can_run_stays_as_it_is() {
        // orch's orch-review.yml, trimmed: on main its `if:` skips the job, so cutting it to main runs it nowhere
        let review = "on:\n  push:\njobs:\n  review-gate:\n    runs-on: ubuntu-latest\n    if: github.ref != format('refs/heads/{0}', github.event.repository.default_branch)\n    steps:\n      - run: ./scripts/lint-frontmatter.sh\n";
        let config = "runner = \"host\"\n[host.checks]\ntest = \"cargo test\"\n";
        let dir = repo(&[
            (".github/workflows/ci.yml", CI),
            (".github/workflows/orch-review.yml", review),
            ("shipcheck.toml", config),
        ]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        assert_eq!(plan.workflows.len(), 1);
        assert!(
            plan.notes.iter().any(|n| n
                .starts_with("orch-review.yml: GitHub keeps running it on pushes to every branch")),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn init_stops_without_anything_to_run_here() {
        // git-cow's ci.yml, trimmed: its only test job is a matrix with a macOS entry
        let ci = "on: [push, pull_request]\njobs:\n  test:\n    strategy:\n      matrix:\n        include:\n          - {os: macos-latest}\n          - {os: ubuntu-latest}\n    runs-on: ${{ matrix.os }}\n    steps:\n      - run: cargo test\n";
        let dir = repo(&[(".github/workflows/ci.yml", ci)]);
        let plan =
            super::plan_at(dir.path(), "main", App::NotPublic, &mut Said::default()).unwrap();
        assert!(plan.stop.as_deref().unwrap().contains(setup::NOTHING_LOCAL));
        assert!(!dir.path().join("shipcheck.toml").exists());
    }

    #[test]
    fn job_checks_reads_the_workflows() {
        let dir = repo(&[
            (".github/workflows/ci.yml", CI),
            (".github/workflows/lint.yml", LINT),
        ]);
        assert_eq!(
            job_checks(dir.path(), "workflows = [\"ci.yml\", \"lint.yml\"]\n").unwrap(),
            ["shipcheck/test", "shipcheck/lint"]
        );
    }

    #[test]
    fn ruleset_requires_the_apps_check_with_branches_up_to_date() {
        let r = ruleset(
            "5400635",
            &["shipcheck/lint".into(), "shipcheck/test".into()],
            &["MINGW64".into()],
        );
        let rule = &r["rules"][0]["parameters"];
        assert_eq!(rule["strict_required_status_checks_policy"], true);
        assert_eq!(
            rule["required_status_checks"],
            serde_json::json!([
                { "context": "shipcheck/lint", "integration_id": 5400635 },
                { "context": "shipcheck/test", "integration_id": 5400635 },
                { "context": "MINGW64", "integration_id": 15368 }
            ])
        );
        assert_eq!(r["conditions"]["ref_name"]["include"][0], "~DEFAULT_BRANCH");
        assert_eq!(r["rules"][1]["type"], "pull_request");
    }
}
