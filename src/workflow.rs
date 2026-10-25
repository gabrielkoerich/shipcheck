use std::collections::{BTreeMap, BTreeSet};

use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

pub(crate) fn key(s: &str) -> Yaml {
    Yaml::String(s.to_string())
}

pub(crate) fn step(name: &str, condition: Option<&str>, run: String) -> Yaml {
    let mut h = Hash::new();
    h.insert(key("name"), key(name));

    if let Some(c) = condition {
        h.insert(key("if"), key(c));
    }

    h.insert(key("run"), Yaml::String(run));
    Yaml::Hash(h)
}

/// The top-level mapping of a workflow
pub(crate) fn load(source: &str) -> Result<Hash, String> {
    match YamlLoader::load_from_str(source)
        .map_err(|e| format!("workflow YAML: {e}"))?
        .into_iter()
        .next()
    {
        Some(Yaml::Hash(root)) => Ok(root),
        _ => Err("workflow is not a YAML mapping".into()),
    }
}

pub(crate) fn emit(root: Hash) -> Result<String, String> {
    let mut out = String::new();
    let mut emitter = YamlEmitter::new(&mut out);
    emitter.multiline_strings(true);
    emitter
        .dump(&Yaml::Hash(root))
        .map_err(|e| format!("workflow YAML: {e}"))?;
    Ok(out)
}

pub(crate) fn jobs_mut(root: &mut Hash) -> Result<&mut Hash, String> {
    match root.get_mut(&key("jobs")) {
        Some(Yaml::Hash(jobs)) => Ok(jobs),
        _ => Err("workflow has no `jobs` mapping".into()),
    }
}

/**
 * Whether GitHub would run a workflow for a feature branch: on `pull_request`, or on `push` with no
 * filter that keeps it to `main` or to tags. Deploys on `push: branches: [main]` or on tags stay
 * out, and so does `pull_request_target`, which runs with the base repository's secrets
 **/
pub fn runs_for_branches(source: &str) -> bool {
    let Ok(root) = load(source) else {
        return false;
    };

    match root.get(&key("on")) {
        Some(Yaml::String(e)) => e == "pull_request" || e == "push",
        Some(Yaml::Array(events)) => events
            .iter()
            .any(|e| matches!(e.as_str(), Some("pull_request" | "push"))),
        Some(Yaml::Hash(events)) => {
            events.get(&key("pull_request")).is_some_and(pr_runs)
                || events.get(&key("push")).is_some_and(push_runs)
        }
        _ => false,
    }
}

/// Whether `pull_request_target` triggers it, which runs the base branch's workflow with its secrets
pub fn on_pull_request_target(source: &str) -> bool {
    load(source).is_ok_and(|root| match root.get(&key("on")) {
        Some(Yaml::String(e)) => e == "pull_request_target",
        Some(Yaml::Array(events)) => events
            .iter()
            .any(|e| e.as_str() == Some("pull_request_target")),
        Some(Yaml::Hash(events)) => events.contains_key(&key("pull_request_target")),
        _ => false,
    })
}

/// Whether GitHub runs a workflow on a PR's commits, by its `pull_request` trigger
pub fn runs_on_prs(source: &str) -> bool {
    let Ok(root) = load(source) else {
        return false;
    };

    match root.get(&key("on")) {
        Some(Yaml::String(e)) => e == "pull_request",
        Some(Yaml::Array(events)) => events.iter().any(|e| e.as_str() == Some("pull_request")),
        Some(Yaml::Hash(events)) => events.get(&key("pull_request")).is_some_and(pr_runs),
        _ => false,
    }
}

/// Whether a `pull_request` trigger runs on a PR's commits. `types: [closed]` runs once, after merge
fn pr_runs(filter: &Yaml) -> bool {
    let pushed = |t: &Yaml| matches!(t.as_str(), Some("opened" | "synchronize" | "reopened"));

    match &filter["types"] {
        Yaml::String(_) => pushed(&filter["types"]),
        Yaml::Array(types) => types.iter().any(pushed),
        _ => true,
    }
}

/// Whether a `push` trigger with this filter runs for a feature branch
fn push_runs(filter: &Yaml) -> bool {
    match filter {
        Yaml::Null | Yaml::BadValue => true,
        Yaml::Hash(f) => {
            if let Some(patterns) = f.get(&key("branches")) {
                branch_patterns_match(patterns)
            } else if let Some(ignored) = f.get(&key("branches-ignore")) {
                !branch_patterns_match(ignored)
            } else {
                // Only tag filters mean pushes to branches never trigger it
                !f.contains_key(&key("tags")) && !f.contains_key(&key("tags-ignore"))
            }
        }
        _ => false,
    }
}

// GitHub's glob rules, where `*` stops at `/`, against an ordinary branch name. `*`, `**` and `**/*`
// match it, `main`, `main*`, `feature/*`, `release/**` and Dependabot's branches do not
fn branch_patterns_match(patterns: &Yaml) -> bool {
    let list = match patterns {
        Yaml::String(p) => vec![p.as_str()],
        Yaml::Array(items) => items.iter().filter_map(Yaml::as_str).collect(),
        _ => return false,
    };

    list.iter()
        .filter(|p| !p.starts_with("dependabot/"))
        .any(|p| {
            globset::GlobBuilder::new(p)
                .literal_separator(true)
                .build()
                .is_ok_and(|g| g.compile_matcher().is_match("shipcheck-any-branch"))
        })
}

/// One `run` or `uses` step, with the working directory and shell from the step, the job or the workflow
#[derive(Clone)]
pub struct RunStep {
    pub name: Option<String>,
    /// Empty for a `uses` step
    pub run: String,
    pub uses: Option<String>,
    /// The ref a SHA pin names in its comment, `stable` for `uses: a/b@<sha> # stable`
    pub ref_name: Option<String>,
    /// A `uses` step's inputs, None for a value that is an expression
    pub with: Vec<(String, Option<String>)>,
    pub dir: Option<String>,
    pub shell: Option<String>,
    /// It has an `if:`, or `continue-on-error` that is not false, so GitHub may skip it or pass it failed
    pub conditional: bool,
    pub env: Vec<(String, Option<String>)>,
}

pub struct Job {
    pub id: String,
    pub needs: Vec<String>,
    pub conditional: bool,
    pub steps: Vec<RunStep>,
    /// The workflow's and the job's `env`, the job's winning. None for a value with an expression
    pub env: Vec<(String, Option<String>)>,
    /// The `runs-on` labels, when they are literal
    pub runs_on: Vec<String>,
    /// A step uses `actions/download-artifact`
    pub downloads: bool,
    /// One axis of `strategy.matrix` and its values, for the person who runs `init`. None without a matrix
    pub matrix: Option<String>,
    pub os: Platforms,
    /// Its `if:` depends on the PR or the default branch
    pub pr_only: bool,
    /// Its `if:` requires one branch, such as a release job's `github.ref == 'refs/heads/main'`
    pub base_only: bool,
    /// A step runs `docker` or `act`, or uses a `docker/` action, which needs a Docker daemon in the job
    pub runs_docker: bool,
    /// The image of its `container:`, as written
    pub container: Option<String>,
    /// The names GitHub gives its check runs, None when they depend on more than this reads
    pub check_names: Option<Vec<String>>,
}

/// The kinds of runner a job's `runs-on` can pick, Linux when its labels name none
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Platforms {
    pub linux: bool,
    pub mac: bool,
    pub windows: bool,
}

/**
 * The platforms of the runner labels a job can pick: its literal `runs-on`, or for an expression
 * such as `${{ matrix.os || 'ubuntu-latest' }}` its quoted words and every value of the matrix
 **/
fn platforms(job: &Yaml) -> Platforms {
    let mut labels: Vec<String> = Vec::new();
    let scalars = |y: &Yaml, out: &mut Vec<String>| match y {
        Yaml::String(s) => out.push(s.clone()),
        Yaml::Array(list) => out.extend(list.iter().filter_map(|l| l.as_str().map(str::to_string))),
        _ => {}
    };
    scalars(&job["runs-on"], &mut labels);

    if labels.iter().any(|l| l.contains("${{")) {
        let quoted: Vec<String> = labels
            .iter()
            .flat_map(|l| {
                l.split('\'')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect();
        labels.extend(quoted);

        if let Yaml::Hash(m) = &job["strategy"]["matrix"] {
            for (k, v) in m {
                match k.as_str() {
                    Some("include") => {
                        for entry in v.as_vec().into_iter().flatten() {
                            if let Yaml::Hash(e) = entry {
                                e.values().for_each(|v| scalars(v, &mut labels));
                            }
                        }
                    }
                    Some("exclude") => {}
                    _ => scalars(v, &mut labels),
                }
            }
        }
    }

    let mut p = Platforms::default();

    for l in labels.iter().map(|l| l.to_lowercase()) {
        p.windows |= l.starts_with("windows");
        p.mac |= l.contains("macos") || l.contains("xcode");
        p.linux |= l.starts_with("ubuntu") || l.contains("linux");
    }

    p.linux |= !p.mac && !p.windows;
    p
}

fn scalar(y: &Yaml) -> Option<String> {
    match y {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Real(r) => Some(r.clone()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The values of a matrix with one combination, such as `python: [3.13]` or one `include` entry
fn single_leg(matrix: &Yaml) -> Option<Vec<(String, String)>> {
    let m = matrix.as_hash()?;
    let mut leg = Vec::new();

    for (k, v) in m {
        match (k.as_str()?, v) {
            ("include", Yaml::Array(entries)) if entries.len() == 1 && m.len() == 1 => {
                for (k, v) in entries[0].as_hash()? {
                    leg.push((k.as_str()?.to_string(), scalar(v)?));
                }
            }
            ("include" | "exclude", _) => return None,
            (k, Yaml::Array(values)) if values.len() == 1 => {
                leg.push((k.to_string(), scalar(&values[0])?));
            }
            _ => return None,
        }
    }

    (!leg.is_empty()).then_some(leg)
}

/// `y` with each `${{ matrix.x }}` of `leg` filled in
fn fill(y: &Yaml, leg: &[(String, String)]) -> Yaml {
    match y {
        Yaml::String(s) => {
            let mut s = s.clone();

            for (k, v) in leg {
                for form in [
                    format!("${{{{ matrix.{k} }}}}"),
                    format!("${{{{matrix.{k}}}}}"),
                ] {
                    s = s.replace(&form, v);
                }
            }

            Yaml::String(s)
        }
        Yaml::Array(a) => Yaml::Array(a.iter().map(|v| fill(v, leg)).collect()),
        Yaml::Hash(h) => Yaml::Hash(h.iter().map(|(k, v)| (k.clone(), fill(v, leg))).collect()),
        other => other.clone(),
    }
}

/// The clause of init's gate that keeps a fork's PR on GitHub, since the daemon never runs one
pub const FORK: &str = "github.event.pull_request.head.repo.fork";

/// Whether a job's `if:` depends on the PR or the default branch, so only GitHub can tell if it runs
fn pr_only(job: &Yaml) -> bool {
    match &job["if"] {
        // init's gate runs the job on the base branch, so only the condition it and-ed with counts
        Yaml::String(c) => reads_pr(&ungate(c).unwrap_or_else(|| c.clone())),
        _ => false,
    }
}

/**
 * Whether a condition reads the PR or the default branch, so only GitHub can tell if it holds on a
 * PR. One that and-s a push or branch term is false on every PR, as act's skip says, so it does not
 **/
pub fn reads_pr(cond: &str) -> bool {
    let false_on_pr = !cond.contains("||")
        && !cond.contains("!(")
        && [
            "github.ref ==",
            "github.ref_name ==",
            "github.event_name == 'push'",
        ]
        .iter()
        .any(|p| cond.contains(p));

    !false_on_pr
        && [
            "default_branch",
            "pull_request",
            "head_ref",
            "base_ref",
            "github.ref !=",
            "github.ref_name !=",
        ]
        .iter()
        .any(|p| cond.contains(p))
}

/// The ids of the jobs whose `if:` reads the PR, see `reads_pr`
pub fn pr_only_jobs(source: &str) -> BTreeSet<String> {
    job_runs(source)
        .into_iter()
        .filter(|j| j.pr_only)
        .map(|j| j.id)
        .collect()
}

/**
 * Whether a job's `if:` requires the ref to be one branch, so it runs only there. An `||` can let it
 * run elsewhere, as init's own gate for Dependabot does, so a condition with one never counts
 **/
fn base_only(job: &Yaml) -> bool {
    job["if"].as_str().is_some_and(|c| {
        !c.contains("||")
            && ["github.ref == ", "github.ref_name == "]
                .iter()
                .any(|p| c.contains(p))
    })
}

/// A YAML parser drops comments, so the name a SHA pin keeps in one comes from the source line
fn ref_name(source: &str, uses: &str) -> Option<String> {
    let (_, at) = uses.split_once('@')?;

    if at.len() != 40 || !at.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }

    source.lines().find_map(|line| {
        let comment = line.split_once(uses)?.1.trim().strip_prefix('#')?;
        comment.split_whitespace().next().map(str::to_string)
    })
}

fn runs_docker(step: &RunStep) -> bool {
    let command = |word: &str| {
        step.run
            .split(|c: char| c.is_whitespace() || ";&|()`$".contains(c))
            .any(|w| w == word)
    };
    step.uses
        .as_deref()
        .is_some_and(|u| u.starts_with("docker/"))
        || command("docker")
        || command("act")
}

/// One axis of a matrix as `name: a, b, c, ...`, preferring one that is not the runner's OS
fn matrix_axis(matrix: &Yaml) -> Option<String> {
    let axes: Vec<(String, Vec<String>)> = match matrix {
        Yaml::BadValue | Yaml::Null => return None,
        Yaml::Hash(m) => {
            let mut axes: Vec<(String, Vec<String>)> = m
                .iter()
                .filter(|(k, _)| !matches!(k.as_str(), Some("include" | "exclude")))
                .filter_map(|(k, v)| {
                    Some((
                        k.as_str()?.to_string(),
                        v.as_vec()?.iter().filter_map(scalar).collect(),
                    ))
                })
                .collect();

            for entry in matrix["include"].as_vec().into_iter().flatten() {
                for (k, v) in entry.as_hash().into_iter().flatten() {
                    let (Some(k), Some(v)) = (k.as_str(), scalar(v)) else {
                        continue;
                    };

                    match axes.iter_mut().find(|(name, _)| name == k) {
                        Some((_, values)) if !values.contains(&v) => values.push(v),
                        Some(_) => {}
                        None => axes.push((k.to_string(), vec![v])),
                    }
                }
            }

            axes
        }
        _ => return Some("from an expression".into()),
    };
    let os = ["os", "platform", "runner", "runs-on", "name"];
    let (name, values) = axes
        .iter()
        .find(|(k, _)| !os.contains(&k.as_str()))
        .or(axes.first())?;
    let mut shown: Vec<&str> = values.iter().take(4).map(String::as_str).collect();

    if values.len() > 4 {
        shown.push("...");
    }

    Some(format!("{name}: {}", shown.join(", ")))
}

/**
 * GitHub's check names for a job: its `name`, else its id. For a matrix of plain axes, one per
 * leg, `id (a, b)` or the name with each `${{ matrix.x }}` filled in. None for anything else
 **/
fn check_names(id: &str, job: &Yaml) -> Option<Vec<String>> {
    let name = match &job["name"] {
        Yaml::BadValue => None,
        n => Some(n.as_str()?.to_string()),
    };
    let matrix = &job["strategy"]["matrix"];

    if matrix.is_badvalue() {
        return match name {
            Some(n) if n.contains("${{") => None,
            n => Some(vec![n.unwrap_or_else(|| id.to_string())]),
        };
    }

    let mut legs: Vec<Vec<(String, String)>> = vec![Vec::new()];

    for (k, v) in matrix.as_hash()? {
        let k = k.as_str().filter(|k| !["include", "exclude"].contains(k));
        let values: Vec<String> = v.as_vec()?.iter().map(scalar).collect::<Option<_>>()?;
        let k = k?.to_string();
        legs = legs
            .iter()
            .flat_map(|leg| {
                values.iter().map(|v| {
                    let mut leg = leg.clone();
                    leg.push((k.clone(), v.clone()));
                    leg
                })
            })
            .collect();
    }

    legs.into_iter()
        .map(|leg| match &name {
            None => Some(format!(
                "{id} ({})",
                leg.iter()
                    .map(|(_, v)| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Some(n) if n.contains("${{") => {
                let mut n = n.clone();

                for (k, v) in &leg {
                    for form in [
                        format!("${{{{ matrix.{k} }}}}"),
                        format!("${{{{matrix.{k}}}}}"),
                    ] {
                        n = n.replace(&form, v);
                    }
                }

                (!n.contains("${{")).then_some(n)
            }
            // GitHub may add the values to a fixed name
            Some(_) => None,
        })
        .collect()
}

/// An `env` mapping's names and values, None for a value that is an expression or not a scalar
fn env_of(y: &Yaml) -> Vec<(String, Option<String>)> {
    let Yaml::Hash(h) = y else {
        return Vec::new();
    };
    h.iter()
        .filter_map(|(k, v)| {
            let value = match v {
                Yaml::String(s) if !s.contains("${{") => Some(s.clone()),
                Yaml::Integer(i) => Some(i.to_string()),
                Yaml::Real(r) => Some(r.clone()),
                Yaml::Boolean(b) => Some(b.to_string()),
                _ => None,
            };
            Some((k.as_str()?.to_string(), value))
        })
        .collect()
}

/// Each job's `run` steps, trimmed, in file order. `uses` steps are left out
pub fn job_runs(source: &str) -> Vec<Job> {
    let Ok(root) = load(source) else {
        return Vec::new();
    };
    let whole = Yaml::Hash(root);
    let top = &whole["defaults"];
    let Yaml::Hash(jobs) = &whole["jobs"] else {
        return Vec::new();
    };
    let conditional = |y: &Yaml| {
        !matches!(y["if"], Yaml::BadValue)
            || !matches!(
                y["continue-on-error"],
                Yaml::BadValue | Yaml::Boolean(false)
            )
    };
    let mut out = Vec::new();

    for (id, original) in jobs.iter() {
        // A matrix of one combination is a plain job, GitHub only adds the values to its name
        let filled = single_leg(&original["strategy"]["matrix"]).map(|leg| {
            let mut job = fill(original, &leg);

            if let Some(Yaml::Hash(s)) = job.as_mut_hash().and_then(|j| j.get_mut(&key("strategy")))
            {
                s.remove(&key("matrix"));
            }

            job
        });
        let job = filled.as_ref().unwrap_or(original);
        let (Some(id), Yaml::Array(steps_yaml)) = (id.as_str(), &job["steps"]) else {
            continue;
        };
        let pick = |step: &Yaml, name: &str| {
            [
                &step[name],
                &job["defaults"]["run"][name],
                &top["run"][name],
            ]
            .into_iter()
            .find_map(|v| v.as_str())
            .map(str::to_string)
        };
        let mut env = env_of(&whole["env"]);

        for (k, v) in env_of(&job["env"]) {
            env.retain(|(name, _)| *name != k);
            env.push((k, v));
        }

        // A `with` value of just `${{ env.NAME }}` is that variable, such as a toolchain version
        let with_of = |y: &Yaml| {
            let mut with = env_of(y);

            for (name, value) in &mut with {
                let raw = y[name.as_str()].as_str().unwrap_or_default().trim();
                let var = raw
                    .strip_prefix("${{")
                    .and_then(|r| r.strip_suffix("}}"))
                    .and_then(|r| r.trim().strip_prefix("env."));

                if let Some(var) = var.filter(|_| value.is_none()) {
                    *value = env
                        .iter()
                        .find(|(k, _)| k == var)
                        .and_then(|(_, v)| v.clone());
                }
            }

            with
        };
        let steps: Vec<RunStep> = steps_yaml
            .iter()
            .filter_map(|s| {
                let run = s["run"].as_str().map(|r| r.trim().to_string());
                let uses = s["uses"].as_str().map(str::to_string);

                if run.is_none() && uses.is_none() {
                    return None;
                }

                Some(RunStep {
                    name: s["name"].as_str().map(str::to_string),
                    dir: if run.is_some() {
                        pick(s, "working-directory")
                    } else {
                        None
                    },
                    run: run.unwrap_or_default(),
                    ref_name: uses.as_deref().and_then(|u| ref_name(source, u)),
                    uses,
                    with: with_of(&s["with"]),
                    shell: pick(s, "shell"),
                    conditional: conditional(s),
                    env: env_of(&s["env"]),
                })
            })
            .collect();
        let needs = match &job["needs"] {
            Yaml::String(n) => vec![n.clone()],
            Yaml::Array(list) => list
                .iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        let runs_on = match &job["runs-on"] {
            Yaml::String(l) => vec![l.clone()],
            Yaml::Array(list) => list
                .iter()
                .filter_map(|l| l.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        let runs_docker = steps.iter().any(runs_docker);
        out.push(Job {
            id: id.to_string(),
            needs,
            conditional: conditional(job),
            steps,
            env,
            runs_on,
            downloads: steps_yaml.iter().any(|s| {
                s["uses"]
                    .as_str()
                    .is_some_and(|u| u.starts_with("actions/download-artifact@"))
            }),
            matrix: matrix_axis(&job["strategy"]["matrix"]),
            os: platforms(job),
            pr_only: pr_only(job),
            base_only: base_only(job),
            runs_docker,
            container: [&job["container"], &job["container"]["image"]]
                .into_iter()
                .find_map(|c| c.as_str())
                .map(str::to_string),
            check_names: check_names(id, original),
        });
    }

    out
}

/**
 * A job's `if:` without the gates `gate_jobs` adds, empty when a gate was all of it. None when it
 * has no gate. act runs a PR's checks on a branch of its own, where a gate would skip every job.
 * A job that two inits gated has two, so each is stripped
 **/
pub fn ungate(cond: &str) -> Option<String> {
    let mut c = cond.trim().to_string();
    let mut found = false;

    loop {
        if is_gate(&c) {
            return Some(String::new());
        }

        let Some(at) = c
            .rfind(") && (")
            .filter(|&at| one_group(&c[..=at]) && is_gate(&c[at + 5..]))
        else {
            return found.then_some(c);
        };
        c = c[1..at].trim().to_string();
        found = true;
    }
}

/// What act, which runs a job as `workflow_dispatch` on a branch of its own, gives other values than a PR
const CONTEXT_ACT_LACKS: &[&str] = &[
    "github.event",
    "github.base_ref",
    "github.head_ref",
    "github.repository_owner",
    "github.actor",
    "github.triggering_actor",
    "default_branch",
];

/**
 * Why act cannot tell what a job does on a PR: a job `if:` of its own, beside init's gate, which act
 * may skip and a skip passes, or a value anywhere in the job that reads the PR or who opened it,
 * which act leaves empty, so a step such as a changed-files lint checks nothing. None when it reads none
 **/
pub fn act_lacks_context(source: &str, id: &str) -> Option<String> {
    let root = load(source).ok()?;
    let mut job = root.get(&key("jobs"))?.as_hash()?.get(&key(id))?.clone();

    if let Yaml::Hash(h) = &mut job
        && let Some(cond) = h.get(&key("if")).and_then(Yaml::as_str).map(str::to_string)
    {
        let own = ungate(&cond).unwrap_or(cond);

        if !own.trim().is_empty() {
            return Some(format!(
                "its `if: {}` can run otherwise under act, which runs it as workflow_dispatch",
                own.trim()
            ));
        }

        h.remove(&key("if"));
    }

    let text = emit(Hash::from_iter([(key(id), job)])).ok()?;
    CONTEXT_ACT_LACKS
        .iter()
        .find(|c| text.contains(*c))
        .map(|c| format!("it reads `{c}`, which act leaves empty or sets otherwise"))
}

/// The jobs whose `if:` holds a gate an init wrote
pub fn gated_jobs(source: &str) -> Vec<String> {
    let Ok(root) = load(source) else {
        return Vec::new();
    };
    let Some(Yaml::Hash(jobs)) = root.get(&key("jobs")) else {
        return Vec::new();
    };
    jobs.iter()
        .filter(|(_, j)| j["if"].as_str().and_then(ungate).is_some())
        .filter_map(|(id, _)| id.as_str().map(str::to_string))
        .collect()
}

/**
 * Whether a condition is a whole gate init wrote: this version's, which ends with the fork clause,
 * or v0.1.4 to v0.1.7's base branch or Dependabot. The bare `github.ref == 'refs/heads/main'` of
 * v0.1.1 to v0.1.3 is left alone, since a release job's own `if:` reads the same and act must not
 * run that
 **/
fn is_gate(c: &str) -> bool {
    let old = c
        .strip_prefix("(github.ref == 'refs/heads/")
        .and_then(|r| r.strip_suffix("' || github.actor == 'dependabot[bot]')"))
        .is_some_and(|b| !b.contains(['\'', '(', ')']));
    old || one_group(c) && c.ends_with(&format!(" || {FORK})"))
}

/// Whether `c` is one parenthesised group, its first `(` closed by its last `)`, quotes skipped
fn one_group(c: &str) -> bool {
    let (mut depth, mut quoted) = (0, false);

    for (i, ch) in c.char_indices() {
        match ch {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            _ => {}
        }

        if depth == 0 && !quoted {
            return i == c.len() - 1 && i > 0;
        }
    }

    false
}

/**
 * The branches besides `base` that GitHub must still run a gated job for, since the daemon handles
 * only PRs into `base`: the push filter's patterns that a feature branch does not match, and
 * whether PRs into another branch trigger it. Negated patterns are dropped, so this errs to more
 **/
pub fn other_branches(source: &str, base: &str) -> (Vec<String>, bool) {
    let Ok(root) = load(source) else {
        return (Vec::new(), false);
    };
    let on = root.get(&key("on")).cloned().unwrap_or(Yaml::BadValue);
    let list = |y: &Yaml| -> Vec<String> {
        match y {
            Yaml::String(p) => vec![p.clone()],
            Yaml::Array(items) => items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        }
    };
    let pushes = match &on["push"]["branches"] {
        Yaml::BadValue => Vec::new(),
        patterns => list(patterns)
            .into_iter()
            .filter(|p| p != base && !p.starts_with('!') && !p.starts_with("dependabot/"))
            .filter(|p| !branch_patterns_match(&key(p)))
            .collect(),
    };
    let prs = match &on {
        Yaml::String(e) => e == "pull_request",
        Yaml::Array(events) => events.iter().any(|e| e.as_str() == Some("pull_request")),
        Yaml::Hash(events) => {
            events
                .get(&key("pull_request"))
                .is_some_and(|pr| match &pr["branches"] {
                    Yaml::BadValue => true,
                    patterns => list(patterns)
                        .iter()
                        .any(|p| p != base && !p.starts_with('!')),
                })
        }
        _ => false,
    };
    (pushes, prs)
}

fn quote(s: &str) -> String {
    s.replace('\'', "''")
}

/**
 * A condition true on a push to a branch the pattern matches, and maybe on a few more, never fewer.
 * GitHub's expressions have no glob, so the text before the first glob character and after the
 * last one pin it. None when neither pins anything, as for `[ab]*`, which would be true everywhere
 **/
pub fn push_condition(pattern: &str) -> Option<String> {
    let globs = ['*', '?', '[', ']', '+'];
    let Some(first) = pattern.find(globs) else {
        return Some(format!("github.ref == 'refs/heads/{}'", quote(pattern)));
    };
    let last = pattern.rfind(globs).unwrap_or(first);
    let (head, tail) = (&pattern[..first], &pattern[last + 1..]);
    let starts = format!("startsWith(github.ref, 'refs/heads/{}')", quote(head));
    let ends = format!("endsWith(github.ref, '{}')", quote(tail));

    match (head.is_empty(), tail.is_empty()) {
        (true, true) => None,
        (false, true) => Some(starts),
        (true, false) => Some(ends),
        (false, false) => Some(format!("({starts} && {ends})")),
    }
}

/// The condition `gate_jobs` writes: true where GitHub still runs a gated job
pub fn gate_condition(source: &str, base: &str) -> String {
    let (pushes, prs) = other_branches(source, base);
    let mut refs = vec![format!("github.ref == 'refs/heads/{}'", quote(base))];

    for cond in pushes.iter().filter_map(|p| push_condition(p)) {
        if !refs.contains(&cond) {
            refs.push(cond);
        }
    }

    if prs {
        refs.push(format!(
            "(github.event_name == 'pull_request' && github.base_ref != '{}')",
            quote(base)
        ));
    }

    format!(
        "({} || github.actor == 'dependabot[bot]' || {FORK})",
        refs.join(" || ")
    )
}

/**
 * The workflow with `if: github.ref == 'refs/heads/<base>'` on each of `jobs`, and-ed with an
 * `if:` a job has, so on a feature branch GitHub runs only the others. Pushes to the other branches
 * the workflow names and PRs into them keep the jobs, since shipcheck runs only PRs into `base`.
 * A fork's PR keeps them too, since the daemon never runs one. Triggers stay as they are. A job
 * an earlier init gated keeps its `if:`, so a second init adds no second gate. None when a job's
 * form is one this does not edit, or the result would parse otherwise
 **/
pub fn gate_jobs(source: &str, jobs: &[String], base: &str) -> Option<String> {
    let plain = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
    };

    if !plain(base) || jobs.is_empty() {
        return None;
    }

    let gate = gate_condition(source, base);
    let root = load(source).ok()?;
    let parsed = Yaml::Hash(root.clone());
    let lines: Vec<&str> = source.lines().collect();
    let start = lines.iter().position(|l| {
        l.strip_prefix("jobs:")
            .is_some_and(|r| r.trim().is_empty() || r.trim_start().starts_with('#'))
    })?;
    let end = (start + 1..lines.len())
        .find(|&i| !blank(lines[i]) && !lines[i].starts_with([' ', '\t']))
        .unwrap_or(lines.len());
    let body = &lines[start + 1..end];
    let (_, entries_found) = entries(body);
    let mut want = Vec::new();
    // Line ranges to replace, in the whole file, and their new lines
    let mut edits: Vec<(usize, usize, String)> = Vec::new();

    for id in jobs {
        let &(_, s, e) = entries_found.iter().find(|(k, _, _)| k == id)?;

        if !value_of(body[s]).is_empty() || e <= s + 1 {
            return None;
        }

        let (indent, keys) = entries(&body[s + 1..e]);
        let old = match &parsed["jobs"][id.as_str()]["if"] {
            Yaml::BadValue => None,
            Yaml::String(c) => Some(c.trim().to_string()),
            Yaml::Boolean(b) => Some(b.to_string()),
            _ => return None,
        };

        if old.as_deref().and_then(ungate).is_some() {
            continue;
        }

        let at = start + 1 + s + 1;

        match (old, keys.iter().find(|(k, _, _)| k == "if")) {
            (None, None) => {
                want.push((id.clone(), gate.clone()));
                edits.push((at, at, format!("{indent}if: {gate}")));
            }
            (Some(old), Some(&(_, ks, ke))) => {
                let inner = match old.strip_prefix("${{").and_then(|o| o.strip_suffix("}}")) {
                    Some(i) if !i.contains("${{") => i.trim().to_string(),
                    Some(_) => return None,
                    None if old.contains("${{") => return None,
                    None => old,
                };
                let cond = format!("({inner}) && {gate}");
                let quoted = format!("\"{}\"", cond.replace('\\', "\\\\").replace('"', "\\\""));
                want.push((id.clone(), cond));
                edits.push((at + ks, at + ke, format!("{indent}if: {quoted}")));
            }
            _ => return None,
        }
    }

    edits.sort_by_key(|(s, _, _)| *s);
    let mut out: Vec<String> = Vec::new();
    let mut at = 0;

    for (s, e, line) in edits {
        out.extend(lines[at..s].iter().map(|l| l.to_string()));
        out.push(line);
        at = e;
    }

    out.extend(lines[at..].iter().map(|l| l.to_string()));
    let mut text = out.join("\n");

    if source.ends_with('\n') {
        text.push('\n');
    }

    // Only the `if:` of those jobs changed
    let strip = |mut r: Hash| {
        if let Some(Yaml::Hash(js)) = r.get_mut(&key("jobs")) {
            for id in jobs {
                if let Some(Yaml::Hash(j)) = js.get_mut(&key(id)) {
                    j.remove(&key("if"));
                }
            }
        }

        r
    };
    let new = load(&text).ok()?;
    let ok = want.iter().all(|(id, c)| {
        new.get(&key("jobs")).map(|j| &j[id.as_str()]["if"]) == Some(&Yaml::String(c.clone()))
    }) && strip(new) == strip(root);
    ok.then_some(text)
}

fn blank(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// The text after a mapping key's colon, without a trailing comment
fn value_of(line: &str) -> &str {
    let v = line.split_once(':').map_or("", |(_, v)| v);
    v.split_once(" #").map_or(v, |(v, _)| v).trim()
}

/// The mapping entries at the first entry's indentation: key, first line, end past the last line that is not blank or a comment
fn entries(lines: &[&str]) -> (String, Vec<(String, usize, usize)>) {
    let indent = lines
        .iter()
        .find(|l| !blank(l))
        .map(|l| l[..l.len() - l.trim_start().len()].to_string())
        .unwrap_or_default();
    let starts: Vec<usize> = (0..lines.len())
        .filter(|&i| {
            !blank(lines[i])
                // A `- item` at the key's own indentation belongs to the key above, which YAML allows
                && lines[i]
                    .strip_prefix(indent.as_str())
                    .is_some_and(|r| !r.starts_with([' ', '\t', '-']))
        })
        .collect();
    let entries = starts
        .iter()
        .enumerate()
        .map(|(n, &s)| {
            let next = starts.get(n + 1).copied().unwrap_or(lines.len());
            let end = (s + 1..next)
                .rev()
                .find(|&i| !blank(lines[i]))
                .map_or(s + 1, |i| i + 1);
            let name = lines[s].trim().split(':').next().unwrap_or_default();
            (name.trim_matches(['"', '\'']).to_string(), s, end)
        })
        .collect();
    (indent, entries)
}

/// Each job id with the ids in its `needs`, in file order
pub fn job_needs(source: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut root = load(source)?;
    let mut out = Vec::new();

    for (id, job) in jobs_mut(&mut root)?.iter() {
        // Job ids become directory names, so only what GitHub allows: letters, digits, `_` and `-`
        let id = id
            .as_str()
            .and_then(job_id)
            .ok_or_else(|| format!("job id {id:?} is not letters, digits, `_` and `-`"))?;
        let needs = match &job["needs"] {
            Yaml::String(n) => vec![n.clone()],
            Yaml::Array(list) => list
                .iter()
                .map(|n| n.as_str().map(String::from))
                .collect::<Option<_>>()
                .ok_or_else(|| format!("job {id} has a `needs` entry that is not a job id"))?,
            Yaml::BadValue | Yaml::Null => Vec::new(),
            _ => {
                return Err(format!("job {id} has a `needs` that is not a list"));
            }
        };
        out.push((id.to_string(), needs));
    }

    let ids: Vec<&String> = out.iter().map(|(id, _)| id).collect();

    for (id, needs) in &out {
        if let Some(missing) = needs.iter().find(|n| !ids.contains(n)) {
            return Err(format!("job {id} needs {missing}, which is not a job"));
        }
    }

    Ok(out)
}

/// A name from a commit as a code span, safe in markdown and on a terminal. Only printable ASCII
/// other than a backtick is kept, so no control, bidi or invisible character can change what shows
pub fn shown(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '`' || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect();
    format!("`{safe}`")
}

/// The largest workflow from a commit that shipcheck parses, in bytes
pub const MAX_COMPARED: u64 = 1 << 20;

/// Whether a workflow from a commit is safe to load: yaml-rust2 copies each alias as it loads, so a
/// few hundred bytes of nested aliases take gigabytes. The event scan does not expand them
pub(crate) fn safe_to_load(src: &str) -> bool {
    if src.len() as u64 > MAX_COMPARED {
        return false;
    }

    let mut parser = yaml_rust2::parser::Parser::new_from_str(src);

    loop {
        match parser.next_token() {
            Ok((yaml_rust2::Event::StreamEnd, _)) => return true,
            Ok((yaml_rust2::Event::Alias(_), _)) | Err(_) => return false,
            Ok(_) => {}
        }
    }
}

/// Jobs a new version of a workflow lets fail, never runs, or drops, where the old version did not
pub fn weakened_jobs(old: &str, new: &str) -> Vec<String> {
    if !safe_to_load(old) || !safe_to_load(new) {
        return vec!["jobs not compared, the file uses YAML aliases or does not parse".into()];
    }

    let jobs = |src: &str| {
        load(src)
            .ok()
            .and_then(|mut root| jobs_mut(&mut root).ok().cloned())
            .unwrap_or_default()
    };
    let (old, new) = (jobs(old), jobs(new));
    // An expression such as `${{ matrix.experimental }}` may allow a failure, so it counts
    let allows = |job: &Yaml| match &job["continue-on-error"] {
        Yaml::Boolean(b) => *b,
        Yaml::String(s) => s.trim() != "false",
        _ => false,
    };
    let disabled = |job: &Yaml| match &job["if"] {
        Yaml::Boolean(b) => !*b,
        Yaml::String(s) => ["false", "${{ false }}"].contains(&s.trim()),
        _ => false,
    };
    let mut found = Vec::new();

    for (id, job) in &new {
        let id = id.as_str().unwrap_or_default();
        let before = old.get(&key(id));

        if allows(job) && !before.is_some_and(allows) {
            found.push(format!("job {} gets continue-on-error", shown(id)));
        }

        if disabled(job) && !before.is_some_and(disabled) {
            found.push(format!("job {} gets if: false", shown(id)));
        }

        // Steps are counted per label, its id, name, run or uses, so two steps with one label never
        // hide each other and the work stays linear
        let label = |step: &Yaml| {
            ["id", "name", "run", "uses"]
                .iter()
                .find_map(|k| step[*k].as_str().map(str::to_string))
                .unwrap_or_default()
        };
        let count = |job: Option<&Yaml>| {
            let mut counts: BTreeMap<String, [usize; 3]> = BTreeMap::new();

            for step in job.and_then(|j| j["steps"].as_vec()).into_iter().flatten() {
                let c = counts.entry(label(step)).or_default();
                c[0] += 1;
                c[1] += usize::from(allows(step));
                c[2] += usize::from(disabled(step));
            }

            counts
        };
        let (was, now) = (count(before), count(Some(job)));

        for (name, now) in &now {
            let was = was.get(name).copied().unwrap_or_default();
            let name = shown(name.lines().next().unwrap_or_default());

            if now[1] > was[1] {
                found.push(format!(
                    "job {} step {name} gets continue-on-error",
                    shown(id)
                ));
            }

            if now[2] > was[2] {
                found.push(format!("job {} step {name} gets if: false", shown(id)));
            }
        }

        for (name, was) in &was {
            if now.get(name).map_or(0, |n| n[0]) < was[0] {
                let name = shown(name.lines().next().unwrap_or_default());
                found.push(format!("job {} step {name} is removed", shown(id)));
            }
        }
    }

    for id in old.keys().filter(|id| !new.contains_key(*id)) {
        found.push(format!(
            "job {} is removed",
            shown(id.as_str().unwrap_or_default())
        ));
    }

    found
}

/// Jobs with `continue-on-error: true`, whose failure does not fail the workflow on GitHub
pub fn allowed_failures(source: &str) -> Result<BTreeSet<String>, String> {
    let mut root = load(source)?;
    Ok(jobs_mut(&mut root)?
        .iter()
        .filter(|(_, job)| {
            matches!(&job["continue-on-error"], Yaml::Boolean(true))
                || job["continue-on-error"].as_str() == Some("true")
        })
        .filter_map(|(id, _)| id.as_str().map(String::from))
        .collect())
}

/// Wraps the steps of every job in start and end marker steps. Only the run copy is edited, the
/// repository's workflows stay as they are
pub fn add_markers(source: &str, nonce: &str) -> Result<String, String> {
    let mut root = load(source)?;

    for job in jobs_mut(&mut root)?.values_mut() {
        let Some(Yaml::Array(steps)) = job.as_mut_hash().and_then(|j| j.get_mut(&key("steps")))
        else {
            continue;
        };
        steps.insert(
            0,
            step(
                "shipcheck start",
                None,
                format!("echo \"shipcheck-start {nonce} ${{{{ github.job }}}}\""),
            ),
        );
        steps.push(step(
            "shipcheck end",
            Some("always()"),
            format!("echo \"shipcheck-end {nonce} ${{{{ github.job }}}} ${{{{ job.status }}}}\""),
        ));
    }

    emit(root)
}

#[derive(Debug, Default, PartialEq)]
pub struct JobMarks {
    pub started: u32,
    pub ended: u32,
    pub failed: bool,
}

impl JobMarks {
    pub fn passed(&self) -> bool {
        self.started > 0 && self.started == self.ended && !self.failed
    }
}

fn job_id(token: &str) -> Option<&str> {
    let ok = !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then_some(token)
}

/// Reads the markers of one run from its log, per job id. A matrix job prints one pair per leg
pub fn read_marks(log: &str, nonce: &str) -> BTreeMap<String, JobMarks> {
    let start = format!("shipcheck-start {nonce} ");
    let end = format!("shipcheck-end {nonce} ");
    let mut marks: BTreeMap<String, JobMarks> = BTreeMap::new();

    for line in log.lines() {
        if let Some((_, rest)) = line.split_once(&start) {
            // act also prints each command before it runs it, with the `${{ }}` still unexpanded
            if let Some(job) = rest.split_whitespace().next().and_then(job_id) {
                marks.entry(job.to_string()).or_default().started += 1;
            }
        } else if let Some((_, rest)) = line.split_once(&end) {
            let mut words = rest.split_whitespace();
            if let (Some(job), Some(status)) = (words.next().and_then(job_id), words.next()) {
                let m = marks.entry(job.to_string()).or_default();
                m.ended += 1;
                m.failed |= status != "success";
            }
        }
    }

    marks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_condition_false_on_every_pr_does_not_read_the_pr() {
        for cond in [
            "github.event_name == 'push' && github.ref == format('refs/heads/{0}', github.event.repository.default_branch)",
            "github.ref == format('refs/heads/{0}', github.event.repository.default_branch)",
            "github.ref_name == github.event.repository.default_branch",
            "github.event_name == 'push' && github.head_ref == ''",
        ] {
            assert!(!reads_pr(cond), "{cond}");
        }

        for cond in [
            "contains(github.event.pull_request.labels.*.name, 'slow')",
            "github.ref != format('refs/heads/{0}', github.event.repository.default_branch)",
            "github.event_name == 'push' || github.event_name == 'pull_request'",
            "!(github.ref == 'refs/heads/main') && github.head_ref != ''",
        ] {
            assert!(reads_pr(cond), "{cond}");
        }

        assert!(!reads_pr("always()"));
    }

    #[test]
    fn reads_base_only_jobs_docker_steps_and_a_sha_pins_name() {
        let ci = "on: push\njobs:\n  a:\n    if: github.ref == 'refs/heads/main'\n    steps:\n      - uses: dtolnay/rust-toolchain@b725eda525362e7cf34af9a7243cac1e3f01a7ec # stable\n      - run: echo\n  b:\n    if: (github.ref == 'refs/heads/main' || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\n    steps:\n      - run: docker version\n  c:\n    steps:\n      - run: act --version\n  d:\n    steps:\n      - uses: docker/setup-buildx-action@v3\n  e:\n    steps:\n      - uses: actions/checkout@v4 # v4\n      - run: hadolint Dockerfile && echo docker-ish exact\n";
        let jobs = job_runs(ci);
        let job = |id: &str| jobs.iter().find(|j| j.id == id).unwrap();
        assert!(job("a").base_only);
        assert!(!job("b").base_only, "an || can run it off the base branch");
        assert!(
            !job("b").pr_only,
            "init's fork clause is false on a push to the base branch"
        );
        let pr = job_runs(
            "on: pull_request\njobs:\n  p:\n    if: github.event.pull_request.draft == false\n    steps:\n      - run: echo\n",
        );
        assert!(pr[0].pr_only);
        assert_eq!(job("a").steps[0].ref_name.as_deref(), Some("stable"));
        assert_eq!(
            job("e").steps[0].ref_name,
            None,
            "only a SHA pin's comment names its ref"
        );
        assert!(!job("a").runs_docker);
        assert!(job("b").runs_docker && job("c").runs_docker && job("d").runs_docker);
        assert!(
            !job("e").runs_docker,
            "a word that only contains docker is not Docker"
        );
    }

    #[test]
    fn pull_request_target_is_its_own_trigger() {
        assert!(on_pull_request_target("on: pull_request_target\n"));
        assert!(on_pull_request_target(
            "on:\n  pull_request_target:\n    types: [opened]\n"
        ));
        assert!(!on_pull_request_target("on: [pull_request]\n"));
    }

    #[test]
    fn gate_jobs_runs_local_jobs_on_github_only_on_the_base() {
        // flask's tests.yaml, trimmed: typing runs in shipcheck, the matrix stays
        let flask = "name: Tests\non:\n  pull_request:\n    paths-ignore: ['docs/**', 'README.md']\n  push:\n    branches: [main, stable]\njobs:\n  tests:\n    name: ${{ matrix.name || matrix.python }}\n    strategy:\n      matrix:\n        include:\n          - {python: '3.14'}\n    steps:\n      - run: uv run tox run\n  typing:\n    runs-on: ubuntu-latest\n    steps:\n      - run: uv run tox run -e typing\n";
        let want = flask.replace(
            "  typing:\n",
            "  typing:\n    if: (github.ref == 'refs/heads/main' || github.ref == 'refs/heads/stable' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\n",
        );
        assert_eq!(
            gate_jobs(flask, &["typing".into()], "main").as_deref(),
            Some(want.as_str())
        );

        // ripgrep's ci.yml, trimmed, with a job that has an `if:` of its own, plain and in `${{ }}`
        let ripgrep = "on:\n  pull_request:\n  push:\n    branches:\n    - master\njobs:\n  test:\n    name: test\n    runs-on: ${{ matrix.os }}\n    strategy:\n      matrix:\n        include:\n        - os: windows-latest\n    steps:\n    - run: cargo test\n  rustfmt:\n    runs-on: ubuntu-latest\n    if: (!cancelled() && !failure()) # after the rest\n    steps:\n    - run: cargo fmt --all --check\n  docs:\n    if: ${{ github.event_name != 'schedule' }}\n    runs-on: ubuntu-latest\n    steps:\n    - run: cargo doc\n";
        let gated = gate_jobs(ripgrep, &["rustfmt".into(), "docs".into()], "master").unwrap();
        assert!(gated.contains(
            "    if: \"((!cancelled() && !failure())) && (github.ref == 'refs/heads/master' || (github.event_name == 'pull_request' && github.base_ref != 'master') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\"\n"
        ));
        assert!(gated.contains(
            "    if: \"(github.event_name != 'schedule') && (github.ref == 'refs/heads/master' || (github.event_name == 'pull_request' && github.base_ref != 'master') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\"\n"
        ));
        assert!(gated.starts_with("on:\n  pull_request:\n  push:\n    branches:\n    - master\n"));
        assert_eq!(gated.lines().count(), ripgrep.lines().count());

        // An Anchor project's ci.yml, trimmed: push on every branch, the PR found from the branch
        let push_only = "on:\n  push:\n\njobs:\n  checks:\n    runs-on: ubuntu-latest\n    steps:\n      - run: bun test\n  tests:\n    strategy:\n      matrix:\n        os: [ubuntu-latest, macos-latest]\n    runs-on: ${{ matrix.os }}\n    steps:\n      - run: cargo test\n";
        let gated = gate_jobs(push_only, &["checks".into()], "main").unwrap();
        assert!(gated.starts_with(
            "on:\n  push:\n\njobs:\n  checks:\n    if: (github.ref == 'refs/heads/main' || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\n    runs-on"
        ));

        // vite's ci.yml, trimmed: pushes to maintenance branches and PRs into them keep lint
        let vite = "on:\n  push:\n    branches:\n      - main\n      - release/*\n      - \"v[0-9]+\"\n      - \"v[0-9]+.[0-9]+\"\n      - hotfix\n      - \"**\"\n      - \"!wip\"\n  pull_request:\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: pnpm run lint\n";
        let gated = gate_jobs(vite, &["lint".into()], "main").unwrap();
        assert!(gated.contains(
            "    if: (github.ref == 'refs/heads/main' || startsWith(github.ref, 'refs/heads/release/') || startsWith(github.ref, 'refs/heads/v') || github.ref == 'refs/heads/hotfix' || (github.event_name == 'pull_request' && github.base_ref != 'main') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\n"
        ), "{gated}");

        // PRs only into the base need no clause for other branches
        let into_main = "on:\n  push:\n    branches: [main]\n  pull_request:\n    branches: [main]\njobs:\n  lint:\n    steps:\n      - run: make lint\n";
        assert!(gate_jobs(into_main, &["lint".into()], "main").unwrap().contains(
            "    if: (github.ref == 'refs/heads/main' || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)\n"
        ));
        assert_eq!(
            other_branches(
                "on:\n  pull_request:\n    branches: [main, stable]\n",
                "main"
            ),
            (Vec::new(), true)
        );

        // A job in flow form, a missing job or an unusual base is left for the owner
        let flow = "on: push\njobs:\n  a: {runs-on: x, steps: [{run: make}]}\n";
        assert_eq!(gate_jobs(flow, &["a".into()], "main"), None);
        assert_eq!(gate_jobs(flask, &["gone".into()], "main"), None);
        assert_eq!(gate_jobs(flask, &["typing".into()], "a b"), None);
    }

    #[test]
    fn ungate_removes_only_the_gate() {
        let gate = "(github.ref == 'refs/heads/main' || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)";
        assert_eq!(ungate(gate).as_deref(), Some(""));
        assert_eq!(
            ungate(&format!("((a) && (b)) && {gate}")).as_deref(),
            Some("(a) && (b)")
        );
        assert_eq!(ungate("github.ref == 'refs/heads/main'"), None);
        assert_eq!(ungate("always()"), None);

        // Two inits' gates, from before gate_jobs left a gated job alone, both go
        assert_eq!(ungate(&format!("({gate}) && {gate}")).as_deref(), Some(""));
        assert_eq!(
            ungate(&format!("((always()) && {gate}) && {gate}")).as_deref(),
            Some("always()")
        );
        // Only a whole gate after the last `&&` counts
        assert_eq!(ungate(&format!("(a) || (b) && {gate}")), None);
        assert_eq!(ungate(&format!("(a) && {gate} && (b)")), None);

        // v0.1.4 to v0.1.7 wrote the base branch or Dependabot, without the fork clause
        let old = "(github.ref == 'refs/heads/master' || github.actor == 'dependabot[bot]')";
        assert_eq!(ungate(old).as_deref(), Some(""));
        assert_eq!(
            ungate(&format!("(always()) && {old}")).as_deref(),
            Some("always()")
        );

        // vite's form, with other branches and PRs into them, round trips through gate_jobs
        let vite = "on:\n  push:\n    branches: [main, \"v[0-9]+\"]\n  pull_request:\njobs:\n  a:\n    steps:\n      - run: make\n  b:\n    if: always()\n    steps:\n      - run: make\n";
        let gated = gate_jobs(vite, &["a".into(), "b".into()], "main").unwrap();
        let jobs = job_runs(&gated);
        let root = Yaml::Hash(load(&gated).unwrap());
        let cond = |id: &str| root["jobs"][id]["if"].as_str().unwrap().to_string();
        assert!(cond("a").contains("startsWith(github.ref, 'refs/heads/v')"));
        assert_eq!(ungate(&cond("a")).as_deref(), Some(""));
        assert_eq!(ungate(&cond("b")).as_deref(), Some("always()"));
        assert!(
            jobs.iter().all(|j| !j.pr_only),
            "the gate runs them on the base branch"
        );
    }

    #[test]
    fn a_second_gate_leaves_a_gated_job_as_it_is() {
        let ci = "on: [push, pull_request]\njobs:\n  a:\n    steps:\n      - run: make\n  b:\n    if: always()\n    steps:\n      - run: make\n  c:\n    steps:\n      - run: make\n";
        let once = gate_jobs(ci, &["a".into(), "b".into()], "main").unwrap();
        assert_eq!(gated_jobs(&once), ["a", "b"]);
        assert_eq!(
            gate_jobs(&once, &["a".into(), "b".into()], "main").as_deref(),
            Some(once.as_str())
        );
        let twice = gate_jobs(&once, &["a".into(), "b".into(), "c".into()], "main").unwrap();
        assert_eq!(
            twice,
            gate_jobs(ci, &["a".into(), "b".into(), "c".into()], "main").unwrap()
        );
        assert!(job_runs(&twice).iter().all(|j| !j.pr_only));

        // A job two inits gated is no PR-only job, and act runs it with no `if:`
        let gate = gate_condition(ci, "main");
        let stacked = format!(
            "on: push\njobs:\n  a:\n    if: \"({gate}) && {gate}\"\n    steps:\n      - run: make\n"
        );
        assert!(!job_runs(&stacked)[0].pr_only);
        assert_eq!(gated_jobs(&stacked), ["a"]);
    }

    #[test]
    fn a_push_pattern_becomes_a_condition_that_never_runs_fewer() {
        // curl's '*/ci', which a prefix would turn into every branch
        assert_eq!(
            push_condition("*/ci").as_deref(),
            Some("endsWith(github.ref, '/ci')")
        );
        assert_eq!(
            push_condition("release/*").as_deref(),
            Some("startsWith(github.ref, 'refs/heads/release/')")
        );
        assert_eq!(
            push_condition("v*.x").as_deref(),
            Some("(startsWith(github.ref, 'refs/heads/v') && endsWith(github.ref, '.x'))")
        );
        assert_eq!(push_condition("[ab]*"), None);
        let curl = "on:\n  push:\n    branches: [master, '*/ci', '[ab]*']\njobs:\n  a:\n    steps:\n      - run: make\n";
        assert_eq!(
            gate_condition(curl, "master"),
            "(github.ref == 'refs/heads/master' || endsWith(github.ref, '/ci') || github.actor == 'dependabot[bot]' || github.event.pull_request.head.repo.fork)"
        );
    }

    #[test]
    fn a_pull_request_closed_trigger_is_not_a_branch_run() {
        // fastapi's create-draft-release.yml
        let closed = "on:\n  pull_request:\n    types:\n      - closed\njobs: {}\n";
        assert!(!runs_for_branches(closed));
        assert!(runs_for_branches(
            "on:\n  pull_request:\n    types: [opened, synchronize, labeled]\njobs: {}\n"
        ));
        assert!(runs_for_branches(
            "on:\n  pull_request:\n    types: synchronize\njobs: {}\n"
        ));
    }

    #[test]
    fn shows_names_safely() {
        assert_eq!(shown("ci.yml"), "`ci.yml`");
        assert_eq!(
            shown("x`\n| passed |\u{1b}[2J"),
            "`x??| passed |?[2J`",
            "a name can neither close the code span nor add lines or terminal escapes"
        );
        assert_eq!(
            shown("ci\u{202e}lmy.\u{200b}x é"),
            "`ci?lmy.?x ?`",
            "bidi overrides and invisible characters show as ?"
        );
    }

    #[test]
    fn refuses_to_load_aliases_from_a_commit() {
        let bomb = "a: &a [x, x, x, x, x, x, x, x, x]\nb: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a]\n\
                    c: &c [*b, *b, *b, *b, *b, *b, *b, *b, *b]\njobs: {t: {}}\n";
        assert_eq!(
            weakened_jobs("jobs: {t: {}}\n", bomb),
            ["jobs not compared, the file uses YAML aliases or does not parse"]
        );
        assert!(safe_to_load("on: push\njobs: {t: {runs-on: x}}\n"));
        assert!(!safe_to_load("jobs: [unclosed\n"));
    }

    #[test]
    fn finds_jobs_a_change_weakens() {
        let old = "on: push\njobs:\n  a: {}\n  b: {continue-on-error: true}\n  c: {}\n  d: {}\n";
        let new = "\
on: push
jobs:
  a: {continue-on-error: true}
  b: {continue-on-error: true}
  c: {if: false}
  e: {continue-on-error: '${{ matrix.experimental }}'}
  f: {continue-on-error: false, if: github.ref == 'refs/heads/main'}
";
        assert_eq!(
            weakened_jobs(old, new),
            [
                "job `a` gets continue-on-error",
                "job `c` gets if: false",
                "job `e` gets continue-on-error",
                "job `d` is removed",
            ]
        );
        assert!(weakened_jobs(old, old).is_empty());
    }

    #[test]
    fn finds_steps_a_change_weakens() {
        let old = "on: push\njobs:\n  t:\n    steps:\n      - run: cargo test\n      - {name: lint, run: x, continue-on-error: true}\n";
        let new = "on: push\njobs:\n  t:\n    steps:\n      - {run: cargo test, continue-on-error: true}\n      - {name: lint, run: x, continue-on-error: true}\n      - {name: docs, run: y, if: false}\n";
        assert_eq!(
            weakened_jobs(old, new),
            [
                "job `t` step `cargo test` gets continue-on-error",
                "job `t` step `docs` gets if: false",
            ]
        );
        // Two steps with one label: the second one newly allowed is still found, and a removed one too
        let old = "on: push\njobs:\n  t:\n    steps:\n      - {run: x, continue-on-error: true}\n      - run: x\n      - run: pytest\n";
        let new = "on: push\njobs:\n  t:\n    steps:\n      - {run: x, continue-on-error: true}\n      - {run: x, continue-on-error: true}\n";
        assert_eq!(
            weakened_jobs(old, new),
            [
                "job `t` step `x` gets continue-on-error",
                "job `t` step `pytest` is removed",
            ]
        );
    }

    #[test]
    fn finds_workflows_that_run_for_feature_branches() {
        let runs = |on: &str| runs_for_branches(&format!("{on}\njobs: {{}}\n"));

        for on in [
            "on: pull_request",
            "on: push",
            "on: [push, release]",
            "on:\n  pull_request:\n    branches: [main]",
            "on:\n  push:\n",
            "on:\n  push:\n    paths: ['src/**']",
            "on:\n  push:\n    branches: ['**']",
            "on:\n  push:\n    branches: [main, '*']",
            "on:\n  push:\n    branches-ignore: [main]",
        ] {
            assert!(runs(on), "{on}");
        }

        for on in [
            "on:\n  push:\n    branches: [main]",
            "on:\n  push:\n    branches: main",
            "on:\n  push:\n    tags: ['v*']",
            "on:\n  push:\n    branches: [main, 'feature/*', 'release/**', 'main*']",
            "on:\n  push:\n    branches-ignore: ['**']",
            "on:\n  push:\n    tags-ignore: ['v*']",
            "on: pull_request_target",
            "on:\n  workflow_dispatch:\n",
            "on: release",
        ] {
            assert!(!runs(on), "{on}");
        }

        assert!(!runs_for_branches("not: [yaml"));
    }

    #[test]
    fn lists_jobs_with_their_needs() {
        let wf = "on: push\njobs:\n  a: {}\n  b: {needs: a}\n  c: {needs: [a, b]}\n";
        let graph = job_needs(wf).unwrap();
        assert_eq!(
            graph,
            [
                ("a".to_string(), vec![]),
                ("b".to_string(), vec!["a".to_string()]),
                ("c".to_string(), vec!["a".to_string(), "b".to_string()]),
            ]
        );
        assert!(job_needs("on: push\njobs:\n  a: {needs: gone}\n").is_err());
        assert!(job_needs("on: push\njobs:\n  ../up: {}\n").is_err());
        assert!(job_needs("on: push\njobs:\n  a: {needs: {x: 1}}\n").is_err());
    }

    #[test]
    fn wraps_steps_with_markers() {
        let wf = "\
on: push
jobs:
  test:
    steps:
      - run: cargo test
  call:
    uses: ./other.yml
";
        let out = &YamlLoader::load_from_str(&add_markers(wf, "abc").unwrap()).unwrap()[0];
        let steps = out["jobs"]["test"]["steps"].as_vec().unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(
            steps[0]["run"].as_str(),
            Some("echo \"shipcheck-start abc ${{ github.job }}\"")
        );
        assert_eq!(steps[1]["run"].as_str(), Some("cargo test"));
        assert_eq!(steps[2]["if"].as_str(), Some("always()"));
        assert_eq!(
            steps[2]["run"].as_str(),
            Some("echo \"shipcheck-end abc ${{ github.job }} ${{ job.status }}\"")
        );
        assert!(out["jobs"]["call"]["steps"].is_badvalue());
    }

    #[test]
    fn reads_marks_from_interleaved_crlf_logs() {
        let log = "[CI/a] | shipcheck-start n a\r\n\
                   [CI/b] | shipcheck-start n b\r\n\
                   [CI/a] | shipcheck-end n a success\r\n\
                   [CI/b] | shipcheck-end n b cancelled\r\n";
        let m = read_marks(log, "n");
        assert!(m["a"].passed());
        assert!(!m["b"].passed());
        assert_eq!(m["b"].ended, 1);
    }

    #[test]
    fn reads_marks_and_ignores_echoed_commands() {
        let log = "\
[CI/test] ⭐ Run Main echo \"shipcheck-start abc ${{ github.job }}\"
[CI/test]   | shipcheck-start abc test
[CI/lint]   | shipcheck-start abc lint
[CI/test]   | shipcheck-end abc test success
[CI/lint]   | shipcheck-end abc lint failure
[CI/build]   | shipcheck-start abc build
[CI/old]   | shipcheck-start zzz old
";
        let m = read_marks(log, "abc");
        assert_eq!(m.len(), 3);
        assert!(m["test"].passed());
        assert!(!m["lint"].passed());
        assert_eq!(
            m["build"],
            JobMarks {
                started: 1,
                ended: 0,
                failed: false
            }
        );
        assert!(!m["build"].passed());
    }
}
