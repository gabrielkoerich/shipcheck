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
    let unfiltered_push = |filter: &Yaml| match filter {
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
    };

    match root.get(&key("on")) {
        Some(Yaml::String(e)) => e == "pull_request" || e == "push",
        Some(Yaml::Array(events)) => events
            .iter()
            .any(|e| matches!(e.as_str(), Some("pull_request" | "push"))),
        Some(Yaml::Hash(events)) => {
            events.contains_key(&key("pull_request"))
                || events.get(&key("push")).is_some_and(unfiltered_push)
        }
        _ => false,
    }
}

// GitHub's glob rules, where `*` stops at `/`, against an ordinary branch name. `*`, `**` and `**/*`
// match it, `main`, `main*`, `feature/*` and `release/**` do not
fn branch_patterns_match(patterns: &Yaml) -> bool {
    let list = match patterns {
        Yaml::String(p) => vec![p.as_str()],
        Yaml::Array(items) => items.iter().filter_map(Yaml::as_str).collect(),
        _ => return false,
    };

    list.iter().any(|p| {
        globset::GlobBuilder::new(p)
            .literal_separator(true)
            .build()
            .is_ok_and(|g| g.compile_matcher().is_match("shipcheck-any-branch"))
    })
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
fn safe_to_load(src: &str) -> bool {
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
