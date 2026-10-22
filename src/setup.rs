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

/**
 * For the repository owner, once: writes shipcheck.toml, to be committed. Host checks when the
 * root has a project file it knows, else act with every workflow. Returns what it wrote and why
 **/
pub fn init(root: &Path) -> Result<Option<String>, String> {
    let config = root.join("shipcheck.toml");

    if config.exists() {
        return Ok(None);
    }

    let (checks, versions) = detect(root);

    if !checks.is_empty() {
        let names: Vec<&str> = checks.iter().map(|(n, _)| n.as_str()).collect();
        let line = |(k, v): &(String, String)| format!("{k} = {}", toml::Value::String(v.clone()));
        let text = format!(
            "# Checks run natively on this machine in an OS sandbox, each in its own clone of the tree\n\
             # (https://shipcheck.run/docs/config/#modes). Detected from this repository's files: keep\n\
             # the ones CI needs. `runner = \"act\"` with `workflows` runs the GitHub workflows instead\n\
             runner = \"host\"\n\n[host]\nversions = [{}]\n\n[host.checks]\n{}\n",
            versions
                .iter()
                .map(|v| toml::Value::String(v.clone()).to_string())
                .collect::<Vec<_>>()
                .join(", "),
            checks.iter().map(line).collect::<Vec<_>>().join("\n")
        );
        crate::agents::write_in_repo(root, &config, &text)?;
        return Ok(Some(format!(
            "wrote shipcheck.toml: host checks {}",
            names.join(", ")
        )));
    }

    let mut workflows: Vec<String> = fs::read_dir(root.join(".github/workflows"))
        .map_err(|_| "no .github/workflows and no package.json scripts, Cargo.toml, pyproject.toml or go.mod in this repository".to_string())?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".yml") || n.ends_with(".yaml"))
        .collect();
    workflows.sort();
    let mut steps = Vec::new();

    for file in &workflows {
        let source =
            fs::read_to_string(root.join(".github/workflows").join(file)).unwrap_or_default();
        steps.extend(crate::workflow::run_steps(&source));
    }

    let text = format!(
        "runner = \"act\"\n{}\n# Splits each one-line pytest step into parallel shards. A test that fails only after another test's leftovers can pass\nshard_tests = true\n{}",
        config_for(&workflows),
        host_suggestion(&steps)
    );
    crate::agents::write_in_repo(root, &config, &text)?;
    Ok(Some(
        "wrote shipcheck.toml with runner = \"act\": no package.json test or lint script, Cargo.toml, pyproject.toml or go.mod at the root, so the GitHub workflows run in containers".into(),
    ))
}

/**
 * Host checks and `versions` from the project files at the root: package.json scripts with the
 * package manager its lockfile names, Cargo.toml, pyproject.toml and go.mod. A name already taken
 * by another ecosystem gets the tool's name after it
 **/
fn detect(root: &Path) -> (Vec<(String, String)>, Vec<String>) {
    let has = |f: &str| root.join(f).exists();
    let mut checks: Vec<(String, String)> = Vec::new();
    let mut versions = Vec::new();
    let mut add = |name: &str, tool: &str, command: String| {
        let name = if checks.iter().any(|(n, _)| n == name) {
            format!("{name}-{tool}")
        } else {
            name.to_string()
        };
        checks.push((name, command));
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
            add(name, pm, run(name));
        }

        if pm != "bun" {
            versions.push("node --version".to_string());
        }

        versions.push(format!("{pm} --version"));
    }

    if has("Cargo.toml") {
        add("test", "cargo", "cargo test".into());
        add(
            "clippy",
            "cargo",
            "cargo clippy --all-targets -- -D warnings".into(),
        );
        versions.extend(["cargo --version".into(), "rustc --version".into()]);
    }

    if has("pyproject.toml") {
        if has("uv.lock") {
            add("test", "pytest", "uv run pytest".into());
            versions.push("uv --version".into());
        } else {
            add("test", "pytest", "pytest".into());
            versions.push("python3 --version".into());
        }
    }

    if has("go.mod") {
        add("test", "go", "go test ./...".into());
        add("vet", "go", "go vet ./...".into());
        versions.push("go version".into());
    }

    (checks, versions)
}

/// Commands that set a machine up rather than check the code, left out of the suggestion
const SETUP: &[&str] = &[
    "npm ci",
    "npm install",
    "yarn install",
    "pnpm install",
    "bun install",
    "pip install",
    "uv sync",
    "apt",
    "sudo",
    "curl",
    "wget",
    "brew",
    "rustup",
    "cargo install",
    "echo",
    "cd ",
];

/**
 * Host checks from the workflows' one-line run steps, commented out, since only the owner knows
 * which ones test the code and which need a runner's setup. Never applied by itself
 **/
fn host_suggestion(steps: &[(String, String)]) -> String {
    let mut seen = std::collections::BTreeSet::new();
    let mut per_job = std::collections::BTreeMap::new();
    let mut checks = Vec::new();

    for (job, run) in steps {
        // A newline or other control character in either would end the comment and write an active line
        let id_ok = job.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && job
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c));

        if !id_ok
            || run.chars().any(char::is_control)
            || SETUP.iter().any(|s| run.starts_with(s))
            || !seen.insert(run)
        {
            continue;
        }

        let n: &mut usize = per_job.entry(job).or_default();
        *n += 1;
        let name = if *n == 1 {
            job.clone()
        } else {
            format!("{job}-{n}")
        };
        checks.push(format!("# {name} = {}", toml::Value::String(run.clone())));
    }

    if checks.is_empty() {
        return String::new();
    }

    format!(
        "\n# Host mode runs checks natively on this machine in an OS sandbox, with no Docker (https://shipcheck.run/docs/config/#modes).\n\
         # These are the workflows' one-line run steps. To use it, keep the ones that check the code,\n\
         # uncomment them and the lines below, and remove `runner`, `workflows` and `shard_tests` above.\n\
         # runner = \"host\"\n\
         # [host]\n\
         # versions = [\"node --version\"]\n\
         # [host.checks]\n{}\n",
        checks.join("\n")
    )
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
    fn init_suggests_host_checks_commented_out() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
        let ci = "on: push\njobs:\n  test:\n    steps:\n      - run: npm ci\n      - run: npm test\n      - run: npm run \"lint\"\n      - run: |\n          a\n          b\n      - run: echo ${{ x }}\n";
        fs::write(dir.path().join(".github/workflows/ci.yml"), ci).unwrap();
        init(dir.path()).unwrap();
        let text = fs::read_to_string(dir.path().join("shipcheck.toml")).unwrap();
        assert!(
            text.contains("# test = \"npm test\"\n# test-2 = 'npm run \"lint\"'\n"),
            "{text}"
        );
        assert!(!text.contains("npm ci"));
        let cfg = RepoConfig::parse(&text).unwrap();
        assert_eq!(cfg.runner, crate::config::RunnerKind::Act);

        // Uncommented as the comment says, it is a valid host config
        let host: String = text
            .lines()
            .filter(|l| {
                !l.starts_with("runner")
                    && !l.starts_with("workflows")
                    && !l.starts_with("shard_tests")
            })
            .map(|l| {
                l.strip_prefix("# ")
                    .filter(|l| !l.contains("  ") && (l.contains(" = ") || l.starts_with('[')))
                    .unwrap_or(l)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let cfg = RepoConfig::parse(&host).unwrap();
        assert_eq!(cfg.host.checks["test"], "npm test");
        assert_eq!(cfg.validate(&BTreeSet::new()), Ok(()));
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
            assert!(said.contains("host checks test, lint"), "{said}");
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
        assert!(
            said.contains("runner = \"act\": no package.json test"),
            "{said}"
        );

        let dir = tempfile::tempdir().unwrap();
        assert!(
            init(dir.path())
                .unwrap_err()
                .contains("no .github/workflows")
        );
    }

    #[test]
    fn suggestions_cannot_break_out_of_their_comment() {
        let steps = [
            ("a\nrunner = \"host\"".to_string(), "true".to_string()),
            (
                "test".to_string(),
                "x\n[host]\nchecks = { t = \"true\" }".to_string(),
            ),
            ("test".to_string(), "x\r\u{7f}".to_string()),
            ("lint".to_string(), "cargo clippy".to_string()),
        ];
        let text = host_suggestion(&steps);
        assert!(
            text.lines().all(|l| l.is_empty() || l.starts_with('#')),
            "{text}"
        );
        assert!(text.contains("# lint = \"cargo clippy\"\n"));
        assert!(!text.contains("checks = {"));
        let cfg = RepoConfig::parse(&format!("workflows = [\"ci.yml\"]\n{text}")).unwrap();
        assert_eq!(cfg.runner, crate::config::RunnerKind::Act);
        assert!(cfg.host.checks.is_empty());
    }

    #[test]
    fn hook_is_marked_and_hands_off_to_shipcheck() {
        assert!(PRE_PUSH.starts_with("#!/bin/sh\n"));
        assert!(PRE_PUSH.contains(MARK));
        assert!(PRE_PUSH.contains(HOOK_LINE));
    }
}
