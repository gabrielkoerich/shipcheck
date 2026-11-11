use std::io::{IsTerminal, Write};
use std::sync::Mutex;

/// Braille frames of the live line's spinner
const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/**
 * A command's output, with one live line under it at a terminal. The live line is redrawn in place
 * with a carriage return, so a line printed while it shows first erases it and then draws it again.
 * Piped, there is no live line and every line is plain text
 **/
pub struct Live {
    pub terminal: bool,
    width: usize,
    /// The stream and the live line on it now
    out: Mutex<(Box<dyn Write + Send>, String)>,
}

impl Live {
    /// On stderr when stdout holds only a JSON result
    pub fn new(stderr: bool) -> Live {
        let (terminal, out): (bool, Box<dyn Write + Send>) = if stderr {
            (std::io::stderr().is_terminal(), Box::new(std::io::stderr()))
        } else {
            (std::io::stdout().is_terminal(), Box::new(std::io::stdout()))
        };
        let dumb = std::env::var("TERM").is_ok_and(|t| t == "dumb");
        let width = ratatui::crossterm::terminal::size().map_or(80, |(w, _)| usize::from(w));
        Live::on(out, terminal && !dumb, width)
    }

    pub fn on(out: Box<dyn Write + Send>, terminal: bool, width: usize) -> Live {
        Live {
            terminal,
            width,
            out: Mutex::new((out, String::new())),
        }
    }

    /// Prints a line that stays, above the live one
    pub fn line(&self, text: &str) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let (stream, shown) = &mut *out;

        let _ = if shown.is_empty() {
            writeln!(stream, "{text}")
        } else {
            write!(stream, "\r\x1b[2K{text}\n{shown}")
        };
        let _ = stream.flush();
    }

    /// Draws the live line with the spinner's `frame`, cut to the terminal's width so it never wraps
    pub fn show(&self, frame: usize, text: &str) {
        if self.terminal {
            let line = format!("{} {text}", FRAMES[frame % FRAMES.len()]);
            self.draw(line.chars().take(self.width.saturating_sub(1)).collect());
        }
    }

    /// Erases the live line
    pub fn clear(&self) {
        self.draw(String::new());
    }

    fn draw(&self, line: String) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());

        if self.terminal && (!line.is_empty() || !out.1.is_empty()) {
            let _ = write!(out.0, "\r\x1b[2K{line}");
            let _ = out.0.flush();
            out.1 = line;
        }
    }
}

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

    /// A stream the test reads back
    #[derive(Clone, Default)]
    struct Seen(std::sync::Arc<Mutex<Vec<u8>>>);

    impl Write for Seen {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seen {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[test]
    fn piped_output_is_plain_lines_with_no_live_line() {
        let seen = Seen::default();
        let live = Live::on(Box::new(seen.clone()), false, 80);
        live.line("ci.yml/test: running, attempt 1");
        live.show(0, "running ci.yml/test, 5s");
        live.line("ci.yml/test: success, 1m24s");
        live.clear();
        assert_eq!(
            seen.text(),
            "ci.yml/test: running, attempt 1\nci.yml/test: success, 1m24s\n"
        );
    }

    #[test]
    fn a_terminal_keeps_one_live_line_under_the_lines_that_stay() {
        let seen = Seen::default();
        let live = Live::on(Box::new(seen.clone()), true, 12);
        live.show(0, "running ci.yml/test, 5s");
        // Cut to the width, so the carriage return always reaches its start
        assert_eq!(seen.text(), "\r\x1b[2K⠋ running c");
        live.line("done");
        assert!(seen.text().ends_with("\r\x1b[2Kdone\n⠋ running c"));
        live.clear();
        assert!(seen.text().ends_with("\r\x1b[2K"));
        live.line("after");
        assert!(seen.text().ends_with("\r\x1b[2Kafter\n"));
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
