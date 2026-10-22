use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{Outcome, RunRequest, Runner};
use crate::config;

/// The one "workflow" of a host run, its jobs are the checks
pub const HOST: &str = "host";

/// Runs the owner's `[host]` checks natively, as the user, each in its own process group
pub struct Host {
    config: config::Host,
    /// Where `versions` run, so a toolchain file in the repository picks the version
    repo: PathBuf,
}

impl Host {
    pub fn new(config: config::Host, repo: &Path) -> Self {
        Host {
            config,
            repo: repo.to_path_buf(),
        }
    }
}

/// What every check gets from the user's environment, besides the names `[host] env` adds.
/// `HOME` and `TMPDIR` are replaced by the jail's
pub const BASE_ENV: &[&str] = &["PATH", "USER", "LOGNAME", "LANG", "LC_ALL", "SHELL", "TERM"];

/// Names `[host] env` may not pass, since they carry credentials or point git at another repository
pub fn refused_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper == "SSH_AUTH_SOCK"
        || ["TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
            .iter()
            .any(|s| upper.contains(s))
        || upper.ends_with("_KEY")
        || ["AWS_", "GOOGLE_", "AZURE_", "GIT_", "SHIPCHECK_"]
            .iter()
            .any(|p| upper.starts_with(p))
}

/// Whether a variable from the user's environment reaches a check: the base list and the owner's
/// names, never a refused one
pub fn keeps_var(name: &str, extra: &[String]) -> bool {
    (BASE_ENV.contains(&name) || extra.iter().any(|e| e == name)) && !refused_env(name)
}

/// The OS sandbox host checks run in, an error naming the fix when this machine has none
pub fn sandbox() -> Result<&'static str, String> {
    if cfg!(target_os = "macos") && Path::new(SANDBOX_EXEC).exists() {
        return Ok("sandbox-exec");
    }

    if cfg!(target_os = "linux") {
        let ran = Command::new("bwrap")
            .args([
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--unshare-pid",
                "true",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        return match ran {
            Ok(s) if s.success() => Ok("bwrap"),
            Err(_) => Err("host mode needs bubblewrap: `sudo apt install bubblewrap` or `sudo dnf install bubblewrap`".into()),
            Ok(_) => Err(userns_fix(
                fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
                    .ok()
                    .as_deref(),
                Path::new(BWRAP_PROFILE).exists(),
            )),
        };
    }

    Err("host mode needs an OS sandbox: sandbox-exec on macOS or bubblewrap on Linux".into())
}

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The AppArmor profile that lets only bwrap create user namespaces, from apparmor's extra profiles
const BWRAP_PROFILE: &str = "/etc/apparmor.d/bwrap-userns-restrict";

/**
 * Why bwrap could not make its user namespace, and the fix. Ubuntu 23.10 and later restrict
 * unprivileged user namespaces with AppArmor, and a profile for bwrap alone lifts that for it
 **/
fn userns_fix(restricted: Option<&str>, profile: bool) -> String {
    let load = format!("sudo apparmor_parser -r {BWRAP_PROFILE}");

    match (restricted.map(str::trim), profile) {
        (Some("1"), false) => format!(
            "AppArmor stops bwrap from creating user namespaces. Allow bwrap alone: `sudo apt install apparmor-profiles && sudo cp /usr/share/apparmor/extra-profiles/bwrap-userns-restrict /etc/apparmor.d/ && {load}`"
        ),
        (Some("1"), true) => format!(
            "AppArmor stops bwrap from creating user namespaces, and {BWRAP_PROFILE} is not loaded: `{load}`"
        ),
        _ => "bwrap cannot create a user namespace: check that `sysctl user.max_user_namespaces` is above 0 and bwrap is not setuid".into(),
    }
}

/**
 * What a check reads of the user's home: the binaries and installed versions of each toolchain
 * manager, never its config. A fixed list, since a `PATH` or `RUSTUP_HOME` from the caller could
 * name any directory
 **/
const TOOLCHAINS: &[&str] = &[
    ".cargo/bin",
    ".rustup/toolchains",
    ".rustup/settings.toml",
    ".rustup/update-hashes",
    ".nvm/versions",
    ".fnm/node-versions",
    ".volta/bin",
    ".volta/tools/image",
    ".bun/bin",
    ".deno/bin",
    ".pyenv/bin",
    ".pyenv/libexec",
    ".pyenv/shims",
    ".pyenv/versions",
    ".rbenv/bin",
    ".rbenv/libexec",
    ".rbenv/shims",
    ".rbenv/versions",
    ".asdf/bin",
    ".asdf/lib",
    ".asdf/shims",
    ".asdf/installs",
    ".elan/bin",
    ".elan/toolchains",
    ".ghcup/bin",
    ".ghcup/ghc",
    "go/bin",
    ".local/bin",
    ".local/share/uv/python",
    ".local/share/mise/installs",
    ".local/share/mise/shims",
    ".local/share/pnpm",
    "Library/pnpm",
];

/**
 * The macOS services a check may look up, all others are denied. Found by running cargo, npm, node,
 * uv, go, cc, git and curl under a profile that denies every lookup: user and group lookups, the
 * system's notifications, logging, temp dirs, the clock, DNS and proxy settings and trustd, which
 * checks TLS certificates for curl and Go. Not securityd, which holds the keychain
 **/
const MACH_SERVICES: &[&str] = &[
    "com.apple.system.opendirectoryd.libinfo",
    "com.apple.system.opendirectoryd.membership",
    "com.apple.system.notification_center",
    "com.apple.logd",
    "com.apple.diagnosticd",
    "com.apple.bsd.dirhelper",
    "com.apple.timed.xpc",
    "com.apple.SystemConfiguration.configd",
    "com.apple.SystemConfiguration.DNSConfiguration",
    "com.apple.trustd",
    "com.apple.trustd.agent",
];

/// Credentials in the user's home, hidden again in case a toolchain path above reaches one
const SECRETS: &[&str] = &[
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".cargo/config",
    ".cargo/config.toml",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".config/pip",
    ".config/uv",
    ".docker/config.json",
    ".ssh",
    ".aws",
    ".config/gh",
];

/// What a check may touch: it writes its clone, a home made new for each run and the repository's
/// tool caches, and reads the system, its toolchains and `readable`
pub struct Jail {
    pub writable: Vec<PathBuf>,
    pub readable: Vec<PathBuf>,
    pub home: PathBuf,
    pub cache: PathBuf,
}

/// The path a symlink resolves to, which is what the sandbox matches, or the path when it is missing
fn resolved(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

impl Jail {
    /// The user's home, which a check cannot read but for its toolchains, an error when unknown
    fn user() -> Result<PathBuf, String> {
        let user = crate::run::user_dir();

        if user.as_os_str().is_empty() {
            return Err(
                "no home directory in the passwd entry, so the sandbox cannot hide it".into(),
            );
        }

        Ok(resolved(user))
    }

    /// The toolchain paths in the user's home that exist, and `readable`
    fn readable(&self, user: &Path) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = TOOLCHAINS
            .iter()
            .map(|t| user.join(t))
            .filter(|p| p.exists())
            .map(resolved)
            .filter(|p| p.starts_with(user))
            .collect();
        found.extend(self.readable.iter().cloned());
        found.sort();
        found.dedup();
        found
    }

    /// Credentials hidden even inside readable paths: the user's, shipcheck's own and the App key
    fn hidden(user: &Path) -> Vec<PathBuf> {
        let home = crate::run::home();
        let mut hidden: Vec<PathBuf> = SECRETS.iter().map(|h| user.join(h)).collect();
        hidden.extend(["token", "app.pem", "secrets"].map(|h| home.join(h)));
        hidden.extend(std::env::var_os("SHIPCHECK_APP_KEY_FILE").map(PathBuf::from));
        hidden.into_iter().map(resolved).collect()
    }

    /**
     * The `sandbox-exec` profile. Writes only to the jail. The user's home is unreadable but for the
     * toolchains and the jail. Unix sockets connect only in the jail and to the DNS resolver, so
     * Docker's or an agent's socket is out of reach. Mach services only from [`MACH_SERVICES`], and
     * no Apple events, so no copy of `open`, `osascript` or `launchctl` reaches launchd, Launch
     * Services or another app, which would start a process outside the sandbox
     **/
    fn profile(&self) -> Result<String, String> {
        let user = Self::user()?;
        let quote = |p: &Path| {
            p.to_str()
                .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
                .ok_or_else(|| format!("{}: not UTF-8", p.display()))
        };
        let subpaths = |paths: &[PathBuf]| -> Result<String, String> {
            paths
                .iter()
                .map(|p| quote(p).map(|q| format!(" (subpath {q})")))
                .collect()
        };
        let writable = subpaths(&self.writable)?;
        let readable = subpaths(&self.readable(&user))?;
        let hidden = subpaths(&Self::hidden(&user))?;
        let user = quote(&user)?;
        let mach: String = MACH_SERVICES
            .iter()
            .map(|s| format!(" (global-name \"{s}\")"))
            .collect();
        Ok(format!(
            "(version 1)\n(allow default)\n\
             (deny file-write*)\n\
             (allow file-write*{writable} (literal \"/dev/null\") (regex #\"^/dev/fd/\"))\n\
             (deny file-read-data (subpath {user}))\n\
             (allow file-read-data{readable}{writable})\n\
             (deny file-read*{hidden})\n\
             (deny network-outbound (remote unix-socket))\n\
             (allow network-outbound (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))\n\
             (allow network-outbound (remote unix-socket{writable}))\n\
             (deny appleevent-send)\n\
             (deny mach-lookup)\n\
             (allow mach-lookup{mach})\n"
        ))
    }

    /**
     * `bwrap` arguments. The root read-only, then new empty `/tmp`, `/run` and user home, so no
     * Unix socket of the host is in reach by path, then the toolchains read-only and the jail
     * writable. The host's network, and a pid namespace and session that die with the check
     **/
    fn bwrap_args(&self) -> Result<Vec<std::ffi::OsString>, String> {
        let user = Self::user()?;
        let mut args: Vec<std::ffi::OsString> = [
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--unshare-pid",
            "--die-with-parent",
            "--new-session",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/run",
        ]
        .map(Into::into)
        .to_vec();

        if fs::symlink_metadata("/var/run").is_ok_and(|m| m.is_dir()) {
            args.extend(["--tmpfs".into(), "/var/run".into()]);
        }

        args.extend(["--tmpfs".into(), user.clone().into()]);
        // systemd-resolved links /etc/resolv.conf into /run
        let resolv = resolved(PathBuf::from("/etc/resolv.conf"));

        if resolv.starts_with("/run") {
            args.extend(["--ro-bind".into(), resolv.clone().into(), resolv.into()]);
        }

        for path in self.readable(&user) {
            args.extend(["--ro-bind".into(), path.clone().into(), path.into()]);
        }

        for path in Self::hidden(&user) {
            match fs::metadata(&path) {
                Ok(m) if m.is_dir() => args.extend(["--tmpfs".into(), path.into()]),
                Ok(_) => args.extend(["--ro-bind".into(), "/dev/null".into(), path.into()]),
                Err(_) => {}
            }
        }

        for path in &self.writable {
            args.extend(["--bind".into(), path.clone().into(), path.clone().into()]);
        }

        // Read-only once every mount point in it exists
        args.extend(["--remount-ro".into(), user.into()]);
        Ok(args)
    }

    /// `sh -c` in the OS sandbox with the filtered environment, in a session of its own with no
    /// controlling terminal, so a timeout stops all of it and it cannot type into the terminal
    fn shell(&self, command: &str, dir: &Path, extra: &[String]) -> Result<Command, String> {
        let mut cmd = match sandbox()? {
            "sandbox-exec" => {
                let mut cmd = Command::new(SANDBOX_EXEC);
                cmd.arg("-p").arg(self.profile()?);
                cmd
            }
            _ => {
                let mut cmd = Command::new("bwrap");
                cmd.args(self.bwrap_args()?).arg("--");
                cmd
            }
        };
        let cache = |dir: &str| self.cache.join(dir);
        let rustup = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::run::user_dir().join(".rustup"));
        cmd.args(["sh", "-c", command])
            .current_dir(dir)
            .env_clear()
            .envs(std::env::vars_os().filter(|(k, _)| keeps_var(&k.to_string_lossy(), extra)))
            .env("HOME", &self.home)
            .env("TMPDIR", &self.home)
            // Toolchains stay where rustup put them, read-only. Caches move to the jail
            .env("RUSTUP_HOME", rustup)
            .env("CARGO_HOME", cache("cargo"))
            .env("npm_config_cache", cache("npm"))
            .env("UV_CACHE_DIR", cache("uv"))
            .env("XDG_CACHE_HOME", cache("xdg"))
            .env("GOPATH", cache("go"))
            .env("GOCACHE", cache("go-build"))
            .env("CI", "true")
            .stdin(Stdio::null());
        // Safe: setsid is async-signal-safe and changes only this child's session
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }

                Ok(())
            });
        }
        Ok(cmd)
    }
}

/**
 * The tool caches host checks share, per repository, written only by checks. Runs of the base's
 * tree get one and every other tree another, so a branch cannot plant a file a run of main reads
 **/
fn cache_dir(repo_id: Option<&str>) -> PathBuf {
    let dir = crate::run::home().join("hostcache");
    repo_id.map_or(dir.clone(), |id| dir.join(id))
}

/// The canonical path, which `sandbox-exec` matches against, of a directory shipcheck owns
fn real_dir(dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(dir)
        .and_then(|_| dir.canonicalize())
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// A new home for one command: an unpredictable name, mode 0700, removed when dropped. In the
/// system's temp dir, since a Unix socket path under shipcheck's home could pass macOS's 104 bytes
fn temp_home() -> Result<(tempfile::TempDir, PathBuf), String> {
    let dir = tempfile::Builder::new()
        .prefix("shipcheck-")
        .tempdir()
        .map_err(|e| format!("temp home: {e}"))?;
    let path = dir.path().canonicalize().map_err(|e| e.to_string())?;
    Ok((dir, path))
}

fn kill_group(pgid: u32) {
    // Safe: killpg only sends a signal, ESRCH when the group is gone is fine
    unsafe {
        libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
    }
}

/// Runs a command until it exits or the timeout passes, then kills its whole process group either
/// way, so nothing it started outlives it. None on timeout
pub fn run_command(
    command: &str,
    dir: &Path,
    jail: &Jail,
    extra: &[String],
    log: &Path,
    timeout: Duration,
) -> Result<Option<i32>, String> {
    let mut file = fs::File::create(log).map_err(|e| format!("{}: {e}", log.display()))?;
    std::io::Write::write_all(&mut file, format!("$ {command}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let out = file.try_clone().map_err(|e| e.to_string())?;
    let mut child = jail
        .shell(command, dir, extra)?
        .stdout(out)
        .stderr(file)
        .spawn()
        .map_err(|e| format!("sandbox: {e}"))?;
    let deadline = Instant::now() + timeout;

    let exit = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break Some(
                status
                    .code()
                    .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            );
        }

        if Instant::now() >= deadline {
            kill_group(child.id());
            let _ = child.wait();
            break None;
        }

        std::thread::sleep(Duration::from_millis(50));
    };

    kill_group(child.id());
    Ok(exit)
}

/// How long one `versions` command may take, since every plan, the daemon's included, runs them
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// The stdout of one `versions` command, run in the sandbox with a home of its own, an error when
/// it fails or outlives the timeout
fn version(
    command: &str,
    dir: &Path,
    extra: &[String],
    timeout: Duration,
) -> Result<String, String> {
    let fail = |why: String| format!("shipcheck.toml [host] versions: {command:?} {why}");
    let (_dir, home) = temp_home().map_err(fail)?;
    // The developer's checkout is readable so a toolchain file in it picks the version, never a home
    let repo = real_dir(dir).map_err(fail)?;

    if crate::run::user_dir().starts_with(&repo) {
        return Err(fail(format!(
            "runs in {}, which holds the home",
            repo.display()
        )));
    }

    let jail = Jail {
        writable: vec![home.clone()],
        readable: vec![repo],
        cache: home.clone(),
        home,
    };
    jail.shell(command, dir, extra)
        .and_then(|cmd| wait_for_stdout(cmd, timeout))
        .map_err(fail)
}

fn wait_for_stdout(mut cmd: Command, timeout: Duration) -> Result<String, String> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;

    // A version line fits the pipe's buffer, so waiting before reading cannot block on a full pipe
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }

        if Instant::now() >= deadline {
            kill_group(child.id());
            let _ = child.wait();
            return Err(format!("took over {}s", timeout.as_secs_f32()));
        }

        std::thread::sleep(Duration::from_millis(10));
    };

    kill_group(child.id());

    if !status.success() {
        return Err(format!("failed with {status}"));
    }

    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().ok_or("no stdout")?, &mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

impl Runner for Host {
    /// The kind, then each `versions` command with its output
    fn identity(&self) -> Result<String, String> {
        let mut id = HOST.to_string();

        for command in &self.config.versions {
            let out = version(command, &self.repo, &self.config.env, VERSION_TIMEOUT)?;
            id.push_str(&format!("\0{command}\0{}", out.trim()));
        }

        Ok(id)
    }

    fn prepare(&self, workflow: &str, _file: &str) -> Result<String, String> {
        Ok(workflow.to_string())
    }

    fn preflight(&self) -> Result<(), String> {
        sandbox().map(|_| ())
    }

    /// The check in the sandbox: it writes only to its clone, a new home and the repository's cache
    fn run(&self, request: &RunRequest, log: &Path) -> Result<Outcome, String> {
        let command = self
            .config
            .checks
            .get(request.job)
            .ok_or_else(|| format!("no host check {}", request.job))?;
        let (_dir, home) = temp_home()?;
        let cache = real_dir(&cache_dir(Some(request.repo_id)).join(if request.on_base {
            "base"
        } else {
            "branches"
        }))?;
        let jail = Jail {
            writable: vec![real_dir(request.run_dir)?, home.clone(), cache.clone()],
            readable: Vec::new(),
            cache,
            home,
        };
        let exit = run_command(
            command,
            request.run_dir,
            &jail,
            &self.config.env,
            log,
            request.timeout,
        )?;
        Ok(Outcome {
            exit,
            unsupported_jobs: Vec::new(),
            outputs: BTreeMap::new(),
            shards: None,
        })
    }

    /// `run` kills each check's process group before it returns, so nothing is left
    fn reclaim(&self, _run_dir: &Path) {}

    fn clean(&self, repo_id: Option<&str>) -> Vec<String> {
        let dir = cache_dir(repo_id);

        if dir.exists() && fs::remove_dir_all(&dir).is_ok() {
            return vec![format!("removed {}", dir.display())];
        }

        Vec::new()
    }
}

/**
 * Files that can change what a host check's command does, such as `npm test` reading package.json.
 * Returns the part that matters, so a dependency bump in the same file is not named. A file that
 * does not parse counts whole
 **/
pub fn check_definition(path: &str, text: &str) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let in_cargo_dir = path == ".cargo/config.toml"
        || path == ".cargo/config"
        || path.ends_with("/.cargo/config.toml")
        || path.ends_with("/.cargo/config");
    let whole = Some(text.to_string());

    match name {
        "package.json" => serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .map(|v| v["scripts"].to_string())
            .or(whole),
        "pyproject.toml" => toml::from_str::<toml::Table>(text)
            .ok()
            .map(|t| format!("{:?}", t.get("tool")))
            .or(whole),
        "Cargo.toml" => toml::from_str::<toml::Table>(text)
            .ok()
            .map(|mut t| {
                for deps in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    t.remove(deps);
                }

                if let Some(toml::Value::Table(ws)) = t.get_mut("workspace") {
                    ws.remove("dependencies");
                }

                if let Some(toml::Value::Table(pkg)) = t.get_mut("package") {
                    pkg.remove("version");
                }

                format!("{t:?}")
            })
            .or(whole),
        "Makefile" | "makefile" | "GNUmakefile" | "justfile" | "Justfile" | ".justfile" => whole,
        _ if in_cargo_dir => whole,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    /// A jail whose clone is `dir`, home and cache inside it
    fn jail(dir: &Path) -> Jail {
        let dir = dir.canonicalize().unwrap();
        Jail {
            writable: vec![dir.clone()],
            readable: Vec::new(),
            home: real_dir(&dir.join("home")).unwrap(),
            cache: real_dir(&dir.join("cache")).unwrap(),
        }
    }

    #[test]
    fn passes_only_the_allowlist_and_the_owners_names() {
        let extra = vec!["NODE_OPTIONS".to_string(), "NPM_TOKEN".to_string()];

        for kept in ["PATH", "LANG", "NODE_OPTIONS"] {
            assert!(keeps_var(kept, &extra), "{kept}");
        }

        // Not listed, or listed but a credential, which validation also refuses
        for dropped in [
            "SSH_AUTH_SOCK",
            "GITHUB_TOKEN",
            "NPM_TOKEN",
            "DATABASE_URL",
            "AWS_ACCESS_KEY_ID",
            "CARGO_HOME",
            "GIT_DIR",
            "HOME",
            "TMPDIR",
        ] {
            assert!(!keeps_var(dropped, &extra), "{dropped}");
        }
    }

    #[test]
    fn refuses_names_that_carry_credentials() {
        for refused in [
            "SSH_AUTH_SOCK",
            "GH_TOKEN",
            "npm_token",
            "MY_SECRET_FILE",
            "DB_PASSWORD",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "OPENAI_API_KEY",
            "AWS_PROFILE",
            "AZURE_TENANT",
            "GIT_DIR",
            "SHIPCHECK_HOME",
        ] {
            assert!(refused_env(refused), "{refused}");
        }

        for allowed in ["NODE_OPTIONS", "RUSTFLAGS", "CARGO_HOME", "KEYBOARD"] {
            assert!(!refused_env(allowed), "{allowed}");
        }
    }

    #[test]
    fn a_timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let pid = dir.path().join("pid");
        // The background sleep is a grandchild that would outlive a kill of sh alone
        let cmd = format!("sleep 30 & echo $! > {}; wait", pid.display());
        let started = Instant::now();
        let exit = run_command(
            &cmd,
            dir.path(),
            &jail(dir.path()),
            &[],
            &log,
            Duration::from_millis(300),
        )
        .unwrap();
        assert_eq!(exit, None);
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: i32 = fs::read_to_string(&pid).unwrap().trim().parse().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // Safe: signal 0 only checks that the process exists
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "the grandchild still runs"
        );
    }

    #[test]
    fn a_finished_check_leaves_nothing_running() {
        let dir = tempfile::tempdir().unwrap();
        let (log, pid) = (dir.path().join("log"), dir.path().join("pid"));
        let cmd = format!("sleep 30 & echo $! > {}; echo hi; exit 3", pid.display());
        let exit = run_command(
            &cmd,
            dir.path(),
            &jail(dir.path()),
            &[],
            &log,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(exit, Some(3));
        assert!(fs::read_to_string(&log).unwrap().contains("hi\n"));
        let pid: i32 = fs::read_to_string(&pid).unwrap().trim().parse().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
    }

    #[test]
    fn the_check_sees_no_token() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        // Safe: tests in this module do not read these variables concurrently
        unsafe {
            std::env::set_var("HOSTRUN_PROBE_TOKEN", "hunter2");
        }
        let exit = run_command(
            "env; test -z \"$HOSTRUN_PROBE_TOKEN\" && test \"$CI\" = true",
            dir.path(),
            &jail(dir.path()),
            &[],
            &log,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(exit, Some(0));
        assert!(!fs::read_to_string(&log).unwrap().contains("hunter2"));
    }

    #[test]
    fn the_profile_lets_a_check_write_only_in_its_jail() {
        let jail = Jail {
            writable: vec!["/j/clone \"x\"".into(), "/j/home".into()],
            readable: vec!["/j/repo".into()],
            home: "/j/home".into(),
            cache: "/j/home".into(),
        };
        let profile = jail.profile().unwrap();
        let paths = r#"(subpath "/j/clone \"x\"") (subpath "/j/home")"#;
        assert!(profile.contains("(allow default)\n(deny file-write*)\n"));
        assert!(profile.contains(&format!(
            "(allow file-write* {paths} (literal \"/dev/null\")"
        )));
        assert!(!profile.contains("/dev/tty"));
        let user = resolved(crate::run::user_dir());
        assert!(profile.contains(&format!(
            "(deny file-read-data (subpath \"{}\"))",
            user.display()
        )));
        assert!(profile.contains(&format!("(subpath \"/j/repo\") {paths})")));
        assert!(profile.contains("(deny network-outbound (remote unix-socket))\n"));
        assert!(profile.contains(&format!(
            "(allow network-outbound (remote unix-socket {paths}))"
        )));
        assert!(profile.contains("(deny appleevent-send)"));
        assert!(profile.contains("(deny mach-lookup)\n(allow mach-lookup (global-name"));
        assert!(!profile.contains("SecurityServer") && !profile.contains("launchservicesd"));
        let bad = Jail {
            writable: vec![PathBuf::from(std::ffi::OsString::from_vec(vec![0xff]))],
            readable: Vec::new(),
            home: "/j".into(),
            cache: "/j".into(),
        };
        assert!(bad.profile().unwrap_err().contains("not UTF-8"));
    }

    #[test]
    fn bwrap_hides_the_hosts_sockets_and_home_and_binds_the_jail() {
        let jail = Jail {
            writable: vec!["/j/clone".into(), "/j/home".into()],
            readable: Vec::new(),
            home: "/j/home".into(),
            cache: "/j/home".into(),
        };
        let args: Vec<String> = jail
            .bwrap_args()
            .unwrap()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let all = args.join(" ");
        assert_eq!(args[..3], ["--ro-bind", "/", "/"]);
        for flag in [
            "--unshare-pid",
            "--new-session",
            "--tmpfs /tmp",
            "--tmpfs /run",
        ] {
            assert!(all.contains(flag), "{flag}");
        }

        let user = resolved(crate::run::user_dir());
        assert!(all.contains(&format!("--tmpfs {}", user.display())));
        assert!(!all.contains("--share-net") && !all.contains("--unshare-net"));
        assert!(all.ends_with(&format!(
            "--bind /j/clone /j/clone --bind /j/home /j/home --remount-ro {}",
            user.display()
        )));
    }

    #[test]
    fn a_check_writes_only_in_its_jail() {
        let (dir, outside) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let log = outside.path().join("log");
        let canary = outside.path().join("canary");
        let run = |cmd: &str| {
            let exit = run_command(
                cmd,
                dir.path(),
                &jail(dir.path()),
                &[],
                &log,
                Duration::from_secs(10),
            );
            (exit.unwrap(), fs::read_to_string(&log).unwrap_or_default())
        };
        let ok = run("echo ok > inside && echo ok > \"$HOME/h\" && echo ok > \"$TMPDIR/t\"");
        assert_eq!(ok.0, Some(0), "{}", ok.1);
        assert!(dir.path().join("inside").exists());
        let escaped = run(&format!("echo x > {}", canary.display()));
        assert_ne!(escaped.0, Some(0));
        assert!(!canary.exists(), "{}", escaped.1);
        // No controlling terminal to type into
        assert_ne!(run("echo x > /dev/tty").0, Some(0));
        // A Unix socket outside the jail, such as Docker's, refuses the connection
        let sock = outside.path().join("s");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let probe = format!(
            "python3 -c 'import socket; socket.socket(socket.AF_UNIX).connect(\"{}\")'",
            sock.display()
        );
        let connected = run(&probe);
        assert_ne!(connected.0, Some(0), "{}", connected.1);
        // The user's home is unreadable but for toolchains, and unwritable
        let secret = tempfile::NamedTempFile::new_in(crate::run::user_dir()).unwrap();
        let read = run(&format!("cat '{}'", secret.path().display()));
        assert_ne!(read.0, Some(0), "{}", read.1);
        let planted =
            crate::run::user_dir().join(format!(".shipcheck-test-{}", std::process::id()));
        assert_ne!(run(&format!("touch '{}'", planted.display())).0, Some(0));
        assert!(!planted.exists());

        // Credentials, also beside a readable toolchain dir, whether or not this machine has them
        let user = crate::run::user_dir();
        let cargo = user.join(".cargo");
        let beside = cargo
            .exists()
            .then(|| tempfile::NamedTempFile::new_in(&cargo).unwrap());

        for path in [user.join(".cargo/credentials.toml"), user.join(".netrc")]
            .into_iter()
            .chain(beside.iter().map(|f| f.path().to_path_buf()))
        {
            if path.exists() {
                // The log is not printed, it would hold the file
                let read = run(&format!("cat '{}' > /dev/null", path.display()));
                assert_ne!(read.0, Some(0), "{} is readable", path.display());
            }
        }

        if let Ok(profile) = jail(dir.path()).profile() {
            for secret in [".cargo/credentials.toml", ".netrc", ".npmrc"] {
                assert!(
                    profile.contains(&format!("{}\")", resolved(user.join(secret)).display())),
                    "{secret}"
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_check_starts_no_process_outside_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let canary = std::env::temp_dir().join(format!("shipcheck-escape-{}", std::process::id()));
        let label = format!("run.shipcheck.escape-{}", std::process::id());
        let plist = format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Label</key><string>{label}</string>\
             <key>ProgramArguments</key><array><string>/usr/bin/touch</string><string>{}</string></array>\
             <key>RunAtLoad</key><true/></dict></plist>",
            canary.display()
        );
        fs::write(dir.path().join("job.plist"), plist).unwrap();
        // Safe: getuid cannot fail
        let uid = unsafe { libc::getuid() };

        for cmd in [
            "open -a TextEdit".to_string(),
            "cp /usr/bin/open ./o && ./o -a TextEdit".to_string(),
            format!(
                "/usr/bin/osascript -e 'tell application \"Terminal\" to do script \"touch {}\"'",
                canary.display()
            ),
            format!(
                "cp /bin/launchctl ./l && ./l submit -l {label} -- /usr/bin/touch {}",
                canary.display()
            ),
            format!("/bin/launchctl bootstrap gui/{uid} job.plist"),
        ] {
            let exit = run_command(
                &cmd,
                dir.path(),
                &jail(dir.path()),
                &[],
                &log,
                Duration::from_secs(10),
            )
            .unwrap();
            assert_ne!(
                exit,
                Some(0),
                "{cmd}: {}",
                fs::read_to_string(&log).unwrap()
            );
        }

        std::thread::sleep(Duration::from_millis(500));
        assert!(!canary.exists());
    }

    #[test]
    fn names_the_apparmor_fix_for_bwrap() {
        let missing = userns_fix(Some("1\n"), false);
        assert!(
            missing.contains(
                "cp /usr/share/apparmor/extra-profiles/bwrap-userns-restrict /etc/apparmor.d/"
            ),
            "{missing}"
        );
        assert!(missing.contains("apparmor_parser -r /etc/apparmor.d/bwrap-userns-restrict"));
        assert!(!missing.contains("sysctl"));
        assert!(userns_fix(Some("1"), true).contains("is not loaded"));
        assert!(userns_fix(None, false).contains("max_user_namespaces"));
        assert!(userns_fix(Some("0"), false).contains("max_user_namespaces"));
    }

    #[test]
    fn identity_holds_each_version_output() {
        let dir = tempfile::tempdir().unwrap();
        let host = |versions: &[&str]| {
            Host::new(
                config::Host {
                    checks: BTreeMap::new(),
                    versions: versions.iter().map(|v| v.to_string()).collect(),
                    env: Vec::new(),
                },
                dir.path(),
            )
            .identity()
        };
        assert_eq!(host(&[]).unwrap(), "host");
        assert_eq!(host(&["echo 1.2"]).unwrap(), "host\0echo 1.2\x001.2");
        assert_ne!(host(&["echo 1.2"]), host(&["echo 1.3"]));
        assert!(host(&["exit 1"]).unwrap_err().contains("\"exit 1\" failed"));
        let started = Instant::now();
        let slow = version("sleep 30", dir.path(), &[], Duration::from_millis(200));
        assert!(slow.unwrap_err().contains("took over"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn finds_files_that_define_checks() {
        let pkg = |scripts: &str, dep: &str| {
            format!(
                "{{\"scripts\": {{\"test\": \"{scripts}\"}}, \"dependencies\": {{\"a\": \"{dep}\"}}}}"
            )
        };
        let def = |p: &str, t: &str| check_definition(p, t);
        assert_eq!(
            def("package.json", &pkg("jest", "1")),
            def("package.json", &pkg("jest", "2"))
        );
        assert_ne!(
            def("package.json", &pkg("jest", "1")),
            def("package.json", &pkg("true", "1"))
        );
        assert_eq!(def("web/package.json", "{bad"), Some("{bad".into()));

        let cargo = |extra: &str| format!("[package]\nname = \"a\"\nversion = \"1\"\n{extra}");
        assert_eq!(
            def("Cargo.toml", &cargo("[dependencies]\nx = \"1\"\n")),
            def("Cargo.toml", &cargo("[dependencies]\nx = \"2\"\n"))
        );
        assert_ne!(
            def("Cargo.toml", &cargo("")),
            def("Cargo.toml", &cargo("[lints.clippy]\nall = \"allow\"\n"))
        );

        let py = |tool: &str| {
            format!("[project]\nname = \"a\"\n[tool.pytest.ini_options]\naddopts = \"{tool}\"\n")
        };
        assert_ne!(
            def("pyproject.toml", &py("-x")),
            def("pyproject.toml", &py("-k nothing"))
        );

        for path in [
            "Makefile",
            "sub/justfile",
            ".cargo/config.toml",
            "a/.cargo/config",
        ] {
            assert!(def(path, "x").is_some(), "{path}");
        }

        for path in [
            "src/main.rs",
            "README.md",
            "cargo/config.toml",
            "package-lock.json",
        ] {
            assert!(def(path, "x").is_none(), "{path}");
        }
    }
}
