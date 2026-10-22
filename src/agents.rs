use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

/// The marker version doctor compares, raised when the block's text changes
pub const BLOCK_VERSION: &str = "v1";
const START: &str = "<!-- shipcheck:start";
const END: &str = "<!-- shipcheck:end -->";

pub const BLOCK: &str = "<!-- shipcheck:start v1 -->
## Tests and commits

- Do not run the test commands (`pytest`, `cargo test`, `npm test`, `go test`, `just test` and the like). Run `shipcheck run`. It runs this repository's CI on your working copy, and its result is the PR's required check.
- Commit with `shipcheck commit -m \"<subject>\"`, never `git commit`. It commits every file, exactly the tree that passed.
- Work in small steps: one change, `shipcheck run`, `shipcheck commit`. Delete scratch files first, because every file is committed.
- Exit 1: read `shipcheck logs`, fix, run again. Exit 2: shipcheck could not run, so run `shipcheck doctor` and follow it.
- If `shipcheck` is not on PATH, run the project's tests and commit with git.
- Never edit `.github/workflows/` or `shipcheck.toml` to get a pass. Details: https://shipcheck.run/llms.txt
<!-- shipcheck:end -->";

const SKILL: &str = include_str!("../SKILL.md");
const SKILL_MARK: &str = "<!-- shipcheck-skill ";
const HOOK: &str = "shipcheck hook";
// Only the commands an agent needs run outside its sandbox, never clean, post, daemon or login
pub const CODEX_RULE: &str = "prefix_rule(pattern=[\"shipcheck\", \"run\"], decision=\"allow\")\n\
prefix_rule(pattern=[\"shipcheck\", \"commit\"], decision=\"allow\")\n\
prefix_rule(pattern=[\"shipcheck\", \"status\"], decision=\"allow\")\n\
prefix_rule(pattern=[\"shipcheck\", \"logs\"], decision=\"allow\")\n\
prefix_rule(pattern=[\"shipcheck\", \"doctor\"], decision=\"allow\")\n";

const SANDBOX_EXCLUDED: &[&str] = &[
    "shipcheck run",
    "shipcheck run *",
    "shipcheck commit *",
    "shipcheck status",
    "shipcheck status *",
    "shipcheck logs",
    "shipcheck logs *",
    "shipcheck doctor",
];

/// The agents shipcheck writes hooks for, with the binaries that show one is installed
pub const AGENTS: [(&str, &[&str]); 5] = [
    ("claude", &["claude"]),
    ("codex", &["codex"]),
    ("cursor", &["cursor-agent", "cursor"]),
    ("gemini", &["gemini"]),
    ("copilot", &["copilot"]),
];

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

pub fn imports_agents(text: &str) -> bool {
    text.lines().any(|l| l.trim() == "@AGENTS.md")
}

/**
 * Refuses a path in the clone that is a symlink, sits under one, or resolves outside the clone. A
 * cloned repository can ship `AGENTS.md` or `.claude` as a link to `~/.zshrc` or `~/.ssh`
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

    if !crate::hook::resolve(path).is_some_and(|p| p.starts_with(&real_root)) {
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

/// With `root`, a file of the clone, where a refusal is a note. Without, a file in the user's home
fn write_if_changed(
    root: Option<&Path>,
    path: &Path,
    text: &str,
    done: &mut Vec<String>,
) -> Result<(), String> {
    if fs::read_to_string(path).ok().as_deref() == Some(text) {
        return Ok(());
    }

    match root {
        Some(root) => {
            if let Err(e) = write_in_repo(root, path, text) {
                done.push(e);
                return Ok(());
            }
        }
        None => {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }

            fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }

    done.push(format!("wrote {}", path.display()));
    Ok(())
}

fn write_block(root: &Path, path: &Path, done: &mut Vec<String>) -> Result<(), String> {
    let text = fs::read_to_string(path).ok();
    write_if_changed(Some(root), path, &merge_block(text.as_deref()), done)
}

/**
 * For `init`: the agent instructions in the files each agent reads. AGENTS.md always. Claude Code
 * reads only CLAUDE.md when both exist, so a CLAUDE.md without `@AGENTS.md` gets the block too
 **/
pub fn instructions(root: &Path) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    write_block(root, &root.join("AGENTS.md"), &mut done)?;

    for name in ["CLAUDE.md", ".claude/CLAUDE.md"] {
        if let Ok(text) = fs::read_to_string(root.join(name))
            && !imports_agents(&text)
        {
            write_block(root, &root.join(name), &mut done)?;
        }
    }

    let gemini = root.join(".gemini/settings.json");

    if root.join("GEMINI.md").exists() {
        write_block(root, &root.join("GEMINI.md"), &mut done)?;
    } else if gemini.exists() {
        let mut doc = read_json(&gemini)?;
        add_context_file(&mut doc)?;
        write_if_changed(Some(root), &gemini, &pretty(&doc), &mut done)?;
    }

    let copilot = root.join(".github/copilot-instructions.md");

    if copilot.exists() {
        write_block(root, &copilot, &mut done)?;
    }

    let aider = root.join(".aider.conf.yml");

    if let Ok(text) = fs::read_to_string(&aider)
        && !text.contains("AGENTS.md")
    {
        if text.lines().any(|l| l.starts_with("read:")) {
            done.push(format!("add AGENTS.md to `read` in {}", aider.display()));
        } else {
            let sep = if text.is_empty() || text.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            write_if_changed(
                Some(root),
                &aider,
                &format!("{text}{sep}read: AGENTS.md\n"),
                &mut done,
            )?;
        }
    }

    Ok(done)
}

/// Gemini reads GEMINI.md by default, so AGENTS.md is added next to whatever it reads
pub fn add_context_file(doc: &mut Value) -> Result<(), String> {
    let context = object(doc)?.entry("context").or_insert_with(|| json!({}));
    let context = context.as_object_mut().ok_or("context is not an object")?;
    let mut names: Vec<Value> = match context.get("fileName") {
        Some(Value::String(s)) => vec![json!(s)],
        Some(Value::Array(a)) => a.clone(),
        _ => vec![json!("GEMINI.md")],
    };

    if !names.contains(&json!("AGENTS.md")) {
        names.push(json!("AGENTS.md"));
    }

    context.insert("fileName".into(), Value::Array(names));
    Ok(())
}

fn object(doc: &mut Value) -> Result<&mut serde_json::Map<String, Value>, String> {
    doc.as_object_mut().ok_or("not a JSON object".to_string())
}

/// A missing file is an empty object. A file that does not parse is an error, never overwritten
fn read_json(path: &Path) -> Result<Value, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| format!("{} is not valid JSON, left alone: {e}", path.display())),
        Err(_) => Ok(json!({})),
    }
}

fn pretty(doc: &Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(doc).unwrap_or_default()
    )
}

fn ours(hook: &Value) -> bool {
    ["command", "bash"].iter().any(|k| {
        hook.get(k)
            .and_then(Value::as_str)
            .is_some_and(|c| c.starts_with(HOOK))
    })
}

/// Puts `entry` in `doc.hooks[event]`, after removing every earlier `shipcheck hook` entry
pub fn set_hook(doc: &mut Value, event: &str, entry: Value) -> Result<(), String> {
    let hooks = object(doc)?.entry("hooks").or_insert_with(|| json!({}));
    let list = hooks
        .as_object_mut()
        .ok_or("hooks is not an object")?
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or(format!("hooks.{event} is not a list"))?;

    list.retain_mut(|e| match e.get_mut("hooks").and_then(Value::as_array_mut) {
        Some(inner) => {
            let before = inner.len();
            inner.retain(|h| !ours(h));
            before == 0 || !inner.is_empty()
        }
        None => !ours(e),
    });
    list.push(entry);
    Ok(())
}

fn command(agent: &str, event: &str) -> String {
    format!("{HOOK} {agent} {event}")
}

/// The Claude, Codex and Gemini shape: a matcher group holding command hooks
fn group(matcher: Option<&str>, agent: &str, event: &str) -> Value {
    let mut entry = json!({"hooks": [{"type": "command", "command": command(agent, event)}]});

    if let Some(m) = matcher {
        entry["matcher"] = json!(m);
    }

    entry
}

/// Adds `items` to the list at `doc[key][field]`, once each
fn add_all(doc: &mut Value, key: &str, field: &str, items: &[String]) -> Result<(), String> {
    let section = object(doc)?.entry(key).or_insert_with(|| json!({}));
    let list = section
        .as_object_mut()
        .ok_or(format!("{key} is not an object"))?
        .entry(field)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or(format!("{key}.{field} is not a list"))?;

    for item in items {
        if !list.contains(&json!(item)) {
            list.push(json!(item));
        }
    }

    Ok(())
}

/// The hook config file of `agent`, relative to the clone, with shipcheck's entries merged in
pub fn hook_config(agent: &str, doc: &mut Value, home: &Path) -> Result<&'static str, String> {
    Ok(match agent {
        "claude" => {
            let edits = "Bash|Edit|Write|MultiEdit|NotebookEdit";
            set_hook(doc, "PreToolUse", group(Some(edits), agent, "pre-tool"))?;
            set_hook(doc, "Stop", group(None, agent, "stop"))?;
            // `//` makes a rule absolute, a space in the path is a literal in gitignore syntax
            let home = home.to_string_lossy();
            let home = home.trim_end_matches('/');
            let deny = [
                format!("Edit(/{home}/**)"),
                format!("Read(/{home}/token)"),
                format!("Read(/{home}/app.pem)"),
                format!("Read(/{home}/secrets/**)"),
            ];
            add_all(doc, "permissions", "deny", &deny)?;
            let excluded: Vec<String> = SANDBOX_EXCLUDED.iter().map(|c| c.to_string()).collect();
            add_all(doc, "sandbox", "excludedCommands", &excluded)?;

            // Earlier installs excluded every shipcheck command
            if let Some(list) = doc["sandbox"]["excludedCommands"].as_array_mut() {
                list.retain(|c| c != "shipcheck *");
            }
            ".claude/settings.local.json"
        }
        "codex" => {
            set_hook(doc, "PreToolUse", group(Some("Bash"), agent, "pre-tool"))?;
            set_hook(doc, "Stop", group(None, agent, "stop"))?;
            ".codex/hooks.json"
        }
        "cursor" => {
            object(doc)?.entry("version").or_insert(json!(1));
            set_hook(
                doc,
                "beforeShellExecution",
                json!({"command": command(agent, "pre-tool")}),
            )?;
            set_hook(
                doc,
                "preToolUse",
                json!({"command": command(agent, "pre-tool"), "matcher": "Write|Delete"}),
            )?;
            set_hook(doc, "stop", json!({"command": command(agent, "stop")}))?;
            ".cursor/hooks.json"
        }
        "gemini" => {
            let tools = "run_shell_command|write_file|replace";
            set_hook(doc, "BeforeTool", group(Some(tools), agent, "pre-tool"))?;
            set_hook(doc, "AfterAgent", group(None, agent, "stop"))?;
            ".gemini/settings.json"
        }
        "copilot" => {
            object(doc)?.entry("version").or_insert(json!(1));
            let entry = |event| json!({"type": "command", "bash": command(agent, event)});
            set_hook(doc, "preToolUse", entry("pre-tool"))?;
            set_hook(doc, "agentStop", entry("stop"))?;
            ".github/hooks/shipcheck.json"
        }
        other => return Err(format!("no hooks for {other}")),
    })
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// The agents with a binary on PATH or a config directory in the user's home or the clone
pub fn detected(root: &Path, home: &Path) -> Vec<&'static str> {
    AGENTS
        .iter()
        .filter(|(name, bins)| {
            let dir = format!(".{name}");
            bins.iter().any(|b| on_path(b)) || home.join(&dir).is_dir() || root.join(&dir).is_dir()
        })
        .map(|(name, _)| *name)
        .collect()
}

fn git_ok(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Hides a file shipcheck wrote from git, so it is never part of a run or a commit
fn exclude(root: &Path, rel: &str) -> Result<(), String> {
    if git_ok(root, &["check-ignore", "-q", rel]) {
        return Ok(());
    }

    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ])
        .output()
        .map_err(|e| e.to_string())?;
    let file = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let text = fs::read_to_string(&file).unwrap_or_default();
    let line = format!("/{rel}");

    if text.lines().any(|l| l == line) {
        return Ok(());
    }

    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }

    let sep = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    write_nofollow(&file, &format!("{text}{sep}{line}\n"))
}

/// Writes one file of this clone that shipcheck owns. A tracked file is the team's, left alone
fn write_local(root: &Path, rel: &str, text: &str, done: &mut Vec<String>) -> Result<(), String> {
    if git_ok(root, &["ls-files", "--error-unmatch", "--", rel]) {
        done.push(format!(
            "{rel} is tracked, so shipcheck leaves it alone. Add the `shipcheck hook` entries to it by hand (https://shipcheck.run/docs/agents/)"
        ));
        return Ok(());
    }

    write_if_changed(Some(root), &root.join(rel), text, done)?;
    exclude(root, rel)
}

/// The skill as installed, with the version line doctor compares, after the frontmatter
pub fn skill_text() -> String {
    let line = format!("{SKILL_MARK}{} -->\n", env!("CARGO_PKG_VERSION"));

    match SKILL
        .strip_prefix("---\n")
        .and_then(|rest| rest.find("\n---\n"))
    {
        Some(at) => {
            let end = at + "---\n".len() + "\n---\n".len();
            format!("{}{line}{}", &SKILL[..end], &SKILL[end..])
        }
        None => format!("{line}{SKILL}"),
    }
}

/// Where each agent's user-level skill goes, for the agents whose config directory exists
pub fn skill_paths(home: &Path) -> Vec<(&'static str, PathBuf)> {
    let mut paths = Vec::new();

    if home.join(".claude").is_dir() {
        paths.push(("claude", home.join(".claude/skills/shipcheck/SKILL.md")));
    }

    if home.join(".agents").is_dir() || home.join(".codex").is_dir() {
        paths.push(("codex", home.join(".agents/skills/shipcheck/SKILL.md")));
    }

    paths
}

/**
 * For `install`: the hook configs of every detected agent in this clone, and the skill in the
 * user's home. Never fails: a file it cannot merge becomes a line to act on
 **/
pub fn install(root: &Path, home: &Path, shipcheck_home: &Path) -> Vec<String> {
    install_for(root, home, shipcheck_home, &detected(root, home))
}

pub fn install_for(
    root: &Path,
    home: &Path,
    shipcheck_home: &Path,
    agents: &[&str],
) -> Vec<String> {
    let mut done = Vec::new();

    for agent in agents {
        let result = (|| {
            let mut doc = json!({});
            let rel = hook_config(agent, &mut doc, shipcheck_home)?;
            let mut doc = read_json(&root.join(rel))?;
            hook_config(agent, &mut doc, shipcheck_home)?;
            write_local(root, rel, &pretty(&doc), &mut done)?;

            if *agent == "codex" {
                write_local(root, ".codex/rules/shipcheck.rules", CODEX_RULE, &mut done)?;
            }

            Ok::<_, String>(())
        })();

        if let Err(e) = result {
            done.push(format!("{agent}: {e}"));
        }
    }

    for (_, path) in skill_paths(home) {
        if let Err(e) = write_if_changed(None, &path, &skill_text(), &mut done) {
            done.push(e);
        }
    }

    done
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Claude Code's sandbox, from the user's and the clone's settings, the last one set wins
fn claude_sandbox(root: &Path, home: &Path) -> bool {
    let mut on = false;

    for file in [
        home.join(".claude/settings.json"),
        root.join(".claude/settings.json"),
        root.join(".claude/settings.local.json"),
    ] {
        if let Some(v) = serde_json::from_str::<Value>(&read(&file))
            .ok()
            .and_then(|d| d.pointer("/sandbox/enabled").and_then(Value::as_bool))
        {
            on = v;
        }
    }

    on
}

/// For doctor: per detected agent, what is missing for it to use shipcheck, and what protects the
/// results from it. Notes, never failures
pub fn notes(root: &Path, home: &Path) -> Vec<(&'static str, String)> {
    let agents_md = read(&root.join("AGENTS.md"));
    let marker = format!("{START} {BLOCK_VERSION} -->");
    let block = if agents_md.contains(&marker) {
        None
    } else if agents_md.contains(START) {
        Some("AGENTS.md has an older shipcheck block: `shipcheck init`".to_string())
    } else {
        Some("AGENTS.md has no shipcheck block: `shipcheck init`".to_string())
    };
    let skills = skill_paths(home);
    let version = format!("{SKILL_MARK}{} -->", env!("CARGO_PKG_VERSION"));

    detected(root, home)
        .into_iter()
        .map(|agent| {
            let mut missing: Vec<String> = block.iter().cloned().collect();

            if agent == "claude" {
                for name in ["CLAUDE.md", ".claude/CLAUDE.md"] {
                    let text = read(&root.join(name));

                    if !text.is_empty() && !imports_agents(&text) && !text.contains(&marker) {
                        missing.push(format!(
                            "{name} has neither `@AGENTS.md` nor the block: `shipcheck init`"
                        ));
                    }
                }
            }

            let mut doc = json!({});

            if let Ok(rel) = hook_config(agent, &mut doc, home)
                && !read(&root.join(rel)).contains(HOOK)
            {
                missing.push(format!("no hooks in {rel}: `shipcheck install`"));
            }

            if agent == "codex" && !root.join(".codex/rules/shipcheck.rules").exists() {
                missing.push("no .codex/rules/shipcheck.rules: `shipcheck install`".into());
            }

            if let Some((_, path)) = skills.iter().find(|(a, _)| *a == agent)
                && !read(path).contains(&version)
            {
                missing.push(format!(
                    "{} is missing or old: `shipcheck install`",
                    path.display()
                ));
            }

            let guard = match agent {
                "codex" => {
                    "sandboxed by default, the results are outside its writable roots".to_string()
                }
                "claude" if claude_sandbox(root, home) => {
                    "sandbox on, the results are outside its writable paths".to_string()
                }
                "claude" => "sandbox off, results protected only by the hook check".to_string(),
                "cursor" => "results protected only by the hook check, file edits through preToolUse, whose path fields Cursor does not document".to_string(),
                _ => "results protected only by the hook check".to_string(),
            };

            let detail = if missing.is_empty() {
                format!("instructions, hooks and skill installed. {guard}")
            } else {
                format!("{}. {guard}", missing.join(", "))
            };
            (agent, detail)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn instructions_skip_a_claude_md_that_imports_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("CLAUDE.md"), "@AGENTS.md\n").unwrap();
        fs::create_dir_all(root.join(".claude")).unwrap();
        fs::write(root.join(".claude/CLAUDE.md"), "# Mine\n").unwrap();
        fs::create_dir_all(root.join(".gemini")).unwrap();
        fs::write(
            root.join(".gemini/settings.json"),
            r#"{"context":{"fileName":"NOTES.md"}}"#,
        )
        .unwrap();
        fs::write(root.join(".aider.conf.yml"), "model: x").unwrap();

        instructions(root).unwrap();

        assert!(read(&root.join("AGENTS.md")).starts_with(START));
        assert_eq!(read(&root.join("CLAUDE.md")), "@AGENTS.md\n");
        assert!(
            read(&root.join(".claude/CLAUDE.md")).starts_with("# Mine\n\n<!-- shipcheck:start v1")
        );
        let gemini: Value =
            serde_json::from_str(&read(&root.join(".gemini/settings.json"))).unwrap();
        assert_eq!(
            gemini["context"]["fileName"],
            json!(["NOTES.md", "AGENTS.md"])
        );
        assert_eq!(
            read(&root.join(".aider.conf.yml")),
            "model: x\nread: AGENTS.md\n"
        );
        assert!(!root.join("GEMINI.md").exists());

        // Idempotent: a second run writes nothing
        assert!(instructions(root).unwrap().is_empty());
    }

    #[test]
    fn hook_merge_keeps_other_hooks_and_replaces_its_own() {
        let home = Path::new("/Users/a b/Library/Application Support/shipcheck");
        let mut doc = json!({
            "model": "x",
            "hooks": {"PreToolUse": [
                {"matcher": "Bash", "hooks": [
                    {"type": "command", "command": "other"},
                    {"type": "command", "command": "shipcheck hook claude old"}
                ]},
                {"matcher": "Edit", "hooks": [{"type": "command", "command": "shipcheck hook claude x"}]}
            ]}
        });
        hook_config("claude", &mut doc, home).unwrap();
        let once = doc.clone();
        hook_config("claude", &mut doc, home).unwrap();
        assert_eq!(doc, once);

        assert_eq!(doc["model"], "x");
        let pre = doc["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(
            pre[0]["hooks"],
            json!([{"type": "command", "command": "other"}])
        );
        assert_eq!(
            pre[1]["hooks"][0]["command"],
            "shipcheck hook claude pre-tool"
        );
        assert_eq!(
            doc["hooks"]["Stop"][0]["hooks"][0]["command"],
            "shipcheck hook claude stop"
        );
        assert_eq!(
            doc["permissions"]["deny"][0],
            "Edit(//Users/a b/Library/Application Support/shipcheck/**)"
        );
        assert_eq!(doc["sandbox"]["excludedCommands"], json!(SANDBOX_EXCLUDED));

        for agent in ["codex", "cursor", "gemini", "copilot"] {
            let mut doc = json!({});
            hook_config(agent, &mut doc, home).unwrap();
            let once = doc.clone();
            hook_config(agent, &mut doc, home).unwrap();
            assert_eq!(doc, once, "{agent}");
        }

        let mut cursor = json!({});
        hook_config("cursor", &mut cursor, home).unwrap();
        assert_eq!(
            cursor,
            json!({"version": 1, "hooks": {
                "beforeShellExecution": [{"command": "shipcheck hook cursor pre-tool"}],
                "preToolUse": [{"command": "shipcheck hook cursor pre-tool", "matcher": "Write|Delete"}],
                "stop": [{"command": "shipcheck hook cursor stop"}]
            }})
        );
    }

    #[test]
    fn install_writes_excluded_files_and_leaves_tracked_ones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let home = dir.path().join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(root.join(".cursor")).unwrap();
        let git = |args: &[&str]| assert!(git_ok(&root, args), "git {args:?}");
        git(&["init", "-q"]);
        // The user's global gitignore may already hide these files
        git(&["config", "core.excludesFile", "/dev/null"]);
        fs::write(root.join(".cursor/hooks.json"), "{}\n").unwrap();
        git(&["add", ".cursor/hooks.json"]);

        let done = install_for(
            &root,
            &home,
            &home.join("state"),
            &["claude", "codex", "cursor"],
        );

        assert!(
            read(&root.join(".claude/settings.local.json")).contains("shipcheck hook claude stop")
        );
        assert_eq!(read(&root.join(".codex/rules/shipcheck.rules")), CODEX_RULE);
        assert_eq!(read(&root.join(".cursor/hooks.json")), "{}\n");
        assert!(
            done.iter()
                .any(|l| l.contains(".cursor/hooks.json is tracked"))
        );
        let exclude = read(&root.join(".git/info/exclude"));
        assert!(exclude.contains("/.claude/settings.local.json\n"));
        assert!(exclude.contains("/.codex/hooks.json\n"));
        assert!(read(&home.join(".claude/skills/shipcheck/SKILL.md")).contains(SKILL_MARK));
        assert!(!home.join(".agents").exists());

        let again = install_for(
            &root,
            &home,
            &home.join("state"),
            &["claude", "codex", "cursor"],
        );
        assert_eq!(again.len(), 1, "{again:?}");
        assert_eq!(read(&root.join(".git/info/exclude")), exclude);
    }

    #[test]
    fn skill_keeps_its_frontmatter_first() {
        let text = skill_text();
        assert!(text.starts_with("---\nname: shipcheck\n"));
        let after = text.split("\n---\n").nth(1).unwrap();
        assert!(after.starts_with(SKILL_MARK));
    }

    #[test]
    fn never_writes_through_a_symlink_the_clone_ships() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("zshrc"), "mine\n").unwrap();
        std::os::unix::fs::symlink(outside.join("zshrc"), root.join("AGENTS.md")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(".claude")).unwrap();
        fs::write(outside.join("CLAUDE.md"), "# Theirs\n").unwrap();
        std::os::unix::fs::symlink(outside.join("nothing"), root.join("GEMINI.md")).unwrap();
        assert!(git_ok(&root, &["init", "-q"]));

        let done = instructions(&root).unwrap();
        let home = dir.path().join("home");
        let installed = install_for(&root, &home, &home.join("state"), &["claude"]);

        assert_eq!(read(&outside.join("zshrc")), "mine\n");
        assert_eq!(read(&outside.join("CLAUDE.md")), "# Theirs\n");
        assert!(!outside.join("nothing").exists());
        assert!(!outside.join("settings.local.json").exists());
        assert!(
            done.iter().filter(|l| l.contains("is a symlink")).count() == 2,
            "{done:?}"
        );
        assert!(
            installed.iter().any(|l| l.contains("is a symlink")),
            "{installed:?}"
        );
        assert!(check_in_repo(&root, &root.join("../outside/x")).is_err());
        assert!(write_nofollow(&root.join("AGENTS.md"), "x").is_err());
    }
}
