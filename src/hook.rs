use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::run;

pub const TESTS: &str = "Use `shipcheck run` instead: it runs this repository's CI on your working copy, and its result is the PR's required check";
pub const COMMIT: &str = "Use `shipcheck commit -m \"<subject>\"` instead: it commits exactly the tree that `shipcheck run` tested";
pub const FORGE: &str = "Only shipcheck's own runs write its results. Read them with `shipcheck status` and `shipcheck logs`";
pub const DIRTY: &str =
    "Uncommitted changes: `shipcheck run`, then `shipcheck commit -m \"<subject>\"`";
pub const NO_RESULT: &str = "HEAD has no shipcheck result: run `shipcheck run`";
pub const FAILED: &str =
    "HEAD failed in shipcheck: read `shipcheck logs`, fix it, `shipcheck run` again";

/// Test runners, matched word by word from the program on. Options between the words are skipped
const RUNNERS: &[&[&str]] = &[
    &["pytest"],
    &["py.test"],
    &["python", "-m", "pytest"],
    &["python3", "-m", "pytest"],
    &["uv", "run", "pytest"],
    &["uv", "run", "python", "-m", "pytest"],
    &["poetry", "run", "pytest"],
    &["pdm", "run", "pytest"],
    &["cargo", "test"],
    &["cargo", "nextest"],
    &["npm", "test"],
    &["npm", "run", "test"],
    &["pnpm", "test"],
    &["pnpm", "run", "test"],
    &["yarn", "test"],
    &["yarn", "run", "test"],
    &["bun", "test"],
    &["bun", "run", "test"],
    &["npx", "jest"],
    &["npx", "vitest"],
    &["go", "test"],
    &["just", "test"],
    &["make", "test"],
    &["tox"],
    &["nox"],
    &["jest"],
    &["vitest"],
    &["act"],
];

/// Words that only start the next program
const PREFIXES: &[&str] = &["env", "time", "command", "exec", "nohup"];

/// The commands of a shell line, split on `&&`, `||`, `;`, `|`, `&`, newlines and parentheses
/// outside quotes. `sh -c "..."`, `$(...)` and variables are not looked into
pub fn segments(line: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut quote = None;

    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
            }
            (Some(_), _) => {}
            (None, '\'' | '"') => {
                quote = Some(c);
            }
            (None, ';' | '|' | '&' | '\n' | '(' | ')') => {
                out.push(String::new());
                continue;
            }
            (None, _) => {}
        }

        if let Some(last) = out.last_mut() {
            last.push(c);
        }
    }

    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The words of one command, without the `VAR=value` and `env` in front, the program by basename
pub fn words(segment: &str) -> Vec<String> {
    let mut words: Vec<String> = segment
        .split_whitespace()
        .map(|w| w.replace(['"', '\''], ""))
        .skip_while(|w| is_assignment(w) || PREFIXES.contains(&w.as_str()))
        .collect();

    if let Some(first) = words.first_mut() {
        *first = first.rsplit('/').next().unwrap_or_default().to_string();
    }

    words
}

fn matches(words: &[String], pattern: &[&str]) -> bool {
    if words.first().map(String::as_str) != pattern.first().copied() {
        return false;
    }

    let mut want = pattern[1..].iter().peekable();

    for word in &words[1..] {
        let Some(next) = want.peek() else {
            break;
        };

        if word == *next || word.starts_with(&format!("{next}:")) {
            want.next();
        } else if !(word.starts_with('-') || word.starts_with('+')) {
            return false;
        }
    }

    want.peek().is_none()
}

fn is_git_commit(words: &[String]) -> bool {
    if words.first().map(String::as_str) != Some("git") {
        return false;
    }

    let mut rest = words[1..].iter();

    while let Some(word) = rest.next() {
        match word.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => {
                rest.next();
            }
            w if w.starts_with('-') => {}
            w => return w == "commit",
        }
    }

    false
}

/// Why a shell line is denied in a shipcheck repository: a test runner or a `git commit`
pub fn shell_reason(line: &str) -> Option<&'static str> {
    for segment in segments(line) {
        let words = words(&segment);

        if RUNNERS.iter().any(|p| matches(&words, p)) {
            return Some(TESTS);
        }

        if is_git_commit(&words) {
            return Some(COMMIT);
        }
    }

    None
}

/// Variables that move where shipcheck keeps its results
const MOVES_HOME: &[&str] = &["SHIPCHECK_HOME", "HOME", "XDG_STATE_HOME"];

/// Flags only a person passes
const HUMAN_FLAGS: &[&str] = &["--home-ok"];

fn moves_home(word: &str) -> bool {
    MOVES_HOME.iter().any(|name| {
        word.strip_prefix(name)
            .is_some_and(|rest| rest.starts_with('=') || rest.starts_with("+="))
    })
}

/**
 * Whether a shell line reaches shipcheck's home or moves it: the path as written, with its spaces
 * escaped or quoted, a variable that moves the home, the service's host, a variable, `env` or
 * `command` in front of `shipcheck`, or a flag only a person passes. It catches habits. A variable,
 * a script or base64 gets past it
 **/
pub fn reaches_home(line: &str, marks: &[String]) -> bool {
    let flat = line.replace(['\\', '"', '\''], "");

    let segments = segments(&flat);
    let flagged = segments
        .iter()
        .flat_map(|s| s.split_whitespace())
        .map(|w| w.trim_start_matches([';', '&', '|', '(', ')']))
        .any(|w| HUMAN_FLAGS.contains(&w) || moves_home(w));

    if flagged {
        return true;
    }

    segments.iter().any(|segment| {
        let raw: Vec<&str> = segment.split_whitespace().collect();
        let skipped = raw
            .iter()
            .take_while(|w| is_assignment(w) || PREFIXES.contains(w) || w.starts_with('-'))
            .count();

        if raw.get(skipped).and_then(|w| w.rsplit('/').next()) == Some("shipcheck") {
            // `--base` or `--no-fetch` would let the agent pick the config, network included, of a run
            // outside its sandbox: a run fetches main first, which overwrites a rewritten origin/main
            let picks_config = raw[skipped..]
                .iter()
                .any(|w| *w == "--base" || w.starts_with("--base=") || *w == "--no-fetch");
            return skipped > 0 || picks_config;
        }

        marks.iter().any(|m| segment.contains(m.as_str())) || segment.contains("api.shipcheck.run")
    })
}

/**
 * The real path of `path`, resolving each symlink, the last one too and dangling ones, before
 * applying the `..` after it, as the kernel does. None when it cannot tell: a `..` below a part
 * that does not exist yet, or links nested too deep
 **/
pub fn resolve(path: &Path) -> Option<PathBuf> {
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

                match std::fs::symlink_metadata(&next) {
                    Ok(m) if m.file_type().is_symlink() => {
                        *links += 1;

                        if *links > MAX_LINKS {
                            return None;
                        }

                        let target = std::fs::read_link(&next).ok()?;
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

/// Each spelling of each home: the one in use and the default, as written and with symlinks
/// resolved, absolute and from `~` or `$HOME`
pub fn home_marks(homes: &[PathBuf], user_homes: &[PathBuf]) -> Vec<String> {
    let spellings = |paths: &[PathBuf]| -> Vec<PathBuf> {
        paths
            .iter()
            .flat_map(|p| [Some(p.clone()), resolve(p)])
            .flatten()
            .collect()
    };
    let users = spellings(user_homes);
    let mut marks = Vec::new();

    for home in spellings(homes) {
        for user in &users {
            if let Ok(rel) = home.strip_prefix(user)
                && !rel.as_os_str().is_empty()
            {
                marks.push(rel.to_string_lossy().into_owned());
            }
        }

        marks.push(home.to_string_lossy().into_owned());
    }

    marks.sort();
    marks.dedup();
    marks
}

/// Whether a file path the agent names is inside a home, through `..` or a symlink too. A path it
/// cannot resolve counts as inside
pub fn in_home(file: &Path, homes: &[PathBuf]) -> bool {
    let Some(file) = resolve(file) else {
        return true;
    };

    homes
        .iter()
        .any(|h| file.starts_with(h) || resolve(h).is_some_and(|real| file.starts_with(real)))
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The hooks apply only where the owner merged shipcheck.toml, since only there is it the check
fn has_config(cwd: &Path) -> bool {
    git(cwd, &["cat-file", "-e", "origin/main:shipcheck.toml"]).is_some()
}

/// A merge, cherry-pick, revert or rebase in progress ends with `git commit`, which stays allowed
fn finishing(cwd: &Path) -> bool {
    [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
    ]
    .iter()
    .any(|name| {
        git(
            cwd,
            &["rev-parse", "--path-format=absolute", "--git-path", name],
        )
        .is_some_and(|p| Path::new(&p).exists())
    })
}

/// The tool's arguments: Claude, Codex and Gemini send `tool_input`, Copilot `toolArgs`, maybe as
/// a JSON string, Cursor the command itself
fn tool_args(input: &Value) -> Value {
    match input.get("tool_input").or_else(|| input.get("toolArgs")) {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
        Some(v) => v.clone(),
        None => input.clone(),
    }
}

/// Every string under a key named like a path, at any depth, in lists too, since Cursor does not
/// document its Write tool's fields
pub fn path_strings(value: &Value, under: bool, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                let key = key.to_ascii_lowercase();
                path_strings(
                    v,
                    under || key.contains("path") || key.contains("file"),
                    out,
                );
            }
        }
        Value::Array(list) => {
            for v in list {
                path_strings(v, under, out);
            }
        }
        Value::String(s) if under => out.push(s.clone()),
        _ => {}
    }
}

fn user_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn pre_tool(input: &Value, cwd: &Path) -> Option<&'static str> {
    if std::env::var("SHIPCHECK_ALLOW_TESTS").as_deref() == Ok("1") {
        return None;
    }

    let args = tool_args(input);
    let homes = [run::home(), run::default_home()];

    let mut paths = Vec::new();
    path_strings(&args, false, &mut paths);

    if paths.iter().any(|f| in_home(&cwd.join(f), &homes)) {
        return Some(FORGE);
    }

    let line = args.get("command").and_then(Value::as_str)?;
    let users = [user_home(), run::user_dir()];

    if reaches_home(line, &home_marks(&homes, &users)) {
        return Some(FORGE);
    }

    if !has_config(cwd) {
        return None;
    }

    match shell_reason(line)? {
        COMMIT if finishing(cwd) => None,
        reason => Some(reason),
    }
}

/// What the stop hook found
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    Unchanged,
    Dirty,
    NoResult,
    Failed,
    Passed,
}

/// None when shipcheck has nothing to say or cannot tell, so an error never traps the agent
fn state(cwd: &Path) -> Option<State> {
    if !has_config(cwd) {
        return None;
    }

    if !git(cwd, &["status", "--porcelain"])?.is_empty() {
        return Some(State::Dirty);
    }

    if git(cwd, &["rev-list", "--count", "origin/main..HEAD"])? == "0" {
        return Some(State::Unchanged);
    }

    let plan = run::plan(cwd, "HEAD", "origin/main").ok()?;

    Some(match run::lookup(&plan) {
        Some(e) if e.passed => State::Passed,
        Some(_) => State::Failed,
        None => State::NoResult,
    })
}

/// Blocks a stop once. An agent already continuing from a block, or stopping on an error, goes
pub fn stop_reason(input: &Value, state: impl FnOnce() -> Option<State>) -> Option<&'static str> {
    let again = input.get("stop_hook_active").and_then(Value::as_bool) == Some(true)
        || input.get("loop_count").and_then(Value::as_u64).unwrap_or(0) > 0
        || input
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| s != "completed");

    if again {
        return None;
    }

    match state()? {
        State::Dirty => Some(DIRTY),
        State::NoResult => Some(NO_RESULT),
        State::Failed => Some(FAILED),
        State::Unchanged | State::Passed => None,
    }
}

/// The reply in `agent`'s shape, None to let the call or the stop go on as usual
/// Cursor's reply depends on the hook that called, which it names in `hook_event_name`
fn cursor(hook_event: &str, reason: Option<&str>) -> Option<Value> {
    Some(match (hook_event, reason) {
        ("beforeShellExecution" | "preToolUse", None) => json!({"permission": "allow"}),
        ("beforeShellExecution" | "preToolUse", Some(r)) => {
            json!({"permission": "deny", "user_message": r, "agent_message": r})
        }
        ("stop", Some(r)) => json!({"followup_message": r}),
        _ => return None,
    })
}

/// The reply in `agent`'s shape, None to let the call or the stop go on as usual. `hook_event` is
/// the event name the agent sent, if any
pub fn format(
    agent: &str,
    event: &str,
    hook_event: Option<&str>,
    reason: Option<&str>,
) -> Option<String> {
    if agent == "cursor" {
        let fallback = if event == "stop" {
            "stop"
        } else {
            "beforeShellExecution"
        };
        return cursor(hook_event.unwrap_or(fallback), reason).map(|v| v.to_string());
    }

    let reply = match (agent, event, reason) {
        (_, _, None) => return None,
        ("claude" | "codex", "pre-tool", Some(r)) => json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": r,
        }}),
        ("copilot", "pre-tool", Some(r)) => {
            json!({"permissionDecision": "deny", "permissionDecisionReason": r})
        }
        ("gemini", "pre-tool" | "stop", Some(r)) => json!({"decision": "deny", "reason": r}),
        ("claude" | "codex" | "copilot", "stop", Some(r)) => {
            json!({"decision": "block", "reason": r})
        }
        _ => return None,
    };
    Some(reply.to_string())
}

/// `shipcheck hook <agent> <event>`: the agent's JSON in, its reply out. It never runs CI
pub fn reply(agent: &str, event: &str, text: &str) -> Option<String> {
    let input: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let cwd = input
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    let reason = match event {
        "pre-tool" => pre_tool(&input, &cwd),
        "stop" => stop_reason(&input, || state(&cwd)),
        _ => None,
    };
    let hook_event = input.get("hook_event_name").and_then(Value::as_str);
    format(agent, event, hook_event, reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_test_runners_and_git_commit_and_allows_the_rest() {
        let denied = [
            ("pytest -q", TESTS),
            ("python3 -m pytest tests/", TESTS),
            ("uv run --frozen pytest", TESTS),
            ("uv run python -m pytest", TESTS),
            ("poetry run pytest", TESTS),
            ("cargo test --lib", TESTS),
            ("cargo +nightly test", TESTS),
            ("cargo nextest run", TESTS),
            ("npm test", TESTS),
            ("npm run test:unit", TESTS),
            ("pnpm test", TESTS),
            ("bun test", TESTS),
            ("go test ./...", TESTS),
            ("just test", TESTS),
            ("make test", TESTS),
            ("tox -e py312", TESTS),
            ("./node_modules/.bin/jest", TESTS),
            ("npx vitest run", TESTS),
            ("act -j build", TESTS),
            ("cd sub && cargo test", TESTS),
            ("RUST_LOG=debug cargo test", TESTS),
            ("env CI=1 npm test", TESTS),
            ("cargo build 2>&1 | tail; cargo test", TESTS),
            ("(cd x; go test)", TESTS),
            ("git commit -m 'a; b'", COMMIT),
            ("git -C repo commit -am x", COMMIT),
            ("git add -A && git commit --amend", COMMIT),
            ("/usr/bin/git commit", COMMIT),
        ];

        for (line, reason) in denied {
            assert_eq!(shell_reason(line), Some(reason), "{line}");
        }

        let allowed = [
            "cargo build --tests",
            "cargo clippy -- -D warnings",
            "cargo fmt",
            "npm run build",
            "npm install",
            "go vet ./...",
            "ruff check .",
            "make",
            "git merge main",
            "git revert HEAD",
            "git cherry-pick abc",
            "git rebase --continue",
            "git log --grep commit",
            "echo 'cargo test'",
            "shipcheck run",
            "shipcheck commit -m 'cargo test'",
            "cat pytest.ini",
            // Not caught, documented: a test run inside another shell
            "sh -c 'cargo test'",
            "bash -lc \"npm test\"",
        ];

        for line in allowed {
            assert_eq!(shell_reason(line), None, "{line}");
        }
    }

    #[test]
    fn denies_commands_that_name_the_home() {
        let home = PathBuf::from("/Users/a/Library/Application Support/shipcheck");
        let marks = home_marks(&[home], &[PathBuf::from("/Users/a")]);
        assert!(marks.contains(&"Library/Application Support/shipcheck".to_string()));

        for line in [
            "echo x > '/Users/a/Library/Application Support/shipcheck/cache/k.json'",
            "cp r.json ~/Library/Application\\ Support/shipcheck/cache/",
            "cd \"$HOME/Library/Application Support/shipcheck\" && ls",
            "SHIPCHECK_HOME=/tmp/fake shipcheck post",
            "export SHIPCHECK_HOME=/tmp/fake",
            "curl -X POST https://api.shipcheck.run/checks",
            "docker run -v '/Users/a/Library/Application Support/shipcheck:/h' alpine",
            "HOME=/tmp/x shipcheck post",
            "XDG_STATE_HOME=/tmp/x shipcheck daemon",
            "export HOME=/tmp/x",
            "env shipcheck post",
            "shipcheck post --home-ok",
            "/usr/local/bin/shipcheck daemon --home-ok",
            "HOME+=/x shipcheck post",
            "command shipcheck post",
            "env -i /usr/local/bin/shipcheck post",
            "echo --home-ok | xargs shipcheck post",
            "ls;HOME=/x shipcheck post",
            "true&&SHIPCHECK_HOME=/x shipcheck post",
            "(XDG_STATE_HOME=/x shipcheck daemon)",
            "ls;--home-ok",
            "shipcheck run --base my-branch",
            "shipcheck run --base=my-branch",
            "shipcheck run --no-fetch",
        ] {
            assert!(reaches_home(line, &marks), "{line}");
        }

        for line in [
            "shipcheck logs",
            "/opt/bin/shipcheck post",
            "rg shipcheck src",
            "cargo install shipcheck",
            "echo $HOME",
            "ls ~/Library",
            "cargo build",
            "gh pr create --base main",
            "shipcheck run",
        ] {
            assert!(!reaches_home(line, &marks), "{line}");
        }
    }

    #[test]
    fn stop_blocks_once_and_fails_open() {
        let fresh = json!({"stop_hook_active": false});
        assert_eq!(stop_reason(&fresh, || Some(State::Dirty)), Some(DIRTY));
        assert_eq!(
            stop_reason(&fresh, || Some(State::NoResult)),
            Some(NO_RESULT)
        );
        assert_eq!(stop_reason(&fresh, || Some(State::Failed)), Some(FAILED));
        assert_eq!(stop_reason(&fresh, || Some(State::Passed)), None);
        assert_eq!(stop_reason(&fresh, || Some(State::Unchanged)), None);
        // No shipcheck.toml on main, or shipcheck failed to tell
        assert_eq!(stop_reason(&fresh, || None), None);

        for again in [
            json!({"stop_hook_active": true}),
            json!({"loop_count": 1, "status": "completed"}),
            json!({"loop_count": 0, "status": "aborted"}),
        ] {
            assert_eq!(
                stop_reason(&again, || panic!("must not look at the repository")),
                None
            );
        }

        assert_eq!(
            stop_reason(&json!({"loop_count": 0, "status": "completed"}), || Some(
                State::Dirty
            )),
            Some(DIRTY)
        );
    }

    #[test]
    fn replies_in_each_agents_shape() {
        let r = Some("why");
        let cases = [
            (
                "claude",
                "pre-tool",
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"why"}}"#,
            ),
            (
                "codex",
                "pre-tool",
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"why"}}"#,
            ),
            (
                "cursor",
                "pre-tool",
                r#"{"agent_message":"why","permission":"deny","user_message":"why"}"#,
            ),
            (
                "gemini",
                "pre-tool",
                r#"{"decision":"deny","reason":"why"}"#,
            ),
            (
                "copilot",
                "pre-tool",
                r#"{"permissionDecision":"deny","permissionDecisionReason":"why"}"#,
            ),
            ("claude", "stop", r#"{"decision":"block","reason":"why"}"#),
            ("codex", "stop", r#"{"decision":"block","reason":"why"}"#),
            ("copilot", "stop", r#"{"decision":"block","reason":"why"}"#),
            ("cursor", "stop", r#"{"followup_message":"why"}"#),
            ("gemini", "stop", r#"{"decision":"deny","reason":"why"}"#),
        ];

        for (agent, event, want) in cases {
            assert_eq!(
                format(agent, event, None, r).as_deref(),
                Some(want),
                "{agent} {event}"
            );
        }

        assert_eq!(format("claude", "pre-tool", None, None), None);
        assert_eq!(format("other", "stop", None, r), None);

        // Cursor, by the hook_event_name it sends: preToolUse and beforeShellExecution take
        // `permission`, stop takes `followup_message`, and any other event gets no reply
        let deny = r#"{"agent_message":"why","permission":"deny","user_message":"why"}"#;
        let allow = r#"{"permission":"allow"}"#;

        for event in ["preToolUse", "beforeShellExecution"] {
            assert_eq!(
                format("cursor", "pre-tool", Some(event), r).as_deref(),
                Some(deny)
            );
            assert_eq!(
                format("cursor", "pre-tool", Some(event), None).as_deref(),
                Some(allow)
            );
        }

        assert_eq!(
            format("cursor", "stop", Some("stop"), r).as_deref(),
            Some(r#"{"followup_message":"why"}"#)
        );
        assert_eq!(format("cursor", "stop", Some("stop"), None), None);
        assert_eq!(format("cursor", "pre-tool", Some("afterFileEdit"), r), None);
    }

    #[test]
    fn reads_the_command_from_every_agents_input() {
        let claude = json!({"tool_name": "Bash", "tool_input": {"command": "cargo test"}});
        let copilot = json!({"toolName": "bash", "toolArgs": "{\"command\":\"cargo test\"}"});
        let cursor = json!({"command": "cargo test", "cwd": "/x"});

        for input in [claude, copilot, cursor] {
            assert_eq!(tool_args(&input)["command"], "cargo test");
        }

        let nested = json!({"tool_input": {
            "edits": [{"target_file": "/h/a"}, {"other": "/h/no"}],
            "files": ["/h/b", {"name": "/h/c"}],
            "content": "/h/no",
        }});
        let mut found = Vec::new();
        path_strings(&tool_args(&nested), false, &mut found);
        found.sort();
        assert_eq!(found, ["/h/a", "/h/b", "/h/c"]);
    }

    #[test]
    fn file_paths_into_the_home_are_caught_through_dots_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("state/shipcheck");
        std::fs::create_dir_all(home.join("cache")).unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::os::unix::fs::symlink(&home, work.join("link")).unwrap();
        let homes = [home.clone()];

        assert!(in_home(&home.join("cache/k.json"), &homes));
        assert!(in_home(
            &work.join("../state/shipcheck/cache/new.json"),
            &homes
        ));
        assert!(in_home(&work.join("link/cache/new.json"), &homes));
        assert!(in_home(&work.join("link/new/dir/x"), &homes));
        assert!(!in_home(&work.join("src/main.rs"), &homes));
        assert!(!in_home(&dir.path().join("state/other"), &homes));

        // A symlink then `..` goes up from the link's target, not from the link
        std::fs::create_dir_all(dir.path().join("state/x")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("state/x"), work.join("up")).unwrap();
        assert!(in_home(&work.join("up/../shipcheck/cache/k.json"), &homes));
        // A `..` below a directory that does not exist yet cannot be told, so it counts
        assert!(in_home(&work.join("new/../x"), &homes));
        // A final symlink, also dangling, is followed
        std::os::unix::fs::symlink(home.join("cache/forged.json"), work.join("out.json")).unwrap();
        assert!(in_home(&work.join("out.json"), &homes));
        std::os::unix::fs::symlink(work.join("loop"), work.join("loop")).unwrap();
        assert!(in_home(&work.join("loop"), &homes));

        // Each link of a chain names the one before it 8 times, which cost 8^8 lookups with a
        // depth limit per level. One budget for the whole path ends it after 40 links
        std::fs::create_dir_all(work.join("chain")).unwrap();
        std::os::unix::fs::symlink(".", work.join("chain/l0")).unwrap();

        for i in 1..30 {
            let prev = vec![format!("l{}", i - 1); 8].join("/");
            std::os::unix::fs::symlink(prev, work.join(format!("chain/l{i}"))).unwrap();
        }

        let started = std::time::Instant::now();
        assert!(in_home(&work.join("chain/l29/x"), &homes));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));

        // On macOS a temp dir is /var/... and /private/var/..., both spellings count
        let real = resolve(&home).unwrap();
        assert!(in_home(&real.join("cache/k.json"), &homes));
        assert!(home_marks(&homes, &[]).contains(&real.to_string_lossy().into_owned()));
    }
}
