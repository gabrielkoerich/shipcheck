use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// The marker version doctor compares, raised when the block's text changes
pub const BLOCK_VERSION: &str = "v2";
const START: &str = "<!-- shipcheck:start";
const END: &str = "<!-- shipcheck:end -->";

pub const BLOCK: &str = "<!-- shipcheck:start v2 -->
## Tests and commits

- Do not run the test commands (`pytest`, `cargo test`, `npm test`, `go test`, `just test` and the like). Run `shipcheck run`. It runs this repository's CI on your working copy, and its result is the PR's required check.
- Commit with `shipcheck commit -m \"<subject>\"`, never `git commit`. It commits every file, exactly the tree that passed.
- Work in small steps: one change, `shipcheck run`, `shipcheck commit`. Delete scratch files first, because every file is committed.
- Exit 1: read `shipcheck logs`, fix, then `shipcheck run --job <id>` for the failed job as often as needed. A partial run is never the check, so finish with one full `shipcheck run`, then `shipcheck commit`. Exit 2: shipcheck could not run, so run `shipcheck doctor` and follow it.
- If `shipcheck` is not on PATH, run the project's tests and commit with git.
- Never edit `.github/workflows/` or `shipcheck.toml` to get a pass. Details: https://shipcheck.run/llms.txt
<!-- shipcheck:end -->";

/// `text` with the block in it: between the markers when they are there, else appended
pub fn merge_block(text: Option<&str>) -> String {
    let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
        return format!("{BLOCK}\n");
    };

    if let Some(start) = text.find(START)
        && let Some(len) = text[start..].find(END)
    {
        let end = start + len + END.len();
        return format!("{}{BLOCK}{}", &text[..start], &text[end..]);
    }

    format!("{}\n\n{BLOCK}\n", text.trim_end())
}

/**
 * The real path of `path`, resolving each symlink, the last one too and dangling ones, before
 * applying the `..` after it, as the kernel does. None when it cannot tell: a `..` below a part
 * that does not exist yet, or links nested too deep
 **/
fn resolve(path: &Path) -> Option<PathBuf> {
    resolve_in(path, &mut 0)
}

/// Links followed in one resolution, shared by every level, as the kernel's MAXSYMLINKS
const MAX_LINKS: usize = 40;

fn resolve_in(path: &Path, links: &mut usize) -> Option<PathBuf> {
    use std::path::Component;

    let mut real = PathBuf::from("/");
    let mut exists = true;

    for part in path.components() {
        match part {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir if exists => {
                real.pop();
            }
            Component::ParentDir => return None,
            Component::Normal(name) => {
                let next = real.join(name);

                match fs::symlink_metadata(&next) {
                    Ok(m) if m.file_type().is_symlink() => {
                        *links += 1;

                        if *links > MAX_LINKS {
                            return None;
                        }

                        let target = fs::read_link(&next).ok()?;
                        real = resolve_in(&real.join(target), links)?;
                        exists = real.exists();
                    }
                    Ok(_) => real = next,
                    Err(_) => {
                        real = next;
                        exists = false;
                    }
                }
            }
        }
    }

    Some(real)
}

/**
 * Refuses a path in the clone that is a symlink, sits under one, or resolves outside the clone. A
 * cloned repository can ship `AGENTS.md` or `.codex` as a link to `~/.zshrc` or `~/.ssh`
 **/
pub fn check_in_repo(root: &Path, path: &Path) -> Result<(), String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| format!("{} is outside the repository", path.display()))?;
    let mut at = root.to_path_buf();

    for part in rel.components() {
        at.push(part);

        match fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("{} is a symlink, left alone", at.display()));
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let real_root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;

    if !resolve(path).is_some_and(|p| p.starts_with(&real_root)) {
        return Err(format!("{} is outside the repository", path.display()));
    }

    Ok(())
}

/// Writes without following a symlink at the last step, which a check before cannot rule out
pub fn write_nofollow(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes a file of the clone, refusing symlinks and paths outside it
pub fn write_in_repo(root: &Path, path: &Path, text: &str) -> Result<(), String> {
    check_in_repo(root, path)?;

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }

    check_in_repo(root, path)?;
    write_nofollow(path, text)
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

pub fn imports_agents(text: &str) -> bool {
    text.lines().any(|l| l.trim() == "@AGENTS.md")
}

/// The instruction files besides AGENTS.md that get the block when they exist. A CLAUDE.md that
/// imports AGENTS.md has it already
const OTHER_INSTRUCTIONS: &[&str] = &[
    "CLAUDE.md",
    ".claude/CLAUDE.md",
    "GEMINI.md",
    ".github/copilot-instructions.md",
];

/// The instruction files that need the block: AGENTS.md always, the others when they exist
fn instruction_files(root: &Path) -> Vec<&'static str> {
    let mut files = vec!["AGENTS.md"];

    for name in OTHER_INSTRUCTIONS {
        if fs::symlink_metadata(root.join(name)).is_ok() && !imports_agents(&read(&root.join(name)))
        {
            files.push(name);
        }
    }

    files
}

/// For `init`: the block in each instruction file. A refusal to write through a symlink is a line
pub fn instructions(root: &Path) -> Result<Vec<String>, String> {
    let mut done = Vec::new();

    for name in instruction_files(root) {
        let path = root.join(name);
        let text = fs::read_to_string(&path).ok();
        let merged = merge_block(text.as_deref());

        if text.as_deref() == Some(merged.as_str()) {
            continue;
        }

        done.push(match write_in_repo(root, &path, &merged) {
            Ok(()) => format!("wrote {}", path.display()),
            Err(e) => e,
        });
    }

    Ok(done)
}

/// For doctor: each instruction file without this version's block
pub fn notes(root: &Path) -> Vec<String> {
    let marker = format!("{START} {BLOCK_VERSION} -->");

    instruction_files(root)
        .into_iter()
        .filter_map(|name| {
            let text = read(&root.join(name));

            if text.contains(&marker) {
                None
            } else if text.contains(START) {
                Some(format!(
                    "{name} has an older shipcheck block: `shipcheck init`"
                ))
            } else {
                Some(format!("{name} has no shipcheck block: `shipcheck init`"))
            }
        })
        .collect()
}

/// The files older versions wrote agent hooks into, relative to the clone
const OLD_HOOK_FILES: &[&str] = &[
    ".claude/settings.local.json",
    ".codex/hooks.json",
    ".cursor/hooks.json",
    ".gemini/settings.json",
    ".github/hooks/shipcheck.json",
];
const OLD_CODEX_RULES: &str = ".codex/rules/shipcheck.rules";
const OLD_EXCLUDED: &[&str] = &[
    "shipcheck *",
    "shipcheck run",
    "shipcheck run *",
    "shipcheck commit *",
    "shipcheck status",
    "shipcheck status *",
    "shipcheck logs",
    "shipcheck logs *",
    "shipcheck doctor",
];

fn git_ok(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn ours(hook: &Value) -> bool {
    ["command", "bash"].iter().any(|k| {
        hook.get(k)
            .and_then(Value::as_str)
            .is_some_and(|c| c.starts_with("shipcheck hook "))
    })
}

/// Drops `items` from the list at `doc[key][field]`
fn drop_items(doc: &mut Value, key: &str, field: &str, items: &[String]) {
    if let Some(list) = doc
        .get_mut(key)
        .and_then(|s| s.get_mut(field))
        .and_then(Value::as_array_mut)
    {
        list.retain(|v| !v.as_str().is_some_and(|s| items.iter().any(|i| i == s)));
    }
}

/// Drops the empty lists and objects at `path` in `doc`, deepest first
fn prune(doc: &mut Value, path: &[&str]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut at = &mut *doc;

    for key in parents {
        match at.get_mut(*key) {
            Some(next) => at = next,
            None => return,
        }
    }

    if let Some(map) = at.as_object_mut()
        && map.get(*last).is_some_and(|v| match v {
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            _ => false,
        })
    {
        map.remove(*last);
    }
}

/// Removes the entries older versions wrote, never the user's. Whether anything changed
fn strip(doc: &mut Value, homes: &[PathBuf]) -> bool {
    let before = doc.clone();

    if let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) {
        for list in hooks.values_mut().filter_map(Value::as_array_mut) {
            list.retain_mut(|e| match e.get_mut("hooks").and_then(Value::as_array_mut) {
                Some(inner) => {
                    let empty = inner.is_empty();
                    inner.retain(|h| !ours(h));
                    empty || !inner.is_empty()
                }
                None => !ours(e),
            });
        }
    }

    let deny: Vec<String> = homes
        .iter()
        .flat_map(|h| {
            let h = h.to_string_lossy().trim_end_matches('/').to_string();
            [
                format!("Edit(/{h}/**)"),
                format!("Read(/{h}/token)"),
                format!("Read(/{h}/app.pem)"),
                format!("Read(/{h}/secrets/**)"),
            ]
        })
        .collect();
    drop_items(doc, "permissions", "deny", &deny);
    let excluded: Vec<String> = OLD_EXCLUDED.iter().map(|c| c.to_string()).collect();
    drop_items(doc, "sandbox", "excludedCommands", &excluded);

    if *doc == before {
        return false;
    }

    let events: Vec<String> = doc
        .get("hooks")
        .and_then(Value::as_object)
        .map(|h| h.keys().cloned().collect())
        .unwrap_or_default();

    for event in &events {
        prune(doc, &["hooks", event]);
    }

    for path in [
        &["hooks"][..],
        &["permissions", "deny"],
        &["permissions"],
        &["sandbox", "excludedCommands"],
        &["sandbox"],
    ] {
        prune(doc, path);
    }

    true
}

/// Drops the `/<rel>` line older versions added to `.git/info/exclude`
fn unexclude(root: &Path, rel: &str) {
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ])
        .output()
    else {
        return;
    };
    let file = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let text = read(&file);
    let line = format!("/{rel}");

    if text.lines().any(|l| l == line) {
        let kept: String = text
            .lines()
            .filter(|l| *l != line)
            .map(|l| format!("{l}\n"))
            .collect();
        let _ = write_nofollow(&file, &kept);
    }
}

fn remove(root: &Path, rel: &str, done: &mut Vec<String>) {
    match fs::remove_file(root.join(rel)) {
        Ok(()) => {
            unexclude(root, rel);
            done.push(format!(
                "removed {rel}, shipcheck no longer writes agent hooks"
            ));
        }
        Err(e) => done.push(format!("{rel}: {e}")),
    }
}

/**
 * For `install`: removes the agent hooks older versions wrote into this clone. Only shipcheck's own
 * entries go, and a file goes only when nothing else is left. Never fails, a problem is a line
 **/
pub fn remove_old_hooks(root: &Path, homes: &[PathBuf]) -> Vec<String> {
    let mut done = Vec::new();

    for rel in OLD_HOOK_FILES {
        let path = root.join(rel);

        if fs::symlink_metadata(&path).is_err() {
            continue;
        }

        if let Err(e) = check_in_repo(root, &path) {
            done.push(e);
            continue;
        }

        let Ok(mut doc) = serde_json::from_str::<Value>(&read(&path)) else {
            continue;
        };

        if !strip(&mut doc, homes) {
            continue;
        }

        // Older versions never wrote a tracked file, so these entries are the team's
        if git_ok(root, &["ls-files", "--error-unmatch", "--", rel]) {
            done.push(format!(
                "{rel} is tracked and has `shipcheck hook` entries, which this version no longer has: remove them by hand"
            ));
            continue;
        }

        if doc
            .as_object()
            .is_some_and(|m| m.keys().all(|k| k == "version"))
        {
            remove(root, rel, &mut done);
            continue;
        }

        let text = format!(
            "{}\n",
            serde_json::to_string_pretty(&doc).unwrap_or_default()
        );

        match write_nofollow(&path, &text) {
            Ok(()) => done.push(format!("removed shipcheck's hooks from {rel}")),
            Err(e) => done.push(e),
        }
    }

    let rules = root.join(OLD_CODEX_RULES);

    if check_in_repo(root, &rules).is_ok()
        && let Ok(text) = fs::read_to_string(&rules)
        && text
            .lines()
            .all(|l| l.trim().is_empty() || l.contains("pattern=[\"shipcheck\","))
        && !git_ok(
            root,
            &["ls-files", "--error-unmatch", "--", OLD_CODEX_RULES],
        )
    {
        remove(root, OLD_CODEX_RULES, &mut done);
    }

    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn block_goes_in_a_new_file_between_markers_and_after_other_text() {
        assert_eq!(merge_block(None), format!("{BLOCK}\n"));

        let old = "# Notes\n\n<!-- shipcheck:start v0 -->\nold\n<!-- shipcheck:end -->\n\nmore\n";
        assert_eq!(
            merge_block(Some(old)),
            format!("# Notes\n\n{BLOCK}\n\nmore\n")
        );

        let plain = "# Notes\nkeep this\n";
        let merged = merge_block(Some(plain));
        assert_eq!(merged, format!("# Notes\nkeep this\n\n{BLOCK}\n"));
        assert_eq!(merge_block(Some(&merged)), merged);
    }

    #[test]
    fn instructions_go_in_every_instruction_file_and_skip_an_import() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("CLAUDE.md"), "@AGENTS.md\n").unwrap();
        fs::create_dir_all(root.join(".claude")).unwrap();
        fs::write(root.join(".claude/CLAUDE.md"), "# Mine\n").unwrap();
        fs::write(root.join("GEMINI.md"), "").unwrap();
        fs::create_dir_all(root.join(".gemini")).unwrap();
        fs::write(root.join(".gemini/settings.json"), "{}").unwrap();

        assert_eq!(notes(root).len(), 3);
        assert_eq!(instructions(root).unwrap().len(), 3);
        assert!(read(&root.join("AGENTS.md")).starts_with(START));
        assert!(read(&root.join("GEMINI.md")).starts_with(START));
        assert_eq!(read(&root.join("CLAUDE.md")), "@AGENTS.md\n");
        assert!(
            read(&root.join(".claude/CLAUDE.md")).starts_with("# Mine\n\n<!-- shipcheck:start v2")
        );
        assert_eq!(read(&root.join(".gemini/settings.json")), "{}");
        assert!(!root.join(".github").exists());
        assert!(notes(root).is_empty());
        assert!(instructions(root).unwrap().is_empty());
    }

    #[test]
    fn never_writes_through_a_symlink_the_clone_ships() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("zshrc"), "mine\n").unwrap();
        fs::write(
            outside.join("hooks.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"command":"shipcheck hook codex stop"}]}]}}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.join("zshrc"), root.join("AGENTS.md")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".codex")).unwrap();
        assert!(git_ok(&root, &["init", "-q"]));

        let done = instructions(&root).unwrap();
        let removed = remove_old_hooks(&root, &[]);

        assert_eq!(read(&outside.join("zshrc")), "mine\n");
        assert!(read(&outside.join("hooks.json")).contains("shipcheck hook"));
        assert!(done.iter().any(|l| l.contains("is a symlink")), "{done:?}");
        assert!(removed[0].contains("is a symlink"), "{removed:?}");
        assert!(check_in_repo(&root, &root.join("../outside/x")).is_err());
        assert!(write_nofollow(&root.join("AGENTS.md"), "x").is_err());
    }

    #[test]
    fn resolve_follows_links_before_dots_and_stops_long_chains() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        fs::create_dir_all(base.join("a/b")).unwrap();
        std::os::unix::fs::symlink(base.join("a/b"), base.join("link")).unwrap();
        assert_eq!(resolve(&base.join("link/../x")), Some(base.join("a/x")));
        assert_eq!(resolve(&base.join("new/../x")), None);

        // Each link of a chain names the one before it 8 times, which cost 8^8 lookups with a
        // depth limit per level. One budget for the whole path ends it after 40 links
        fs::create_dir_all(base.join("chain")).unwrap();
        std::os::unix::fs::symlink(".", base.join("chain/l0")).unwrap();

        for i in 1..30 {
            let prev = vec![format!("l{}", i - 1); 8].join("/");
            std::os::unix::fs::symlink(prev, base.join(format!("chain/l{i}"))).unwrap();
        }

        let started = std::time::Instant::now();
        assert_eq!(resolve(&base.join("chain/l29/x")), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    /// The files older versions wrote, each with shipcheck's entries and, when `theirs`, one of the user's
    fn old_files(home: &str, theirs: bool) -> Vec<(&'static str, Value)> {
        let group = |agent: &str, event: &str, matcher: Option<&str>| {
            let mut g = json!({"hooks": [{"type": "command", "command": format!("shipcheck hook {agent} {event}")}]});

            if let Some(m) = matcher {
                g["matcher"] = json!(m);
            }

            g
        };
        let mut claude = json!({
            "hooks": {
                "PreToolUse": [group("claude", "pre-tool", Some("Bash|Edit|Write|MultiEdit|NotebookEdit"))],
                "Stop": [group("claude", "stop", None)],
            },
            "permissions": {"deny": [
                format!("Edit(/{home}/**)"),
                format!("Read(/{home}/token)"),
                format!("Read(/{home}/app.pem)"),
                format!("Read(/{home}/secrets/**)"),
            ]},
            "sandbox": {"excludedCommands": OLD_EXCLUDED[1..]},
        });
        let mut codex = json!({"hooks": {
            "PreToolUse": [group("codex", "pre-tool", Some("Bash"))],
            "Stop": [group("codex", "stop", None)],
        }});
        let mut cursor = json!({"version": 1, "hooks": {
            "beforeShellExecution": [{"command": "shipcheck hook cursor pre-tool"}],
            "preToolUse": [{"command": "shipcheck hook cursor pre-tool", "matcher": "Write|Delete"}],
            "stop": [{"command": "shipcheck hook cursor stop"}],
        }});
        let mut gemini = json!({"hooks": {
            "BeforeTool": [group("gemini", "pre-tool", Some("run_shell_command|write_file|replace"))],
            "AfterAgent": [group("gemini", "stop", None)],
        }});
        let mut copilot = json!({"version": 1, "hooks": {
            "preToolUse": [{"type": "command", "bash": "shipcheck hook copilot pre-tool"}],
            "agentStop": [{"type": "command", "bash": "shipcheck hook copilot stop"}],
        }});

        if theirs {
            claude["model"] = json!("x");
            claude["permissions"]["deny"]
                .as_array_mut()
                .unwrap()
                .push(json!("Read(./.env)"));
            claude["hooks"]["PreToolUse"][0]["hooks"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type": "command", "command": "mine"}));
            codex["hooks"]["Stop"]
                .as_array_mut()
                .unwrap()
                .push(json!({"hooks": [{"type": "command", "command": "mine"}]}));
            cursor["hooks"]["stop"]
                .as_array_mut()
                .unwrap()
                .push(json!({"command": "mine"}));
            gemini["context"] = json!({"fileName": ["GEMINI.md", "AGENTS.md"]});
            copilot["hooks"]["agentStop"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type": "command", "bash": "mine"}));
        }

        vec![
            (".claude/settings.local.json", claude),
            (".codex/hooks.json", codex),
            (".cursor/hooks.json", cursor),
            (".gemini/settings.json", gemini),
            (".github/hooks/shipcheck.json", copilot),
        ]
    }

    fn repo(dir: &Path, home: &str, theirs: bool) -> PathBuf {
        let root = dir.join("repo");
        fs::create_dir_all(&root).unwrap();
        assert!(git_ok(&root, &["init", "-q"]));
        let mut exclude = String::new();

        for (rel, doc) in old_files(home, theirs) {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
            exclude.push_str(&format!("/{rel}\n"));
        }

        fs::create_dir_all(root.join(".codex/rules")).unwrap();
        fs::write(
            root.join(OLD_CODEX_RULES),
            "prefix_rule(pattern=[\"shipcheck\", \"run\"], decision=\"allow\")\n\
             prefix_rule(pattern=[\"shipcheck\", \"doctor\"], decision=\"allow\")\n",
        )
        .unwrap();
        exclude.push_str(&format!("/{OLD_CODEX_RULES}\n"));
        fs::write(root.join(".git/info/exclude"), format!("# mine\n{exclude}")).unwrap();
        root
    }

    #[test]
    fn removes_files_that_hold_only_old_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let home = "/Users/a b/Library/Application Support/shipcheck";
        let root = repo(dir.path(), home, false);

        let done = remove_old_hooks(&root, &[PathBuf::from(home)]);

        for (rel, _) in old_files(home, false) {
            assert!(!root.join(rel).exists(), "{rel}");
        }

        assert!(!root.join(OLD_CODEX_RULES).exists());
        assert_eq!(done.len(), 6, "{done:?}");
        assert_eq!(read(&root.join(".git/info/exclude")), "# mine\n");
        assert!(remove_old_hooks(&root, &[PathBuf::from(home)]).is_empty());
    }

    #[test]
    fn keeps_the_users_entries_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let home = "/h/shipcheck";
        let root = repo(dir.path(), home, true);
        fs::write(
            root.join(OLD_CODEX_RULES),
            "prefix_rule(pattern=[\"ls\"])\n",
        )
        .unwrap();

        remove_old_hooks(&root, &[PathBuf::from(home)]);
        let doc = |rel: &str| serde_json::from_str::<Value>(&read(&root.join(rel))).unwrap();

        let mine = json!([{"type": "command", "command": "mine"}]);
        assert_eq!(
            doc(".claude/settings.local.json"),
            json!({
                "model": "x",
                "hooks": {"PreToolUse": [{"matcher": "Bash|Edit|Write|MultiEdit|NotebookEdit", "hooks": mine}]},
                "permissions": {"deny": ["Read(./.env)"]},
            })
        );
        assert_eq!(
            doc(".codex/hooks.json"),
            json!({"hooks": {"Stop": [{"hooks": mine}]}})
        );
        assert_eq!(
            doc(".cursor/hooks.json"),
            json!({"version": 1, "hooks": {"stop": [{"command": "mine"}]}})
        );
        assert_eq!(
            doc(".gemini/settings.json"),
            json!({"context": {"fileName": ["GEMINI.md", "AGENTS.md"]}})
        );
        assert_eq!(
            doc(".github/hooks/shipcheck.json"),
            json!({"version": 1, "hooks": {"agentStop": [{"type": "command", "bash": "mine"}]}})
        );
        assert!(root.join(OLD_CODEX_RULES).exists());
        assert!(read(&root.join(".git/info/exclude")).contains("/.codex/hooks.json\n"));
    }

    #[test]
    fn leaves_a_tracked_file_and_one_that_does_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        let root = repo(dir.path(), "/h", false);
        fs::write(root.join(".gemini/settings.json"), "{ not json").unwrap();
        assert!(git_ok(&root, &["add", "-f", ".cursor/hooks.json"]));
        let cursor = read(&root.join(".cursor/hooks.json"));

        let done = remove_old_hooks(&root, &[]);

        assert_eq!(read(&root.join(".cursor/hooks.json")), cursor);
        assert_eq!(read(&root.join(".gemini/settings.json")), "{ not json");
        assert!(
            done.iter()
                .any(|l| l.contains(".cursor/hooks.json is tracked"))
        );
        // The home given was not the one in the deny rules, so those stay
        assert!(read(&root.join(".claude/settings.local.json")).contains("Edit(//h/**)"));
    }
}
