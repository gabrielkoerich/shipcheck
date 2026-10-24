use std::io::IsTerminal;
use std::path::Path;
use std::process::{Command, ExitCode};

use shipcheck::config::Trigger;
use shipcheck::record::Run as Record;
use shipcheck::service::{self, Poster, Sender};
use shipcheck::ui::Style;
use shipcheck::{agents, autostart, background, check, report, run, setup};

const USAGE: &str = "usage: shipcheck run [--sha <rev> | --staged] [--no-fetch] [--base <ref>] [--job <id>]... [--workflow <file>]... [--json] | status [<rev> | --staged] [--json] | logs [<rev>] [--base <ref>] | history [--json] [--limit <n>] | commit -m <message> [--allow-failed] | post [<rev>] [--home-ok] | login | logout [--all] | token [--name <machine>] | clean [--all] | init [--yes] | install | doctor | daemon [--home-ok | --uninstall]";

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

    run::migrate_home();

    match cli(std::env::args().skip(1).collect()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{} {e}", Style::stderr().red("shipcheck:"));
            ExitCode::from(2)
        }
    }
}

const COMMANDS: &[&str] = &[
    "run",
    "status",
    "logs",
    "history",
    "commit",
    "post",
    "login",
    "logout",
    "token",
    "clean",
    "init",
    "install",
    "doctor",
    "daemon",
    "on-commit",
    "on-push",
];

fn cli(args: Vec<String>) -> Result<ExitCode, String> {
    // Help for an unknown command is an error, so a tool can test which commands this version has
    let known = COMMANDS.contains(&args.first().map(String::as_str).unwrap_or_default());

    if args.iter().any(|a| a == "-h" || a == "--help")
        && (known || matches!(args.first().map(String::as_str), Some("-h" | "--help")))
    {
        println!("{USAGE}\n\nDocs: https://shipcheck.run/docs/");
        return Ok(ExitCode::SUCCESS);
    }

    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;

    if args.first().map(String::as_str) == Some("commit") {
        return commit(&cwd, &args[1..]);
    }

    // Only the token goes to stdout, so a script can capture it
    if args.first().map(String::as_str) == Some("token") {
        let name = match args.get(1..).unwrap_or_default() {
            [] => "token".to_string(),
            [flag, name] if flag == "--name" => name.clone(),
            _ => {
                return Err(USAGE.into());
            }
        };
        println!("{}", service::new_token(&name)?);
        eprintln!(
            "set it as SHIPCHECK_TOKEN on the other machine, `shipcheck logout` there or `shipcheck logout --all` anywhere revokes it"
        );
        return Ok(ExitCode::SUCCESS);
    }

    let mut rev = "HEAD".to_string();
    let mut fetch = true;
    let mut base = "origin/main".to_string();
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
    let mut rest = args.iter().skip(1);

    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--sha" => {
                rev = rest.next().ok_or("--sha needs a revision")?.clone();
                explicit = true;
            }
            "--staged" => {
                staged = true;
            }
            "--json" => {
                json = true;
            }
            "--no-fetch" => {
                fetch = false;
            }
            "--all" => {
                all = true;
            }
            "--home-ok" => {
                home_ok = true;
            }
            "--yes" => {
                yes = true;
            }
            "--uninstall" => {
                uninstall = true;
            }
            "--background" => {
                posts = Some(rest.next().ok_or("--background needs a name")?.clone());
            }
            "--job" => {
                jobs.push(rest.next().ok_or("--job needs a job id")?.clone());
            }
            "--workflow" => {
                workflows.push(rest.next().ok_or("--workflow needs a file")?.clone());
            }
            "--limit" => {
                let n = rest.next().ok_or("--limit needs a number")?;
                limit = n
                    .parse()
                    .map_err(|_| format!("--limit needs a number, not {n}"))?;
            }
            "--base" => {
                // For trials: read shipcheck.toml and the workflows from another ref than origin/main
                base = rest.next().ok_or("--base needs a ref")?.clone();
                base_set = true;
                fetch = false;
            }
            other if !other.starts_with('-') => {
                rev = other.to_string();
                explicit = true;
            }
            other => {
                return Err(format!("unknown option {other}\n{USAGE}"));
            }
        }
    }

    if (!jobs.is_empty() || !workflows.is_empty()) && args[0] != "run" {
        return Err("--job and --workflow work only with run".into());
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

    match args.first().map(String::as_str) {
        Some("run") => {
            if fetch {
                let status = Command::new("git")
                    .args(["fetch", "-q", "origin", "main"])
                    .current_dir(&cwd)
                    .status();

                if !status.map(|s| s.success()).unwrap_or(false) {
                    return Err("git fetch origin main failed, use --no-fetch to run against the local origin/main".into());
                }
            }

            // A run that posts reads the config from GitHub's main, not one the clone was told
            if posts.is_some() && !background::fetch_base(&cwd, &base) {
                say(&format!("could not fetch {base}, so nothing is posted"));
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
        Some("doctor") => {
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
        Some("status") => {
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
        Some("history") => {
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
        Some("logs") => {
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
        Some("post") => {
            refuse_moved_home(home_ok)?;
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
        Some("login") => {
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
        Some("logout") => {
            println!("{}", service::logout(all)?);
            Ok(ExitCode::SUCCESS)
        }
        // Called by the hooks, so they never fail and never make a commit or a push wait
        Some("on-commit") => {
            let branch = current_branch(&cwd);

            if let Ok(plan) = run::plan(&cwd, "HEAD", &base)
                && plan.config.runs_on(Trigger::Commit)
                && !run::skip_requested(&plan)
            {
                background::start(&cwd, &plan, &base, &branch, false, "hook-commit", false);
            }

            Ok(ExitCode::SUCCESS)
        }
        Some("on-push") => {
            let refname = args.get(1).cloned().unwrap_or_default();
            let branch = refname.trim_start_matches("refs/heads/").to_string();

            if let Some(sha) = args.get(2)
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
        Some("clean") => {
            let repo = if all { None } else { Some(toplevel(&cwd)?) };

            for line in run::clean(repo.as_deref(), all)? {
                println!("{line}");
            }

            if all {
                println!("{}", autostart::uninstall()?);
            }

            Ok(ExitCode::SUCCESS)
        }
        Some("daemon") => {
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
        Some("init") => {
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

            if let Some(wrote) = setup::init(&root)? {
                println!("{wrote}");
            }

            // Instructions, hooks and the daemon for a repository with nothing to run would only mislead
            if !root.join("shipcheck.toml").exists() {
                return Ok(ExitCode::SUCCESS);
            }

            machine_setup(&root, &mut |line| println!("{line}"))?;
            println!("next: review shipcheck.toml and AGENTS.md, commit them, and merge to main");
            Ok(ExitCode::SUCCESS)
        }
        Some("install") => {
            let root = toplevel(&cwd)?;
            remove_old_hooks(&root, &mut |line| println!("{line}"));
            println!("{}", setup::install(&root)?);
            sign_in(&mut |line| println!("{line}"));
            println!("{}", autostart::install(&root));
            Ok(ExitCode::SUCCESS)
        }
        _ => Err(USAGE.into()),
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

/// `shipcheck commit -m <msg> | -F <file> [--allow-failed] [--no-verify]`: commits the tree the last run tested
fn commit(cwd: &Path, args: &[String]) -> Result<ExitCode, String> {
    let mut git_args = Vec::new();
    let mut allow_failed = false;
    let mut rest = args.iter();

    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-m" | "-F" => {
                let value = rest.next().ok_or(format!("{arg} needs a value"))?;
                git_args.extend([arg.clone(), value.clone()]);
            }
            "--allow-failed" => {
                allow_failed = true;
            }
            "--no-verify" => {
                git_args.push(arg.clone());
            }
            other => {
                return Err(format!(
                    "unknown option {other}\nusage: shipcheck commit -m <message> | -F <file> [--allow-failed] [--no-verify]"
                ));
            }
        }
    }

    if !git_args.iter().any(|a| a == "-m" || a == "-F") {
        return Err("shipcheck commit needs -m <message> or -F <file>".into());
    }

    let repo = toplevel(cwd)?;

    // Plain `git commit` still works on it, the hooks never block git
    if let Some(branch) = run::on_default_branch(&repo, "origin/main", None) {
        return Err(format!(
            "{branch} is the default branch, PRs carry shipcheck's checks: `git switch -c <branch>`, then `shipcheck commit`"
        ));
    }

    let tested = run::last_tested(&repo).ok_or("no `shipcheck run` of this working copy yet")?;
    let sha = run::commit(cwd, &tested, &git_args, allow_failed)?;
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
                None => run::run(plan, &mut |line| say(&run::printable(line))),
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

    if entry.passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
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
