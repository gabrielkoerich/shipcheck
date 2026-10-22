use std::path::Path;
use std::process::{Command, ExitCode};

use shipcheck::config::Trigger;
use shipcheck::service::{self, Poster};
use shipcheck::{agents, check, config, github, run, setup};

const USAGE: &str = "usage: shipcheck run [--sha <rev> | --staged] [--no-fetch] [--base <ref>] | status [<rev> | --staged] | logs [<rev>] | commit -m <message> [--allow-failed] | post [<rev>] [--with-logs] [--home-ok] | login | logout | clean [--all] | init | install | doctor | hook <agent> <event> | daemon [--home-ok]";

fn main() -> ExitCode {
    let existing = std::env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);

    for (key, value) in run::safe_git_env(existing) {
        // Safe: no other thread exists yet
        unsafe { std::env::set_var(key, value) };
    }

    match cli(std::env::args().skip(1).collect()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("shipcheck: {e}");
            ExitCode::from(2)
        }
    }
}

const COMMANDS: &[&str] = &[
    "run",
    "status",
    "logs",
    "commit",
    "post",
    "login",
    "logout",
    "clean",
    "init",
    "install",
    "doctor",
    "daemon",
    "on-commit",
    "on-push",
    "hook",
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

    // Called by the agents' hooks with their JSON on stdin, so it never fails
    if args.first().map(String::as_str) == Some("hook") {
        let mut input = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
        let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or_default();

        if let Some(reply) = shipcheck::hook::reply(arg(1), arg(2), &input) {
            println!("{reply}");
        }

        return Ok(ExitCode::SUCCESS);
    }

    let mut rev = "HEAD".to_string();
    let mut fetch = true;
    let mut base = "origin/main".to_string();
    let mut with_logs = false;
    let mut all = false;
    let mut explicit = false;
    let mut staged = false;
    let mut home_ok = false;
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
            "--no-fetch" => {
                fetch = false;
            }
            "--with-logs" => {
                with_logs = true;
            }
            "--all" => {
                all = true;
            }
            "--home-ok" => {
                home_ok = true;
            }
            "--base" => {
                // For trials: read shipcheck.toml and the workflows from another ref than origin/main
                base = rest.next().ok_or("--base needs a ref")?.clone();
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

            let started = std::time::Instant::now();
            let (tested, snapshot) = subject(&cwd, &rev, explicit, staged)?;
            let plan = run::plan(&cwd, &tested, &base)?;
            let what = snapshot.map_or_else(|| plan.sha[..12].to_string(), String::from);
            let tree = &plan.tree[..12];

            if plan.defaults {
                println!(
                    "no shipcheck.toml on {base}, running every workflow that runs for branches, on pull_request or push: {}. This result is not a PR check, `shipcheck init` and merge shipcheck.toml to main for that",
                    plan.config.workflows.join(", ")
                );
            }

            if snapshot.is_some() && !staged {
                warn_untracked(&plan.repo)?;
            }

            let _key = run::lock_key(&plan.key)?;

            // Only a pass is reused, a cached failure runs again so a flaky one cannot stick
            let entry = match run::lookup(&plan).filter(|e| e.passed) {
                Some(entry) => {
                    println!(
                        "tested {what}, tree {tree}: cached result in {:.1?}",
                        started.elapsed()
                    );
                    entry
                }
                None => {
                    let entry = run::run(&plan, &mut |line| println!("{}", run::printable(line)))?;
                    println!(
                        "tested {what}, tree {tree}: ran in {:.1?}",
                        started.elapsed()
                    );
                    entry
                }
            };
            let code = report(&entry);

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
                println!(
                    "commit exactly what ran with `shipcheck commit -m <message>` (or `{git}`) to reuse this result"
                );
            }

            Ok(code)
        }
        Some("doctor") => {
            let checks = shipcheck::doctor::checks(&cwd, &base);
            let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);

            for c in &checks {
                let mark = if c.ok { "ok " } else { "fix" };
                println!("{mark}  {:width$}  {}", c.name, c.detail);
            }

            if checks.iter().all(|c| c.ok) {
                println!("\nready: `shipcheck run` runs this commit's CI");
                Ok(ExitCode::SUCCESS)
            } else {
                println!(
                    "\nnot ready: fix the lines marked fix, then run `shipcheck doctor` again"
                );
                Ok(ExitCode::from(1))
            }
        }
        Some("status") => {
            // Before a commit, whether committing all of it or the index as it is reuses a result
            if !explicit {
                for (label, staged) in [("working copy", false), ("index", true)] {
                    let (tested, _) = subject(&cwd, &rev, false, staged)?;
                    let plan = run::plan(&cwd, &tested, &base)?;
                    let verdict = match run::lookup(&plan) {
                        Some(e) if e.passed => "passed",
                        Some(_) => "failed",
                        None => "no result",
                    };
                    println!("{label}, tree {}: {verdict}", &plan.tree[..12]);
                }
            }

            let (tested, _) = subject(&cwd, &rev, explicit, staged)?;
            let plan = run::plan(&cwd, &tested, &base)?;

            match run::lookup(&plan) {
                Some(entry) => Ok(report(&entry)),
                None => {
                    println!("no result for tree {}", plan.tree);
                    Ok(ExitCode::from(2))
                }
            }
        }
        Some("logs") => {
            let (tested, _) = subject(&cwd, &rev, explicit, staged)?;
            let plan = run::plan(&cwd, &tested, &base)?;
            print_logs(&run::log_dir(&plan.key))?;
            Ok(ExitCode::SUCCESS)
        }
        Some("post") => {
            refuse_moved_home(home_ok)?;
            let plan = run::plan(&cwd, &rev, &base)?;

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
            let repo = run::repo_id(&plan.repo)?;
            let logs = run::log_dir(&plan.key);
            let with_logs = with_logs || plan.config.logs == config::Logs::Failed;
            let secrets = if with_logs {
                run::secret_values(&repo)?
            } else {
                Vec::new()
            };
            let body = check::body(
                &plan.sha,
                &entry,
                with_logs.then_some((logs.as_path(), secrets.as_slice())),
            );
            let (_, url) = match service::poster() {
                Poster::Key => {
                    let token = github::App::from_env()?.installation_token(&repo)?;
                    github::post_check(&token, &repo, &body)?
                }
                Poster::Service(token) => service::post_check(&token, &repo, &body)?,
                Poster::None => {
                    return Err("nothing to post the check with: run `shipcheck login`, or set SHIPCHECK_APP_ID and SHIPCHECK_APP_KEY_FILE for an App of your own".into());
                }
            };
            println!(
                "posted {} on {}: {url}",
                if entry.passed { "success" } else { "failure" },
                &plan.sha[..12]
            );
            Ok(ExitCode::SUCCESS)
        }
        Some("login") => {
            let login = service::login()?;
            println!(
                "signed in as {login}, `shipcheck post` now posts through {}",
                service::api()
            );
            Ok(ExitCode::SUCCESS)
        }
        Some("logout") => {
            service::logout()?;
            println!("signed out, this machine's token is revoked");
            Ok(ExitCode::SUCCESS)
        }
        // Called by the hooks, so they never fail and never make a commit or a push wait
        Some("on-commit") => {
            let branch = current_branch(&cwd);

            if let Ok(plan) = run::plan(&cwd, "HEAD", &base)
                && plan.config.runs_on(Trigger::Commit)
            {
                background(&cwd, &plan, &branch);
            }

            Ok(ExitCode::SUCCESS)
        }
        Some("on-push") => {
            let refname = args.get(1).cloned().unwrap_or_default();
            let branch = refname.trim_start_matches("refs/heads/").to_string();

            if let Some(sha) = args.get(2)
                && let Ok(plan) = run::plan(&cwd, sha, &base)
                && plan.config.runs_on(Trigger::Push)
            {
                background(&cwd, &plan, &branch);
            }

            Ok(ExitCode::SUCCESS)
        }
        Some("clean") => {
            let repo = if all { None } else { Some(toplevel(&cwd)?) };

            for line in run::clean(repo.as_deref(), all)? {
                println!("{line}");
            }

            Ok(ExitCode::SUCCESS)
        }
        Some("daemon") => {
            refuse_moved_home(home_ok)?;
            let config = shipcheck::daemon::DaemonConfig::load()?;
            let app = github::App::from_env()?;
            println!(
                "shipcheck daemon: watching {} repositories every {}s",
                config.repos.len(),
                config.interval_seconds
            );
            shipcheck::daemon::serve(&config, &app)
        }
        Some("init") => {
            let root = toplevel(&cwd)?;

            if let Some(wrote) = setup::init(&root)? {
                println!("{wrote}");
            }

            for line in agents::instructions(&root)? {
                println!("{line}");
            }

            install_agents(&root);
            println!("{}", setup::install(&root)?);
            println!("next: review shipcheck.toml and AGENTS.md, commit them, and merge to main");
            Ok(ExitCode::SUCCESS)
        }
        Some("install") => {
            install_agents(&toplevel(&cwd)?);
            println!("{}", setup::install(&toplevel(&cwd)?)?);
            Ok(ExitCode::SUCCESS)
        }
        _ => Err(USAGE.into()),
    }
}

fn install_agents(root: &Path) {
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());

    for line in agents::install(root, &home, &run::home()) {
        println!("{line}");
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
        "{count} part of this run: {names}. Gitignore or delete them if they are not part of the change"
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

    let tested =
        run::last_tested(&toplevel(cwd)?).ok_or("no `shipcheck run` of this working copy yet")?;
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

/**
 * Starts a background run of the plan's commit, unless that tree already passed. One run per
 * repository and branch, like `cancel-in-progress` in Actions: a newer commit stops the older
 * run's process group first. A run of the same commit is left alone, so a push right after a
 * commit does not restart the run the commit started
 **/
fn background(cwd: &Path, plan: &run::Plan, branch: &str) {
    if run::skip_requested(plan) || run::lookup(plan).is_some_and(|e| e.passed) {
        return;
    }

    let Ok(repo) = run::repo_id(&plan.repo) else {
        return;
    };
    let dir = run::home().join("background").join(repo);
    let record = dir.join(format!("{}.pid", branch.replace('/', "%2F")));

    if let Ok(text) = std::fs::read_to_string(&record)
        && let Some((pid, sha)) = text.trim().split_once(' ')
    {
        if sha == plan.sha && alive(pid) {
            return;
        }

        // Only a group a background run of ours still leads, never a number that may now be another's
        if our_run(pid) {
            let _ = Command::new("kill")
                .args(["-s", "TERM", "--", &format!("-{pid}")])
                .status();
        }
    }

    if let Ok(pid) = start_background_run(cwd, &plan.sha) {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(&record, format!("{pid} {}", plan.sha));
    }
}

/// Whether `pid` is a running shipcheck process that leads its own process group, as a background
/// run started by `start_background_run` does
fn our_run(pid: &str) -> bool {
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) || pid == "0" || pid == "1" {
        return false;
    }

    let Ok(out) = Command::new("ps")
        .args(["-o", "pgid=", "-o", "comm=", "-p", pid])
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.split_whitespace();

    matches!(
        (fields.next(), fields.next()),
        (Some(pgid), Some(comm)) if pgid == pid && comm.rsplit('/').next() == Some("shipcheck")
    )
}

fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn start_background_run(cwd: &Path, sha: &str) -> Result<u32, String> {
    use std::os::unix::process::CommandExt;

    let log_dir = run::home().join("logs");
    std::fs::create_dir_all(&log_dir).map_err(|e| e.to_string())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("background.log"))
        .map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;

    // Its own process group, so Ctrl-C in the terminal after a commit does not stop the run.
    // git points hooks at the commit's index, which would break every git call the run makes
    let child = Command::new(exe)
        .args(["run", "--no-fetch", "--sha", sha])
        .current_dir(cwd)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_PREFIX")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(child.id())
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

fn report(entry: &run::Entry) -> ExitCode {
    if !entry.ci_changes.is_empty() {
        println!(
            "this commit changes CI: workflow and shipcheck.toml changes did not run, the base's versions did, changes to what a command runs did run:"
        );

        for change in &entry.ci_changes {
            println!("  {change}");
        }
    }

    if let Some(reason) = &entry.skipped {
        println!("nothing to run: {reason}");
    }

    for wf in &entry.workflows {
        let exit = wf
            .exit
            .map_or("timed out".to_string(), |c| format!("exit {c}"));
        let attempts = if wf.attempts == 1 {
            "1 attempt".to_string()
        } else {
            format!("{} attempts", wf.attempts)
        };
        println!(
            "{}: {}, {exit}, {attempts}",
            check::name(&wf.file),
            if wf.passed { "passed" } else { "failed" },
        );

        for (job, state) in &wf.jobs {
            println!("  {job}: {state}");
        }
    }

    if entry.passed {
        println!("passed");
        ExitCode::SUCCESS
    } else {
        println!("failed, see `shipcheck logs`");
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
