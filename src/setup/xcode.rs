use super::input;
use super::shell::{without_xcode_pick, words};
use crate::workflow::{Job, RunStep};

pub(super) fn sets_up_xcode(step: &RunStep) -> bool {
    step.uses
        .as_deref()
        .is_some_and(|u| u.split('@').next() == Some("maxim-lobanov/setup-xcode"))
}

/// The path `xcode-select -s`, `--switch` or `--switch=` sets, in a line's words
pub(super) fn xcode_select(words: &[String]) -> Option<&str> {
    let at = words.iter().position(|w| w == "xcode-select")?;

    for (i, w) in words.iter().enumerate().skip(at + 1) {
        if ["&&", "||", ";", "|"].contains(&w.as_str()) {
            break;
        }

        if let Some(path) = w.strip_prefix("--switch=") {
            return Some(path);
        }

        if ["-s", "--switch", "-switch"].contains(&w.as_str()) {
            return words.get(i + 1).map(String::as_str);
        }
    }

    None
}

pub(super) fn xcode(job: &Job) -> Option<Result<String, String>> {
    let expression = || Some(Err("picks its Xcode with an expression".to_string()));
    // None for a path that is not an Xcode, such as the Command Line Tools
    let pick = |path: &str| {
        if path.contains('$') {
            return expression();
        }

        path.find(".app").map(|i| Ok(path[..i + 4].to_string()))
    };
    let from_env = |env: &[(String, Option<String>)]| {
        env.iter()
            .find(|(k, _)| k == "DEVELOPER_DIR")
            .and_then(|(_, v)| v.as_deref().map_or_else(expression, pick))
    };
    let mut picks: Vec<Result<String, String>> = from_env(&job.env).into_iter().collect();

    for step in &job.steps {
        picks.extend(from_env(&step.env));

        for line in step.run.lines() {
            let words = words(line);
            // `export DEVELOPER_DIR=` and `echo "DEVELOPER_DIR=..." >> $GITHUB_ENV` pick one too
            let mut set = words
                .iter()
                .filter_map(|w| w.strip_prefix("DEVELOPER_DIR="))
                .chain(xcode_select(&words));
            picks.extend(set.find_map(pick));
        }

        if sets_up_xcode(step) {
            picks.push(match input(step, "xcode-version") {
                Some(v) if v.starts_with("latest") => Ok(String::new()),
                Some(v) if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.') => {
                    Ok(format!("/Applications/Xcode_{v}.app"))
                }
                _ => Err("picks its Xcode with a version init cannot match".into()),
            });
        }
    }

    // One check has one DEVELOPER_DIR, so a job that switches Xcode between steps stays whole
    let first = picks.first()?.clone();

    if first.is_ok() && picks.iter().any(|p| *p != first) {
        return Some(Err("picks more than one Xcode".into()));
    }

    Some(first)
}

/// A `run` whose every line only picks an Xcode, `|| true` or not, or hands `DEVELOPER_DIR` to later steps
pub(super) fn picks_xcode_only(run: &str) -> bool {
    let picks = |line: &str| {
        let words = words(line);
        let to_env = words.len() == 4
            && words[0] == "echo"
            && words[1].starts_with("DEVELOPER_DIR=")
            && words[2] == ">>"
            && words[3] == "$GITHUB_ENV";
        to_env
            || without_xcode_pick(line.trim().trim_end_matches("|| true").trim())
                .is_ok_and(|l| l.is_empty())
    };
    let mut lines = run.lines().filter(|l| !l.trim().is_empty()).peekable();
    lines.peek().is_some() && lines.all(picks)
}

/// A job's or step's `DEVELOPER_DIR` pick, which becomes the check's own, or none when this machine lacks it
pub(super) fn picked(name: &str, value: Option<&String>) -> bool {
    name == "DEVELOPER_DIR" && value.is_none_or(|v| v.contains(".app"))
}
