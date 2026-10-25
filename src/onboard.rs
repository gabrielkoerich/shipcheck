use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;
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
    /// Why yes is not the default, when it is not
    pub warning: Option<String>,
    pub recommended: bool,
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
    let path = root.join("shipcheck.toml");
    let mut plan = Plan {
        base: base_branch(root),
        config: None,
        existing: None,
        workflows: Vec::new(),
        notes: Vec::new(),
        app,
        ruleset: None,
        stop: None,
        failed: Vec::new(),
    };
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
        plan.existing = Some(match cfg.runner {
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
        });
    }

    workflows(root, &cfg, &mut plan)?;
    plan.ruleset = ruleset_plan(root, &text, &plan.app, &plan.failed);
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
    // ponytail: act mode writes its workflows untried, trying them needs Docker and act's own runs
    if RepoConfig::parse(&text)?.runner != RunnerKind::Host {
        return Ok(Some((text, said)));
    }

    let (results, versions) = try_checks(root, &text, r)?;
    let mut d = setup::detect_on(root, machine);
    let mut failed = Vec::new();
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

    for (from, why) in failed {
        // A check from project files has no job, so nothing stays on GitHub for it
        let Some((file, job)) = from.split_once(", job ") else {
            d.skipped.push(format!("{from}: {why}"));
            continue;
        };
        let job = job.split(", after job ").next().unwrap_or(job);
        plan.failed.push((file.into(), job.into(), why.clone()));
        d.github.push(setup::Kept {
            file: file.into(),
            job: job.into(),
            why,
            checks: None,
        });
    }

    if d.checks.is_empty() {
        plan.stop = Some(format!(
            "no check passed when init tried it, so init wrote no shipcheck.toml and set nothing up:\n{}",
            d.explain()
        ));
        return Ok(None);
    }

    Ok(Some((
        setup::host_config(&d.checks, &d.versions),
        format!("these host checks:\n{}", d.explain()),
    )))
}

/// Each check's command once it passed, or why it failed
type Tried = BTreeMap<String, Result<String, String>>;

/**
 * Tries each `versions` command, then each check, then each safe variant of a check that failed.
 * A version that fails would fail every run, so it is left out, and only the checks that start
 * its tool fail with it. Returns the results and the versions that answered
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
        .into_keys()
        .map(|name| {
            let t = crate::run::Trial {
                passed: true,
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

#[cfg(test)]
fn probe(_: &Path, _: &crate::config::Host, _: &str) -> Result<(), String> {
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
        });
    }

    kept
}

impl Plan {
    /// The choices init makes without asking
    pub fn recommended(&self) -> Answers {
        Answers {
            config: true,
            workflows: self.workflows.iter().map(|w| w.recommended).collect(),
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

                if let Some(warning) = &w.warning {
                    out.push(format!("{} {}", s.warn(), s.yellow(warning)));
                }

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
            "Review shipcheck.toml, the workflow changes and AGENTS.md, commit them, and merge to main".into(),
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
    let _ = writeln!(out, "{}", s.bold("shipcheck init"));
    let plan = plan(root, app, &mut Lines { out, style: s })?;

    if let Some(why) = &plan.stop {
        let _ = writeln!(out, "\n{why}");
        return Ok(false);
    }

    let _ = writeln!(
        out,
        "{}",
        s.dim("--yes takes the recommended choice of each step")
    );

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

    let _ = writeln!(out, "\n{}", s.bold("Applying"));
    let next = apply(root, &plan, &a, finish, &mut Lines { out, style: s })?;
    next_steps(out, s, &next);
    Ok(true)
}

/// `init` for an agent or a script: shipcheck.toml alone, with only the checks that passed when init tried them
pub fn config_only(root: &Path, out: &mut dyn Write) -> Result<Option<String>, String> {
    if root.join("shipcheck.toml").exists() {
        return Ok(None);
    }

    let style = Style::stdout();
    let plan = plan(root, App::NotPublic, &mut Lines { out, style })?;

    if let Some(why) = plan.stop {
        return Ok(Some(why));
    }

    let Some((text, said)) = plan.config else {
        return Ok(None);
    };
    crate::agents::write_in_repo(root, &root.join("shipcheck.toml"), &text)?;
    Ok(Some(format!("wrote shipcheck.toml with {said}")))
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Then {
    Apply,
    Change,
    Quit,
}

/**
 * The terminal wizard: QuickStart or every step, a review that can reopen a step, then apply.
 * Nothing is written before apply, and Ctrl-C or quit leaves everything as it was
 **/
pub fn tui(root: &Path, find: fn() -> App, finish: Finish) -> Result<bool, String> {
    match wizard(root, find, finish) {
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
            let _ = cliclack::outro_cancel("Quit, nothing changed");
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

    let quick = cliclack::select("Setup")
        .item(true, "QuickStart", "recommended choices, then a review")
        .item(false, "Advanced", "every step, one by one")
        .interact()?;
    let mut a = plan.recommended();

    if !quick {
        for step in plan.steps() {
            ask(&plan, step, &mut a, s)?;
        }
    }

    loop {
        cliclack::note(s.bold("Review"), plan.review(&a).join("\n"))?;
        let then = cliclack::select("Apply these choices?")
            .item(Then::Apply, "Apply", "")
            .item(Then::Change, "Change a step", "")
            .item(Then::Quit, "Quit without changes", "")
            .interact()?;

        match then {
            Then::Apply => break,
            Then::Quit => {
                cliclack::outro_cancel("Quit, nothing changed")?;
                return Ok(Ok(false));
            }
            Then::Change => {
                let mut pick = cliclack::select("Which step?");

                for (n, step) in plan.steps().into_iter().enumerate() {
                    pick = pick.item(
                        step,
                        format!("{}  {}", n + 1, plan.title(step)),
                        plan.effect(step, &a),
                    );
                }

                let step = pick.interact()?;
                ask(&plan, step, &mut a, s)?;
            }
        }
    }

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

/// Shows a step and records its answer, the current one preselected
fn ask(plan: &Plan, step: Step, a: &mut Answers, s: Style) -> std::io::Result<()> {
    cliclack::note(s.bold(&plan.title(step)), plan.details(step, s).join("\n"))?;
    let recommended = plan.default(step);
    let label = |yes: bool| {
        let mut a = a.clone();
        plan.set(step, &mut a, yes);
        plan.effect(step, &a)
    };
    let hint = |yes: bool| {
        if yes == recommended {
            "recommended"
        } else {
            ""
        }
    };
    let yes = cliclack::select(plan.question(step))
        .item(true, format!("Yes: {}", label(true)), hint(true))
        .item(false, format!("No: {}", label(false)), hint(false))
        .initial_value(plan.answer(step, a))
        .interact()?;
    plan.set(step, a, yes);
    Ok(())
}

/// The remote's default branch, else main
fn base_branch(root: &Path) -> String {
    Command::new("git")
        .args(["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .and_then(|o| {
            let name = String::from_utf8_lossy(&o.stdout).trim().to_string();
            name.strip_prefix("origin/").map(str::to_string)
        })
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "main".into())
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

    if cfg.runner != RunnerKind::Host {
        let listed = cfg.workflows.iter().any(|w| w == file);
        return jobs
            .iter()
            .map(|j| Cover {
                job: j.id.clone(),
                by: if listed {
                    vec![name(file, &j.id)]
                } else {
                    Vec::new()
                },
                missing: if listed {
                    Vec::new()
                } else {
                    vec!["shipcheck.toml does not list the workflow".into()]
                },
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

/// The jobs no check covers, for a warning
fn uncovered(covers: &[&Cover]) -> String {
    covers
        .iter()
        .map(|c| format!("job {} ({})", c.job, c.missing.join(", ")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The workflow edits for the jobs shipcheck checks, and notes for the workflows left alone
fn workflows(root: &Path, cfg: &RepoConfig, plan: &mut Plan) -> Result<(), String> {
    let base = plan.base.clone();
    let kept = kept(root, &plan.failed);
    let mut offered = false;

    for file in setup::workflow_files(root).unwrap_or_default() {
        let path = root.join(".github/workflows").join(&file);
        let source = fs::read_to_string(&path).map_err(|e| format!("{file}: {e}"))?;
        let jobs = crate::workflow::job_runs(&source);
        let covers = coverage(root, cfg, &file, &jobs);
        let mut staying: Vec<&str> = kept
            .iter()
            .filter(|k| k.file == file)
            .map(|k| k.job.as_str())
            .collect();

        // No host check can run a job with no command, so it stays on GitHub and the rest is gated
        for job in jobs.iter().filter(|_| cfg.runner == RunnerKind::Host) {
            if setup::job_commands(job).is_empty() && !staying.contains(&job.id.as_str()) {
                staying.push(&job.id);
            }
        }

        if !staying.is_empty() {
            offered = true;
            gate(&file, &source, &jobs, &covers, &staying, &base, plan);
            continue;
        }

        let Some(edited) = crate::workflow::main_only(&source, &base) else {
            if crate::workflow::runs_for_branches(&source) {
                offered = true;
                plan.notes.push(format!(
                    "{file}: its `on:` has a form shipcheck does not edit, change it by hand"
                ));
            }

            continue;
        };
        offered = true;
        let mut by: Vec<String> = Vec::new();

        for c in &covers {
            by.extend(
                c.by.iter()
                    .filter(|n| !by.contains(n))
                    .cloned()
                    .collect::<Vec<_>>(),
            );
        }

        let on_prs = crate::workflow::runs_on_prs(&source);

        // Its branch pushes are no PR check, and no shipcheck check takes their place
        if by.is_empty() && !on_prs {
            plan.notes.push(format!(
                "{file}: GitHub keeps running it on pushes to every branch, and no shipcheck check runs its jobs"
            ));
            continue;
        }

        let missing: Vec<&Cover> = covers.iter().filter(|c| !c.missing.is_empty()).collect();
        let runs = if by.is_empty() {
            "nothing runs its jobs".to_string()
        } else {
            format!("shipcheck runs {}", by.join(", "))
        };
        plan.workflows.push(Change {
            diff: diff(&file, &source, &edited),
            yes: format!("on PRs {runs}, GitHub keeps running {file} on pushes to {base}"),
            no: if on_prs {
                format!("GitHub keeps running {file} on PRs and on pushes to {base}")
            } else {
                format!("GitHub keeps running {file} on pushes to every branch")
            },
            short_yes: format!(
                "PRs: {}, {base}: GitHub",
                if by.is_empty() {
                    "nothing".into()
                } else {
                    by.join(", ")
                }
            ),
            short_no: if on_prs {
                format!("PRs and {base}: GitHub, as now")
            } else {
                "pushes: GitHub, as now".into()
            },
            warning: (!missing.is_empty()).then(|| {
                format!(
                    "no shipcheck check runs {}. Change it and PRs run that nowhere",
                    uncovered(&missing)
                )
            }),
            recommended: missing.is_empty(),
            edited,
            file,
        });
    }

    if !offered {
        plan.notes
            .push("no workflow runs on feature branches".into());
    }

    Ok(())
}

/**
 * A workflow with jobs that stay on GitHub keeps its triggers, and the jobs shipcheck checks run
 * on GitHub only on the base branch. A job that a job left on GitHub needs is not gated, or GitHub
 * would skip that job too
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
    let stay = staying.join(", ");

    if gated.is_empty() {
        plan.notes.push(if crate::workflow::runs_on_prs(source) {
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

    let Some(edited) = crate::workflow::gate_jobs(source, &gated, base) else {
        plan.notes.push(format!(
            "{file}: a job has a form shipcheck does not edit. To run {} on GitHub only on {base}, give each `if: github.ref == 'refs/heads/{base}' || github.actor == 'dependabot[bot]'` by hand",
            gated.join(", ")
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
        yes: format!(
            "on PRs shipcheck runs {}, GitHub keeps running {stay} on PRs and all of {file} on pushes to {base}{}",
            by.join(", "),
            elsewhere(source, base)
        ),
        no: format!("GitHub keeps running all of {file} on PRs, beside shipcheck's checks"),
        short_yes: format!("PRs: {}, GitHub {stay}, {base}: GitHub", by.join(", ")),
        short_no: format!("PRs and {base}: GitHub, as now"),
        warning: None,
        recommended: true,
        edited,
    });
}

/// Where else GitHub keeps running the gated jobs, since shipcheck runs only PRs into the base
fn elsewhere(source: &str, base: &str) -> String {
    let (pushes, prs) = crate::workflow::other_branches(source, base);
    let mut more: Vec<String> = Vec::new();

    if !pushes.is_empty() {
        more.push(format!("on pushes to {}", pushes.join(", ")));
    }

    if prs {
        more.push("on PRs into other branches".to_string());
    }

    if more.is_empty() {
        return String::new();
    }

    format!(", and {}", more.join(" and "))
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

fn ruleset_plan(
    root: &Path,
    text: &str,
    app: &App,
    failed: &[(String, String, String)],
) -> Option<Ruleset> {
    let app = app.id().filter(|a| a.parse::<u64>().is_ok())?;
    let names = job_checks(root, text).ok().filter(|n| !n.is_empty())?;
    let mut github = Vec::new();
    let mut notes = Vec::new();

    for kept in kept(root, failed) {
        match kept.checks {
            Some(checks) => github.extend(checks),
            None => notes.push(format!(
                "{}, job {}: its check names depend on its matrix or an expression, require them by hand once a PR shows them",
                kept.file, kept.job
            )),
        }
    }

    Some(Ruleset {
        app,
        names,
        github,
        notes,
    })
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
        let ci = OWN_CI.split("\n  service:").next().unwrap();
        let dir = repo(&[
            (".github/workflows/ci.yml", ci),
            ("shipcheck.toml", SHIPCHECK_TOML),
        ]);
        let plan = super::plan(dir.path(), App::NotPublic, &mut Said::default()).unwrap();
        let ci = &plan.workflows[0];
        assert!(ci.recommended, "{:?}", ci.warning);
        assert_eq!(
            ci.yes,
            "on PRs shipcheck runs shipcheck/fmt, shipcheck/clippy, shipcheck/test, GitHub keeps running ci.yml on pushes to main"
        );
        assert!(ci.diff.contains("-  pull_request:"), "{}", ci.diff);
    }

    #[test]
    fn coverage_names_the_jobs_no_check_runs() {
        let dir = repo(&[
            (".github/workflows/ci.yml", OWN_CI),
            (".github/workflows/e2e.yml", OWN_E2E),
            (".github/workflows/leaks.yml", OWN_LEAKS),
            ("shipcheck.toml", SHIPCHECK_TOML),
        ]);
        let plan = super::plan(dir.path(), App::NotPublic, &mut Said::default()).unwrap();
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
        assert!(plan.workflows.iter().all(|w| !w.recommended));
        let warning = |i: usize| plan.workflows[i].warning.clone().unwrap_or_default();
        assert_eq!(
            warning(0),
            "no shipcheck check runs job service (`bun run test` in service, `bun run test:worker` in service, `bunx tsc --noEmit` in service). Change it and PRs run that nowhere"
        );
        assert!(
            plan.workflows[0].yes.starts_with(
                "on PRs shipcheck runs shipcheck/fmt, shipcheck/clippy, shipcheck/test,"
            ),
            "{}",
            plan.workflows[0].yes
        );
    }

    #[test]
    fn apply_writes_only_what_is_answered() {
        let dir = repo(&[
            (".github/workflows/ci.yml", CI),
            (".github/workflows/lint.yml", LINT),
            (
                "shipcheck.toml",
                "runner = \"host\"\n[host.checks]\ntest = \"cargo test\"\n",
            ),
        ]);
        let plan = super::plan(dir.path(), App::NotPublic, &mut Said::default()).unwrap();
        let mut a = plan.recommended();
        // ci.yml is checked, so yes is its default. lint.yml is not
        assert_eq!(a.workflows, [true, false]);
        assert!(
            plan.workflows[1]
                .warning
                .as_deref()
                .unwrap()
                .contains("job lint (`cargo clippy`)")
        );

        // Changing step 2, lint.yml, and back again, as the review's "Change a step" does
        plan.set(Step::Workflow(1), &mut a, true);
        assert!(
            plan.review(&a)[2].contains("PRs: nothing, main: GitHub"),
            "{:?}",
            plan.review(&a)
        );
        plan.set(Step::Workflow(1), &mut a, false);
        assert!(plan.review(&a)[2].contains("PRs and main: GitHub, as now"));

        let mut said = Said::default();
        let next = apply(dir.path(), &plan, &a, &mut finish_nothing, &mut said).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
            "on:\n  push:\n    branches: [main, \"dependabot/**\"]\njobs:\n  test:\n    steps:\n      - run: cargo test\n"
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
        let plan = super::plan(dir.path(), App::NotPublic, &mut Said::default()).unwrap();
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
        let plan = super::plan(dir.path(), App::NotPublic, &mut Said::default()).unwrap();
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
        let plan = super::plan(dir.path(), App::NotPublic, &mut Said::default()).unwrap();
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
                "  test:\n    if: (github.ref == 'refs/heads/main' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]')\n"
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
        let plan = super::plan(dir.path(), App::NotPublic, &mut Said::default()).unwrap();
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
                "  lint:\n    if: (github.ref == 'refs/heads/main' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]')\n"
            ),
        );
        assert_eq!(
            plan.workflows[0].yes,
            "on PRs shipcheck runs shipcheck/lint, GitHub keeps running tests on PRs and all of ci.yml on pushes to main, and on PRs into other branches"
        );

        // The matrix needs lint, so lint stays on GitHub too and nothing is gated
        let needs = ci.replace("  tests:\n", "  tests:\n    needs: [lint]\n");
        let dir = repo(&[
            (".github/workflows/ci.yml", &needs),
            ("shipcheck.toml", config),
        ]);
        let plan = super::plan(dir.path(), App::NotPublic, &mut Said::default()).unwrap();
        assert!(plan.workflows.is_empty());
        assert!(
            plan.notes[0].contains("ci.yml: GitHub keeps running it on PRs, for tests, lint"),
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
        let plan = super::plan(dir.path(), App::NotPublic).unwrap();
        let change = &plan.workflows[0];
        assert!(change.recommended, "{:?}", change.warning);
        assert_eq!(
            Some(change.edited.clone()),
            crate::workflow::gate_jobs(lint, &["ruby".to_string()], "main")
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
        let plan = super::plan(dir.path(), App::NotPublic, &mut Said::default()).unwrap();
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
        let plan = super::plan(dir.path(), App::NotPublic, &mut Said::default()).unwrap();
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
