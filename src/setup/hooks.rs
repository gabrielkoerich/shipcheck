use super::git;
use crate::{base, daemon::autostart, paths};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub(super) const MARK: &str = "# Installed by `shipcheck install`";

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
pub(super) enum Entry {
    Registered,
    Missing,
}

/// Whether the hooks shipcheck wrote match the ones this version writes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hooks {
    Current,
    Stale,
}

pub(super) fn gap_line(name: &str, entry: Entry, hooks: Hooks) -> Option<String> {
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
pub(super) fn stale_hooks(root: &Path) -> bool {
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

pub(super) fn write_hook(hook: &Path, script: &str, line: &str) -> Result<String, String> {
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
pub(super) fn make_executable(path: &Path) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
pub(super) fn make_executable(_: &Path) -> Result<(), String> {
    Ok(())
}
