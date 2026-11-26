use super::Found;
use crate::workflow::{Job, RunStep};
use std::fs;
#[cfg(not(test))]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Marks a note of `job_check` that the check runs with another version than the workflow's
pub(super) const WARNING: &str = "warning: ";

/// Actions that report to GitHub or publish, which only GitHub's run of the job may do
pub(super) const REPORTERS: &[&str] = &[
    "github/codeql-action/",
    "actions/dependency-review-action",
    "actions/upload-pages-artifact",
    "actions/deploy-pages",
    "actions/attest",
    "codecov/",
    "coverallsapp/",
    "peaceiris/actions-gh-pages",
    "jamesives/github-pages-deploy-action",
    "softprops/action-gh-release",
];

/// Actions that read the PR, such as its changed files, which act's run has none of
pub(super) const PR_AWARE: &[&str] = &[
    "tj-actions/changed-files",
    "dorny/paths-filter",
    "reviewdog/",
    "danger/",
];

/// Commands that read the PR's event or its review themselves, and under act or here find none and may pass
pub(super) const PR_AWARE_COMMANDS: &[&str] =
    &["danger", "danger-swift", "danger-kotlin", "reviewdog"];

/// The flags of each launcher that take the next word as their value. pnpm's `-w` is `--workspace-root` and takes none
pub(super) fn value_flags(launcher: &str) -> &'static [&'static str] {
    match launcher {
        "npm" => &["-w", "--workspace", "--prefix"],
        "pnpm" => &["--filter", "-F", "--dir", "-C"],
        "yarn" => &["--cwd"],
        "bun" => &["--cwd", "--filter", "-F"],
        "npx" | "bunx" | "pnpx" => &["-p", "--package"],
        _ => &[],
    }
}

/// `words` past their leading flags, and the value a flag in `values` takes, such as `--filter web`
pub(super) fn past_flags<'a>(words: &'a [String], values: &[&str]) -> &'a [String] {
    let mut at = 0;

    while let Some(w) = words.get(at).filter(|w| w.starts_with('-')) {
        at += if values.contains(&w.as_str()) { 2 } else { 1 };
    }

    &words[at.min(words.len())..]
}

/// `words` past a package manager, its flags and `yarn workspace <name>`, so at its subcommand or script
pub(super) fn past_manager(words: &[String]) -> &[String] {
    let Some(manager) = words.first() else {
        return words;
    };
    let values = value_flags(manager);
    let after = past_flags(&words[1..], values);

    if manager == "yarn" && after.first().is_some_and(|w| w == "workspace") {
        return past_flags(after.get(2..).unwrap_or_default(), values);
    }

    after
}

/// The command `words` runs, past launchers such as `npx`, `yarn dlx`, `pnpm --filter web exec`, `bundle exec` or `go run`
pub(super) fn launched(words: &[String]) -> &[String] {
    let mut rest = words;

    loop {
        let first = rest.first().map_or_default(String::as_str);
        let values = value_flags(first);
        rest = match first {
            "npx" | "bunx" | "pnpx" => past_flags(&rest[1..], values),
            "yarn" | "pnpm" | "bun" => {
                let after = past_manager(rest);

                if matches!(
                    after.first().map(String::as_str),
                    Some("run" | "dlx" | "exec" | "x")
                ) {
                    past_flags(&after[1..], values)
                } else {
                    after
                }
            }
            "npm" => {
                let after = past_manager(rest);

                if after.first().is_some_and(|w| w == "exec") {
                    past_flags(&after[1..], values)
                } else {
                    return rest;
                }
            }
            "bundle" if rest.get(1).is_some_and(|w| w == "exec") => &rest[2..],
            // `mint run danger/swift danger-swift ci` names the package, then its command
            "mint" if rest.get(1).is_some_and(|w| w == "run") => {
                let after = past_flags(&rest[2..], values);
                if after.len() > 1 && after[0].contains('/') {
                    &after[1..]
                } else {
                    after
                }
            }
            "swift" | "go" if rest.get(1).is_some_and(|w| w == "run") => {
                past_flags(&rest[2..], values)
            }
            _ => return rest,
        };
    }
}

/// The command of `words` that reads the PR's event itself, past a launcher. `danger local` and reviewdog's local reporter read none
pub(super) fn reads_the_event(words: &[String]) -> Option<String> {
    let run = launched(words);
    let first = run.first()?;
    // `npx danger@11 ci` and `go run github.com/reviewdog/reviewdog/cmd/reviewdog@latest` name a version
    let path = first
        .split_once('@')
        .filter(|(p, _)| !p.is_empty())
        .map_or(first.as_str(), |(p, _)| p);
    let name = path.rsplit('/').next()?;
    let args = &run[1..];
    let local = match name {
        "danger" | "danger-swift" | "danger-kotlin" => args
            .iter()
            .find(|a| !a.starts_with('-'))
            .is_some_and(|a| a == "local"),
        "reviewdog" => {
            args.iter().any(|a| a == "-reporter=local")
                || args
                    .windows(2)
                    .any(|w| w[0] == "-reporter" && w[1] == "local")
        }
        _ => true,
    };

    (PR_AWARE_COMMANDS.contains(&name) && !local).then(|| name.to_string())
}

pub(super) fn task_lines(dir: &Path, words: &[String]) -> Vec<String> {
    // The same cap as a file a step names
    let read = |name: &str| {
        let file = dir.join(name);
        let small = fs::metadata(&file).is_ok_and(|m| m.is_file() && m.len() < 1 << 20);
        small.then(|| fs::read_to_string(&file).ok()).flatten()
    };
    let named = |rest: &[String]| {
        rest.iter()
            .find(|w| !w.starts_with('-') && !w.contains('='))
            .cloned()
    };

    match words.first().map(String::as_str) {
        Some(manager @ ("npm" | "yarn" | "pnpm" | "bun")) => {
            let values = value_flags(manager);
            let after = past_manager(words);
            let script = match (manager, after.first().map(String::as_str)) {
                ("npm", Some("run" | "run-script")) => past_flags(&after[1..], values).first(),
                ("npm", Some("test" | "start")) => after.first(),
                ("npm", _) => None,
                (_, Some("run")) => past_flags(&after[1..], values).first(),
                _ => after.first(),
            };
            let json: Option<serde_json::Value> =
                read("package.json").and_then(|t| serde_json::from_str(&t).ok());
            script
                .and_then(|s| json?["scripts"][s.as_str()].as_str().map(String::from))
                .into_iter()
                .collect()
        }
        Some("make" | "gmake") => {
            let text = ["GNUmakefile", "makefile", "Makefile"]
                .iter()
                .find_map(|f| read(f))
                .unwrap_or_default();
            recipe(&text, named(&words[1..]).as_deref(), |l| {
                l.starts_with('\t')
            })
        }
        Some("just") => {
            let text = ["justfile", "Justfile", ".justfile"]
                .iter()
                .find_map(|f| read(f))
                .unwrap_or_default();
            recipe(&text, named(&words[1..]).as_deref(), |l| {
                l.starts_with(' ') || l.starts_with('\t')
            })
        }
        _ => Vec::new(),
    }
}

/// The tool that reads the PR's event in the scripts or recipes `words` runs, three task runners deep
pub(super) fn event_in_tasks(dir: &Path, words: &[String], depth: u8) -> Option<String> {
    if depth == 3 {
        return None;
    }

    task_lines(dir, words)
        .iter()
        .flat_map(|l| started(l))
        .find_map(|w| reads_the_event(&w).or_else(|| event_in_tasks(dir, &w, depth + 1)))
}

/// The body lines of the rule `target` of a Makefile or justfile, the first rule when None
pub(super) fn recipe(text: &str, target: Option<&str>, body: impl Fn(&str) -> bool) -> Vec<String> {
    let mut lines = text.lines().peekable();
    // A special target such as `.PHONY`, an assignment such as `X := 1`, or just's `set` and `export` heads no rule
    let heads = |l: &str| {
        let assigns = [":=", "::=", "?=", "+=", "!="]
            .iter()
            .any(|a| l.contains(a));
        !body(l)
            && !l.starts_with(['#', '.'])
            && !l.starts_with("set ")
            && !l.starts_with("export ")
            && !assigns
            && l.split_once(':').is_some_and(|(h, _)| !h.contains('='))
    };

    while let Some(line) = lines.next() {
        if !heads(line) {
            continue;
        }

        let names = line.split_once(':').map_or("", |(h, _)| h);
        let hit = target.is_none_or(|t| names.split_whitespace().next() == Some(t));

        if hit {
            let mut out = Vec::new();

            while let Some(l) = lines.next_if(|l| body(l) || l.trim().is_empty()) {
                out.push(l.trim().trim_start_matches(['@', '-']).to_string());
            }

            return out;
        }
    }

    Vec::new()
}

/// Commands that run containers, whose daemon a job gets only on a VM of its own
pub(super) const CONTAINERS: &[&str] = &[
    "act",
    "buildah",
    "docker",
    "docker-compose",
    "kind",
    "minikube",
    "nerdctl",
    "podman",
];

/// Commands that change the kernel, its namespaces or the machine's services, which a container never may
pub(super) const KERNEL: &[&str] = &[
    "apparmor_parser",
    "bwrap",
    "chroot",
    "insmod",
    "iptables",
    "losetup",
    "modprobe",
    "mount",
    "nsenter",
    "service",
    "setcap",
    "sysctl",
    "systemctl",
    "unshare",
];

/// Commands that start the command after them, with flags of their own
pub(super) const WRAPPERS: &[&str] = &["sudo", "env", "time", "exec", "nohup"];

pub(super) fn started(run: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();

    for part in run
        .replace("&&", "\n")
        .replace("||", "\n")
        .split(['\n', ';', '|', '&', '(', ')', '`'])
    {
        let words = words(part);
        let mut at = 0;

        while let Some(w) = words.get(at) {
            if WRAPPERS.contains(&w.as_str()) {
                at += 1;

                while let Some(flag) = words.get(at).filter(|f| f.starts_with('-')) {
                    // `sudo -u <user>` and `-g <group>` take the word after them
                    at += if ["-u", "-g"].contains(&flag.as_str()) {
                        2
                    } else {
                        1
                    };
                }
            } else if w.contains('=') && !w.starts_with('-') {
                at += 1;
            } else {
                break;
            }
        }

        let words = words[at.min(words.len())..].to_vec();

        if words.is_empty() {
            continue;
        }

        let shell = ["sh", "bash", "zsh", "dash"].contains(&words[0].as_str());

        match words.iter().position(|w| w == "-c").filter(|_| shell) {
            Some(c) => out.extend(words.get(c + 1).map_or_default(|s| started(s))),
            None => out.push(words),
        }
    }

    out
}

/// A command's name for the lists above: a bare name, or the basename of one in a system directory
pub(super) fn command_word(word: &str) -> Option<&str> {
    let system = [
        "/bin/",
        "/sbin/",
        "/usr/bin/",
        "/usr/sbin/",
        "/usr/local/bin/",
    ];

    match word.rsplit_once('/') {
        None => Some(word),
        Some((dir, name)) if system.contains(&format!("{dir}/").as_str()) => Some(name),
        Some(_) => None,
    }
}

/// A line split into words as a shell does, quote marks removed and what they hold kept
pub(super) fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;

    for c in line.chars() {
        match (quote, c) {
            _ if escaped => {
                word.push(c);
                escaped = false;
            }
            (Some('"') | None, '\\') => escaped = true,
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, _) if c.is_whitespace() => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            _ => word.push(c),
        }
    }

    if !word.is_empty() {
        out.push(word);
    }

    out
}

/// The command a line's words start, after `sudo` and its flags
pub(super) fn command_name(words: &[String]) -> Option<&str> {
    let mut sudo = false;
    words
        .iter()
        .find(|w| {
            let skip = *w == "sudo" || sudo && w.starts_with('-');
            sudo |= *w == "sudo";
            !skip
        })
        .map(String::as_str)
}

/// A job's command and the `NAME=value` exports it needs in front
pub(super) type JobCommand = (String, Vec<String>);

/// `export <exports> && <command>`, or the command alone
pub(super) fn with_exports(exports: &[String], command: &str) -> String {
    if exports.is_empty() {
        return command.to_string();
    }

    format!("export {} && {command}", exports.join(" "))
}

/// Shell builtins, coreutils, git and tools that set the machine up, whose version never changes what a check finds
pub(super) const PLAIN: &[&str] = &[
    "cd", "test", "[", "true", "false", "echo", "printf", "mkdir", "cp", "mv", "rm", "ls", "cat",
    "diff", "sh", "bash", "env", "time", "export", "exit", "sleep", "xargs", "grep", "sed", "awk",
    "find", "tee", "head", "tail", "sort", "uniq", "wc", "tr", "cut", "touch", "chmod", "ln",
    "pwd", "command", "git", "sudo", "apt", "apt-get", "brew", "curl", "wget", "rustup",
    "truncate", "mount", "umount", "chown",
];

pub(super) fn tool_versions(
    checks: &[Found],
    known: &[String],
    answers: fn(&str) -> bool,
) -> Vec<String> {
    let covered: Vec<&str> = known
        .iter()
        .filter_map(|v| v.split_whitespace().find(|w| !w.contains('=')))
        .collect();
    let mut tools: Vec<String> = Vec::new();

    for tool in checks.iter().flat_map(|c| tools_in(&c.command)) {
        if !covered.contains(&tool.as_str()) && !tools.contains(&tool) {
            tools.push(tool);
        }
    }

    tools
        .into_iter()
        .filter(|t| answers(t))
        .map(|t| format!("{t} --version"))
        .collect()
}

/// `<check> calls <tool>` for each tool a check starts that `installed` does not find
pub(super) fn missing_tools(checks: &[Found], installed: fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();

    for check in checks {
        // What the check runs after an install or a venv's `activate` can come from it
        let command = &check.command;
        let cut = [" install", "activate"]
            .iter()
            .filter_map(|w| command.find(w))
            .min()
            .unwrap_or(command.len());

        for tool in tools_in(&command[..cut]) {
            let line = format!("{} calls {tool}", check.name);

            if !installed(&tool) && !out.contains(&line) {
                out.push(line);
            }
        }
    }

    out
}

/// Shell builtins that `PLAIN` and `STATEFUL` do not name, which no PATH holds
pub(super) const BUILTINS: &[&str] = &[
    "eval", "exec", "read", "type", "wait", "trap", "shift", "local", "declare", "return", "hash",
];

/// The tools a command starts, each once, without builtins, coreutils and setup tools
pub(super) fn tools_in(command: &str) -> Vec<String> {
    let words = command.replace(['(', ')', '{', '}', ';', '|', '&'], "\n");
    let mut tools: Vec<String> = Vec::new();

    for segment in words.lines() {
        // The tool `xargs` starts is the one that runs, after its flags
        let mut after_xargs = false;
        let tool = segment
            .split_whitespace()
            .find(|w| {
                after_xargs |= *w == "xargs";
                !(["time", "xargs"].contains(w)
                    || w.contains('=')
                    || after_xargs && w.starts_with('-'))
            })
            .unwrap_or_default();
        let named = tool.starts_with(|c: char| c.is_ascii_alphanumeric())
            && tool
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c));

        if named
            && !PLAIN.contains(&tool)
            && !STATEFUL.contains(&tool)
            && !BUILTINS.contains(&tool)
            && !tool.starts_with("mkfs")
            && !tools.iter().any(|t| t == tool)
        {
            tools.push(tool.to_string());
        }
    }

    tools
}

/// Whether `<tool> --version` exits 0 within a few seconds on this machine
#[cfg(not(test))]
pub(super) fn answers(tool: &str) -> bool {
    responds(tool, &["--version"])
}

#[cfg(not(test))]
pub(super) fn responds(tool: &str, args: &[&str]) -> bool {
    let Ok(mut child) = std::process::Command::new(tool)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };

    for _ in 0..60 {
        if let Ok(Some(status)) = child.try_wait() {
            return status.success();
        }

        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let _ = child.kill();
    let _ = child.wait();
    false
}

// Tests see the same file on every machine, whatever is installed
#[cfg(test)]
pub(super) fn answers(_: &str) -> bool {
    false
}

/// Whether an executable file of that name is in a directory of `PATH`
#[cfg(not(test))]
pub(super) fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| {
            fs::metadata(d.join(tool))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

#[cfg(test)]
pub(super) fn on_path(_: &str) -> bool {
    true
}

/// The jobs `needs` names, and the jobs those need, each once, every job after the ones it needs
pub(super) fn needed(
    jobs: &[Job],
    needs: &[String],
    out: &mut Vec<String>,
    path: &mut Vec<String>,
) {
    for need in needs {
        // A cycle is GitHub's error, and this stops at it
        if out.contains(need) || path.contains(need) {
            continue;
        }

        if let Some(job) = jobs.iter().find(|j| &j.id == need) {
            path.push(need.clone());
            needed(jobs, &job.needs, out, path);
            path.pop();
        }

        out.push(need.clone());
    }
}

pub(super) fn sets_up(words: &[&str]) -> bool {
    let words = match words {
        ["sudo", rest @ ..] => {
            let flags = rest.iter().take_while(|w| w.starts_with('-')).count();
            &rest[flags..]
        }
        _ => words,
    };
    let package = [
        "install",
        "update",
        "upgrade",
        "purge",
        "remove",
        "autoremove",
    ];

    match words {
        ["pip" | "pip3" | "go" | "cargo", "install", ..]
        | ["python" | "python3", "-m", "pip", "install", ..]
        | ["npm" | "git", "config", ..]
        | ["rustup" | "which", ..]
        | [
            "brew",
            "install" | "update" | "upgrade" | "tap" | "bundle",
            ..,
        ]
        | ["command", "-v", ..] => true,
        ["apt" | "apt-get" | "dnf" | "yum", rest @ ..] => rest.iter().any(|w| package.contains(w)),
        _ => false,
    }
}

/// A line's commands between `&&`, `||`, `|`, `;` and parentheses, each as its words
pub(super) fn line_commands(line: &str) -> Vec<Vec<&str>> {
    line.split(|c: char| "&|;()".contains(c))
        .map(|c| c.split_whitespace().collect::<Vec<_>>())
        .filter(|c| !c.is_empty())
        .collect()
}

/// Whether a line only sets the machine up: installs, or a `curl` or `wget` piped into a shell
pub(super) fn setup_line(line: &str) -> bool {
    let commands = line_commands(line);
    let shell = |c: &Vec<&str>| {
        matches!(
            c.as_slice(),
            ["sh" | "bash", ..] | ["sudo", "sh" | "bash", ..]
        )
    };
    let piped = matches!(commands.first().map(|c| c[0]), Some("curl" | "wget"))
        && commands.len() > 1
        && commands[1..].iter().all(shell);

    piped || commands.iter().all(|c| sets_up(c))
}

/// Whether a line sends its output to a file, such as `echo x >> gradle.properties`, which the build may read
pub(super) fn writes_a_file(line: &str) -> bool {
    line.split('>')
        .skip(1)
        .map(str::trim_start)
        .filter(|rest| !rest.is_empty() && !rest.starts_with('&'))
        .any(|rest| rest.split_whitespace().next() != Some("/dev/null"))
}

/// Whether a command or a job id has deploy, publish or release as a word of its own
pub(super) fn ships(text: &str, separators: &[char]) -> bool {
    text.to_lowercase()
        .split(|c: char| c.is_whitespace() || separators.contains(&c))
        .any(|w| ["deploy", "publish", "release"].contains(&w))
}

/// Builtins that change the shell they run in, so a step that uses them runs in a subshell
pub(super) const STATEFUL: &[&str] = &[
    "cd", "export", "set", "unset", "source", ".", "alias", "pushd", "popd", "shopt", "ulimit",
    "umask",
];

/// Commands that move files or talk to git, which pass whatever the code is
pub(super) const HOUSEKEEPING: &[&str] = &[
    "cd", "export", "mkdir", "touch", "cp", "mv", "rm", "ln", "chmod", "find", "ls", "cat", "echo",
    "printf", "tar", "unzip", "install", "git", "sleep", "true", "pwd", "which", "sed",
];

/// The tool and subcommand of package installs and lockfile updates that `install` does not name
pub(super) const INSTALLS: &[[&str; 2]] = &[
    ["npm", "ci"],
    ["uv", "sync"],
    ["poetry", "lock"],
    ["cargo", "fetch"],
    ["cargo", "update"],
    ["cargo", "generate-lockfile"],
    ["go", "mod"],
];

pub(super) fn checks_nothing(command: &str) -> bool {
    unquoted(command)
        .split(['&', '|', ';', '(', ')', '{', '}'])
        .all(|c| {
            // `VAR=1` before the command and `+nightly` after cargo do not change what runs
            let words: Vec<&str> = c
                .split_whitespace()
                .filter(|w| !w.contains('=') && !w.starts_with('+'))
                .collect();
            words.is_empty()
                || HOUSEKEEPING.contains(&words[0])
                || words.contains(&"install")
                || INSTALLS.iter().any(|i| words.starts_with(i))
        })
}

/// `NAME=value` for the shell, or None for a name a shell cannot export or a value with a control character
pub(super) fn assignment(name: &str, value: &str) -> Option<String> {
    let name_ok = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');

    if !name_ok || value.chars().any(char::is_control) {
        return None;
    }

    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:,@+%".contains(c))
    {
        return Some(format!("{name}={value}"));
    }

    Some(format!("{name}='{}'", value.replace('\'', "'\\''")))
}

/// Commands that need root, which a check never has, as a command's first word
pub(super) const ROOT: &[&str] = &[
    "sudo", "mount", "umount", "losetup", "apt", "apt-get", "dnf", "yum", "modprobe",
];

pub(super) fn needs_root(line: &str) -> bool {
    line_commands(line)
        .iter()
        .any(|c| ROOT.contains(&c[0]) || c[0].starts_with("mkfs"))
}

/// Commands on a runner that reach the network or GitHub, which a check cannot, as a command's first word
pub(super) const RUNNER_ONLY: &[&str] = &["curl", "wget", "gh", "brew"];

/// Variables the sandbox sets for each check, which a workflow's value would point elsewhere
pub(super) const SANDBOX_VARS: &[&str] = &["TMPDIR", "HOME"];

pub(super) const CONTROL: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "select", "{", "}", "exit", "return",
];

/// What separates commands, unless a quote or a `\` makes it literal
pub(super) const SEPARATORS: &str = "&|;(){}";

pub(super) fn unquoted(line: &str) -> String {
    let mut out = String::new();
    // `'`, `"`, or `$` for `$'...'`, where `\` escapes as it does in `"`
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if escaped {
            escaped = false;

            if !SEPARATORS.contains(c) {
                out.push(c);
            }

            continue;
        }

        match (quote, c) {
            (None | Some('"' | '$'), '\\') => escaped = true,
            (None, '$') if chars.peek() == Some(&'\'') => {
                chars.next();
                quote = Some('$');
            }
            (None, '\'' | '"') => quote = Some(c),
            (Some('$'), '\'') => quote = None,
            (Some(q), _) if q == c => quote = None,
            (Some(_), _) if SEPARATORS.contains(c) => {}
            _ => out.push(c),
        }
    }

    out
}

/// The `NAME=value` a command exports, as `assignment` wrote it
pub(super) fn assigned<'a>(command: &'a str, name: &str) -> Option<&'a str> {
    let rest = &command[command.find(&format!("{name}="))?..];
    let value = &rest[name.len() + 1..];
    let end = match value.strip_prefix('\'') {
        Some(quoted) => name.len() + 3 + quoted.find('\'')?,
        None => rest.find(char::is_whitespace).unwrap_or(rest.len()),
    };
    Some(&rest[..end])
}

pub(super) fn step_command(step: &RunStep) -> Result<Option<String>, (bool, String)> {
    let leave = |why: &str| Err((false, why.to_string()));

    if step.conditional {
        return leave("a step with `if:` or `continue-on-error`");
    }

    if step.run.chars().any(|c| c.is_control() && c != '\n') {
        return leave("a step with a control character");
    }

    if step
        .shell
        .as_deref()
        .is_some_and(|s| s != "bash" && s != "sh")
    {
        return leave("a step in another shell");
    }

    let mut lines = Vec::new();

    for line in step.run.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let first = line.split_whitespace().next().unwrap_or_default();

        if ["$RUNNER_", "${RUNNER_", "$GITHUB_", "${GITHUB_"]
            .iter()
            .any(|v| line.contains(v))
        {
            return leave("a step with a variable only GitHub's runner sets");
        }

        if line.contains("${{") {
            return leave("a step with an expression");
        }

        if line.ends_with('\\') {
            return leave("a step with a `\\` continuation");
        }

        if line.contains("<<") {
            return leave("a step with a heredoc");
        }

        if CONTROL.contains(&first) || line.contains("()") {
            return leave("a step with shell control flow or a function");
        }

        if unquoted(line).contains(';') {
            return leave("a step with `;`, which GitHub's `bash -e` stops at");
        }

        // The job's Xcode becomes its `DEVELOPER_DIR`, rather than a switch for the whole machine
        let line = &match without_xcode_pick(line) {
            Ok(line) => line,
            Err(why) => return leave(&why),
        };

        if line.is_empty() {
            continue;
        }

        if setup_line(line) {
            continue;
        }

        // `echo` only prints, unless it writes a file the build reads
        if first == "echo" && !writes_a_file(line) {
            continue;
        }

        if needs_root(line) {
            return leave("a step that needs root, with `sudo`, `mount`, `mkfs` or `apt`");
        }

        if line_commands(line)
            .iter()
            .any(|c| RUNNER_ONLY.contains(&c[0]))
        {
            return leave("a step with `curl`, `wget`, `gh` or `brew` that is not an install");
        }

        if ships(line, &[]) {
            return Err((true, "a step deploys, publishes or releases".into()));
        }

        lines.push(line.clone());
    }

    // A step that only changes its own shell, such as a lone `cd` after its install was left out, does nothing
    if lines.iter().all(|l| {
        !l.contains(['&', '|'])
            && STATEFUL.contains(&l.split_whitespace().next().unwrap_or_default())
    }) {
        return Ok(None);
    }

    // GitHub sets a step's `env` for its process only
    let mut exports = Vec::new();

    for (name, value) in &step.env {
        match value.as_deref().and_then(|v| assignment(name, v)) {
            Some(a) => exports.push(a),
            None => return leave("a step with an expression"),
        }
    }

    if !exports.is_empty() {
        lines.insert(0, format!("export {}", exports.join(" ")));
    }

    Ok(match lines.as_slice() {
        [] => None,
        [one] => Some(one.clone()),
        many => Some(
            many.iter()
                .map(|l| {
                    if l.contains(['|', '&']) {
                        format!("{{ {l}; }}")
                    } else {
                        l.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(" && "),
        ),
    })
}

pub(super) fn without_xcode_pick(line: &str) -> Result<String, String> {
    if !line.contains("xcode-select") && !line.contains("DEVELOPER_DIR=") {
        return Ok(line.to_string());
    }

    // A pick joined by `||`, `|` or `&` is left with the line, which then says so
    let picks = |part: &str| {
        let words = words(part);
        !unquoted(part).contains(['|', '&']) && command_name(&words) == Some("xcode-select")
            || words.len() == 2 && words[0] == "export" && words[1].starts_with("DEVELOPER_DIR=")
    };
    let kept: Vec<&str> = line
        .split("&&")
        .map(str::trim)
        .filter(|p| !picks(p))
        .collect();

    if kept.iter().any(|p| p.contains("xcode-select")) {
        return Err("a step that runs xcode-select beside other commands".into());
    }

    Ok(kept.join(" && "))
}
