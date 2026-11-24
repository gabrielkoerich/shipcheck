//! The `shipcheck` command line
#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a CLI talks on stdout and stderr"
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};

use shipcheck::background::Posting;
use shipcheck::check::Tails;
use shipcheck::config::Trigger;
use shipcheck::daemon;
use shipcheck::daemon_config::DaemonConfig;
use shipcheck::onboard::{self, App};
use shipcheck::paths::MovedHome;
use shipcheck::queue::Arrival;
use shipcheck::record::{self, Run as Record};
use shipcheck::report::Coverage;
use shipcheck::run::{Failed, Tree};
use shipcheck::runner::host;
use shipcheck::service::{self, Credentials, Poster, Revoke, Sender};
use shipcheck::ui::{Live, Screen, Stream, Style};
use shipcheck::{agents, autostart, background, check, doctor, queue, report, run, setup, trust};
use shipcheck::{base, clock, git, history, paths, ui};

const PARTIAL: &str = "partial run: not the check, run `shipcheck run` before committing";

fn main() -> ExitCode {
    let existing = std::env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);

    for (key, value) in git::safe_git_env(existing) {
        // SAFETY: no other thread exists yet
        unsafe { std::env::set_var(key, value) };
    }

    // git, bwrap and act come only from where a check finds tools, never a fake on the caller's PATH.
    // `shipcheck commit` gives the user's own git hooks the caller's PATH
    if let Some(path) = std::env::var_os("PATH") {
        let _ = run::CALLER_PATH.set(path);
    }

    // SAFETY: no other thread exists yet
    unsafe { std::env::set_var("PATH", host::jail_path()) };

    paths::migrate_home();

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
    /// Set up this repository: shipcheck.toml, agent instructions, hooks. Without a terminal and without --yes it only previews
    Init {
        /// Take the recommended choices without asking, as a script or an agent does
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
        /// Keep what passed in this branch's last failed result and run only the rest, when
        /// `keep_passed = true` in shipcheck.toml on main allows it
        #[arg(long, conflicts_with_all = ["jobs", "workflows", "background"])]
        only_failed: bool,
        /// Print the result as JSON on stdout
        #[arg(long)]
        json: bool,
        /// Print no line each minute that says where the run stands, when the output is not a terminal
        #[arg(long)]
        no_heartbeat: bool,
        /// Start the run in the background, print its id and exit, `shipcheck wait <id>` collects it
        #[arg(long = "async", visible_alias = "detach", conflicts_with_all = ["jobs", "workflows", "background"])]
        detach: bool,
        /// Set by the hooks and the daemon on a run that posts its checks
        #[arg(long, hide = true, value_name = "name")]
        background: Option<String>,
        /// Set with `--background` when the daemon runs from a moved `SHIPCHECK_HOME`
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
    /// Wait for the run of a tree that is queued or going, then print its result
    Wait {
        /// A run's id or a commit, instead of the working copy
        target: Option<String>,
        /// The index instead of the working copy
        #[arg(long, conflicts_with = "target")]
        staged: bool,
        /// Stop waiting after this long, as `90`, `90s`, `10m` or `1h`, and exit 3 while the run still goes
        #[arg(long, value_name = "duration")]
        timeout: Option<String>,
        /// Print the result as JSON on stdout
        #[arg(long)]
        json: bool,
        /// Print no line each minute that says where the run stands, when the output is not a terminal
        #[arg(long)]
        no_heartbeat: bool,
        /// The ref the run read shipcheck.toml from
        #[arg(long, value_name = "ref")]
        base: Option<String>,
    },
    /// Print the stored result of a tree, or where the run with this id stands
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
        /// Post from a `SHIPCHECK_HOME` other than the default
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
        /// How many runs to list, 20 unless `--flaky`, which reads the last 200
        #[arg(long, value_name = "n")]
        limit: Option<usize>,
        /// List the tests and jobs that failed and then passed on a retry, most often first
        #[arg(long)]
        flaky: bool,
    },
    /// List the runs of every repository going or queued on this machine
    Queue {
        /// Print the runs as JSON on stdout
        #[arg(long)]
        json: bool,
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
    /// Let this repository's checks with sandbox = false run outside the sandbox on this machine
    Trust {
        /// Stop trusting it, so those checks run in the sandbox again
        #[arg(long)]
        remove: bool,
    },
    /// Remove what shipcheck keeps for this repository
    Clean {
        /// For every repository, and the stored results, logs and daemon too
        #[arg(long)]
        all: bool,
    },
    /// Watch the open PRs and run each new head, as a service
    Daemon {
        /// Run from a `SHIPCHECK_HOME` other than the default
        #[arg(long)]
        home_ok: bool,
        /// Stop the service and remove it
        #[arg(long)]
        uninstall: bool,
    },
    /// Called by the post-commit hook
    #[command(hide = true)]
    OnCommit,
    /// Called by the pre-push hook with the pushed ref, its commit and the ref it updates on the remote
    #[command(hide = true)]
    OnPush {
        refname: Option<String>,
        sha: Option<String>,
        remote_ref: Option<String>,
    },
}

#[expect(
    clippy::too_many_lines,
    reason = "one arm per subcommand, read as one table"
)]
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
    let (mut json, mut only_failed) = (false, false);
    let mut limit = 20;
    let mut uninstall = false;
    let mut heartbeat = true;
    let mut flaky = false;
    let mut detach = false;
    let mut timeout: Option<String> = None;
    // Set by the hooks and the daemon on a background run that posts its checks
    let mut posts: Option<String> = None;
    let (mut refname, mut pushed, mut remote_ref) = (String::new(), None, None);
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

            let failed = if allow_failed {
                Failed::Allow
            } else {
                Failed::Refuse
            };
            return commit(&cwd, &args, failed);
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
            staged: run_staged,
            no_fetch,
            base: run_base,
            jobs: run_jobs,
            workflows: run_workflows,
            only_failed: run_only_failed,
            json: run_json,
            no_heartbeat,
            detach: run_detach,
            background,
            home_ok: run_home_ok,
            rev: run_rev,
        } => {
            detach = run_detach;
            heartbeat = !no_heartbeat;
            only_failed = run_only_failed;
            at(sha.or(run_rev), &mut rev, &mut explicit);
            home_ok = run_home_ok;
            staged = run_staged;
            fetch = !no_fetch;
            on_base(run_base, &mut base, &mut base_set, &mut fetch);
            (jobs, workflows, json, posts) = (run_jobs, run_workflows, run_json, background);
            "run"
        }
        Cmd::Wait {
            target,
            staged: st,
            timeout: t,
            json: js,
            no_heartbeat,
            base: b,
        } => {
            at(target, &mut rev, &mut explicit);
            (staged, json, timeout, heartbeat) = (st, js, t, !no_heartbeat);
            on_base(b, &mut base, &mut base_set, &mut fetch);
            "wait"
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
        Cmd::History {
            json: js,
            limit: l,
            flaky: f,
        } => {
            (json, flaky) = (js, f);
            limit = l.unwrap_or(if flaky { 200 } else { 20 });
            "history"
        }
        Cmd::Queue { json: js } => {
            json = js;
            "queue"
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
        Cmd::Trust { remove: r } => {
            all = r;
            "trust"
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
        Cmd::OnPush {
            refname: r,
            sha,
            remote_ref: to,
        } => {
            (refname, pushed, remote_ref) = (r.unwrap_or_default(), sha, to);
            "on-push"
        }
    };

    let reads_base = matches!(
        name,
        "run" | "status" | "logs" | "post" | "doctor" | "on-commit" | "on-push"
    );

    if !base_set && reads_base {
        base = base::command_base(name, &cwd);
    }

    // With --json, stdout holds only the result object
    let style = if json {
        Style::stderr()
    } else {
        Style::stdout()
    };
    let live = Live::new(if json { Stream::Stderr } else { Stream::Stdout });
    let say = |line: &str| live.line(&style.status(line));
    let moved_home = if home_ok {
        MovedHome::Allowed
    } else {
        MovedHome::Refused
    };
    let tree_kind = if staged { Tree::Staged } else { Tree::Working };
    let given = if base_set { Base::Given } else { Base::Default };
    let format = if json { Format::Json } else { Format::Text };
    let beat = if heartbeat {
        Heartbeat::On
    } else {
        Heartbeat::Off
    };

    match name {
        "run" => {
            if let Some(gap) = setup::install_gap(&cwd) {
                eprintln!("{}", Style::stderr().yellow(&format!("warning: {gap}")));
            }

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

            if posts.is_some() && !base::posts_from(&base, &cwd) {
                say(&format!(
                    "{base} is not the base this repository posts from, so nothing is posted"
                ));
                posts = None;
            }

            let started = std::time::Instant::now();
            let mut record = Record::new(
                &std::env::var("SHIPCHECK_TRIGGER").unwrap_or_else(|_| "cli".into()),
                clock::unix_now(),
            );
            record.branch = std::env::var("SHIPCHECK_BRANCH")
                .ok()
                .or_else(|| Some(current_branch(&cwd)))
                .filter(|b| b != "detached");
            let (tested, snapshot) = subject(&cwd, explicit.then_some(rev.as_str()), tree_kind)?;
            let plan = run::plan(&cwd, &tested, &base)?;
            let what = snapshot.map_or_else(|| plan.sha[..12].to_string(), String::from);
            let tree = &plan.tree[..12];

            if snapshot.is_some() && !staged {
                warn_untracked(&plan.repo)?;
            }

            if detach {
                let mut args = vec!["run".to_string(), "--no-fetch".into()];

                if base_set {
                    args.extend(["--base".into(), base]);
                }

                if explicit {
                    args.extend(["--sha".into(), rev.clone()]);
                }

                for (flag, set) in [
                    ("--staged", staged),
                    ("--only-failed", only_failed),
                    ("--no-heartbeat", !heartbeat),
                ] {
                    if set {
                        args.push(flag.into());
                    }
                }

                return detached(&cwd, &plan, &args, format, &say);
            }

            if !jobs.is_empty() || !workflows.is_empty() {
                let _key = run::lock_key(&plan.key)?;
                let entry = progress(&live, beat, &own_run, || {
                    run::run_partial(&plan, &jobs, &workflows, &mut |line| {
                        say(&ui::printable(line));
                    })
                })?;
                say(&format!(
                    "tested {what}, tree {tree}: ran in {:.1?}",
                    started.elapsed()
                ));
                let code = report(&entry, snapshot, format, Coverage::Partial);

                if !explicit {
                    run::record_tested(
                        &plan.repo,
                        &run::Tested {
                            tree: plan.tree.clone(),
                            staged,
                            passed: entry.passed,
                            partial: true,
                        },
                    )?;
                }

                record.partial = true;
                keep_record(
                    &plan,
                    &entry,
                    &record,
                    &paths::partial_log_dir(&plan.key),
                    Upload::Off,
                );
                say(PARTIAL);
                return Ok(code);
            }

            if posts.is_some() && !background::home_allows_post(paths::which_home(), moved_home) {
                say(&format!(
                    "{}: not posted, SHIPCHECK_HOME is not the default, so its results may not come from runs. A person who set it can pass --home-ok",
                    &plan.sha[..12]
                ));

                if let Ok(repo) = git::repo_id(&plan.repo) {
                    let fields = serde_json::json!({"sha": plan.sha, "reason": "home_moved"});
                    record::keep(&repo, &record::event(&repo, "skipped", fields));
                }

                posts = None;
            }

            let sender = posts.as_ref().and_then(|_| {
                git::repo_id(&plan.repo)
                    .and_then(|repo| Sender::new(&repo))
                    .map_err(|e| say(&format!("{}: not posted, {e}", &plan.sha[..12])))
                    .ok()
            });

            if let (Some(sender), Some(name)) = (&sender, &posts) {
                let repo = git::repo_id(&plan.repo)?;
                let posted = posting_run(&plan, sender, name, &mut record, &mut |line| say(line));

                if let Err(e) = &posted
                    && let Some(refused) = record.post_error.clone()
                {
                    background::remember_post(&repo, Some(&refused));

                    if let Some(entry) = run::lookup(&plan) {
                        keep_record(
                            &plan,
                            &entry,
                            &record,
                            &paths::log_dir(&plan.key),
                            Upload::Off,
                        );
                    }

                    return Err(format!(
                        "{e}\n{}",
                        background::unposted_line(&repo, &refused)
                    ));
                }

                let posted = posted?;

                if let Some(entry) = &posted {
                    background::remember_post(&repo, None);
                    record.posted_checks = true;
                    record.posted_comment = daemon::comment(sender, None, &plan, &base, entry);
                    keep_record(
                        &plan,
                        entry,
                        &record,
                        &paths::log_dir(&plan.key),
                        Upload::On,
                    );
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
                return Ok(report(&entry, snapshot, format, Coverage::Full));
            }

            if run::running(&plan.key) {
                say(&format!(
                    "run {} of this tree is already going, waiting for its result",
                    queue::id(&plan.key)
                ));
            }

            let (_key, waited) = run::lock_tree(&plan.key)?;
            let mut estimated = None;
            let entry = match run::reusable(&plan, waited) {
                // A `paths` skip is never stored, so it is no cached result
                Some(entry) if entry.skipped.is_some() => {
                    say(&format!("tested {what}, tree {tree}: nothing ran"));
                    entry
                }
                Some(entry) => {
                    record.cache_hit = true;
                    let how = if entry.passed {
                        "cached result"
                    } else {
                        "result of the run of this tree it waited for, which failed,"
                    };
                    say(&format!(
                        "tested {what}, tree {tree}: {how} in {:.1?}",
                        started.elapsed()
                    ));
                    entry
                }
                None => {
                    let (forecast, numbers) = estimate(&plan);
                    say(&forecast);
                    estimated = numbers;
                    let say = &mut |line: &str| say(&ui::printable(line));
                    let entry = progress(&live, beat, &own_run, || {
                        if only_failed {
                            run::run_only_failed(&plan, say)
                        } else {
                            run::run(&plan, say)
                        }
                    })?;
                    say(&format!(
                        "tested {what}, tree {tree}: ran in {:.1?}",
                        started.elapsed()
                    ));
                    entry
                }
            };
            let code = report_with(&entry, snapshot, format, Coverage::Full, estimated);
            // Like a post, an upload needs the base this repository posts from, never a fallback
            let upload = if plan.config.team.records && base::posts_from(&base, &cwd) {
                Upload::On
            } else {
                Upload::Off
            };
            keep_record(&plan, &entry, &record, &paths::log_dir(&plan.key), upload);

            if !explicit {
                run::record_tested(
                    &plan.repo,
                    &run::Tested {
                        tree: plan.tree.clone(),
                        staged,
                        passed: entry.passed,
                        partial: false,
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

            if let Some(line) = unposted(&plan.repo) {
                say(&line);
            }

            Ok(code)
        }
        "doctor" => {
            let checks = doctor::checks(&cwd, &base);
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
        "wait" => {
            // An id names a tree that ran or runs here, anything else is a commit
            let known = explicit
                .then(|| run::find_key(&paths::home(), &rev))
                .flatten();
            let (plan, snapshot) = if known.is_some() {
                (None, None)
            } else {
                if !base_set {
                    base = base::command_base(name, &cwd);
                }

                let (tested, snapshot) =
                    subject(&cwd, explicit.then_some(rev.as_str()), tree_kind)?;
                (Some(run::plan(&cwd, &tested, &base)?), snapshot)
            };
            let key = known
                .or_else(|| plan.as_ref().map(|p| p.key.clone()))
                .unwrap_or_default();
            let id = queue::id(&key);
            let limit = timeout.as_deref().map(duration).transpose()?;
            let started = Instant::now();
            let mut went = false;
            let late = progress(&live, beat, &|r| r["key"] == key.as_str(), || {
                while run::running(&key) {
                    went = true;

                    if limit.is_some_and(|limit| started.elapsed() >= limit) {
                        return true;
                    }

                    std::thread::sleep(Duration::from_millis(200));
                }

                false
            });

            if late {
                let state = run_state(&key);
                say(&format!(
                    "run {id} still goes after {}: {}",
                    clock::clock(started.elapsed().as_secs()),
                    state["state"].as_str().unwrap_or_default()
                ));

                if json {
                    println!("{state}");
                }

                return Ok(ExitCode::from(STILL_GOING));
            }

            let result = match &plan {
                Some(plan) => run::lookup(plan),
                None => run::cached(&key),
            };

            if let Some(entry) = result {
                Ok(report(&entry, snapshot, format, Coverage::Full))
            } else {
                if json {
                    println!("{}", run_state(&key));
                }

                if went {
                    say(&format!(
                        "run {id} ended without a result: it could not run, was stopped, or was a partial run"
                    ));
                    Ok(ExitCode::from(2))
                } else {
                    say(&format!(
                        "no run of this tree is queued or going, and it has no result, `shipcheck run` starts one (id {id})"
                    ));
                    Ok(ExitCode::from(NO_RUN))
                }
            }
        }
        "status" => {
            if explicit && let Some(key) = run::find_key(&paths::home(), &rev) {
                let state = run_state(&key);

                if state["state"] == "queued"
                    || state["state"] == "running"
                    || state["state"] == "starting"
                {
                    if json {
                        println!("{state}");
                    } else {
                        let rows = queue::list(&paths::home().join("runners"), clock::unix_now());
                        let secs = state["elapsed_s"].as_u64().unwrap_or_default();
                        let at = queue::standing(&rows, &|r| r["key"] == key.as_str(), secs);
                        say(&format!("run {}: {at}", queue::id(&key)));
                    }

                    return Ok(ExitCode::from(STILL_GOING));
                }

                if let Some(entry) = run::cached(&key) {
                    return Ok(report(&entry, None, format, Coverage::Full));
                }
            }

            // Before a commit, whether committing all of it or the index as it is reuses a result
            if !explicit {
                for (label, tree) in [("working copy", Tree::Working), ("index", Tree::Staged)] {
                    let (tested, _) = subject(&cwd, None, tree)?;
                    let mut plan = run::plan(&cwd, &tested, &base)?;
                    last_run_key(&mut plan, given, |k| run::cached(k).is_some());
                    let verdict = match run::lookup(&plan) {
                        Some(e) if e.passed => "passed",
                        Some(_) => "failed",
                        None => "no result",
                    };
                    say(&format!("{label}, tree {}: {verdict}", &plan.tree[..12]));
                }
            }

            let (tested, snapshot) = subject(&cwd, explicit.then_some(rev.as_str()), tree_kind)?;
            let mut plan = run::plan(&cwd, &tested, &base)?;

            if let Some(other) = last_run_key(&mut plan, given, |k| run::cached(k).is_some()) {
                say(&format!(
                    "the last run of this tree read shipcheck.toml from {other}, not {base}"
                ));
            }

            if let Some(line) = unposted(&plan.repo) {
                say(&line);
            }

            match run::lookup(&plan) {
                Some(entry) => Ok(report(&entry, snapshot, format, Coverage::Full)),
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
            let repo = git::repo_id(&toplevel(&cwd)?)?;
            let runs = history::runs(&paths::home(), &repo, limit);

            if flaky {
                let rows = history::flaky(&runs);

                if json {
                    println!("{}", serde_json::Value::from(rows));
                } else if rows.is_empty() {
                    println!(
                        "no job passed on a retry in the last {} runs of {repo} on this machine",
                        runs.len()
                    );
                } else {
                    print!("{}", history::flaky_table(&rows));
                }

                return Ok(ExitCode::SUCCESS);
            }

            if json {
                println!("{}", serde_json::Value::from(runs));
            } else if runs.is_empty() {
                println!("no runs recorded for {repo} on this machine yet");
            } else {
                print!("{}", record::table(&runs));
            }

            Ok(ExitCode::SUCCESS)
        }
        "queue" => {
            let mut runs = queue::list(&paths::home().join("runners"), clock::unix_now());
            queue::forecast(
                &mut runs,
                run::max_runs(),
                Arrival::Background,
                &mut typical_times(),
            );

            if json {
                println!("{}", serde_json::Value::from(runs));
            } else if runs.is_empty() {
                println!("no run is going or queued on this machine");
            } else {
                print!("{}", queue::table(&runs));
            }

            Ok(ExitCode::SUCCESS)
        }
        "logs" => {
            let (tested, _) = subject(&cwd, explicit.then_some(rev.as_str()), tree_kind)?;
            let mut plan = run::plan(&cwd, &tested, &base)?;

            if let Some(other) = last_run_key(&mut plan, given, |k| {
                paths::log_dir(k).exists() || paths::partial_log_dir(k).exists()
            }) {
                println!("the last run of this tree read shipcheck.toml from {other}, not {base}");
            }

            let (full, partial) = (paths::log_dir(&plan.key), paths::partial_log_dir(&plan.key));

            if full.exists() || !partial.exists() {
                print_logs(&full)?;
            }

            if partial.exists() {
                println!("==> the last partial run, {PARTIAL} <==");
                print_logs(&partial)?;
            }

            if let Ok(repo) = git::repo_id(&plan.repo)
                && let Some(line) = record::posting(
                    &history::runs(&paths::home(), &repo, usize::MAX),
                    &repo,
                    &plan.sha,
                )
            {
                println!("{line}");
            }

            Ok(ExitCode::SUCCESS)
        }
        "post" => {
            refuse_moved_home(moved_home)?;

            if !base::posts_from(&base, &cwd) {
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

            let entry = run::lookup(&plan).ok_or_else(|| {
                format!(
                    "no result for tree {}, run `shipcheck run` first",
                    plan.tree
                )
            })?;
            // Secrets that cannot be read stop the post, so no log goes out unmasked
            run::secret_values(&git::repo_id(&plan.repo)?)?;
            let sender = Sender::new(&git::repo_id(&plan.repo)?)?;
            // Job checks go first, so a failed post leaves `shipcheck` unposted
            let bodies = check::final_bodies(&plan, &entry, Tails::Shown);
            let repo = git::repo_id(&plan.repo)?;
            let url =
                background::finish(&|b, id| sender.send(b, id), &bodies, &BTreeMap::default())
                    .inspect_err(|e| background::remember_post(&repo, Some(e)))?;
            background::remember_post(&repo, None);

            println!(
                "posted {} on {}: {url}",
                if entry.passed { "success" } else { "failure" },
                &plan.sha[..12]
            );

            let commented = daemon::comment(&sender, None, &plan, &base, &entry);
            let fields = serde_json::json!({
                "sha": plan.sha,
                "tree": plan.tree,
                "passed": entry.passed,
                "checks": true,
                "comment": commented,
            });
            record::keep(&repo, &record::event(&repo, "posted", fields));
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
            println!(
                "{}",
                service::logout(if all {
                    Revoke::All
                } else {
                    Revoke::ThisMachine
                })?
            );
            Ok(ExitCode::SUCCESS)
        }
        // Called by the hooks, so they never fail and never make a commit or a push wait
        "on-commit" => {
            let branch = current_branch(&cwd);

            if let Ok(plan) = run::plan(&cwd, "HEAD", &base)
                && plan.config.runs_on(Trigger::Commit)
                && !run::skip_requested(&plan)
            {
                background::start(
                    &cwd,
                    &plan,
                    &base,
                    &branch,
                    Posting::Off,
                    "hook-commit",
                    MovedHome::Refused,
                );
            }

            Ok(ExitCode::SUCCESS)
        }
        "on-push" => {
            let branch = refname.trim_start_matches("refs/heads/").to_string();

            if let Some(sha) = &pushed
                && !pushes_to_base(remote_ref.as_deref().unwrap_or(&refname), &base)
                && let Ok(plan) = run::plan(&cwd, sha, &base)
                && plan.config.runs_on(Trigger::Push)
                && !run::skip_requested(&plan)
            {
                // The pushed commit's checks, posted by its run, unless the result could be an agent's
                let post = background::posts(paths::which_home(), service::credentials());

                if post == Posting::Off {
                    background::note(&format!(
                        "{} on {branch}: checks not posted, run `shipcheck login`, or set SHIPCHECK_TOKEN from `shipcheck token`",
                        &plan.sha[..12]
                    ));
                }

                background::start(
                    &cwd,
                    &plan,
                    &base,
                    &branch,
                    post,
                    "hook-push",
                    MovedHome::Refused,
                );

                // One line on the push, git shows a hook's output and the push goes on
                if let Some(line) = unposted(&plan.repo) {
                    eprintln!("shipcheck: {line}");
                }
            }

            Ok(ExitCode::SUCCESS)
        }
        "trust" => {
            let root = toplevel(&cwd)?;
            let origin = if all {
                trust::remove(&root)?
            } else {
                trust::add(&root)?
            };

            if all {
                println!(
                    "{origin} is no longer trusted: its checks with sandbox = false run in the sandbox"
                );
            } else {
                println!(
                    "trusted {origin}: its checks with sandbox = false run outside the sandbox on this machine, with your full access"
                );
            }

            Ok(ExitCode::SUCCESS)
        }
        "clean" => {
            let repo = if all { None } else { Some(toplevel(&cwd)?) };

            let scope = repo.as_deref().map_or(run::Scope::All, run::Scope::Repo);

            for line in run::clean(scope)? {
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

            refuse_moved_home(moved_home)?;
            let config = DaemonConfig::load()?;
            println!(
                "shipcheck daemon: watching {} repositories every {}s",
                config.repos.len(),
                config.interval_seconds
            );
            daemon::serve(moved_home)
        }
        "init" => {
            let root = toplevel(&cwd)?;
            let terminal = std::io::stdin().is_terminal()
                && std::io::stdout().is_terminal()
                && std::io::stderr().is_terminal();
            let finish = &mut |say: &mut dyn FnMut(&str)| machine_setup(&root, say);

            // A person gets the wizard, `--yes` the recommended choices, an agent or a script without `--yes` only the plan
            if yes {
                onboard::plain(&root, App::find(), &mut std::io::stdout(), finish)?;
                return Ok(ExitCode::SUCCESS);
            }

            if terminal {
                onboard::tui(&root, App::find, finish)?;
                return Ok(ExitCode::SUCCESS);
            }

            // The preview's own process, which the one below starts with a scratch home
            if std::env::var_os(run::PREVIEW).is_some() {
                onboard::preview(&root, App::find(), &mut std::io::stdout())?;
                return Ok(ExitCode::SUCCESS);
            }

            Ok(ExitCode::from(onboard::preview_apart(&root)?))
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
    let credentials = if service::poster() == Poster::None {
        Credentials::Missing
    } else {
        Credentials::Found
    };

    match service::sign_in(credentials, Screen::of(&std::io::stdin())) {
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
        &[paths::home(), paths::default_home(), paths::legacy_home()],
    ) {
        say(&line);
    }
}

/// An agent can point `SHIPCHECK_HOME` at a folder it wrote, so only a person passes `--home-ok`
fn refuse_moved_home(moved: MovedHome) -> Result<(), String> {
    if paths::home_overridden() && moved == MovedHome::Refused {
        return Err(format!(
            "SHIPCHECK_HOME is {}, not the default {}, so its results may not come from runs. A person who set it can pass --home-ok",
            paths::home().display(),
            paths::default_home().display()
        ));
    }

    Ok(())
}

/// What `run`, `status` and `logs` test: the `named` revision, or with none the working copy, or the
/// index with `--staged`, as a commit no ref names, else HEAD when it has no change. Named when not
/// a commit
fn subject(
    cwd: &Path,
    named: Option<&str>,
    tree: Tree,
) -> Result<(String, Option<&'static str>), String> {
    if let Some(rev) = named {
        return Ok((rev.to_string(), None));
    }

    let what = match tree {
        Tree::Staged => "the index",
        Tree::Working => "the working copy",
    };

    Ok(match run::snapshot(cwd, tree)? {
        Some(sha) => (sha, Some(what)),
        None => ("HEAD".to_string(), None),
    })
}

/// Whether the command was given `--base`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Base {
    Given,
    Default,
}

/// Without `--base`, a tree whose key has nothing to show takes the key of its last recorded run
fn last_run_key(plan: &mut run::Plan, base: Base, has: impl Fn(&str) -> bool) -> Option<String> {
    if base == Base::Given || has(&plan.key) {
        return None;
    }

    let repo = git::repo_id(&plan.repo).ok()?;
    let (key, base) = run::last_run(&paths::home(), &repo, &plan.tree, has)?;
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
        .map(|f| ui::printable(f))
        .collect::<Vec<_>>()
        .join(", ");

    if files.len() > 20 {
        let _ = write!(names, " and {} more", files.len() - 20);
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
fn commit(cwd: &Path, git_args: &[String], failed: Failed) -> Result<ExitCode, String> {
    let repo = toplevel(cwd)?;

    if let Some(gap) = setup::install_gap(&repo) {
        eprintln!("{}", Style::stderr().yellow(&format!("warning: {gap}")));
    }

    // Plain `git commit` still works on it, the hooks never block git
    if let Some(branch) = run::on_default_branch(&repo, &base::command_base("commit", &repo), None)
    {
        return Err(format!(
            "{branch} is the default branch, PRs carry shipcheck's checks: `git switch -c <branch>`, then `shipcheck commit`"
        ));
    }

    let tested = run::last_tested(&repo).ok_or("no `shipcheck run` of this working copy yet")?;
    let sha = run::commit(cwd, &tested, git_args, failed)?;
    println!(
        "committed {}, the tree that {} in `shipcheck run`",
        &sha[..12],
        if tested.passed { "passed" } else { "failed" }
    );
    Ok(ExitCode::SUCCESS)
}

/// The line of this repository's last refused post, None once a post succeeded
// The default branch gets hosted CI after each merge, so a push to it starts no run
fn pushes_to_base(remote_ref: &str, base: &str) -> bool {
    let branch = base.strip_prefix("origin/").unwrap_or(base);
    remote_ref.trim_start_matches("refs/heads/") == branch
}

fn unposted(repo: &Path) -> Option<String> {
    background::unposted(&git::repo_id(repo).ok()?)
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
    let repo = git::repo_id(&plan.repo)?;
    let record = background::record(&repo, name);
    let secrets = run::secret_values(&repo).unwrap_or_default();
    let pid = std::process::id();
    let first = AtomicU64::new(0);
    let cached = run::lookup(plan).filter(|e| e.passed);
    let mut hit = cached.is_some();
    let refused = std::sync::Mutex::new(None);
    let send = |body: &serde_json::Value, id| {
        let sent = sender.send(body, id).inspect_err(|e: &String| {
            if let Ok(mut r) = refused.lock() {
                *r = Some(e.clone());
            }
        })?;

        if body["status"] == "completed" {
            let _ =
                first.compare_exchange(0, clock::unix_now(), Ordering::Relaxed, Ordering::Relaxed);
        }

        Ok(sent)
    };
    // The daemon stops a run whose notes stop, which only progress writes
    let done = AtomicBool::new(false);
    let beat = background::beat_path(&repo, pid);
    let logs = [paths::log_dir(&plan.key), paths::partial_log_dir(&plan.key)];
    let posted = std::thread::scope(|s| {
        s.spawn(|| background::heartbeat(&beat, &logs, &done, background::BEAT));
        let posted = background::post_run(
            &send,
            &|| sender.find_checks(&plan.sha),
            &plan.sha,
            &check::names(plan),
            cached,
            |jobs| {
                // Another process running this tree is this commit's run, so its checks show running
                if run::running(&plan.key) {
                    run::HAS_SLOT.store(true, Ordering::Relaxed);
                }

                let (_key, waited) = run::lock_tree(&plan.key)?;

                if let Some(entry) = run::reusable(plan, waited) {
                    hit = true;
                    Ok(entry)
                } else {
                    // The error becomes the checks' log, so it gets the masking a job log gets
                    let mut entry =
                        run::run_reporting(plan, &mut |line| say(&ui::printable(line)), jobs)
                            .map_err(|e| check::mask(e, &secrets))?;

                    // A fresh pass is checked like a stored one before it is posted, and fails closed
                    if entry.passed && !run::holds(plan, &entry) {
                        say(
                            "the run's tree, base or logs do not match its result, posting a failure",
                        );
                        run::fail_closed(&mut entry);
                    }

                    Ok(entry)
                }
            },
            &run::HAS_SLOT,
            |entry| check::final_bodies(plan, entry, Tails::Shown),
            || background::current(&record, pid),
            &background::WAITS,
            Some(&|job: &run::JobDone| check::job_body(plan, job)),
        );
        done.store(true, Ordering::Relaxed);
        posted
    });
    // Only a refused post is remembered, a run that could not run is not one
    rec.post_error = posted
        .as_ref()
        .err()
        .and_then(|_| refused.lock().ok()?.clone());
    let posted = posted?;
    rec.cache_hit = hit;
    rec.first_check = Some(first.load(Ordering::Relaxed)).filter(|t| *t > 0);
    Ok(posted)
}

/// Whether a run's record may go to the service
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Upload {
    On,
    Off,
}

/// Appends the run to this machine's records, read by `shipcheck history`, and with `Upload::On`
/// uploads it as `[team] records` says. A record never fails a run
fn keep_record(plan: &run::Plan, entry: &run::Entry, record: &Record, logs: &Path, upload: Upload) {
    let Ok(repo) = git::repo_id(&plan.repo) else {
        return;
    };
    let secrets = run::secret_values(&repo).unwrap_or_default();
    let line = record::line(&repo, plan, entry, record, logs, &secrets);
    record::keep(&repo, &line);

    if upload == Upload::On {
        record::share(plan, &repo, &line);
    }
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
/// `wait` and `status` exit with this while the run is queued or going
const STILL_GOING: u8 = 3;

/// `wait` exits with this when the tree has no run and no result
const NO_RUN: u8 = 4;

/// How long `run --async` waits for the run it started to take its tree's lock
const ASYNC_START: Duration = Duration::from_secs(60);

/// This process's row of the machine's queue
fn own_run(row: &serde_json::Value) -> bool {
    row["pid"] == std::process::id()
}

/// `90`, `90s`, `10m` or `1h`
fn duration(text: &str) -> Result<Duration, String> {
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => text.split_at(at),
        None => (text, "s"),
    };
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => {
            return Err(format!(
                "--timeout {text}: use a number with s, m or h, as 10m"
            ));
        }
    };
    let number: u64 = number
        .parse()
        .map_err(|_| format!("--timeout {text}: use a number with s, m or h, as 10m"))?;
    Ok(Duration::from_secs(number.saturating_mul(scale)))
}

/// Where the run of `key` stands, without waiting: queued, running, starting while it holds the
/// tree's lock and no place in the queue yet, finished with its verdict, or none
fn run_state(key: &str) -> serde_json::Value {
    let rows = queue::list(&paths::home().join("runners"), clock::unix_now());
    let mut state = serde_json::json!({"id": queue::id(key), "key": key, "result": null});

    if let Some(row) = rows.iter().find(|r| r["key"] == key) {
        for field in [
            "state",
            "position",
            "jobs",
            "elapsed_s",
            "started_unix",
            "queued_unix",
        ] {
            state[field] = row[field].clone();
        }
    } else if run::running(key) {
        state["state"] = "starting".into();
    } else if let Some(entry) = run::cached(key) {
        state["state"] = "finished".into();
        state["passed"] = entry.passed.into();
    } else {
        state["state"] = "none".into();
    }

    state
}

/**
 * `shipcheck run --async`: starts the run as its own process group and returns once that run holds
 * the tree's lock, so a `shipcheck wait` right after finds it. The run takes its own snapshot of
 * the working copy before it takes the lock, so edits made after this returns are not in it. A tree
 * that passed or already runs starts nothing, and its id names that result or run
 **/
fn detached(
    cwd: &Path,
    plan: &run::Plan,
    args: &[String],
    format: Format,
    say: &dyn Fn(&str),
) -> Result<ExitCode, String> {
    let id = queue::id(&plan.key);
    let tree = &plan.tree[..12];
    let dir = paths::home().join("async");
    let log = dir.join(format!("{id}.log"));
    let attached = run::running(&plan.key);
    let stored = run::lookup(plan).is_some_and(|e| e.passed);

    if !attached && !stored {
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let out = std::fs::File::create(&log).map_err(|e| format!("{}: {e}", log.display()))?;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut child = Command::new(exe)
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(out.try_clone().map_err(|e| e.to_string())?)
            .stderr(out)
            .process_group(0)
            .spawn()
            .map_err(|e| e.to_string())?;
        let started = Instant::now();

        while !run::running(&plan.key) {
            // A run that ended at once stored its result, or says in its log why it could not run
            if child.try_wait().is_ok_and(|ended| ended.is_some()) {
                break;
            }

            if started.elapsed() > ASYNC_START {
                return Err(format!(
                    "the run did not start on tree {tree} in {}s, its output is {}",
                    ASYNC_START.as_secs(),
                    log.display()
                ));
            }

            std::thread::sleep(Duration::from_millis(50));
        }
    }

    let state = run_state(&plan.key);
    let at = match state["state"].as_str().unwrap_or_default() {
        "queued" => format!("queued at position {}", state["position"]),
        "finished" if stored => "already passed".to_string(),
        "finished" => "finished".to_string(),
        "none" => format!("ended without a result, its output is {}", log.display()),
        _ if attached => "already going".to_string(),
        _ => "started in the background".to_string(),
    };
    say(&format!(
        "run {id} of tree {tree}: {at}, `shipcheck wait {id}` waits for its result"
    ));

    if format == Format::Json {
        let mut state = state;
        state["tree"] = plan.tree.clone().into();
        state["log"] = (!attached && !stored)
            .then(|| log.display().to_string())
            .into();
        println!("{state}");
    }

    Ok(ExitCode::SUCCESS)
}

/// How often a run that is not at a terminal says where it stands
const HEARTBEAT: Duration = Duration::from_secs(60);

/// Whether a run away from a terminal says where it stands each `HEARTBEAT`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Heartbeat {
    On,
    Off,
}

/**
 * Runs `run` while a thread shows where it stands: the live line at a terminal, and otherwise one
 * line each `HEARTBEAT` with `Heartbeat::On`. It reads the run's own row of the machine's queue
 **/
fn progress<T>(
    live: &Live,
    heartbeat: Heartbeat,
    mine: &(dyn Fn(&serde_json::Value) -> bool + Sync),
    run: impl FnOnce() -> T,
) -> T {
    let done = AtomicBool::new(false);
    let standing = |secs: u64| {
        let rows = queue::list(&paths::home().join("runners"), clock::unix_now());
        queue::standing(&rows, mine, secs)
    };

    std::thread::scope(|s| {
        s.spawn(|| {
            let started = Instant::now();
            let (mut frame, mut beats, mut at) = (0, 0, String::new());

            while !done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                let elapsed = started.elapsed();

                if live.screen == Screen::Terminal {
                    // The queue is read once a second, the spinner turns ten times
                    if frame % 10 == 0 {
                        at = standing(elapsed.as_secs());
                    }

                    live.show(frame, &at);
                    frame += 1;
                } else if heartbeat == Heartbeat::On
                    && elapsed.as_secs() / HEARTBEAT.as_secs() > beats
                {
                    beats = elapsed.as_secs() / HEARTBEAT.as_secs();
                    live.line(&standing(elapsed.as_secs()));
                }
            }

            live.clear();
        });
        let out = run();
        done.store(true, Ordering::Relaxed);
        out
    })
}

/// Each repository's typical run time on this machine, read from its records once
fn typical_times() -> impl FnMut(&str) -> Option<u64> {
    let mut known = std::collections::BTreeMap::new();

    move |repo: &str| {
        *known.entry(repo.to_string()).or_insert_with(|| {
            let runs = history::runs(&paths::home(), repo, usize::MAX);
            history::typical(&runs).map(|t| t.median_s)
        })
    }
}

/// The line `shipcheck run` prints before a run starts, and its numbers for `--json`, none
/// without a full run in this repository's records
fn estimate(plan: &run::Plan) -> (String, Option<serde_json::Value>) {
    let runs = git::repo_id(&plan.repo)
        .map_or_default(|repo| history::runs(&paths::home(), &repo, usize::MAX));
    let Some(typical) = history::typical(&runs) else {
        return (
            "estimate: none yet, this machine has no full run of this repository on record".into(),
            None,
        );
    };
    let mut rows = queue::list(&paths::home().join("runners"), clock::unix_now());
    let arrival = if std::env::var_os("SHIPCHECK_TRIGGER").is_none() {
        Arrival::Cli
    } else {
        Arrival::Background
    };
    let wait = queue::forecast(&mut rows, run::max_runs(), arrival, &mut typical_times());
    let mut numbers = serde_json::json!(typical);
    numbers["queue"] = serde_json::json!(wait);

    (queue::estimate_line(&typical, &wait), Some(numbers))
}

/// How a command prints its result
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Json,
    Text,
}

fn report(
    entry: &run::Entry,
    snapshot: Option<&str>,
    format: Format,
    coverage: Coverage,
) -> ExitCode {
    report_with(entry, snapshot, format, coverage, None)
}

/// [`report`] with the estimate the run printed at its start, for `--json`
fn report_with(
    entry: &run::Entry,
    snapshot: Option<&str>,
    format: Format,
    coverage: Coverage,
    estimate: Option<serde_json::Value>,
) -> ExitCode {
    let logs = if coverage == Coverage::Partial {
        paths::partial_log_dir(&entry.key)
    } else {
        paths::log_dir(&entry.key)
    };
    let workflows = report::workflows(entry, &logs);
    let tested = snapshot.map_or(entry.sha.as_str(), |w| w.trim_start_matches("the "));

    if format == Format::Json {
        let mut result = report::json(entry, tested, &workflows, coverage);
        result["estimate"] = estimate.into();
        println!("{result}");
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
        print!("{}", ui::printable(&String::from_utf8_lossy(&log)));
    }

    Ok(())
}

#[cfg(test)]
mod tests;
