use std::io::IsTerminal;

/// Colors for a person at a terminal. Off, every method returns its text unchanged, which agents parse
#[derive(Clone, Copy, Default)]
pub struct Style {
    pub on: bool,
}

impl Style {
    pub fn stdout() -> Self {
        Self::detect(std::io::stdout().is_terminal())
    }

    pub fn stderr() -> Self {
        Self::detect(std::io::stderr().is_terminal())
    }

    fn detect(terminal: bool) -> Self {
        Style {
            on: wanted(terminal, |name| std::env::var(name).ok()),
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    pub fn red(&self, text: &str) -> String {
        self.paint("31", text)
    }

    pub fn green(&self, text: &str) -> String {
        self.paint("32", text)
    }

    pub fn yellow(&self, text: &str) -> String {
        self.paint("33", text)
    }

    pub fn ok(&self) -> String {
        self.green("✔")
    }

    pub fn fail(&self) -> String {
        self.red("✘")
    }

    pub fn warn(&self) -> String {
        self.yellow("!")
    }

    /// A unified diff line: + green, - red, @@ dim
    pub fn diff(&self, line: &str) -> String {
        if line.starts_with("+++") || line.starts_with("---") {
            self.bold(line)
        } else if line.starts_with('+') {
            self.green(line)
        } else if line.starts_with('-') {
            self.red(line)
        } else if line.starts_with("@@") {
            self.dim(line)
        } else {
            line.to_string()
        }
    }

    /// `name: state` with the state green when it passed and red when it failed
    pub fn status(&self, line: &str) -> String {
        let Some((name, state)) = line.split_once(": ").filter(|_| self.on) else {
            return line.to_string();
        };
        let word = state.split([',', ' ']).next().unwrap_or_default();
        let state = match word {
            "passed" | "success" => self.green(state),
            "failed" | "failure" | "timed" | "cancelled" => self.red(state),
            "skipped" | "not" | "no" => self.yellow(state),
            _ => return line.to_string(),
        };
        format!("{name}: {state}")
    }
}

/// Color only on a terminal, unless NO_COLOR is set, CLICOLOR is 0 or TERM is dumb
fn wanted(terminal: bool, var: impl Fn(&str) -> Option<String>) -> bool {
    terminal
        && var("NO_COLOR").is_none()
        && var("CLICOLOR").as_deref() != Some("0")
        && var("TERM").as_deref() != Some("dumb")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_only_on_a_terminal_that_wants_it() {
        let none = |_: &str| None;
        assert!(wanted(true, none));
        assert!(!wanted(false, none));
        assert!(!wanted(true, |n| (n == "NO_COLOR").then(|| "1".into())));
        assert!(!wanted(true, |n| (n == "CLICOLOR").then(|| "0".into())));
        assert!(!wanted(true, |n| (n == "TERM").then(|| "dumb".into())));
    }

    #[test]
    fn off_leaves_text_as_it_is() {
        let s = Style { on: false };
        assert_eq!(s.status("ci.yml: failed, exit 1"), "ci.yml: failed, exit 1");
        assert_eq!(s.diff("+a"), "+a");
        assert_eq!(s.bold("a"), "a");
        let s = Style { on: true };
        assert_eq!(
            s.status("ci.yml: failed, exit 1"),
            "ci.yml: \x1b[31mfailed, exit 1\x1b[0m"
        );
    }
}
