use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::{RepoConfig, RunnerKind};
use crate::setup;

/// The hosted App's slug and ID, from issue #13
const APP_SLUG: &str = "shipcheck-run";
const APP_ID: &str = "5400635";

/// Questions on a terminal. With `yes`, each one takes its default without reading
pub struct Prompt<'a> {
    pub input: &'a mut dyn BufRead,
    pub out: &'a mut dyn Write,
    pub yes: bool,
}

impl Prompt<'_> {
    pub fn say(&mut self, text: &str) {
        let _ = writeln!(self.out, "{text}");
    }

    /// Empty, unreadable or anything but y or n takes the default
    pub fn ask(&mut self, question: &str, default: bool) -> bool {
        let hint = if default { "[Y/n]" } else { "[y/N]" };

        if self.yes {
            self.say(&format!(
                "{question} {hint} {}",
                if default { "y" } else { "n" }
            ));
            return default;
        }

        let _ = write!(self.out, "{question} {hint} ");
        let _ = self.out.flush();
        let mut line = String::new();

        if self.input.read_line(&mut line).is_err() {
            return default;
        }

        match line.trim().to_lowercase().chars().next() {
            Some('y') => true,
            Some('n') => false,
            _ => default,
        }
    }
}

/// The interactive `init`: config, App, workflows, branch protection. The caller installs the hooks,
/// and with no shipcheck.toml written neither does anything after step 1
pub fn run(root: &Path, p: &mut Prompt) -> Result<(), String> {
    p.say("1. shipcheck.toml");
    checks(root, p)?;

    // The App, the workflow edits and the ruleset are all for checks this repository does not have
    if !root.join("shipcheck.toml").exists() {
        return Ok(());
    }

    p.say("\n2. GitHub App");
    let app = app(p);
    p.say("\n3. Hosted Actions only on the base branch");
    workflows(root, &base_branch(root), p)?;
    p.say("\n4. Branch protection");
    protection(root, app.as_deref(), p);
    p.say("\n5. Hooks");
    Ok(())
}

/// Step 1: shows the detected checks and writes the ones the person keeps
pub fn checks(root: &Path, p: &mut Prompt) -> Result<(), String> {
    let config = root.join("shipcheck.toml");

    if config.exists() {
        p.say("shipcheck.toml exists, left as is");
        return Ok(());
    }

    let found = setup::detect(root);

    if found.checks.is_empty() {
        if let Some(wrote) = setup::init(root)? {
            p.say(&wrote);
        }

        return Ok(());
    }

    p.say(&format!("found these checks:\n{}", found.explain()));
    let mut keep = Vec::new();

    if p.ask("Keep all of them?", true) {
        keep = found.checks;
    } else {
        for c in found.checks {
            if p.ask(&format!("Keep {} = {}?", c.name, c.command), true) {
                keep.push(c);
            }
        }
    }

    if keep.is_empty() {
        p.say("no check kept, so init wrote no shipcheck.toml and set nothing up");
        return Ok(());
    }

    crate::agents::write_in_repo(root, &config, &setup::host_config(&keep, &found.versions))?;
    p.say("wrote shipcheck.toml");
    Ok(())
}

/// Step 2: the App that posts the check. Returns its ID, for the branch rule, when there is one
fn app(p: &mut Prompt) -> Option<String> {
    if let Ok(id) = std::env::var("SHIPCHECK_APP_ID") {
        p.say(&format!(
            "SHIPCHECK_APP_ID is {id}: install that App on this repository from its settings on GitHub"
        ));
        return Some(id);
    }

    let public = ureq::get(format!("https://api.github.com/apps/{APP_SLUG}"))
        .header("User-Agent", "shipcheck")
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .call()
        .is_ok();

    if !public {
        p.say("The hosted shipcheck App is not public yet. Until it is, the check needs an App of your own: set SHIPCHECK_APP_ID and put its key at app.pem in shipcheck's home (https://shipcheck.run/docs/install/#what-works-without-the-github-app)");
        return None;
    }

    let url = format!("https://github.com/apps/{APP_SLUG}/installations/new");
    p.say(&format!("Install the App on this repository: {url}"));

    if !p.yes && p.ask("Open it in the browser?", true) {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = Command::new(opener).arg(&url).status();
    }

    p.say("This machine signs in at the end, another machine signs in when `shipcheck install` runs there");
    Some(APP_ID.into())
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

/**
 * Workflows whose jobs shipcheck now checks, each with the jobs a host check came from: the
 * listed ones in act mode, with no jobs named, since act runs all of them
 **/
fn covered(root: &Path) -> BTreeMap<String, BTreeSet<String>> {
    let Some(cfg) = fs::read_to_string(root.join("shipcheck.toml"))
        .ok()
        .and_then(|t| RepoConfig::parse(&t).ok())
    else {
        return BTreeMap::new();
    };

    if cfg.runner == RunnerKind::Act {
        return cfg
            .workflows
            .into_iter()
            .map(|w| (w, BTreeSet::new()))
            .collect();
    }

    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for c in setup::detect(root).checks {
        if cfg.host.checks.get(&c.name) != Some(&c.command) {
            continue;
        }

        if let Some((file, rest)) = c.from.split_once(", job ") {
            let job = rest.split(", ").next().unwrap_or_default();
            out.entry(file.to_string())
                .or_default()
                .insert(job.to_string());
        }
    }

    out
}

/// Step 3: for each workflow GitHub runs on feature branches, shows the `on:` edit and writes it if asked
pub fn workflows(root: &Path, base: &str, p: &mut Prompt) -> Result<(), String> {
    let covered = covered(root);
    let kept = setup::detect(root).github;
    let mut offered = false;

    for file in setup::workflow_files(root).unwrap_or_default() {
        let path = root.join(".github/workflows").join(&file);
        let source = fs::read_to_string(&path).map_err(|e| format!("{file}: {e}"))?;
        let staying: Vec<&str> = kept
            .iter()
            .filter(|k| k.file == file)
            .map(|k| k.job.as_str())
            .collect();

        if !staying.is_empty() {
            offered = true;
            gate(root, &file, &source, &staying, covered.get(&file), base, p)?;
            continue;
        }

        let Some(edited) = crate::workflow::main_only(&source, base) else {
            if crate::workflow::runs_for_branches(&source) {
                p.say(&format!(
                    "{file}: its `on:` has a form shipcheck does not edit, change it by hand"
                ));
            }

            continue;
        };
        offered = true;
        p.say(&diff(&file, &source, &edited));
        let checked = covered.contains_key(&file);

        if !checked {
            p.say(&format!(
                "no shipcheck check runs {file}'s jobs, so PRs would not run them anywhere"
            ));
        }

        if p.ask(
            &format!("Run {file} on GitHub only on pushes to {base}?"),
            checked,
        ) {
            crate::agents::write_in_repo(root, &path, &edited)?;
            p.say(&format!("wrote .github/workflows/{file}"));
        }
    }

    if !offered {
        p.say("no workflow runs on feature branches");
    }

    Ok(())
}

/**
 * A workflow with jobs that stay on GitHub keeps its triggers, and the jobs shipcheck checks run
 * on GitHub only on the base branch. A job that a job left on GitHub needs is not gated, or GitHub
 * would skip that job too
 **/
fn gate(
    root: &Path,
    file: &str,
    source: &str,
    staying: &[&str],
    local: Option<&BTreeSet<String>>,
    base: &str,
    p: &mut Prompt,
) -> Result<(), String> {
    let jobs = crate::workflow::job_runs(source);
    let mut ungated: BTreeSet<String> = jobs
        .iter()
        .map(|j| j.id.clone())
        .filter(|id| !local.is_some_and(|l| l.contains(id)))
        .collect();

    // Until no job left on GitHub needs a gated one
    while let Some(need) = jobs
        .iter()
        .filter(|j| ungated.contains(&j.id))
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
    p.say(&format!(
        "{file}: on PRs GitHub runs {}, which stay on GitHub",
        staying.join(", ")
    ));

    if gated.is_empty() {
        p.say(&format!(
            "{file}: no job of it runs only in shipcheck, left as is"
        ));
        return Ok(());
    }

    let Some(edited) = crate::workflow::gate_jobs(source, &gated, base) else {
        p.say(&format!(
            "{file}: a job has a form shipcheck does not edit. To run {} on GitHub only on {base}, give each `if: github.ref == 'refs/heads/{base}'` by hand",
            gated.join(", ")
        ));
        return Ok(());
    };
    p.say(&diff(file, source, &edited));

    if p.ask(
        &format!(
            "Run {} of {file} on GitHub only on {base}, and keep the rest on PRs?",
            gated.join(", ")
        ),
        true,
    ) {
        crate::agents::write_in_repo(root, &root.join(".github/workflows").join(file), &edited)?;
        p.say(&format!("wrote .github/workflows/{file}"));
    } else {
        p.say(&format!(
            "{file} then runs fully on GitHub for PRs, beside shipcheck's checks"
        ));
    }

    Ok(())
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

/// The job check names of the config in `root`, the checks `init` requires
pub fn job_checks(root: &Path) -> Result<Vec<String>, String> {
    let text = fs::read_to_string(root.join("shipcheck.toml"))
        .map_err(|e| format!("shipcheck.toml: {e}"))?;
    let cfg = RepoConfig::parse(&text)?;
    let jobs = crate::run::expected_jobs(&cfg, |file| {
        fs::read_to_string(root.join(".github/workflows").join(file)).ok()
    })?;
    Ok(crate::check::job_names(&jobs)
        .into_iter()
        .map(|(_, _, name)| name)
        .collect())
}

/// The GitHub Actions App, which posts the checks of jobs that stay on GitHub
const ACTIONS_APP_ID: u64 = 15368;

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
        }]
    })
}

/// Step 4: creates the ruleset with the person's own `gh` login, which shipcheck never stores
fn protection(root: &Path, app: Option<&str>, p: &mut Prompt) {
    let Some(app) = app.filter(|a| a.parse::<u64>().is_ok()) else {
        p.say("skipped: no App posts the check yet, and a required check nobody posts blocks every PR");
        return;
    };

    let names = match job_checks(root) {
        Ok(names) if !names.is_empty() => names,
        Ok(_) => {
            p.say("skipped: shipcheck.toml names no job to require");
            return;
        }
        Err(e) => {
            p.say(&format!("skipped: {e}"));
            return;
        }
    };

    p.say(&format!(
        "One required check per job:\n{}",
        names
            .iter()
            .map(|n| format!("  {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    ));
    let mut github = Vec::new();

    for kept in setup::detect(root).github {
        match kept.checks {
            Some(checks) => github.extend(checks),
            None => p.say(&format!(
                "{}, job {}: its check names depend on its matrix or an expression, require them by hand once a PR shows them",
                kept.file, kept.job
            )),
        }
    }

    if !github.is_empty() {
        p.say(&format!(
            "And from GitHub Actions, for the jobs that stay on GitHub:\n{}",
            github
                .iter()
                .map(|n| format!("  {n}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    p.say("A required check that nobody posts blocks every PR. Turn this on once a first PR shows these checks, and update the ruleset when a job is added, renamed or removed (https://shipcheck.run/docs/branch-protection/)");

    if !p.ask(
        "Require these checks on the default branch, with branches up to date, using `gh`?",
        false,
    ) {
        return;
    }

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
        .spawn();
    let result = child.and_then(|mut c| {
        if let Some(mut stdin) = c.stdin.take() {
            stdin.write_all(ruleset(app, &names, &github).to_string().as_bytes())?;
        }

        c.wait()
    });

    match result {
        Ok(s) if s.success() => p.say("created the ruleset `shipcheck`"),
        Ok(_) => {
            p.say("gh could not create the ruleset, see its error above, or set it up by hand")
        }
        Err(e) => p.say(&format!("could not run gh: {e}. Set the rule up by hand")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt<'a>(input: &'a mut &'static [u8], out: &'a mut Vec<u8>) -> Prompt<'a> {
        Prompt {
            input,
            out,
            yes: false,
        }
    }

    #[test]
    fn ask_reads_answers_and_takes_the_default() {
        let mut input: &[u8] = b"n\n\nyes\nwhat\n";
        let mut out = Vec::new();
        let mut p = prompt(&mut input, &mut out);
        assert!(!p.ask("a?", true));
        assert!(p.ask("b?", true));
        assert!(p.ask("c?", false));
        assert!(!p.ask("d?", false));
        // End of input
        assert!(p.ask("e?", true));
        assert!(String::from_utf8(out).unwrap().contains("a? [Y/n] "));
    }

    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();

        for (name, text) in files {
            fs::create_dir_all(dir.path().join(name).parent().unwrap()).unwrap();
            fs::write(dir.path().join(name), text).unwrap();
        }

        dir
    }

    const CI: &str =
        "on:\n  pull_request:\n  push:\njobs:\n  test:\n    steps:\n      - run: cargo test\n";
    const LINT: &str =
        "on: [push, pull_request]\njobs:\n  lint:\n    steps:\n      - run: cargo clippy\n";

    #[test]
    fn checks_keeps_the_chosen_ones() {
        let dir = repo(&[
            (".github/workflows/ci.yml", CI),
            (".github/workflows/lint.yml", LINT),
        ]);
        let mut input: &[u8] = b"n\ny\nn\n";
        let mut out = Vec::new();
        checks(dir.path(), &mut prompt(&mut input, &mut out)).unwrap();
        let cfg =
            RepoConfig::parse(&fs::read_to_string(dir.path().join("shipcheck.toml")).unwrap())
                .unwrap();
        assert_eq!(cfg.host.checks.keys().collect::<Vec<_>>(), ["test"]);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("test = cargo test  (from ci.yml, job test)")
        );
    }

    #[test]
    fn workflows_writes_only_what_is_accepted() {
        let dir = repo(&[
            (".github/workflows/ci.yml", CI),
            (".github/workflows/lint.yml", LINT),
            (
                "shipcheck.toml",
                "runner = \"host\"\n[host.checks]\ntest = \"cargo test\"\n",
            ),
        ]);
        // ci.yml is checked, so yes is its default. lint.yml is not, and is declined
        let mut input: &[u8] = b"\nn\n";
        let mut out = Vec::new();
        workflows(dir.path(), "main", &mut prompt(&mut input, &mut out)).unwrap();
        let ci = fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap();
        assert_eq!(
            ci,
            "on:\n  push:\n    branches: [main]\njobs:\n  test:\n    steps:\n      - run: cargo test\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/lint.yml")).unwrap(),
            LINT
        );
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.contains("@@ -2 +2 @@\n-  pull_request:\n@@ -4 +3 @@\n+    branches: [main]"),
            "{said}"
        );
        assert!(
            said.contains("no shipcheck check runs lint.yml's jobs"),
            "{said}"
        );
    }

    #[test]
    fn workflows_keeps_a_matrix_on_github_and_gates_the_rest() {
        let ci = "on:\n  pull_request:\n  push:\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make lint\n  tests:\n    runs-on: ubuntu-latest\n    strategy:\n      matrix:\n        python: [\"3.12\", \"3.13\"]\n    steps:\n      - run: make test\n";
        let config = "runner = \"host\"\n[host.checks]\nlint = \"make lint\"\n";
        let dir = repo(&[(".github/workflows/ci.yml", ci), ("shipcheck.toml", config)]);
        let mut input: &[u8] = b"\n";
        let mut out = Vec::new();
        workflows(dir.path(), "main", &mut prompt(&mut input, &mut out)).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
            ci.replace(
                "  lint:\n",
                "  lint:\n    if: github.ref == 'refs/heads/main'\n"
            ),
            "{said}"
        );
        assert!(
            said.contains("ci.yml: on PRs GitHub runs tests, which stay on GitHub"),
            "{said}"
        );

        // Declined, the workflow stays whole on PRs
        let dir = repo(&[(".github/workflows/ci.yml", ci), ("shipcheck.toml", config)]);
        let mut input: &[u8] = b"n\n";
        let mut out = Vec::new();
        workflows(dir.path(), "main", &mut prompt(&mut input, &mut out)).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
            ci
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("ci.yml then runs fully on GitHub for PRs, beside shipcheck's checks")
        );

        // The matrix needs lint, so lint stays on GitHub too and nothing is gated
        let needs = ci.replace("  tests:\n", "  tests:\n    needs: [lint]\n");
        let dir = repo(&[
            (".github/workflows/ci.yml", &needs),
            ("shipcheck.toml", config),
        ]);
        let mut input: &[u8] = b"";
        let mut out = Vec::new();
        workflows(dir.path(), "main", &mut prompt(&mut input, &mut out)).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
            needs
        );
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.contains("on PRs GitHub runs tests, lint, which stay on GitHub"),
            "{said}"
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
        let mut input: &[u8] = b"\n";
        let mut out = Vec::new();
        workflows(dir.path(), "main", &mut prompt(&mut input, &mut out)).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/orch-review.yml")).unwrap(),
            review
        );
        assert!(
            !said.contains("Run orch-review.yml on GitHub only on pushes"),
            "{said}"
        );
        assert!(
            said.contains("orch-review.yml: no job of it runs only in shipcheck, left as is"),
            "{said}"
        );
    }

    #[test]
    fn init_stops_after_step_one_without_a_config() {
        // git-cow's ci.yml, trimmed: its only test job is a matrix with a macOS entry
        let ci = "on: [push, pull_request]\njobs:\n  test:\n    strategy:\n      matrix:\n        include:\n          - {os: macos-latest}\n          - {os: ubuntu-latest}\n    runs-on: ${{ matrix.os }}\n    steps:\n      - run: cargo test\n";
        let dir = repo(&[(".github/workflows/ci.yml", ci)]);
        let mut input: &[u8] = b"";
        let mut out = Vec::new();
        run(dir.path(), &mut prompt(&mut input, &mut out)).unwrap();
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains(setup::NOTHING_LOCAL), "{said}");
        assert!(!said.contains("2. GitHub App"), "{said}");
        assert!(!dir.path().join("shipcheck.toml").exists());
    }

    #[test]
    fn job_checks_reads_the_workflows() {
        let dir = repo(&[
            (".github/workflows/ci.yml", CI),
            (".github/workflows/lint.yml", LINT),
            ("shipcheck.toml", "workflows = [\"ci.yml\", \"lint.yml\"]\n"),
        ]);
        assert_eq!(
            job_checks(dir.path()).unwrap(),
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
    }
}
