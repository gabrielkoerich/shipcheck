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

/// For the repository owner, once: writes shipcheck.toml, to be committed
pub fn init(root: &Path) -> Result<Option<String>, String> {
    let config = root.join("shipcheck.toml");

    if config.exists() {
        return Ok(None);
    }

    let mut workflows: Vec<String> = fs::read_dir(root.join(".github/workflows"))
        .map_err(|_| "no .github/workflows in this repository".to_string())?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".yml") || n.ends_with(".yaml"))
        .collect();
    workflows.sort();
    fs::write(&config, config_for(&workflows)).map_err(|e| e.to_string())?;
    Ok(Some("shipcheck.toml".into()))
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
    fn hook_is_marked_and_hands_off_to_shipcheck() {
        assert!(PRE_PUSH.starts_with("#!/bin/sh\n"));
        assert!(PRE_PUSH.contains(MARK));
        assert!(PRE_PUSH.contains(HOOK_LINE));
    }
}
