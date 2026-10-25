use std::io::IsTerminal;

use clap::{Parser, Subcommand};
use std::path::Path;
use std::process::{Command, ExitCode};

use shipcheck::config::Trigger;
use shipcheck::record::Run as Record;
use shipcheck::service::{self, Poster, Sender};
use shipcheck::ui::Style;
use shipcheck::{agents, autostart, background, check, report, run, setup};

const PARTIAL: &str = "partial run: not the check, run `shipcheck run` before committing";

fn main() -> ExitCode {
    let existing = std::env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);

    for (key, value) in run::safe_git_env(existing) {
        // Safe: no other thread exists yet
        unsafe { std::env::set_var(key, value) };
    }

    // git, bwrap and act come only from where a check finds tools, never a fake on the caller's PATH.
    // `shipcheck commit` gives the user's own git hooks the caller's PATH
    if let Some(path) = std::env::var_os("PATH") {
        let _ = run::CALLER_PATH.set(path);
    }

    // Safe: no other thread exists yet
    unsafe { std::env::set_var("PATH", shipcheck::runner::host::jail_path()) };

    run::migrate_home();

    match cli(Cli::parse().command) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{} {e}", Style::stderr().red("shipcheck:"));
            ExitCode::from(2)
        }
    }
}

/// Run your CI checks locally and post them as the PR's required checks
#[derive(Parser)]
#[command(
    name = "shipcheck",
    version,
    arg_required_else_help = true,
    after_help = "Docs: https://shipcheck.run/docs/"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Set up this repository: shipcheck.toml, agent instructions, hooks
    Init {
        /// Take the recommended choices without asking
        #[arg(long)]
        yes: bool,
    },
    /// Install the hooks and the daemon in another clone
    Install,
    /// Run the checks on the working copy and wait for the verdict
    Run {
        /// Test this commit instead of the working copy
        #[arg(long, value_name = "rev", conflicts_with = "staged")]
        sha: Option<String>,
        /// Test the index instead of the working copy
        #[arg(long)]
        staged: bool,
        /// Run against the local copy of the remote's default branch, without fetching it
        #[arg(long)]
        no_fetch: bool,
        /// Read shipcheck.toml and the workflows from this ref, for a trial
        #[arg(long, value_name = "ref")]
        base: Option<String>,
        /// Run only this job and the jobs it needs, a partial run
        #[arg(long = "job", value_name = "id")]
        jobs: Vec<String>,
        /// Run only the jobs of this workflow, a partial run
        #[arg(long = "workflow", value_name = "file")]
        workflows: Vec<String>,
        /// Print the result as JSON on stdout
        #[arg(long)]
        json: bool,
        /// Set by the hooks and the daemon on a run that posts its checks
        #[arg(long, hide = true, value_name = "name")]
        background: Option<String>,
        /// Set with `--background` when the daemon runs from a moved SHIPCHECK_HOME
        #[arg(long, hide = true)]
        home_ok: bool,
        #[arg(hide = true)]
        rev: Option<String>,
    },
    /// Commit exactly the tree the last run tested
    Commit {
        /// The commit message, as git takes it
        #[arg(short = 'm', value_name = "message", required_unless_present = "file")]
        message: Vec<String>,
        /// Read the commit message from a file
        #[arg(short = 'F', value_name = "file")]
        file: Option<String>,
        /// Commit a tree whose run failed
        #[arg(long)]
        allow_failed: bool,
        /// Skip git's commit hooks
        #[arg(long)]
        no_verify: bool,
    },
    /// Print the stored result of a tree
    Status {
        /// Accepted for scripts written for older versions, these commands never fetch
        #[arg(long, hide = true)]
        no_fetch: bool,
        /// A commit instead of the working copy
        rev: Option<String>,
        /// The index instead of the working copy
        #[arg(long, conflicts_with = "rev")]
        staged: bool,
        /// Print the result as JSON on stdout
        #[arg(long)]
        json: bool,
        /// The ref the run read shipcheck.toml from
        #[arg(long, value_name = "ref")]
        base: Option<String>,
    },
    /// Print the logs of a tree's last run
    Logs {
        /// Accepted for scripts written for older versions, these commands never fetch
        #[arg(long, hide = true)]
        no_fetch: bool,
        /// A commit instead of the working copy
        rev: Option<String>,
        /// The index instead of the working copy
        #[arg(long, conflicts_with = "rev")]
        staged: bool,
        /// The ref the run read shipcheck.toml from
        #[arg(long, value_name = "ref")]
        base: Option<String>,
    },
    /// Post a commit's result as its PR's checks
    Post {
        /// Accepted for scripts written for older versions, these commands never fetch
        #[arg(long, hide = true)]
        no_fetch: bool,
        /// The commit to post, HEAD by default
        rev: Option<String>,
        /// Post from a SHIPCHECK_HOME other than the default
        #[arg(long)]
        home_ok: bool,
        #[arg(long, hide = true, value_name = "ref")]
        base: Option<String>,
    },
    /// List the last runs of this repository on this machine
    History {
        /// Print the runs as JSON on stdout
        #[arg(long)]
        json: bool,
        /// How many runs to list
        #[arg(long, default_value_t = 20, value_name = "n")]
        limit: usize,
    },
    /// Check this machine and repository, and say what to fix
    Doctor {
        /// Accepted for scripts written for older versions, these commands never fetch
        #[arg(long, hide = true)]
        no_fetch: bool,
        #[arg(long, hide = true, value_name = "ref")]
        base: Option<String>,
    },
    /// Sign this machine in, so it can post checks
    Login,
    /// Revoke this machine's token
    Logout {
        /// Revoke every token of your GitHub user, on every machine
        #[arg(long)]
        all: bool,
    },
    /// Make a token for another machine
    Token {
        /// A name for the machine, shown when it posts
        #[arg(long, default_value = "token", value_name = "machine")]
        name: String,
    },
    /// Remove what shipcheck keeps for this repository
    Clean {
        /// For every repository, and the stored results, logs and daemon too
        #[arg(long)]
        all: bool,
    },
    /// Watch the open PRs and run each new head, as a service
    Daemon {
        /// Run from a SHIPCHECK_HOME other than the default
        #[arg(long)]
        home_ok: bool,
        /// Stop the service and remove it
        #[arg(long)]
        uninstall: bool,
    },
    /// Called by the post-commit hook
    #[command(hide = true)]
    OnCommit,
    /// Called by the pre-push hook with the pushed ref and commit
    #[command(hide = true)]
    OnPush {
        refname: Option<String>,
        sha: Option<String>,
    },
}

fn cli(command: Cmd) -> Result<ExitCode, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut rev = "HEAD".to_string();
    let mut fetch = true;
    let mut base = String::new();
    let mut base_set = false;
    let mut all = false;
    let mut explicit = false;
    let mut staged = false;
    let mut home_ok = false;
    let mut yes = false;
    let (mut jobs, mut workflows) = (Vec::new(), Vec::new());
    let mut json = false;
    let mut limit = 20;
    let mut uninstall = false;
    // Set by the hooks and the daemon on a background run that posts its checks
    let mut posts: Option<String> = None;
    let (mut refname, mut pushed) = (String::new(), None);
    let on_base = |b: Option<String>, base: &mut String, base_set: &mut bool, fetch: &mut bool| {
        // For trials: read shipcheck.toml and the workflows from another ref than the default base
        if let Some(b) = b {
            *base = b;
            *base_set = true;
            *fetch = false;
        }
    };
    let at = |r: Option<String>, rev: &mut String, explicit: &mut bool| {
        if let Some(r) = r {
            *rev = r;
            *explicit = true;
        }
    };

    let name = match command {
        Cmd::Commit {
            message,
            file,
            allow_failed,
            no_verify,
        } => {
            let mut args = Vec::new();

            for m in message {
                args.extend(["-m".to_string(), m]);
            }

            if let Some(f) = file {
                args.extend(["-F".to_string(), f]);
            }

            if no_verify {
                args.push("--no-verify".into());
            }

            return commit(&cwd, &args, allow_failed);
        }
        // Only the token goes to stdout, so a script can capture it
        Cmd::Token { name } => {
            println!("{}", service::new_token(&name)?);
            eprintln!(
                "set it as SHIPCHECK_TOKEN on the other machine, `shipcheck logout` there or `shipcheck logout --all` anywhere revokes it"
            );
            return Ok(ExitCode::SUCCESS);
        }
        Cmd::Init { yes: y } => {
            yes = y;
            "init"
        }
        Cmd::Install => "install",
        Cmd::Run {
            sha,
            staged: st,
            no_fetch,
            base: b,
            jobs: j,
            workflows: w,
            json: js,
            background,
            home_ok: h,
            rev: r,
        } => {
            at(sha.or(r), &mut rev, &mut explicit);
            home_ok = h;
            staged = st;
            fetch = !no_fetch;
            on_base(b, &mut base, &mut base_set, &mut fetch);
            (jobs, workflows, json, posts) = (j, w, js, background);
            "run"
        }
        Cmd::Status {
            rev: r,
            staged: st,
            json: js,
            base: b,
            ..
        } => {
            at(r, &mut rev, &mut explicit);
            (staged, json) = (st, js);
            on_base(b, &mut base, &mut base_set, &mut fetch);
            "status"
        }
        Cmd::Logs {
            rev: r,
            staged: st,
            base: b,
            ..
        } => {
            at(r, &mut rev, &mut explicit);
            staged = st;
            on_base(b, &mut base, &mut base_set, &mut fetch);
            "logs"
        }
        Cmd::Post {
            rev: r,
            home_ok: h,
            base: b,
            ..
        } => {
            at(r, &mut rev, &mut explicit);
            home_ok = h;
            on_base(b, &mut base, &mut base_set, &mut fetch);
            "post"
        }
        Cmd::History { json: js, limit: l } => {
            (json, limit) = (js, l);
            "history"
        }
        Cmd::Doctor { base: b, .. } => {
            on_base(b, &mut base, &mut base_set, &mut fetch);
            "doctor"
        }
        Cmd::Login => "login",
        Cmd::Logout { all: a } => {
            all = a;
            "logout"
        }
        Cmd::Clean { all: a } => {
            all = a;
            "clean"
        }
        Cmd::Daemon {
            home_ok: h,
            uninstall: u,
        } => {
            (home_ok, uninstall) = (h, u);
            "daemon"
        }
        Cmd::OnCommit => "on-commit",
        Cmd::OnPush { refname: r, sha } => {
            (refname, pushed) = (r.unwrap_or_default(), sha);
            "on-push"
        }
    };

    let reads_base = matches!(
        name,
        "run" | "status" | "logs" | "post" | "doctor" | "on-commit" | "on-push"
    );

    if !base_set && reads_base {
        base = shipcheck::daemon::command_base(name, &cwd);
    }

    // With --json, stdout holds only the result object
    let style = if json {
        Style::stderr()
    } else {
        Style::stdout()
    };
    let say = |line: &str| {
        let line = style.status(line);

        if json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };

    match name {
        "run" => {
            if fetch && !background::fetch_base(&cwd, &base) {
                return Err(format!(
                    "git fetch of {base} failed, use --no-fetch to run against the local {base}"
                ));
            }

            // A run that posts reads the config from GitHub's base branch, not one the clone was told
            if posts.is_some() && !background::fetch_base(&cwd, &base) {
                say(&format!("could not fetch {base}, so nothing is posted"));
                posts = None;
            }

            if posts.is_some() && !shipcheck::daemon::posts_from(&base, &cwd) {
                say(&format!(
                    "{base} is not the base this repository posts from, so nothing is posted"
                ));
                posts = None;
            }

            let started = std::time::Instant::now();
            let mut record = Record::new(
                &std::env::var("SHIPCHECK_TRIGGER").unwrap_or_else(|_| "cli".into()),
                run::unix_now(),
            );
            record.branch = std::env::var("SHIPCHECK_BRANCH")
                .ok()
                .or_else(|| Some(current_branch(&cwd)))
                .filter(|b| b != "detached");
            let (tested, snapshot) = subject(&cwd, &rev, explicit, staged)?;
            let plan = run::plan(&cwd, &tested, &base)?;
            let what = snapshot.map_or_else(|| plan.sha[..12].to_string(), String::from);
            let tree = &plan.tree[..12];

            if plan.defaults {
                say(&format!(
                    "no shipcheck.toml on {base}, running every workflow that runs for branches, on pull_request or push: {}. This result is not a PR check, `shipcheck init` and merge shipcheck.toml to main for that",
                    plan.config.workflows.join(", ")
                ));
            }

            if snapshot.is_some() && !staged {
                warn_untracked(&plan.repo)?;
            }

            if !jobs.is_empty() || !workflows.is_empty() {
                let _key = run::lock_key(&plan.key)?;
                let entry = run::run_partial(&plan, &jobs, &workflows, &mut |line| {
                    say(&run::printable(line))
                })?;
                say(&format!(
                    "tested {what}, tree {tree}: ran in {:.1?}",
                    started.elapsed()
                ));
                let code = report(&entry, snapshot, json, true);
                record.partial = true;
                keep_record(&plan, &entry, &record, &run::partial_log_dir(&plan.key));
                say(PARTIAL);
                return Ok(code);
            }

            if posts.is_some() && !background::home_allows_post(run::home_overridden(), home_ok) {
                say(&format!(
                    "{}: not posted, SHIPCHECK_HOME is not the default, so its results may not come from runs. A person who set it can pass --home-ok",
                    &plan.sha[..12]
                ));

                if let Ok(repo) = run::repo_id(&plan.repo) {
                    let fields = serde_json::json!({"sha": plan.sha, "reason": "home_moved"});
                    shipcheck::record::keep(
                        &repo,
                        &shipcheck::record::event(&repo, "skipped", fields),
                    );
                }

                posts = None;
            }

            let sender = posts.as_ref().filter(|_| !plan.defaults).and_then(|_| {
                run::repo_id(&plan.repo)
                    .and_then(|repo| Sender::new(&repo))
                    .map_err(|e| say(&format!("{}: not posted, {e}", &plan.sha[..12])))
                    .ok()
            });

            if let (Some(sender), Some(name)) = (&sender, &posts) {
                let posted = posting_run(&plan, sender, name, &mut record, &mut |line| say(line))?;

                if let Some(entry) = &posted {
                    record.posted_checks = true;
                    record.posted_comment =
                        shipcheck::daemon::comment(sender, None, &plan, &base, entry);
                    keep_record(&plan, entry, &record, &run::log_dir(&plan.key));
                }

                let Some(entry) = posted else {
                    say(&format!(
                        "{}: a newer commit replaced this run, nothing posted",
                        &plan.sha[..12]
                    ));
                    return Ok(ExitCode::SUCCESS);
                };
                say(&format!(
                    "tested {what}, tree {tree}: posted in {:.1?}",
                    started.elapsed()
                ));
                return Ok(report(&entry, snapshot, json, false));
            }

            let _key = run::lock_key(&plan.key)?;
            // Only a pass is reused, a cached failure runs again so a flaky one cannot stick
            let entry = match run::lookup(&plan).filter(|e| e.passed) {
                Some(entry) => {
                    record.cache_hit = true;
                    say(&format!(
                        "tested {what}, tree {tree}: cached result in {:.1?}",
                        started.elapsed()
                    ));
                    entry
                }
                None => {
                    let entry = run::run(&plan, &mut |line| say(&run::printable(line)))?;
                    say(&format!(
                        "tested {what}, tree {tree}: ran in {:.1?}",
                        started.elapsed()
                    ));
                    entry
                }
            };
            let code = report(&entry, snapshot, json, false);
            keep_record(&plan, &entry, &record, &run::log_dir(&plan.key));

            if !explicit {
                run::record_tested(
                    &plan.repo,
                    &run::Tested {
                        tree: plan.tree.clone(),
                        staged,
                        passed: entry.passed,
                    },
                )?;
            }

            if snapshot.is_some() {
                let git = if staged {
                    "git commit"
                } else {
                    "git add -A && git commit"
                };
                say(&format!(
                    "commit exactly what ran with `shipcheck commit -m <message>` (or `{git}`) to reuse this result"
                ));
            }

            Ok(code)
        }
        "doctor" => {
            let checks = shipcheck::doctor::checks(&cwd, &base);
            let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
            // Agents and pipes read the plain words, a person at a terminal gets marks and color
            let s = Style::stdout();
            let (ok, fix) = if s.on {
                (s.ok(), s.fail())
            } else {
                ("ok ".into(), "fix".into())
            };

            for c in &checks {
                let mark = if c.ok { &ok } else { &fix };
                println!("{mark}  {:width$}  {}", c.name, s.dim(&c.detail));
            }

            if checks.iter().all(|c| c.ok) {
                println!("\nready: `shipcheck run` runs this commit's CI");
                Ok(ExitCode::SUCCESS)
            } else {
                println!(
                    "\nnot ready: fix the lines marked {fix}, then run `shipcheck doctor` again"
                );
                Ok(ExitCode::from(1))
            }
        }
        "status" => {
            // Before a commit, whether committing all of it or the index as it is reuses a result
            if !explicit {
                for (label, staged) in [("working copy", false), ("index", true)] {
                    let (tested, _) = subject(&cwd, &rev, false, staged)?;
                    let mut plan = run::plan(&cwd, &tested, &base)?;
                    last_run_key(&mut plan, base_set, |k| run::cached(k).is_some());
                    let verdict = match run::lookup(&plan) {
                        Some(e) if e.passed => "passed",
                        Some(_) => "failed",
                        None => "no result",
                    };
                    say(&format!("{label}, tree {}: {verdict}", &plan.tree[..12]));
                }
            }

            let (tested, snapshot) = subject(&cwd, &rev, explicit, staged)?;
            let mut plan = run::plan(&cwd, &tested, &base)?;

            if let Some(other) = last_run_key(&mut plan, base_set, |k| run::cached(k).is_some()) {
                say(&format!(
                    "the last run of this tree read shipcheck.toml from {other}, not {base}"
                ));
            }

            match run::lookup(&plan) {
                Some(entry) => Ok(report(&entry, snapshot, json, false)),
                // With --json, stdout stays JSON an agent can parse, even with nothing to report
                None if json => {
                    println!("{}", serde_json::json!({"tree": plan.tree, "result": null}));
                    Ok(ExitCode::from(2))
                }
                None => {
                    println!("no result for tree {}", plan.tree);
                    Ok(ExitCode::from(2))
                }
            }
        }
        "history" => {
            let repo = run::repo_id(&toplevel(&cwd)?)?;
            let runs = shipcheck::record::runs(&run::home(), &repo, limit);

            if json {
                println!("{}", serde_json::Value::from(runs));
            } else if runs.is_empty() {
                println!("no runs recorded for {repo} on this machine yet");
            } else {
                print!("{}", shipcheck::record::table(&runs));
            }

            Ok(ExitCode::SUCCESS)
        }
        "logs" => {
            let (tested, _) = subject(&cwd, &rev, explicit, staged)?;
            let mut plan = run::plan(&cwd, &tested, &base)?;

            if let Some(other) = last_run_key(&mut plan, base_set, |k| {
                run::log_dir(k).exists() || run::partial_log_dir(k).exists()
            }) {
                println!("the last run of this tree read shipcheck.toml from {other}, not {base}");
            }

            let (full, partial) = (run::log_dir(&plan.key), run::partial_log_dir(&plan.key));

            if full.exists() || !partial.exists() {
                print_logs(&full)?;
            }

            if partial.exists() {
                println!("==> the last partial run, {PARTIAL} <==");
                print_logs(&partial)?;
            }

            Ok(ExitCode::SUCCESS)
        }
        "post" => {
            refuse_moved_home(home_ok)?;

            if !shipcheck::daemon::posts_from(&base, &cwd) {
                return Err(format!(
                    "{base} is not the base this repository posts from: its default branch, or the daemon's base for it. A result from another base is a trial, never a check"
                ));
            }

            let plan = run::plan(&cwd, &rev, &base)?;

            if let Some(branch) = run::on_default_branch(&plan.repo, &base, Some(&plan.sha)) {
                return Err(format!(
                    "{branch} is the default branch, PRs carry shipcheck's checks: `git switch -c <branch>`, push it, then `shipcheck post`"
                ));
            }

            if plan.defaults {
                return Err(format!(
                    "{base} has no shipcheck.toml, so this result ran the defaults and is not a check. Run `shipcheck init` and merge shipcheck.toml to main"
                ));
            }

            let entry = run::lookup(&plan).ok_or_else(|| {
                format!(
                    "no result for tree {}, run `shipcheck run` first",
                    plan.tree
                )
            })?;
            // Secrets that cannot be read stop the post, so no log goes out unmasked
            run::secret_values(&run::repo_id(&plan.repo)?)?;
            let sender = Sender::new(&run::repo_id(&plan.repo)?)?;
            // Job checks go first, so a failed post leaves `shipcheck` unposted
            let bodies = check::final_bodies(&plan, &entry, true);
            let url =
                background::finish(&|b, id| sender.send(b, id), &bodies, &Default::default())?;

            println!(
                "posted {} on {}: {url}",
                if entry.passed { "success" } else { "failure" },
                &plan.sha[..12]
            );

            let commented = shipcheck::daemon::comment(&sender, None, &plan, &base, &entry);
            let repo = run::repo_id(&plan.repo)?;
            let fields = serde_json::json!({
                "sha": plan.sha,
                "tree": plan.tree,
                "passed": entry.passed,
                "checks": true,
                "comment": commented,
            });
            shipcheck::record::keep(&repo, &shipcheck::record::event(&repo, "posted", fields));
            Ok(ExitCode::SUCCESS)
        }
        "login" => {
            let login = service::login()?;
            let s = Style::stdout();
            let mark = if s.on {
                format!("{} ", s.ok())
            } else {
                String::new()
            };
            println!(
                "{mark}signed in as {}, `shipcheck post` now posts through {}",
                s.bold(&login),
                service::api()
            );
            Ok(ExitCode::SUCCESS)
        }
        "logout" => {
            println!("{}", service::logout(all)?);
            Ok(ExitCode::SUCCESS)
        }
        // Called by the hooks, so they never fail and never make a commit or a push wait
        "on-commit" => {
            let branch = current_branch(&cwd);

            if let Ok(plan) = run::plan(&cwd, "HEAD", &base)
                && plan.config.runs_on(Trigger::Commit)
                && !run::skip_requested(&plan)
            {
                background::start(&cwd, &plan, &base, &branch, false, "hook-commit", false);
            }

            Ok(ExitCode::SUCCESS)
        }
        "on-push" => {
            let branch = refname.trim_start_matches("refs/heads/").to_string();

            if let Some(sha) = &pushed
                && let Ok(plan) = run::plan(&cwd, sha, &base)
                && plan.config.runs_on(Trigger::Push)
                && !run::skip_requested(&plan)
            {
                // The pushed commit's checks, posted by its run, unless the result could be an agent's
                let post =
                    background::posts(plan.defaults, run::home_overridden(), service::can_post());

                if !plan.defaults && !post {
                    background::note(&format!(
                        "{} on {branch}: checks not posted, run `shipcheck login`, or set SHIPCHECK_APP_ID and put the key of an App of your own at app.pem in shipcheck's home",
                        &plan.sha[..12]
                    ));
                }

                background::start(&cwd, &plan, &base, &branch, post, "hook-push", false);
            }

            Ok(ExitCode::SUCCESS)
        }
        "clean" => {
            let repo = if all { None } else { Some(toplevel(&cwd)?) };

            for line in run::clean(repo.as_deref(), all)? {
                println!("{line}");
            }

            if all {
                println!("{}", autostart::uninstall()?);
            }

            Ok(ExitCode::SUCCESS)
        }
        "daemon" => {
            if uninstall {
                println!("{}", autostart::uninstall()?);
                return Ok(ExitCode::SUCCESS);
            }

            refuse_moved_home(home_ok)?;
            let config = shipcheck::daemon::DaemonConfig::load()?;
            println!(
                "shipcheck daemon: watching {} repositories every {}s",
                config.repos.len(),
                config.interval_seconds
            );
            shipcheck::daemon::serve(home_ok)
        }
        "init" => {
            let root = toplevel(&cwd)?;
            let terminal = std::io::stdin().is_terminal()
                && std::io::stdout().is_terminal()
                && std::io::stderr().is_terminal();
            let finish = &mut |say: &mut dyn FnMut(&str)| machine_setup(&root, say);

            // A person gets the wizard, `--yes` the recommended choices, an agent or a script no change outside the repository
            if yes {
                shipcheck::onboard::plain(
                    &root,
                    shipcheck::onboard::App::find(),
                    &mut std::io::stdout(),
                    finish,
                )?;
                return Ok(ExitCode::SUCCESS);
            }

            if terminal {
                shipcheck::onboard::tui(&root, shipcheck::onboard::App::find, finish)?;
                return Ok(ExitCode::SUCCESS);
            }

            if let Some(wrote) = shipcheck::onboard::config_only(&root, &mut std::io::stdout())? {
                println!("{wrote}");
            }

            // Instructions, hooks and the daemon for a repository with nothing to run would only mislead
            if !root.join("shipcheck.toml").exists() {
                return Ok(ExitCode::SUCCESS);
            }

            machine_setup(&root, &mut |line| println!("{line}"))?;
            println!(
                "next: review shipcheck.toml and AGENTS.md, commit them, and merge to {}",
                shipcheck::daemon::known_base(&root)
                    .unwrap_or_else(|| "origin/main".into())
                    .trim_start_matches("origin/")
            );
            Ok(ExitCode::SUCCESS)
        }
        "install" => {
            let root = toplevel(&cwd)?;
            remove_old_hooks(&root, &mut |line| println!("{line}"));
            println!("{}", setup::install(&root)?);
            sign_in(&mut |line| println!("{line}"));
            println!("{}", autostart::install(&root));
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("unknown command {other}")),
    }
}

/// What `init` sets up on the machine once the repository has a shipcheck.toml
fn machine_setup(root: &Path, say: &mut dyn FnMut(&str)) -> Result<(), String> {
    for line in agents::instructions(root)? {
        say(&line);
    }

    remove_old_hooks(root, say);
    say(&setup::install(root)?);
    sign_in(say);
    say(&autostart::install(root));
    Ok(())
}

/// Signs the machine in for `install` and `init` when it cannot post, never failing them
fn sign_in(say: &mut dyn FnMut(&str)) {
    let terminal = std::io::stdin().is_terminal();

    match service::sign_in(service::poster() != Poster::None, terminal) {
        service::SignIn::Already => {
            say(&format!(
                "signed in: checks post with {}",
                service::identity().unwrap_or_default()
            ));
        }
        service::SignIn::Skip => {
            say("not signed in: run `shipcheck login` once, or set SHIPCHECK_TOKEN (see docs)");
        }
        service::SignIn::Ask => {
            say("signing in with GitHub, so pushes post their checks");

            match service::login() {
                Ok(login) => say(&format!("signed in as {login}")),
                Err(e) => say(&format!("not signed in: {e}, run `shipcheck login` later")),
            }
        }
    }
}

fn remove_old_hooks(root: &Path, say: &mut dyn FnMut(&str)) {
    for line in agents::remove_old_hooks(
        root,
        &[run::home(), run::default_home(), run::legacy_home()],
    ) {
        say(&line);
    }
}

/// An agent can point SHIPCHECK_HOME at a folder it wrote, so only a person passes `--home-ok`
fn refuse_moved_home(home_ok: bool) -> Result<(), String> {
    if run::home_overridden() && !home_ok {
        return Err(format!(
            "SHIPCHECK_HOME is {}, not the default {}, so its results may not come from runs. A person who set it can pass --home-ok",
            run::home().display(),
            run::default_home().display()
        ));
    }

    Ok(())
}

/// What `run`, `status` and `logs` test with no revision: the working copy, or the index with
/// `--staged`, as a commit no ref names, else HEAD when it has no change. Named when not a commit
fn subject(
    cwd: &Path,
    rev: &str,
    explicit: bool,
    staged: bool,
) -> Result<(String, Option<&'static str>), String> {
    if explicit {
        return Ok((rev.to_string(), None));
    }

    let what = if staged {
        "the index"
    } else {
        "the working copy"
    };

    Ok(match run::snapshot(cwd, staged)? {
        Some(sha) => (sha, Some(what)),
        None => (rev.to_string(), None),
    })
}

/// Without `--base`, a tree whose key has nothing to show takes the key of its last recorded run
fn last_run_key(
    plan: &mut run::Plan,
    base_set: bool,
    has: impl Fn(&str) -> bool,
) -> Option<String> {
    if base_set || has(&plan.key) {
        return None;
    }

    let repo = run::repo_id(&plan.repo).ok()?;
    let (key, base) = run::last_run(&run::home(), &repo, &plan.tree, has)?;
    plan.key = key;
    Some(base[..base.len().min(12)].to_string())
}

/// Untracked files change the tree, so a stray one makes the commit miss the result
fn warn_untracked(repo: &Path) -> Result<(), String> {
    let files = run::untracked(repo)?;

    if files.is_empty() {
        return Ok(());
    }

    let mut names = files
        .iter()
        .take(20)
        .map(|f| run::printable(f))
        .collect::<Vec<_>>()
        .join(", ");

    if files.len() > 20 {
        names.push_str(&format!(" and {} more", files.len() - 20));
    }

    let count = match files.len() {
        1 => "1 untracked file is".to_string(),
        n => format!("{n} untracked files are"),
    };
    println!(
        "{}",
        Style::stdout().yellow(&format!(
            "{count} part of this run: {names}. Gitignore or delete them if they are not part of the change"
        ))
    );
    Ok(())
}

/// Commits the tree the last run tested, `git_args` being the message and `--no-verify` for git
fn commit(cwd: &Path, git_args: &[String], allow_failed: bool) -> Result<ExitCode, String> {
    let repo = toplevel(cwd)?;

    // Plain `git commit` still works on it, the hooks never block git
    if let Some(branch) = run::on_default_branch(
        &repo,
        &shipcheck::daemon::command_base("commit", &repo),
        None,
    ) {
        return Err(format!(
            "{branch} is the default branch, PRs carry shipcheck's checks: `git switch -c <branch>`, then `shipcheck commit`"
        ));
    }

    let tested = run::last_tested(&repo).ok_or("no `shipcheck run` of this working copy yet")?;
    let sha = run::commit(cwd, &tested, git_args, allow_failed)?;
    println!(
        "committed {}, the tree that {} in `shipcheck run`",
        &sha[..12],
        if tested.passed { "passed" } else { "failed" }
    );
    Ok(ExitCode::SUCCESS)
}

fn current_branch(cwd: &Path) -> String {
    Command::new("git")
        .args(["symbolic-ref", "--short", "-q", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "detached".into())
}

/// A background run that posts its checks under record `name`, None when a newer commit replaced it
fn posting_run(
    plan: &run::Plan,
    sender: &Sender,
    name: &str,
    rec: &mut Record,
    say: &mut dyn FnMut(&str),
) -> Result<Option<run::Entry>, String> {
    use std::sync::atomic::{AtomicU64, Ordering};

    let record = background::record(&run::repo_id(&plan.repo)?, name);
    let pid = std::process::id();
    let first = AtomicU64::new(0);
    let cached = run::lookup(plan).filter(|e| e.passed);
    let mut hit = cached.is_some();
    let send = |body: &serde_json::Value, id| {
        let sent = sender.send(body, id)?;

        if body["status"] == "completed" {
            let _ =
                first.compare_exchange(0, run::unix_now(), Ordering::Relaxed, Ordering::Relaxed);
        }

        Ok(sent)
    };
    let posted = background::post_run(
        &send,
        &|| sender.find_checks(&plan.sha),
        &plan.sha,
        &check::names(plan),
        cached,
        || {
            let _key = run::lock_key(&plan.key)?;

            match run::lookup(plan).filter(|e| e.passed) {
                Some(entry) => {
                    hit = true;
                    Ok(entry)
                }
                None => {
                    let mut entry = run::run(plan, &mut |line| say(&run::printable(line)))?;

                    // A fresh pass is checked like a stored one before it is posted, and fails closed
                    if entry.passed && !run::holds(plan, &entry) {
                        say(
                            "the run's tree, base or logs do not match its result, posting a failure",
                        );
                        run::fail_closed(&mut entry);
                    }

                    Ok(entry)
                }
            }
        },
        |entry| check::final_bodies(plan, entry, true),
        || background::current(&record, pid),
        &background::WAITS,
    )?;
    rec.cache_hit = hit;
    rec.first_check = Some(first.load(Ordering::Relaxed)).filter(|t| *t > 0);
    Ok(posted)
}

/// Appends the run to this machine's records, read by `shipcheck history`. A record never fails a run
fn keep_record(plan: &run::Plan, entry: &run::Entry, record: &Record, logs: &Path) {
    let Ok(repo) = run::repo_id(&plan.repo) else {
        return;
    };
    let secrets = run::secret_values(&repo).unwrap_or_default();
    let line = shipcheck::record::line(&repo, plan, entry, record, logs, &secrets);
    shipcheck::record::keep(&repo, &line);
}

fn toplevel(cwd: &Path) -> Result<std::path::PathBuf, String> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .map_err(|e| e.to_string())?;

    if !out.status.success() {
        return Err("not inside a git repository".into());
    }

    Ok(String::from_utf8_lossy(&out.stdout).trim().into())
}

/// `snapshot` names the working copy or the index when the run tested one instead of a commit
fn report(entry: &run::Entry, snapshot: Option<&str>, json: bool, partial: bool) -> ExitCode {
    let logs = if partial {
        run::partial_log_dir(&entry.key)
    } else {
        run::log_dir(&entry.key)
    };
    let workflows = report::workflows(entry, &logs);
    let tested = snapshot.map_or(entry.sha.as_str(), |w| w.trim_start_matches("the "));

    if json {
        println!("{}", report::json(entry, tested, &workflows, partial));
    } else {
        let s = Style::stdout();

        for line in report::text(entry, tested, &workflows).lines() {
            println!("{}", s.status(line));
        }
    }

    ExitCode::from(run::exit_code(entry))
}

fn print_logs(dir: &Path) -> Result<(), String> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map_err(|_| "no logs for this tree, run `shipcheck run` first".to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        // Each workflow's log, the per-job logs it joins sit in a directory beside it
        .filter(|p| p.is_file())
        .collect();
    files.sort();

    for f in files {
        let log = std::fs::read(&f).map_err(|e| e.to_string())?;
        println!("==> {} <==", f.display());
        print!("{}", run::printable(&String::from_utf8_lossy(&log)));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parses(call: &[&str]) -> bool {
        Cli::try_parse_from(std::iter::once("shipcheck").chain(call.iter().copied())).is_ok()
    }

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    /// Every way shipcheck is called by its own hooks, its own background runs, its tests and its
    /// docs must keep parsing, since clap refuses a flag a command does not declare
    #[test]
    fn every_known_call_parses() {
        let sha = "0".repeat(40);
        let sha = sha.as_str();
        let calls: &[&[&str]] = &[
            // The hooks, setup.rs
            &["on-push", "refs/heads/feature", sha],
            &["on-commit"],
            // The end-to-end tests
            &["run", "--no-fetch"],
            &["run", "--no-fetch", "cheat"],
            &["status", "--no-fetch"],
            &["status", sha],
            &["status", "feature"],
            &["logs", "main"],
            &["post"],
            &["post", "--home-ok"],
            &["doctor"],
            &["clean", "--all"],
            // SKILL.md, the README and the site's docs
            &["run", "--staged"],
            &["run", "--sha", "HEAD"],
            &["run", "--json"],
            &["run", "--job", "test", "--job", "lint"],
            &["run", "--workflow", "ci.yml"],
            &["run", "--base", "shipcheck-base", "--no-fetch"],
            &["status", "--staged", "--json"],
            &["logs", "--staged"],
            &["history", "--json", "--limit", "5"],
            &["commit", "-m", "subject"],
            &[
                "commit",
                "-m",
                "subject",
                "-m",
                "body",
                "--allow-failed",
                "--no-verify",
            ],
            &["commit", "-F", "message.txt"],
            &["init", "--yes"],
            &["install"],
            &["login"],
            &["logout", "--all"],
            &["token", "--name", "ci-box"],
            &["clean"],
            &["daemon", "--home-ok"],
            &["daemon", "--uninstall"],
        ];

        for call in calls {
            assert!(
                parses(call),
                "shipcheck {} no longer parses",
                call.join(" ")
            );
        }
    }

    #[test]
    fn what_shipcheck_passes_itself_parses() {
        let sha = "0".repeat(40);

        for (post, home_ok) in [(false, false), (true, false), (false, true), (true, true)] {
            let args = background::run_args(&sha, "origin/main", "pr:1", post, home_ok);
            let call: Vec<&str> = args.iter().map(String::as_str).collect();
            assert!(
                parses(&call),
                "background run: shipcheck {}",
                call.join(" ")
            );
        }

        // The hook lines setup writes, with the values git hands them
        for line in [setup::HOOK_LINE, setup::COMMIT_LINE] {
            let call: Vec<String> = line
                .split_whitespace()
                .skip(1)
                .map(|w| {
                    w.replace("\"$local_ref\"", "refs/heads/feature")
                        .replace("\"$local_sha\"", &sha)
                })
                .collect();
            let call: Vec<&str> = call.iter().map(String::as_str).collect();
            assert!(parses(&call), "hook: {line}");
        }
    }

    #[test]
    fn a_wrong_call_is_refused() {
        for call in [
            &["frob"][..],
            &["run", "--frob"],
            &["run", "--sha", "HEAD", "--staged"],
            &["commit"],
            &["history", "--limit", "many"],
        ] {
            assert!(
                !parses(call),
                "shipcheck {} should be refused",
                call.join(" ")
            );
        }
    }
}
