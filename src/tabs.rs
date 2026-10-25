//! `shipcheck init` at a terminal: its steps as tabs, which the person moves between freely

use std::collections::BTreeMap;
use std::path::Path;

use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};

use crate::config::Engine;
use crate::onboard::{Answers, App, Plan};
use crate::setup::{Detected, Kept};

pub const TABS: [&str; 5] = ["Checks", "Install", "Workflows", "GitHub", "Review"];

/// Where a job runs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Host,
    Act,
    Vm,
    GitHub,
}

impl Place {
    fn word(self) -> &'static str {
        match self {
            Place::Host => "host check",
            Place::Act => "act, on Docker",
            Place::Vm => "act, on a colima VM",
            Place::GitHub => "GitHub",
        }
    }
}

/// One job of the Checks tab, where it runs now and where the person may move it
#[derive(Debug, PartialEq)]
pub struct Row {
    pub file: String,
    pub job: String,
    pub place: Place,
    pub choices: Vec<Place>,
    pub why: String,
}

/// The job a check stands for, from its `from`, None for one from a project file
fn job_of(from: &str) -> Option<(String, String)> {
    let (file, rest) = from.split_once(", job ")?;
    let job = rest.split(", after job ").next().unwrap_or(rest);
    Some((file.to_string(), job.to_string()))
}

fn colima_able(k: &Kept) -> bool {
    k.act
        .as_deref()
        .is_some_and(|a| a.contains("colima could run it"))
}

/**
 * The jobs init placed, each with the places it may go: a host check, which ran in its trial, may
 * go to act or stay on GitHub, a job on act may stay on GitHub, and a job only a VM runs may go to
 * colima, which reaches this machine's loopback, so it is never the default
 **/
pub fn rows(d: &Detected, docker: bool) -> Vec<Row> {
    let mut rows = Vec::new();

    for c in &d.checks {
        if let Some((file, job)) = job_of(&c.from) {
            rows.push(Row {
                file,
                job,
                place: Place::Host,
                // act runs only where Docker does
                choices: if docker {
                    vec![Place::Host, Place::Act, Place::GitHub]
                } else {
                    vec![Place::Host, Place::GitHub]
                },
                why: c.command.clone(),
            });
        }
    }

    for (k, e) in &d.acts {
        let place = if *e == Engine::Colima {
            Place::Vm
        } else {
            Place::Act
        };
        rows.push(Row {
            file: k.file.clone(),
            job: k.job.clone(),
            place,
            choices: vec![place, Place::GitHub],
            why: k.why.clone(),
        });
    }

    for k in &d.github {
        let mut choices = vec![Place::GitHub];

        if colima_able(k) {
            choices.push(Place::Vm);
        }

        rows.push(Row {
            file: k.file.clone(),
            job: k.job.clone(),
            place: Place::GitHub,
            choices,
            why: match &k.act {
                Some(act) => format!("{}, and act cannot run it either: {act}", k.why),
                None => k.why.clone(),
            },
        });
    }

    rows
}

/// `base` with each job the person moved in its new place
pub fn placed(base: &Detected, moves: &BTreeMap<(String, String), Place>) -> Detected {
    let mut d = base.clone();

    for ((file, job), to) in moves {
        let this = |f: &str, j: &str| f == file && j == job;
        let mut kept = None;

        if let Some(i) = d
            .checks
            .iter()
            .position(|c| job_of(&c.from).is_some_and(|(f, j)| this(&f, &j)))
        {
            if *to == Place::Host {
                continue;
            }

            let c = d.checks.remove(i);
            kept = Some(Kept {
                file: file.clone(),
                job: job.clone(),
                why: format!("moved from host check `{}` in init", c.command),
                checks: None,
                act: None,
            });
        } else if let Some(i) = d.acts.iter().position(|(k, _)| this(&k.file, &k.job)) {
            kept = Some(d.acts.remove(i).0);
        } else if let Some(i) = d.github.iter().position(|k| this(&k.file, &k.job)) {
            kept = Some(d.github.remove(i));
        }

        let Some(k) = kept else {
            continue;
        };

        match to {
            Place::Act => d.acts.push((k, Engine::Docker)),
            Place::Vm => d.acts.push((k, Engine::Colima)),
            Place::GitHub | Place::Host => d.github.push(k),
        }
    }

    d
}

/// What the wizard ended with
#[derive(Debug, PartialEq, Eq)]
pub enum End {
    Apply,
    Quit,
}

/// The wizard's state, which renders and takes keys without a terminal, so tests drive it
pub struct Wizard<'a> {
    root: &'a Path,
    pub plan: Plan,
    pub answers: Answers,
    base: Option<Detected>,
    moves: BTreeMap<(String, String), Place>,
    docker: bool,
    pub tab: usize,
    pub cursor: usize,
    pub error: Option<String>,
    pub end: Option<End>,
}

impl<'a> Wizard<'a> {
    pub fn new(root: &'a Path, plan: Plan) -> Self {
        let answers = plan.recommended();
        Wizard {
            root,
            base: plan.detected.clone(),
            plan,
            answers,
            moves: BTreeMap::new(),
            docker: crate::setup::Machine::this().docker,
            tab: 0,
            cursor: 0,
            error: None,
            end: None,
        }
    }

    /// The lines the person moves the cursor over in the current tab
    pub fn items(&self) -> Vec<String> {
        let yes = |b: bool| if b { "yes" } else { "no" };
        let a = &self.answers;

        match TABS[self.tab] {
            "Checks" => {
                let mut out = Vec::new();

                if self.plan.config.is_some() {
                    out.push(format!("Write shipcheck.toml: {}", yes(a.config)));
                }

                out.extend(self.rows().iter().map(|r| {
                    let more = if r.choices.len() > 1 {
                        "  (space: change)"
                    } else {
                        ""
                    };
                    format!("{}, job {}: {}{more}", r.file, r.job, r.place.word())
                }));
                out
            }
            "Workflows" => self
                .plan
                .workflows
                .iter()
                .zip(&a.workflows)
                .map(|(w, on)| format!("Change {}: {}", w.file, yes(*on)))
                .collect(),
            "GitHub" => {
                let mut out = Vec::new();

                if self.plan.app == App::Hosted {
                    out.push(format!("Open the App's install page: {}", yes(a.open_app)));
                }

                if self.plan.ruleset.is_some() {
                    out.push(format!("Protect the default branch: {}", yes(a.ruleset)));
                }

                out
            }
            "Review" => vec!["Apply".into(), "Quit, nothing written".into()],
            _ => Vec::new(),
        }
    }

    /// The jobs where they are now, each with the places init first offered it
    fn rows(&self) -> Vec<Row> {
        let first = self
            .base
            .as_ref()
            .map(|d| rows(d, self.docker))
            .unwrap_or_default();
        let mut now = self
            .plan
            .detected
            .as_ref()
            .map(|d| rows(d, self.docker))
            .unwrap_or_default();

        for r in &mut now {
            if let Some(f) = first
                .iter()
                .find(|f| (&f.file, &f.job) == (&r.file, &r.job))
            {
                r.choices = f.choices.clone();
            }
        }

        now
    }

    /// What the selected line means, or the tab's text when it has no lines
    pub fn detail(&self) -> Vec<String> {
        let a = &self.answers;

        match TABS[self.tab] {
            "Checks" => {
                let offset = usize::from(self.plan.config.is_some());

                match self
                    .cursor
                    .checked_sub(offset)
                    .and_then(|i| self.rows().into_iter().nth(i))
                {
                    Some(r) => vec![r.why],
                    None => match (&self.plan.config, &self.plan.existing) {
                        (Some((_, said)), _) => said.lines().map(String::from).collect(),
                        (None, Some(existing)) => {
                            vec![format!("shipcheck.toml exists, init keeps it: {existing}")]
                        }
                        _ => Vec::new(),
                    },
                }
            }
            "Install" => vec![
                "init always does these, after the review:".into(),
                "  installs the pre-push and post-commit hooks, which never block git".into(),
                "  writes the shipcheck section of AGENTS.md".into(),
                "  starts the daemon, which runs each new PR head".into(),
                "  signs this machine in, so its runs post the checks".into(),
            ],
            "Workflows" => match self.plan.workflows.get(self.cursor) {
                Some(w) => {
                    let mut out = vec![if a.workflows[self.cursor] {
                        w.yes.clone()
                    } else {
                        w.no.clone()
                    }];
                    out.extend(w.diff.lines().map(String::from));
                    out
                }
                None => self.plan.notes.clone(),
            },
            "GitHub" => match &self.plan.ruleset {
                Some(r) => {
                    let mut out = vec!["The required checks on the default branch:".to_string()];
                    out.extend(r.names.iter().map(|n| format!("  {n}")));
                    out
                }
                None => {
                    vec!["No branch rule: init could not read this repository on GitHub".into()]
                }
            },
            _ => self.plan.review(a),
        }
    }

    /// One key press, which never writes anything
    pub fn key(&mut self, code: KeyCode) {
        let tabs = TABS.len();
        let count = self.items().len();

        match code {
            KeyCode::Right | KeyCode::Tab => self.go((self.tab + 1) % tabs),
            KeyCode::Left | KeyCode::BackTab => self.go((self.tab + tabs - 1) % tabs),
            KeyCode::Char(c) if ('1'..='5').contains(&c) => self.go(c as usize - '1' as usize),
            KeyCode::Down if self.cursor + 1 < count => self.cursor += 1,
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char(' ') | KeyCode::Enter => self.choose(),
            KeyCode::Char('q') | KeyCode::Esc => self.end = Some(End::Quit),
            _ => {}
        }
    }

    fn go(&mut self, tab: usize) {
        self.tab = tab;
        self.cursor = 0;
    }

    fn choose(&mut self) {
        let a = &mut self.answers;

        match (TABS[self.tab], self.cursor) {
            ("Checks", 0) if self.plan.config.is_some() => a.config = !a.config,
            ("Checks", i) => {
                let i = i - usize::from(self.plan.config.is_some());

                if let Some(r) = self.rows().into_iter().nth(i) {
                    self.move_job(r);
                }
            }
            ("Workflows", i) => {
                if let Some(on) = a.workflows.get_mut(i) {
                    *on = !*on;
                }
            }
            ("GitHub", i) => {
                let app = self.plan.app == App::Hosted;

                match (app, i) {
                    (true, 0) => a.open_app = !a.open_app,
                    _ => a.ruleset = !a.ruleset,
                }
            }
            ("Review", 0) => self.end = Some(End::Apply),
            ("Review", _) => self.end = Some(End::Quit),
            _ => {}
        }
    }

    /// Moves a job to its next place, and works out the plan again for it
    fn move_job(&mut self, r: Row) {
        let Some(base) = &self.base else {
            return;
        };
        let at = r.choices.iter().position(|p| *p == r.place).unwrap_or(0);
        let to = r.choices[(at + 1) % r.choices.len()];
        let job = (r.file, r.job);
        self.moves.insert(job.clone(), to);
        let d = placed(base, &self.moves);
        let keep = (
            self.answers.config,
            self.answers.open_app,
            self.answers.ruleset,
        );

        match crate::onboard::replan(self.root, &mut self.plan, d) {
            Ok(()) => self.error = None,
            Err(e) => self.error = Some(e),
        }

        // The workflow edits follow the new placement, the other answers stay
        self.answers = self.plan.recommended();
        (
            self.answers.config,
            self.answers.open_app,
            self.answers.ruleset,
        ) = keep;
        self.answers.ruleset &= self.plan.ruleset.is_some();

        // The cursor stays on the job, wherever its line went
        if let Some(i) = self
            .rows()
            .iter()
            .position(|r| (&r.file, &r.job) == (&job.0, &job.1))
        {
            self.cursor = i + usize::from(self.plan.config.is_some());
        }
    }

    pub fn draw(&self, f: &mut Frame) {
        let [top, body, help] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(f.area());
        let tabs = Tabs::new(
            TABS.iter()
                .enumerate()
                .map(|(n, t)| format!("{} {t}", n + 1)),
        )
        .select(self.tab)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" shipcheck init "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED));
        f.render_widget(tabs, top);

        let items = self.items();
        let [list, detail] = if items.is_empty() {
            [ratatui::layout::Rect::default(), body]
        } else {
            let height = (items.len() as u16 + 2).min(body.height / 2).max(3);
            Layout::vertical([Constraint::Length(height), Constraint::Min(3)]).areas(body)
        };

        if !items.is_empty() {
            let mut state = ListState::default().with_selected(Some(self.cursor));
            let list_widget = List::new(items.into_iter().map(ListItem::new))
                .block(Block::default().borders(Borders::ALL))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("> ");
            f.render_stateful_widget(list_widget, list, &mut state);
        }

        let mut text: Vec<Line> = self.detail().into_iter().map(Line::from).collect();

        if let Some(e) = &self.error {
            text.insert(0, Line::from(format!("! {e}")));
        }

        let detail_widget = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        f.render_widget(detail_widget, detail);
        f.render_widget(
            Paragraph::new(
                "←/→ or 1-5: step   ↑/↓: line   space: change   q: quit, nothing written",
            ),
            help,
        );
    }
}

/**
 * Runs the tabs until the person applies or quits, and gives back the plan and answers to apply.
 * Nothing is written here, `apply` writes after it returns, and Ctrl-C quits as `q` does
 **/
pub fn run(root: &Path, plan: Plan) -> std::io::Result<Option<(Plan, Answers)>> {
    let mut w = Wizard::new(root, plan);
    let mut terminal = ratatui::init();
    let result = loop {
        if let Err(e) = terminal.draw(|f| w.draw(f)) {
            break Err(e);
        }

        match event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                    w.end = Some(End::Quit);
                } else {
                    w.key(k.code);
                }
            }
            Ok(_) => {}
            Err(e) => break Err(e),
        }

        match w.end {
            Some(End::Apply) => break Ok(Some((w.plan, w.answers))),
            Some(End::Quit) => break Ok(None),
            None => {}
        }
    };
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboard::Report;
    use std::fs;

    struct Quiet;

    impl Report for Quiet {
        fn start(&mut self, _: &str) {}
        fn done(&mut self, _: bool, _: &str) {}
    }

    const CI: &str = "on: pull_request\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps:\n      - run: make lint\n  heredoc:\n    runs-on: ubuntu-latest\n    steps:\n      - run: |\n          cat <<EOF > a\n          x\n          EOF\n          make test\n  e2e:\n    runs-on: ubuntu-latest\n    steps:\n      - run: docker build .\n";

    /// A wizard on a repository whose jobs init placed on a machine with Docker and colima
    fn wizard(dir: &Path) -> Wizard<'_> {
        fs::create_dir_all(dir.join(".github/workflows")).unwrap();
        fs::write(dir.join(".github/workflows/ci.yml"), CI).unwrap();
        let mut plan = crate::onboard::plan(dir, App::NotPublic, &mut Quiet).unwrap();
        let machine = crate::setup::Machine {
            docker: true,
            colima: true,
            ..Default::default()
        };
        crate::onboard::replan(dir, &mut plan, crate::setup::detect_on(dir, &machine)).unwrap();
        let mut w = Wizard::new(dir, plan);
        w.docker = true;
        w
    }

    fn config(w: &Wizard) -> String {
        w.plan.config.as_ref().unwrap().0.clone()
    }

    fn select(w: &mut Wizard, job: &str) {
        w.go(0);
        let at = w
            .items()
            .iter()
            .position(|i| i.contains(&format!("job {job}:")))
            .unwrap();
        w.cursor = at;
    }

    #[test]
    fn each_job_shows_where_it_runs_and_the_person_moves_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = wizard(dir.path());
        let items = w.items();
        assert_eq!(items[0], "Write shipcheck.toml: yes");
        assert!(
            items.contains(&"ci.yml, job lint: host check  (space: change)".to_string()),
            "{items:?}"
        );
        assert!(
            items.contains(&"ci.yml, job heredoc: act, on Docker  (space: change)".to_string())
        );
        // colima is a choice, never the default
        assert!(items.contains(&"ci.yml, job e2e: GitHub  (space: change)".to_string()));
        assert!(!config(&w).contains("colima"));

        // The host check moves to act, the shipcheck.toml and its gates follow
        select(&mut w, "lint");
        w.key(KeyCode::Char(' '));
        assert!(
            config(&w).contains("[jobs.\"ci.yml/lint\"]\nrunner = \"act\""),
            "{}",
            config(&w)
        );
        assert!(!config(&w).contains("lint = \"make lint\""));
        // Then to GitHub, then back to the check it was
        w.key(KeyCode::Char(' '));
        assert!(!config(&w).contains("ci.yml/lint") && !config(&w).contains("make lint"));
        w.key(KeyCode::Char(' '));
        assert!(config(&w).contains("lint = \"make lint\""));

        // The person opts the Docker job into colima, which reaches this machine's loopback
        select(&mut w, "e2e");
        w.key(KeyCode::Char(' '));
        assert!(
            config(&w).contains("[jobs.\"ci.yml/e2e\"]\nrunner = \"act\"\n# A VM of its own"),
            "{}",
            config(&w)
        );
        assert!(config(&w).contains("engine = \"colima\""));
        assert!(
            w.items()
                .iter()
                .any(|i| i == "ci.yml, job e2e: act, on a colima VM  (space: change)")
        );

        // The workflow edit follows: every job of ci.yml now runs in shipcheck, so all are gated
        w.go(2);
        assert_eq!(w.items(), ["Change ci.yml: yes"]);
        assert_eq!(
            w.plan.workflows[0]
                .diff
                .matches("github.event.pull_request.head.repo.fork")
                .count(),
            3
        );
    }

    #[test]
    fn the_steps_go_back_and_forth_and_nothing_is_written_before_apply() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = wizard(dir.path());

        for (key, tab) in [
            (KeyCode::Right, 1),
            (KeyCode::Right, 2),
            (KeyCode::Left, 1),
            (KeyCode::Char('5'), 4),
            (KeyCode::Tab, 0),
            (KeyCode::BackTab, 4),
            (KeyCode::Char('3'), 2),
        ] {
            w.key(key);
            assert_eq!(w.tab, tab, "{key:?}");
        }

        // A choice changed and changed back, on any step
        w.key(KeyCode::Char(' '));
        assert_eq!(w.answers.workflows, [false]);
        w.key(KeyCode::Char('1'));
        w.key(KeyCode::Char(' '));
        assert!(!w.answers.config);
        w.key(KeyCode::Char(' '));
        w.key(KeyCode::Char('3'));
        w.key(KeyCode::Char(' '));
        assert_eq!(w.answers, w.plan.recommended());

        w.key(KeyCode::Char('q'));
        assert_eq!(w.end, Some(End::Quit));
        let written: Vec<_> = ["shipcheck.toml", "AGENTS.md", ".git"]
            .iter()
            .filter(|f| dir.path().join(f).exists())
            .collect();
        assert!(written.is_empty(), "{written:?}");
        assert_eq!(
            fs::read_to_string(dir.path().join(".github/workflows/ci.yml")).unwrap(),
            CI
        );

        // Apply is on the review, the last step
        w.end = None;
        w.key(KeyCode::Char('5'));
        w.key(KeyCode::Enter);
        assert_eq!(w.end, Some(End::Apply));
    }

    #[test]
    fn every_step_draws_in_a_small_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = wizard(dir.path());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();

        for tab in 0..TABS.len() {
            w.go(tab);
            terminal.draw(|f| w.draw(f)).unwrap();
            let screen: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(
                screen.contains("1 Checks") && screen.contains("5 Review"),
                "{tab}"
            );
            let want = [
                "job heredoc",
                "pre-push",
                "Change ci.yml",
                "branch rule",
                "Apply",
            ][tab];
            assert!(screen.contains(want), "{tab}: {want}\n{screen}");
        }
    }
}
