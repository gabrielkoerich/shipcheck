use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
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

const TEMP: &[&str] = &[
    "/tmp",
    "/private/tmp",
    "/var/tmp",
    "/private/var/tmp",
    "/var/folders",
    "/private/var/folders",
    "/dev/shm",
];

/**
 * Whether a check may find tools in `dir`. Not in a temp dir or a git checkout, where an agent can
 * write a fake `cargo` that exits 0 and put it first on the `PATH` it runs `shipcheck run` with.
 * The user's home is not a checkout here, so a dotfiles repository leaves `~/.cargo/bin` usable
 **/
fn trusted_dir(dir: &Path, user: &Path) -> bool {
    let mut temp: Vec<PathBuf> = TEMP.iter().map(PathBuf::from).collect();
    temp.push(std::env::temp_dir());
    trusted_in(dir, user, &temp)
}

fn trusted_in(dir: &Path, user: &Path, temp: &[PathBuf]) -> bool {
    let real = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());

    if !dir.is_absolute()
        || temp
            .iter()
            .any(|t| dir.starts_with(t) || real.starts_with(t))
    {
        return false;
    }

    // Homebrew installs from a git checkout of its own, which an agent's sandbox cannot write
    if BREW.iter().any(|b| real.starts_with(b)) {
        return true;
    }

    let user = user.canonicalize().unwrap_or_else(|_| user.to_path_buf());

    !real
        .ancestors()
        .take_while(|a| *a != user && a.parent().is_some())
        .any(|a| a.join(".git").exists())
}

const BREW: &[&str] = &["/opt/homebrew", "/home/linuxbrew/.linuxbrew"];

/// The caller's `PATH` without the entries `trusted_dir` refuses
fn trusted_path(path: &std::ffi::OsStr, user: &Path) -> std::ffi::OsString {
    let kept: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|d| trusted_dir(d, user))
        .collect();
    or_system(std::env::join_paths(kept).unwrap_or_default())
}

// An empty `PATH` makes the shell search the current directory, which is the branch's clone
fn or_system(path: std::ffi::OsString) -> std::ffi::OsString {
    if path.is_empty() {
        return "/usr/bin:/bin:/usr/sbin:/sbin".into();
    }

    path
}

#[cfg(test)]
fn trusted_path_in(path: &std::ffi::OsStr, user: &Path, temp: &[PathBuf]) -> std::ffi::OsString {
    let kept: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|d| trusted_in(d, user, temp))
        .collect();
    std::env::join_paths(kept).unwrap_or_default()
}

/// The OS sandbox host checks run in, an error naming the fix when this machine has none
pub fn sandbox() -> Result<&'static str, String> {
    if cfg!(target_os = "macos") && Path::new(SANDBOX_EXEC).exists() {
        // macOS refuses a sandbox inside a sandbox, such as an agent's, unless that one is as strict
        static STARTS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let starts = *STARTS.get_or_init(|| {
            in_a_jail()
                || Command::new(SANDBOX_EXEC)
                    // One real rule, since macOS starts a nested profile that restricts nothing
                    .args([
                        "-p",
                        "(version 1)(allow default)(deny file-write* (literal \"/var/empty/shipcheck\"))",
                        "/usr/bin/true",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
        });

        if !starts {
            return Err(NESTED.into());
        }

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

const LAUNCHCTL: &str = "/bin/launchctl";

const NESTED: &str = "the host checks' sandbox cannot start inside another sandbox, such as an agent's, so nothing ran: run shipcheck outside it, for example by exempting the `shipcheck` command in the agent's sandbox settings. A push still gets its checks from the hook's run and the daemon, which run outside it";

/**
 * Whether this process already runs in a jail as strict as a host check's, as shipcheck's own
 * tests do inside its host checks. The kernel answers through `sandbox_check`, which job code
 * cannot fake, for what the profile denies: reading and writing the home, and Docker's socket.
 * An agent's looser sandbox fails one of them, so its checks still get their own jail
 **/
#[cfg(target_os = "macos")]
pub fn in_a_jail() -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn sandbox_check(
            pid: libc::pid_t,
            operation: *const libc::c_char,
            kind: libc::c_int,
            ...
        ) -> libc::c_int;
    }
    // From sandbox.h: no filter asks whether there is a sandbox at all, 1 filters by path
    const NONE: libc::c_int = 0;
    const PATH: libc::c_int = 1;
    let home = crate::run::user_dir();
    let denied = |operation: &str, path: &Path| {
        let (Ok(op), Ok(path)) = (
            CString::new(operation),
            CString::new(path.as_os_str().as_bytes()),
        ) else {
            return false;
        };
        // Safe: both strings outlive the call, which only reads them
        unsafe { sandbox_check(libc::getpid(), op.as_ptr(), PATH, path.as_ptr()) == 1 }
    };

    // Safe: a null operation with no filter takes no further argument
    let sandboxed = unsafe { sandbox_check(libc::getpid(), std::ptr::null(), NONE) } == 1;
    // A sandbox that passes these is reused only where results never post, since any can pass them
    crate::run::home_overridden()
        && sandboxed
        && denied("file-read-data", &home)
        && denied("file-write-data", &home.join(".shipcheck-probe"))
        && denied("network-outbound", &home.join(".docker/run/docker.sock"))
        && denied("network-outbound", Path::new("/var/run/docker.sock"))
}

#[cfg(not(target_os = "macos"))]
pub fn in_a_jail() -> bool {
    false
}

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
    ".rubies",
    ".local/share/mise/shims",
    ".local/share/pnpm",
    "Library/pnpm",
    ".avm/bin",
    ".avm/.version",
    ".local/share/solana/install",
    ".cache/solana",
    ".docker/cli-plugins",
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

/// The setuid binaries of the system, read once
// ponytail: by path only, a hard link to a setuid binary on the data volume gets past it, a setuid filter if Sandbox gets one
fn setuid() -> &'static [PathBuf] {
    use std::os::unix::fs::PermissionsExt;

    static FOUND: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();
    FOUND.get_or_init(|| {
        let dirs = ["/usr/bin", "/bin", "/usr/sbin", "/sbin", "/usr/libexec"];
        let mut found: Vec<PathBuf> = dirs
            .iter()
            .flat_map(|d| fs::read_dir(d).into_iter().flatten().flatten())
            .map(|e| e.path())
            .chain([PathBuf::from(ARD_AGENT)])
            .filter(|p| {
                fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o4000 != 0)
            })
            .map(resolved)
            .collect();
        found.sort();
        found.dedup();
        found
    })
}

/// A setuid root binary outside the scanned dirs
const ARD_AGENT: &str =
    "/System/Library/CoreServices/RemoteManagement/ARDAgent.app/Contents/MacOS/ARDAgent";

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
     * no Apple events, so no copy of `open` or `osascript` reaches Launch Services or another app,
     * which would start a process outside the sandbox. `launchctl` talks to launchd over the
     * bootstrap port, which no sandbox rule here covers, so it may not run at all. Signals reach
     * only the check's own processes, never the user's other ones or another check's. A pseudo-terminal
     * only when the check opened it, which the `com.apple.sandbox.pty` extension marks, to read as
     * to write, so the terminal shipcheck runs in is out of reach. Nothing in the temp dirs but the
     * jail's own paths
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
        // A `remote unix-socket` filter takes one path, so each writable dir gets its own
        let sockets: String = self
            .writable
            .iter()
            .map(|p| quote(p).map(|q| format!(" (remote unix-socket (subpath {q}))")))
            .collect::<Result<_, String>>()?;
        let readable = subpaths(&self.readable(&user))?;
        let hidden = subpaths(&Self::hidden(&user))?;
        // Other checks' homes and whatever else is in the temp dirs, the jail's own paths allowed below
        let mut temp: Vec<PathBuf> = TEMP.iter().map(PathBuf::from).collect();
        temp.push(std::env::temp_dir());
        let mut temp: Vec<PathBuf> = temp.into_iter().map(resolved).collect();
        temp.sort();
        temp.dedup();
        let temp = subpaths(&temp)?;
        let user = quote(&user)?;
        let mach: String = MACH_SERVICES
            .iter()
            .map(|s| format!(" (global-name \"{s}\")"))
            .collect();
        // A setuid binary runs as root, which shipcheck cannot stop when it outlives its check
        let setuid: String = setuid()
            .iter()
            .map(|p| quote(p).map(|q| format!(" (literal {q})")))
            .collect::<Result<_, String>>()?;
        // A copy of launchctl outside /bin is killed at exec, so the path is enough
        // ponytail: assumes no allowed mach service reaches launchd for the check, launch_msg and XPC need a denied lookup
        let exec = format!("(deny process-exec* (literal \"{LAUNCHCTL}\"){setuid})\n");
        Ok(format!(
            "(version 1)\n(allow default)\n\
             (deny file-write*)\n\
             (allow file-write*{writable} (literal \"/dev/null\") (regex #\"^/dev/fd/\"))\n\
             (allow file-write* (literal \"/dev/ptmx\") (require-all (regex #\"^/dev/ttys[0-9]+$\") (extension \"com.apple.sandbox.pty\")))\n\
             (deny file-read-data (subpath {user}){temp})\n\
             (allow file-read-data{readable}{writable})\n\
             (deny file-read*{hidden})\n\
             (deny file-read* (require-all (regex #\"^/dev/ttys[0-9]+$\") (require-not (extension \"com.apple.sandbox.pty\"))))\n\
             (deny network-outbound (remote unix-socket))\n\
             (allow network-outbound (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))\n\
             (allow network-outbound{sockets})\n\
             (deny appleevent-send)\n\
             (deny mach-lookup)\n\
             (allow mach-lookup{mach})\n\
             (deny signal)\n\
             (allow signal (target same-sandbox))\n\
             {exec}"
        ))
    }

    /**
     * `bwrap` arguments. The root read-only, then new empty `/tmp`, `/var/tmp`, `/run` and user
     * home, so no Unix socket of the host is in reach by path, then the toolchains read-only and the
     * jail writable. The host's network, and a pid namespace and session that die with the check
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

        // As on macOS, a check reads nothing another process left in the host's temp dirs
        for dir in ["/var/run", "/var/tmp"] {
            if fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
                args.extend(["--tmpfs".into(), dir.into()]);
            }
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

    /**
     * The trusted `PATH` and then where `go install`, `cargo install`, `npm install -g` and
     * `uv tool` or `pipx` put binaries, as GitHub's setup actions add them. Last, so a binary left
     * in a cache never shadows a tool of the user's
     **/
    fn path(&self, trusted: std::ffi::OsString) -> std::ffi::OsString {
        let bins = [
            self.cache.join("go/bin"),
            self.cache.join("cargo/bin"),
            self.home.join(NPM_PREFIX).join("bin"),
            self.home.join(".local/bin"),
        ];
        let dirs = std::env::split_paths(&trusted).chain(bins);
        std::env::join_paths(dirs).unwrap_or(trusted)
    }

    /// `sh -c` in the OS sandbox with the filtered environment, in a session of its own with no
    /// controlling terminal, so a timeout stops all of it and it cannot type into the terminal
    fn shell(&self, command: &str, dir: &Path, extra: &[String]) -> Result<Command, String> {
        let mut cmd = match sandbox()? {
            // macOS refuses a sandbox inside a sandbox, and the one around this process is as strict
            "sandbox-exec" if in_a_jail() => Command::new("/usr/bin/env"),
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
        let base = self.path(jail_path());
        let shims = self.home.join(SHIMS);
        // `gem install` goes to the home each check gets new, and the commands it installs run from
        // there, after the trusted PATH so none shadows a tool of the user's
        let gems = self.home.join(".gem");
        let dirs = Some(shims)
            .filter(|s| s.is_dir())
            .into_iter()
            .chain(std::env::split_paths(&base).collect::<Vec<_>>())
            .chain([gems.join("bin")]);
        let path = std::env::join_paths(dirs).unwrap_or(base);
        let rustup = Some(self.home.join(".rustup"))
            .filter(|r| r.is_dir())
            .unwrap_or_else(|| user_tool("RUSTUP_HOME", ".rustup"));
        cmd.args(["/bin/sh", "-c", command])
            .current_dir(dir)
            .env_clear()
            .envs(std::env::vars_os().filter(|(k, _)| keeps_var(&k.to_string_lossy(), extra)))
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("TMPDIR", &self.home)
            // Toolchains stay where rustup and avm put them, read-only. Caches move to the jail
            .env("RUSTUP_HOME", rustup)
            .env("AVM_HOME", user_tool("AVM_HOME", ".avm"))
            .env("BUN_INSTALL_CACHE_DIR", cache("bun"))
            .env("CARGO_HOME", cache("cargo"))
            // Bundler keeps a directory per Ruby version under it, and the system gem dir is read-only here
            .env("BUNDLE_PATH", cache("bundle"))
            .env("GEM_HOME", gems)
            .env("npm_config_cache", cache("npm"))
            .env("npm_config_prefix", self.home.join(NPM_PREFIX))
            .env("UV_CACHE_DIR", cache("uv"))
            .env("XDG_CACHE_HOME", cache("xdg"))
            .env("GOPATH", cache("go"))
            .env("GOCACHE", cache("go-build"))
            .env("CI", "true")
            .env("GITHUB_STEP_SUMMARY", self.home.join(STEP_SUMMARY))
            // Tests of the jail read it, since macOS refuses a sandbox inside a sandbox
            .env("SHIPCHECK_JAIL", "1")
            .stdin(Stdio::null());
        if self.home.join(SBF_SDK).is_dir() {
            cmd.env("SBF_SDK_PATH", self.home.join(SBF_SDK));
        }

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
    toolchains(&path, &crate::run::user_dir())?;
    rustup_home(&path, &user_tool("RUSTUP_HOME", ".rustup"))?;
    sbf_sdk(&path, &jail_path())?;

    if cfg!(target_os = "macos") {
        mktemp_shim(&path)?;
    }

    Ok((dir, path))
}

/// Where the jail's own commands go, first on `PATH`
const SHIMS: &str = ".shipcheck/bin";

/**
 * macOS `mktemp` with no template ignores `TMPDIR` for the per-user temp dir from `confstr`, which
 * the jail denies, and no variable moves that dir. This one gives it `-p "$TMPDIR"` then, the
 * jail's home. A template or `-p` of the caller's own runs as given
 **/
const MKTEMP: &str = r#"#!/bin/sh
for a in "$@"; do
  case $a in
    *XXX* | -p* | --tmpdir*) exec /usr/bin/mktemp "$@" ;;
  esac
done
exec /usr/bin/mktemp -p "${TMPDIR:-/tmp}" "$@"
"#;

fn mktemp_shim(home: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.join(SHIMS);
    fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
    let file = bin.join("mktemp");
    fs::write(&file, MKTEMP).map_err(|e| e.to_string())?;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

/// The prefix of `npm install -g` in the jail's home, since node's own is read-only here
const NPM_PREFIX: &str = ".npm-global";

/// Toolchains found only through `$HOME`, linked from the jail's home to the user's, readable as
/// part of `TOOLCHAINS`. Never all of `~/.docker`, whose config can name a credential store
const HOME_LINKS: &[&str] = &[".docker/cli-plugins", ".cache/solana"];

pub fn jail_path() -> std::ffi::OsString {
    trusted_path(
        &std::env::var_os("PATH").unwrap_or_default(),
        &crate::run::user_dir(),
    )
}

/// A toolchain manager's home from its variable when that is trusted, else the default in `~`
fn user_tool(var: &str, dir: &str) -> PathBuf {
    let user = crate::run::user_dir();
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| trusted_dir(p, &user))
        .unwrap_or_else(|| user.join(dir))
}

/// A rustup home of the jail's own: a link per installed toolchain and a copy of the settings.
/// cargo-build-sbf relinks its `solana` toolchain to a path under `$HOME`, which the user's
/// read-only rustup home refuses, so here the link lands in the jail and the toolchains stay as they are
fn rustup_home(home: &Path, source: &Path) -> Result<(), String> {
    let Ok(entries) = fs::read_dir(source.join("toolchains")) else {
        return Ok(());
    };
    let jail = home.join(".rustup");
    fs::create_dir_all(jail.join("toolchains")).map_err(|e| e.to_string())?;

    for entry in entries.flatten() {
        std::os::unix::fs::symlink(
            entry.path(),
            jail.join("toolchains").join(entry.file_name()),
        )
        .map_err(|e| e.to_string())?;
    }

    for file in ["settings.toml", "update-hashes"] {
        let from = source.join(file);

        if from.is_file() {
            fs::copy(&from, jail.join(file)).map_err(|e| e.to_string())?;
        } else if from.is_dir() {
            std::os::unix::fs::symlink(&from, jail.join(file)).map_err(|e| e.to_string())?;
        }
    }

    Ok(())
}

/// Where the jail's copy of Solana's SBF SDK goes, named by `SBF_SDK_PATH`
const SBF_SDK: &str = ".sbf-sdk";

/**
 * A writable copy of the SBF SDK next to the `cargo-build-sbf` on `PATH`. On every build it
 * relinks `dependencies/platform-tools` to `$HOME/.cache/solana/<version>`, and when the user's
 * read-only SDK refuses that it takes the platform tools for broken and tries to delete them
 **/
fn sbf_sdk(home: &Path, path: &std::ffi::OsStr) -> Result<(), String> {
    let Some(sdk) = std::env::split_paths(path)
        .filter_map(|d| d.join("cargo-build-sbf").canonicalize().ok())
        .filter_map(|bin| Some(bin.parent()?.join("platform-tools-sdk/sbf")))
        .find(|sdk| sdk.is_dir())
    else {
        return Ok(());
    };
    // cp -R copies links as links, and the SDK is a few hundred KB
    let copied = Command::new("cp")
        .arg("-R")
        .arg(&sdk)
        .arg(home.join(SBF_SDK))
        .status()
        .map_err(|e| format!("cp: {e}"))?;

    if !copied.success() {
        return Err(format!("could not copy {}", sdk.display()));
    }

    Ok(())
}

fn toolchains(home: &Path, user: &Path) -> Result<(), String> {
    for dir in HOME_LINKS {
        let target = user.join(dir);

        if !target.is_dir() {
            continue;
        }

        let link = home.join(dir);
        fs::create_dir_all(link.parent().unwrap_or(home)).map_err(|e| e.to_string())?;
        std::os::unix::fs::symlink(&target, &link).map_err(|e| e.to_string())?;
    }

    Ok(())
}

/**
 * Each check runs in its own session, outside the run's process group, so the SIGTERM that stops a
 * superseded background run never reaches it. The run keeps its checks' groups here, and a signal
 * handler kills them before the run dies. Atomics only, since the handler may read them mid-write
 **/
static GROUPS: [AtomicI32; 64] = [const { AtomicI32::new(0) }; 64];

/// The write end of the pipe the stopper thread waits on, -1 before it runs
static WAKE: AtomicI32 = AtomicI32::new(-1);

/**
 * Installs the handler, and a thread that does what a handler cannot: on a signal it reaps each
 * running launchd check, boots out its job and removes its record, kills the groups, then dies of it
 **/
fn install() {
    static HANDLER: std::sync::Once = std::sync::Once::new();
    HANDLER.call_once(|| {
        let mut fds = [0; 2];

        // Safe: pipe writes two descriptors into `fds`
        if unsafe { libc::pipe(fds.as_mut_ptr()) } == 0 {
            let read = fds[0];
            WAKE.store(fds[1], Ordering::SeqCst);
            std::thread::spawn(move || {
                let mut sig = 0u8;

                // Safe: one byte into `sig`, which outlives the call
                if unsafe { libc::read(read, (&mut sig as *mut u8).cast(), 1) } == 1 {
                    #[cfg(target_os = "macos")]
                    let _held = job::stop_all();
                    die(sig as libc::c_int);
                }
            });
        }

        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            // Safe: the handler calls only async-signal-safe functions
            unsafe {
                libc::signal(sig, stop as *const () as libc::sighandler_t);
            }
        }
    });
}

// ponytail: 64 slots, a run with more checks at once leaves the extra ones to outlive a signal
fn register(pgid: u32) -> Option<usize> {
    install();
    GROUPS.iter().position(|g| {
        g.compare_exchange(0, pgid as i32, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    })
}

fn kill_registered(groups: &[AtomicI32]) {
    for g in groups {
        let pgid = g.load(Ordering::SeqCst);

        if pgid > 0 {
            // Safe: killpg only sends a signal
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

extern "C" fn stop(sig: libc::c_int) {
    let wake = WAKE.load(Ordering::SeqCst);
    let byte = sig as u8;

    // Safe: write is async-signal-safe, and the byte outlives the call
    if wake >= 0 && unsafe { libc::write(wake, (&byte as *const u8).cast(), 1) } == 1 {
        return;
    }

    die(sig);
}

fn die(sig: libc::c_int) {
    kill_registered(&GROUPS);
    // Safe: back to the default action, then die of the same signal
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn kill_group(pgid: u32) {
    super::signal_group(pgid, libc::SIGKILL);
}

/// Reaps the processes host checks left in this slot when shipcheck itself died, before any
/// checkout of the slot. A no-op where checks have a pid namespace of their own
pub fn reap_slot(slot: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return job::reap_slot(slot);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = slot;
        Ok(())
    }
}

/// Whether host checks can start as launchd jobs here, which they need to be stopped whole, for doctor
#[cfg(target_os = "macos")]
pub fn launchd() -> Result<String, String> {
    if in_a_jail() {
        return Ok("not used inside a host check's own jail".into());
    }

    job::probe()
}

/// The live members of a coalition, for tests that check nothing outlived a run
#[cfg(target_os = "macos")]
pub fn coalition_members(coal: u64) -> Vec<libc::pid_t> {
    job::members(coal)
}

/**
 * macOS has no pid namespace, and a check that runs `set -m` or `setsid` leaves its process group,
 * so `killpg` misses it. launchd puts every job in a new coalition, which each child inherits and
 * no unprivileged process can leave or join, so each check runs as a job and its coalition is reaped
 **/
#[cfg(target_os = "macos")]
mod job {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    // From XNU's proc_info_private.h, readable for any pid by any user
    const PROC_PIDCOALITIONINFO: libc::c_int = 20;
    // Process states from sys/proc.h
    const SSTOP: u32 = 4;
    const SZOMB: u32 = 5;
    // How long launchd may take to run a job's script, which takes about 0.1 s
    const START: Duration = Duration::from_secs(10);
    // Rounds of a scan 1 ms apart, so a member that forks for longer than about 2 s fails the check
    const ROUNDS: usize = 2000;

    #[repr(C)]
    struct CoalitionInfo {
        id: [u64; 2],
        reserved: [u64; 3],
    }

    /// The resource coalition of a live process, None when it is gone or a zombie
    pub fn coalition(pid: libc::pid_t) -> Option<u64> {
        let mut info = CoalitionInfo {
            id: [0; 2],
            reserved: [0; 3],
        };
        let size = std::mem::size_of::<CoalitionInfo>() as libc::c_int;
        // Safe: the kernel writes at most `size` bytes into `info`
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                PROC_PIDCOALITIONINFO,
                0,
                (&mut info as *mut CoalitionInfo).cast(),
                size,
            )
        };
        (n == size && info.id[0] != 0 && state(pid).is_some_and(|s| s != SZOMB))
            .then_some(info.id[0])
    }

    fn state(pid: libc::pid_t) -> Option<u32> {
        // Safe: a plain C struct of integers, which zero is a valid value of
        let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
        // Safe: the kernel writes at most `size` bytes into `info`
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDT_SHORTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdshortinfo).cast(),
                size,
            )
        };
        (n == size).then_some(info.pbsi_status)
    }

    /// Every pid on the machine, in a buffer doubled until the list fits with room to spare
    fn all_pids() -> Vec<libc::pid_t> {
        // Safe: a null buffer only asks for the count
        let mut cap =
            unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) }.max(0) as usize + 256;

        loop {
            let mut pids: Vec<libc::pid_t> = vec![0; cap];
            let size = std::mem::size_of_val(pids.as_slice()) as libc::c_int;
            // Safe: the kernel writes at most `size` bytes into `pids`
            let n =
                unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) }.max(0) as usize;

            if n < cap {
                pids.truncate(n);
                return pids;
            }

            cap *= 2;
        }
    }

    pub fn members(coal: u64) -> Vec<libc::pid_t> {
        let mut pids = all_pids();
        pids.retain(|&p| p > 1 && coalition(p) == Some(coal));
        pids
    }

    /**
     * Stops every member of the coalition until a scan finds all of them stopped, since a stopped
     * process cannot fork, then kills them. Each pid is checked again right before its signal.
     * Returns every pid signaled, an error when a member cannot be stopped or keeps forking
     **/
    pub fn reap(coal: u64) -> Result<Vec<libc::pid_t>, String> {
        // Safe: getpid cannot fail
        if coal <= 1 || coalition(unsafe { libc::getpid() }) == Some(coal) {
            return Err(format!(
                "refused to reap coalition {coal}, which is not a check's own"
            ));
        }

        let mut signaled = Vec::new();

        for _ in 0..ROUNDS {
            let pids = members(coal);

            if pids.is_empty() {
                return Ok(signaled);
            }

            let sig = if pids.iter().all(|&p| state(p) == Some(SSTOP)) {
                libc::SIGKILL
            } else {
                libc::SIGSTOP
            };

            for pid in pids {
                if coalition(pid) != Some(coal) {
                    continue;
                }

                // ponytail: a pid reused between the check and the kill needs a full pid wrap, no pidfd on macOS
                // Safe: kill only sends a signal
                if unsafe { libc::kill(pid, sig) } == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
                {
                    return Err(format!(
                        "process {pid} of a host check runs as another user, so shipcheck cannot stop it"
                    ));
                }

                signaled.push(pid);
            }

            std::thread::sleep(Duration::from_millis(1));
        }

        Err("a host check's processes kept forking faster than shipcheck could stop them".into())
    }

    /// The launchd domain of the user's jobs: the login session's, else the background one over ssh
    fn domain() -> &'static str {
        static DOMAIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        DOMAIN.get_or_init(|| {
            // Safe: getuid cannot fail
            let uid = unsafe { libc::getuid() };
            let gui = format!("gui/{uid}");
            let found = Command::new(LAUNCHCTL)
                .args(["print", &gui])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());

            if found { gui } else { format!("user/{uid}") }
        })
    }

    /// The boot session's id, None when the kernel does not say, so no record can match across a restart
    fn boot() -> Option<String> {
        let mut buf = [0u8; 64];
        let mut len = buf.len();
        // Safe: the kernel writes at most `len` bytes into `buf`
        let ok = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        } == 0;
        let id = String::from_utf8_lossy(&buf[..if ok { len } else { 0 }])
            .trim_end_matches('\0')
            .to_string();
        (!id.is_empty() && !id.contains(' ')).then_some(id)
    }

    /// Each job this process started and has not booted out, with its coalition and record once known
    static RUNNING: std::sync::Mutex<BTreeMap<String, Option<(u64, PathBuf)>>> =
        std::sync::Mutex::new(BTreeMap::new());

    fn running() -> std::sync::MutexGuard<'static, BTreeMap<String, Option<(u64, PathBuf)>>> {
        RUNNING.lock().unwrap_or_else(|e| e.into_inner())
    }

    /**
     * What a signal does before shipcheck dies: reap every running check and boot out its job.
     * Returns the lock, which the caller holds until it dies, so no check starts meanwhile
     **/
    pub fn stop_all() -> std::sync::MutexGuard<'static, BTreeMap<String, Option<(u64, PathBuf)>>> {
        let mut held = running();

        for (target, known) in std::mem::take(&mut *held) {
            // A record whose reap failed stays, so the slot's next run tries again
            if let Some((coal, record)) = known
                && reap(coal).is_ok()
            {
                let _ = fs::remove_file(record);
            }

            bootout(&target);
        }

        held
    }

    const NO_BOOT: &str = "the kernel did not give its boot session id, so shipcheck cannot tell its checks' processes from another boot's";

    fn bootout(target: &str) {
        let _ = Command::new(LAUNCHCTL)
            .args(["bootout", target])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    /// Where the coalition of the check that ran in `dir` is kept until it is reaped, outside every jail
    fn record(dir: &Path) -> PathBuf {
        use sha2::{Digest, Sha256};

        let id = crate::cache::hex(&Sha256::digest(dir.as_os_str().as_bytes())[..16]);
        crate::run::home().join("reap").join(id)
    }

    pub fn reap_slot(slot: &Path) -> Result<(), String> {
        let dir = crate::run::home().join("reap");

        if fs::read_dir(&dir).map_or(true, |mut d| d.next().is_none()) {
            return Ok(());
        }

        let boot = boot().ok_or(NO_BOOT)?;

        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let text = fs::read_to_string(entry.path()).unwrap_or_default();
            // `<coalition> <boot session> <launchd job> <checkout>`, the checkout last since it may hold spaces
            let mut parts = text.trim_end_matches('\n').splitn(4, ' ');
            let (Some(coal), Some(at), Some(target), Some(checkout)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };

            if !Path::new(checkout).starts_with(slot) {
                continue;
            }

            // A coalition id is new for every job, but only until the machine restarts
            if at == boot {
                let coal = coal
                    .parse()
                    .map_err(|_| format!("{}: not a coalition", entry.path().display()))?;
                reap(coal)?;
                bootout(target);
            }

            fs::remove_file(entry.path())
                .map_err(|e| format!("{}: {e}", entry.path().display()))?;
        }

        Ok(())
    }

    fn quote(s: &std::ffi::OsStr) -> Vec<u8> {
        let mut q = b"'".to_vec();

        for &b in s.as_bytes() {
            if b == b'\'' {
                q.extend_from_slice(b"'\\''");
            } else {
                q.push(b);
            }
        }

        q.push(b'\'');
        q
    }

    fn xml(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    /**
     * The job's script, which launchd runs outside the sandbox. It writes its pid and waits until
     * shipcheck has recorded its coalition, so no check runs before it can be reaped
     **/
    fn script(cmd: &Command, dir: &Path, log: &Path, ctl: &Path) -> Vec<u8> {
        let q = |p: &Path| quote(p.as_os_str());
        let mut s = b"exec >>".to_vec();
        s.extend(q(log));
        s.extend_from_slice(b" 2>&1 </dev/null\necho $$ >");
        s.extend(q(&ctl.join("pid.new")));
        s.extend_from_slice(b" && /bin/mv ");
        s.extend(q(&ctl.join("pid.new")));
        s.push(b' ');
        s.extend(q(&ctl.join("pid")));
        s.extend_from_slice(b" || exit 2\nread _ <");
        s.extend(q(&ctl.join("go")));
        s.extend_from_slice(b"\ncd ");
        s.extend(q(dir));
        s.extend_from_slice(b" || exit 2\n/usr/bin/env -i");

        for (k, v) in cmd.get_envs() {
            if let Some(v) = v {
                let mut kv = k.to_os_string();
                kv.push("=");
                kv.push(v);
                s.push(b' ');
                s.extend(quote(&kv));
            }
        }

        s.push(b' ');
        s.extend(quote(cmd.get_program()));

        for a in cmd.get_args() {
            s.push(b' ');
            s.extend(quote(a));
        }

        s.extend_from_slice(b"\necho $? >");
        s.extend(q(&ctl.join("status.new")));
        s.extend_from_slice(b" && /bin/mv ");
        s.extend(q(&ctl.join("status.new")));
        s.push(b' ');
        s.extend(q(&ctl.join("status")));
        s.push(b'\n');
        s
    }

    /// Starts a job of `/usr/bin/true` and boots it out, the error naming why launchd refused it
    pub fn probe() -> Result<String, String> {
        let keep = tempfile::Builder::new()
            .prefix("shipcheck-job-")
            .tempdir()
            .map_err(|e| format!("job dir: {e}"))?;
        let name = keep
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("job dir: not UTF-8")?;
        let label = format!(
            "run.shipcheck.probe.{}",
            name.trim_start_matches("shipcheck-job-")
        );
        let plist = keep.path().join("job.plist");
        fs::write(
            &plist,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
                 <key>Label</key><string>{}</string>\
                 <key>ProgramArguments</key><array><string>/usr/bin/true</string></array>\
                 <key>RunAtLoad</key><true/><key>KeepAlive</key><false/>\
                 </dict></plist>\n",
                xml(&label)
            ),
        )
        .map_err(|e| e.to_string())?;
        let out = Command::new(LAUNCHCTL)
            .args(["bootstrap", domain()])
            .arg(&plist)
            .output()
            .map_err(|e| format!("launchctl: {e}"))?;
        bootout(&format!("{}/{label}", domain()));

        if !out.status.success() {
            return Err(format!(
                "launchctl bootstrap {} failed ({}), so every host check fails with exit 2: shipcheck runs each check as a launchd job to stop all it starts. Run shipcheck outside another app's sandbox, and from a login session rather than ssh alone",
                domain(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        Ok(format!(
            "{}: each host check runs as a launchd job, and every process it starts is stopped with it",
            domain()
        ))
    }

    /// Runs the check as a launchd job, then reaps its coalition and removes the job. None on timeout
    pub fn run(
        cmd: &Command,
        dir: &Path,
        log: &Path,
        timeout: Duration,
    ) -> Result<Option<i32>, String> {
        reaped(cmd, dir, log, timeout).map(|(exit, _)| exit)
    }

    /// [`run`], and every pid the reap signaled
    pub fn reaped(
        cmd: &Command,
        dir: &Path,
        log: &Path,
        timeout: Duration,
    ) -> Result<(Option<i32>, Vec<libc::pid_t>), String> {
        let deadline = Instant::now() + timeout;
        let keep = tempfile::Builder::new()
            .prefix("shipcheck-job-")
            .tempdir()
            .map_err(|e| format!("job dir: {e}"))?;
        let ctl = real_dir(keep.path())?;
        let go = std::ffi::CString::new(ctl.join("go").as_os_str().as_bytes())
            .map_err(|e| e.to_string())?;

        // Safe: the path is a valid C string that outlives the call
        if unsafe { libc::mkfifo(go.as_ptr(), 0o600) } != 0 {
            return Err(format!("mkfifo: {}", std::io::Error::last_os_error()));
        }

        let sh = ctl.join("job.sh");
        fs::write(&sh, script(cmd, dir, log, &ctl)).map_err(|e| e.to_string())?;
        let name = ctl
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("job dir: not UTF-8")?;
        let label = format!(
            "run.shipcheck.check.{}",
            name.trim_start_matches("shipcheck-job-")
        );
        let sh_path = sh.to_str().ok_or("job dir: not UTF-8")?;
        // Interactive, so launchd does not throttle a build as background work
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\
             <key>Label</key><string>{}</string>\
             <key>ProgramArguments</key><array><string>/bin/sh</string><string>{}</string></array>\
             <key>RunAtLoad</key><true/><key>KeepAlive</key><false/>\
             <key>AbandonProcessGroup</key><true/>\
             <key>ProcessType</key><string>Interactive</string>\
             </dict></plist>\n",
            xml(&label),
            xml(sh_path)
        );
        let plist_path = ctl.join("job.plist");
        fs::write(&plist_path, plist).map_err(|e| e.to_string())?;
        let target = format!("{}/{label}", domain());
        // Before the bootstrap, so a signal right after it still boots the job out
        running().insert(target.clone(), None);
        let started = Command::new(LAUNCHCTL)
            .args(["bootstrap", domain()])
            .arg(&plist_path)
            .output()
            .map_err(|e| format!("launchctl: {e}"))?;

        if !started.status.success() {
            running().remove(&target);
            let why = format!(
                "shipcheck could not start this check as a launchd job in {}, which it needs to stop every process the check starts: {}\n",
                domain(),
                String::from_utf8_lossy(&started.stderr).trim()
            );
            fs::OpenOptions::new()
                .append(true)
                .open(log)
                .and_then(|mut f| std::io::Write::write_all(&mut f, why.as_bytes()))
                .map_err(|e| format!("{}: {e}", log.display()))?;
            return Ok((Some(2), Vec::new()));
        }

        let result = supervise(dir, &ctl, &go, &target, deadline);
        bootout(&target);
        running().remove(&target);
        result
    }

    fn supervise(
        dir: &Path,
        ctl: &Path,
        go: &std::ffi::CStr,
        target: &str,
        deadline: Instant,
    ) -> Result<(Option<i32>, Vec<libc::pid_t>), String> {
        let started = Instant::now();
        let pid = loop {
            if let Ok(pid) = fs::read_to_string(ctl.join("pid")) {
                break pid
                    .trim()
                    .parse::<libc::pid_t>()
                    .map_err(|e| format!("job pid: {e}"))?;
            }

            // The job waits for `go`, so nothing ran yet and the bootout stops it
            if Instant::now() >= deadline {
                return Ok((None, Vec::new()));
            }

            if Instant::now() >= started + START {
                return Err(format!(
                    "launchd took over {}s to start the check's job",
                    START.as_secs()
                ));
            }

            std::thread::sleep(Duration::from_millis(5));
        };
        // The script's `mv` of its pid file may not have exited yet
        let coal = loop {
            if let Some(c) = own_coalition(pid) {
                break c;
            }

            if Instant::now() >= started + START {
                return Err("launchd did not give the check a coalition of its own".into());
            }

            std::thread::sleep(Duration::from_millis(5));
        };
        let record = record(dir);
        let parent = record.parent().ok_or("no reap dir")?;
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        let new = record.with_extension("new");
        let line = format!("{coal} {} {target} ", boot().ok_or(NO_BOOT)?);
        let mut text = line.into_bytes();
        text.extend_from_slice(dir.as_os_str().as_bytes());
        fs::write(&new, text)
            .and_then(|_| fs::rename(&new, &record))
            .map_err(|e| format!("{}: {e}", record.display()))?;
        running().insert(target.to_string(), Some((coal, record.clone())));

        let exit = release(go, deadline).and_then(|released| {
            if !released {
                return Ok(None);
            }

            loop {
                if let Ok(code) = fs::read_to_string(ctl.join("status")) {
                    break code
                        .trim()
                        .parse()
                        .map(Some)
                        .map_err(|e| format!("job status: {e}"));
                }

                if Instant::now() >= deadline {
                    break Ok(None);
                }

                std::thread::sleep(Duration::from_millis(20));
            }
        });

        // The record stays when the reap fails, so the slot's next run tries again before its checkout
        let signaled = reap(coal)?;
        fs::remove_file(&record).map_err(|e| format!("{}: {e}", record.display()))?;
        exit.map(|e| (e, signaled))
    }

    /// The coalition of a job's waiting script, None unless it is the only member, so never a shared one
    pub fn own_coalition(pid: libc::pid_t) -> Option<u64> {
        // Safe: getpid cannot fail
        let own = coalition(unsafe { libc::getpid() });
        coalition(pid).filter(|&c| c > 1 && Some(c) != own && members(c) == [pid])
    }

    /// Lets the job's script run the check, false when the deadline passed first
    fn release(go: &std::ffi::CStr, deadline: Instant) -> Result<bool, String> {
        loop {
            // Safe: the path is a valid C string, and a nonblocking open fails until the script reads
            let fd = unsafe {
                libc::open(
                    go.as_ptr(),
                    libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };

            if fd >= 0 {
                // Safe: fd is ours, and the byte outlives the call
                unsafe {
                    libc::write(fd, b"\n".as_ptr().cast(), 1);
                    libc::close(fd);
                }
                return Ok(true);
            }

            if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENXIO) {
                return Err(format!("job fifo: {}", std::io::Error::last_os_error()));
            }

            if Instant::now() >= deadline {
                return Ok(false);
            }

            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// A check's `$GITHUB_STEP_SUMMARY`, in its jail's home
const STEP_SUMMARY: &str = "step-summary.md";

/**
 * What a check wrote to `$GITHUB_STEP_SUMMARY`, empty when it wrote nothing. The check could leave a
 * link, FIFO or hard link to a file it cannot read there, and the comment is public, so only a
 * regular file with one link is read, never through a link, and a refusal names nothing of it
 **/
fn read_summary(home: &Path) -> String {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(home)
        .and_then(|dir| crate::runner::shard::open_at(&dir, STEP_SUMMARY, false));
    let file = match file {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return String::new(),
        Err(_) => return REFUSED.into(),
    };

    if file.metadata().map_or(true, |m| m.nlink() != 1) {
        return REFUSED.into();
    }

    let mut body = Vec::new();

    if file
        .take(crate::runner::MAX_SUMMARY as u64 + 1)
        .read_to_end(&mut body)
        .is_err()
    {
        return REFUSED.into();
    }

    crate::runner::cap_summary(&String::from_utf8_lossy(&body))
}

const REFUSED: &str =
    "shipcheck did not read this summary: it was not a regular file of the check's own";

/// Runs a command until it exits or the timeout passes, then kills everything it started either
/// way, so nothing outlives it. None on timeout
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

    // A check's own jail, as in shipcheck's tests of itself, cannot reach launchd
    #[cfg(target_os = "macos")]
    if !in_a_jail() {
        drop(file);
        install();
        return job::run(&jail.shell(command, dir, extra)?, dir, log, timeout);
    }

    let out = file.try_clone().map_err(|e| e.to_string())?;
    let mut child = jail
        .shell(command, dir, extra)?
        .stdout(out)
        .stderr(file)
        .spawn()
        .map_err(|e| format!("sandbox: {e}"))?;
    let slot = register(child.id());
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

    if let Some(slot) = slot {
        GROUPS[slot].store(0, Ordering::SeqCst);
    }

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
    // No launchd job, which costs 0.1 s a command on every plan: a leftover writes only this
    // temp home, removed on return, and signals only its own sandbox
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

    // A process the command left in another group can hold the pipe open, so the read has a limit
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = tx.send(std::io::Read::read_to_string(&mut stdout, &mut out).map(|_| out));
    });
    let left = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(500));

    match rx.recv_timeout(left) {
        Ok(read) => read.map_err(|e| e.to_string()),
        Err(_) => Err("exited, but a process it left kept its output open".into()),
    }
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
            summary: read_summary(&jail.home),
            skipped: false,
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
    fn gems_install_into_the_jail_and_their_commands_are_on_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let jail = jail(dir.path());
        let cmd = jail.shell("true", dir.path(), &[]).unwrap();
        let env = |name: &str| {
            cmd.get_envs()
                .find(|(k, _)| *k == name)
                .and_then(|(_, v)| v)
                .map(|v| v.to_string_lossy().into_owned())
                .unwrap()
        };
        assert_eq!(
            env("BUNDLE_PATH"),
            jail.cache.join("bundle").to_string_lossy()
        );
        assert_eq!(env("GEM_HOME"), jail.home.join(".gem").to_string_lossy());
        // Last, so a gem's command never shadows a tool of the user's
        let gem_bin = format!(":{}", jail.home.join(".gem/bin").display());
        assert!(env("PATH").ends_with(&gem_bin), "{}", env("PATH"));
        assert!(cmd.get_args().any(|a| a == "/bin/sh"));
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
    fn a_check_gets_the_toolchains_and_nothing_else_of_docker() {
        let (home, user) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        assert!(HOME_LINKS.iter().all(|l| TOOLCHAINS.contains(l)));
        toolchains(home.path(), user.path()).unwrap();
        assert!(!home.path().join(".docker").exists());
        fs::create_dir_all(user.path().join(".docker/cli-plugins")).unwrap();
        fs::create_dir_all(user.path().join(".cache/solana/v1.52")).unwrap();
        fs::write(
            user.path().join(".docker/config.json"),
            "{\"credsStore\": \"desktop\"}",
        )
        .unwrap();
        toolchains(home.path(), user.path()).unwrap();
        assert!(home.path().join(".cache/solana/v1.52").is_dir());
        assert_eq!(
            fs::read_link(home.path().join(".docker/cli-plugins")).unwrap(),
            user.path().join(".docker/cli-plugins")
        );
        assert!(!home.path().join(".docker/config.json").exists());
    }

    #[test]
    fn a_check_finds_no_tool_in_a_temp_dir_or_a_checkout() {
        let (user, repo) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let user = user.path().canonicalize().unwrap();
        fs::create_dir_all(user.join(".git")).unwrap();
        fs::create_dir_all(user.join(".cargo/bin")).unwrap();
        fs::create_dir_all(user.join("src/app/.git")).unwrap();
        fs::create_dir_all(user.join("src/app/bin")).unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        let path = std::env::join_paths([
            Path::new("/tmp/fake"),
            Path::new("bin"),
            &user.join("src/app/bin"),
            repo.path(),
            &user.join(".cargo/bin"),
            Path::new("/usr/bin"),
        ])
        .unwrap();
        assert_eq!(
            trusted_path(std::ffi::OsStr::new("/tmp/fake:bin"), &user),
            "/usr/bin:/bin:/usr/sbin:/sbin"
        );
        if Path::new("/opt/homebrew/.git").exists() {
            assert!(trusted_in(Path::new("/opt/homebrew/bin"), &user, &[]));
        }

        // Linux keeps tempdir() itself under /tmp, so the temp root here must not hold `user`
        let kept = trusted_path_in(&path, &user, &[PathBuf::from("/tmp/fake")]);
        assert_eq!(
            std::env::split_paths(&kept).collect::<Vec<_>>(),
            [user.join(".cargo/bin"), PathBuf::from("/usr/bin")]
        );
    }

    #[test]
    fn the_jail_gets_a_copy_of_the_sbf_sdk_it_can_relink() {
        let (home, release) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let bin = release.path().join("bin");
        let deps = bin.join("platform-tools-sdk/sbf/dependencies");
        fs::create_dir_all(&deps).unwrap();
        fs::write(bin.join("cargo-build-sbf"), "").unwrap();
        std::os::unix::fs::symlink("/user/.cache/solana/v1.52", deps.join("platform-tools"))
            .unwrap();
        sbf_sdk(home.path(), std::ffi::OsStr::new("/nowhere")).unwrap();
        assert!(!home.path().join(SBF_SDK).exists());
        sbf_sdk(home.path(), bin.as_os_str()).unwrap();
        let link = home
            .path()
            .join(SBF_SDK)
            .join("dependencies/platform-tools");
        assert_eq!(
            fs::read_link(&link).unwrap(),
            Path::new("/user/.cache/solana/v1.52")
        );
        // cargo-build-sbf's relink changes the copy and not the user's SDK
        fs::remove_file(&link).unwrap();
        assert!(fs::symlink_metadata(deps.join("platform-tools")).is_ok());
    }

    #[test]
    fn the_jail_has_a_rustup_home_of_its_own_with_the_toolchains_linked() {
        let (home, source) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        rustup_home(home.path(), source.path()).unwrap();
        assert!(!home.path().join(".rustup").exists());
        fs::create_dir_all(source.path().join("toolchains/stable")).unwrap();
        fs::create_dir_all(source.path().join("update-hashes")).unwrap();
        fs::write(
            source.path().join("settings.toml"),
            "default_toolchain = \"stable\"\n",
        )
        .unwrap();
        rustup_home(home.path(), source.path()).unwrap();
        let jail = home.path().join(".rustup");
        assert_eq!(
            fs::read_link(jail.join("toolchains/stable")).unwrap(),
            source.path().join("toolchains/stable")
        );
        assert!(
            fs::symlink_metadata(jail.join("settings.toml"))
                .unwrap()
                .is_file()
        );
        // A relink, as cargo-build-sbf does, changes the jail's rustup home and not the user's
        fs::remove_file(jail.join("toolchains/stable")).unwrap();
        assert!(source.path().join("toolchains/stable").is_dir());
    }

    #[test]
    fn a_stopped_run_kills_the_checks_it_registered() {
        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        // A list of its own, since GROUPS holds the checks other tests run meanwhile
        let groups = [AtomicI32::new(child.id() as i32), AtomicI32::new(0)];
        kill_registered(&groups);
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    #[test]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let pid = dir.path().join("pid");
        // The background sleep is a grandchild that would outlive a kill of sh alone. A slow start can
        // pass the timeout before the pid is written, so a longer one tries again, which `mv` keeps whole
        let cmd = format!(
            "sleep 30 & echo $! > {0}.tmp && mv {0}.tmp {0}; wait",
            pid.display()
        );

        for timeout in [300, 1000, 3000, 10000].map(Duration::from_millis) {
            let started = Instant::now();
            let exit =
                run_command(&cmd, dir.path(), &jail(dir.path()), &[], &log, timeout).unwrap();
            assert_eq!(exit, None);
            assert!(started.elapsed() < timeout + Duration::from_secs(5));

            if pid.exists() {
                break;
            }
        }

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
    #[cfg(target_os = "macos")]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_check_reads_no_temp_dir_but_its_own_and_no_terminal() {
        let (dir, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        fs::write(other.path().join("secret"), "x").unwrap();
        fs::write(dir.path().join("mine"), "x").unwrap();
        let log = dir.path().join("log");
        let run = |cmd: &str| {
            run_command(
                cmd,
                dir.path(),
                &jail(dir.path()),
                &[],
                &log,
                Duration::from_secs(10),
            )
            .unwrap()
        };
        assert_eq!(
            run("cat mine"),
            Some(0),
            "{}",
            fs::read_to_string(&log).unwrap()
        );
        let secret = other.path().join("secret");
        assert_ne!(run(&format!("cat {}", secret.display())), Some(0));
        let tty = fs::read_dir("/dev")
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .strip_prefix("ttys")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                    && fs::File::open(p).is_ok()
            });

        // A pseudo-terminal of the check's own still works, as `script` opens one
        assert_eq!(
            run("/usr/bin/script -q /dev/null /bin/echo hi"),
            Some(0),
            "{}",
            fs::read_to_string(&log).unwrap()
        );
        // A terminal this process can open, as one an agent runs shipcheck in
        if let Some(tty) = tty {
            assert_ne!(
                run(&format!("exec 3< {}", tty.display())),
                Some(0),
                "{}",
                tty.display()
            );
        }
    }

    #[test]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_check_runs_what_it_installed() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let jail = jail(dir.path());
        let mut exits = Vec::new();

        // As `go install`, `cargo install`, `npm install -g` and `uv tool install` do
        for (bin, name) in [
            ("$GOPATH/bin", "probe-go"),
            ("$CARGO_HOME/bin", "probe-cargo"),
            ("$npm_config_prefix/bin", "probe-npm"),
            ("$HOME/.local/bin", "probe-uv"),
        ] {
            let cmd = format!(
                "mkdir -p {bin} && printf '#!/bin/sh\\necho ran\\n' > {bin}/{name} && chmod +x {bin}/{name} && {name}"
            );
            let exit = run_command(&cmd, dir.path(), &jail, &[], &log, Duration::from_secs(10));
            exits.push((name, exit.unwrap(), fs::read_to_string(&log).unwrap()));
        }

        for (name, exit, log) in exits {
            assert_eq!(exit, Some(0), "{name}: {log}");
            assert!(log.ends_with("ran\n"), "{name}: {log}");
        }
    }

    #[test]
    fn installed_binaries_come_after_the_trusted_path() {
        let dir = tempfile::tempdir().unwrap();
        let jail = jail(dir.path());
        let path = jail.path("/usr/bin:/bin".into());
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(
            dirs[..2],
            [PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
        );
        assert_eq!(dirs[2], jail.cache.join("go/bin"));
        assert_eq!(dirs.len(), 6);
    }

    #[test]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
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
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
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
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_check_writes_its_summary_in_its_jail() {
        let dir = tempfile::tempdir().unwrap();
        let jail = jail(dir.path());
        let exit = run_command(
            "printf '| a |\\n' >> \"$GITHUB_STEP_SUMMARY\" && echo more >> \"$GITHUB_STEP_SUMMARY\"",
            dir.path(),
            &jail,
            &[],
            &dir.path().join("log"),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(exit, Some(0));
        assert_eq!(read_summary(&jail.home), "| a |\nmore");
    }

    #[test]
    fn a_summary_that_is_not_the_checks_own_file_is_never_read() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("id_ed25519");
        fs::write(&secret, "PRIVATE KEY").unwrap();
        let home = |name: &str| {
            let h = dir.path().join(name);
            fs::create_dir(&h).unwrap();
            h
        };
        let link = home("link");
        std::os::unix::fs::symlink(&secret, link.join(STEP_SUMMARY)).unwrap();
        let hard = home("hard");
        fs::hard_link(&secret, hard.join(STEP_SUMMARY)).unwrap();
        let fifo = home("fifo");
        let path =
            std::ffi::CString::new(fifo.join(STEP_SUMMARY).into_os_string().into_vec()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        // A home that is itself a link to a directory holding the secret as the summary
        let outside = home("outside");
        fs::write(outside.join(STEP_SUMMARY), "PRIVATE KEY").unwrap();
        let parent = dir.path().join("parent");
        std::os::unix::fs::symlink(&outside, &parent).unwrap();

        for h in [link, hard, fifo, parent] {
            assert_eq!(read_summary(&h), REFUSED, "{}", h.display());
        }

        let big = home("big");
        fs::write(big.join(STEP_SUMMARY), "x".repeat(50_000)).unwrap();
        let text = read_summary(&big);
        assert!(text.len() < 20_100 && text.ends_with("bytes...]"));
        assert_eq!(read_summary(&home("none")), "");
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
        assert!(!profile.contains("/dev/tty\""));
        let user = resolved(crate::run::user_dir());
        let denied = profile
            .lines()
            .find(|l| l.starts_with("(deny file-read-data"))
            .unwrap();

        for dir in [
            user.to_str().unwrap(),
            "/private/tmp",
            "/private/var/folders",
        ] {
            assert!(denied.contains(&format!("(subpath \"{dir}\")")), "{denied}");
        }

        assert!(profile.contains(
            r#"(deny file-read* (require-all (regex #"^/dev/ttys[0-9]+$") (require-not (extension "com.apple.sandbox.pty"))))"#
        ));
        assert!(profile.contains(&format!("(subpath \"/j/repo\") {paths})")));
        assert!(profile.contains("(deny network-outbound (remote unix-socket))\n"));
        assert!(profile.contains(
            r#"(allow network-outbound (remote unix-socket (subpath "/j/clone \"x\"")) (remote unix-socket (subpath "/j/home")))"#
        ));
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
    fn knows_it_runs_in_a_jail_only_inside_a_host_check() {
        let inside = std::env::var_os("SHIPCHECK_JAIL").is_some() && cfg!(target_os = "macos");
        assert_eq!(in_a_jail(), inside);
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

        if Path::new("/var/tmp").is_dir() && !Path::new("/var/tmp").is_symlink() {
            assert!(all.contains("--tmpfs /var/tmp"), "{all}");
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
    #[cfg(target_os = "linux")]
    fn a_check_sees_none_of_the_hosts_var_tmp() {
        let clone = tempfile::tempdir().unwrap();
        let (_home, home) = temp_home().unwrap();
        let jail = Jail {
            writable: vec![real_dir(clone.path()).unwrap(), home.clone()],
            readable: Vec::new(),
            home: home.clone(),
            cache: home,
        };
        let left = tempfile::NamedTempFile::new_in("/var/tmp").unwrap();
        let log = clone.path().join("log");
        let cmd = format!("test ! -e '{}'", left.path().display());
        let exit = run_command(
            &cmd,
            clone.path(),
            &jail,
            &[],
            &log,
            Duration::from_secs(10),
        );
        assert_eq!(
            exit.unwrap(),
            Some(0),
            "{}",
            fs::read_to_string(&log).unwrap()
        );
    }

    #[test]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn mktemp_makes_its_files_in_the_jails_tmpdir() {
        let (clone, cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (_home, home) = temp_home().unwrap();
        let cache = real_dir(cache.path()).unwrap();
        let jail = Jail {
            writable: vec![real_dir(clone.path()).unwrap(), home.clone(), cache.clone()],
            readable: Vec::new(),
            home,
            cache,
        };
        let log = clone.path().join("log");
        // As cli/cli's script/licenses does, then with a prefix, then with a template of its own.
        // A bare `-t pre` is BSD's form, the one the macOS shim handles, GNU wants X's in it
        let prefix = if cfg!(target_os = "macos") {
            "pre"
        } else {
            "preXXXXXX"
        };
        let cmd = format!(
            "set -e; for d in \"$(mktemp -d)\" \"$(mktemp -d -t {prefix})\" \"$(mktemp)\"; do case $d in \"$TMPDIR\"/*) ;; *) exit 9 ;; esac; done; mktemp \"$PWD/own.XXXX\""
        );
        let exit = run_command(
            &cmd,
            clone.path(),
            &jail,
            &[],
            &log,
            Duration::from_secs(10),
        );
        let log = fs::read_to_string(&log).unwrap();
        assert_eq!(exit.unwrap(), Some(0), "{log}");
        assert!(
            log.contains(&format!(
                "{}/own.",
                clone.path().canonicalize().unwrap().display()
            )),
            "{log}"
        );
    }

    #[test]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_check_serves_a_unix_socket_in_its_tmpdir_only() {
        // A jail as `run` makes it: the clone, a home of its own that is TMPDIR, and a cache
        let (clone, cache, outside) = (
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let (_home, home) = temp_home().unwrap();
        let cache = real_dir(cache.path()).unwrap();
        let jail = Jail {
            writable: vec![real_dir(clone.path()).unwrap(), home.clone(), cache.clone()],
            readable: Vec::new(),
            home,
            cache,
        };
        let log = outside.path().join("log");
        let run = |cmd: &str| {
            let exit = run_command(cmd, clone.path(), &jail, &[], &log, Duration::from_secs(10));
            (exit.unwrap(), fs::read_to_string(&log).unwrap_or_default())
        };
        // gin's TestUnixSocket, which serves on a socket in os.TempDir()
        let serve = "python3 -c 'import os, socket; p = os.environ[\"TMPDIR\"] + \"/s\"; l = socket.socket(socket.AF_UNIX); l.bind(p); l.listen(1); socket.socket(socket.AF_UNIX).connect(p)'";
        let served = run(serve);
        assert_eq!(served.0, Some(0), "{}", served.1);
        let sock = outside.path().join("s");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let probe = format!(
            "python3 -c 'import socket; socket.socket(socket.AF_UNIX).connect(\"{}\")'",
            sock.display()
        );
        let connected = run(&probe);
        assert_ne!(connected.0, Some(0), "{}", connected.1);
    }

    #[test]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
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
        // A pseudo-terminal of its own, as pre-commit's `--color=always` opens, but no one else's
        let pty = run(
            "python3 -c 'import os; r, w = os.openpty(); os.write(w, b\"x\"); assert os.read(r, 1) == b\"x\"'",
        );
        assert_eq!(pty.0, Some(0), "{}", pty.1);
        let (mut master, mut slave) = (0, 0);
        let mut name = [0 as libc::c_char; 128];
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                name.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0);
        let theirs = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
            .to_str()
            .unwrap()
            .to_string();
        // Another terminal of the same user, such as the one shipcheck was started from
        let open =
            format!("python3 -c 'import os; os.open(\"{theirs}\", os.O_WRONLY | os.O_NOCTTY)'");
        assert_ne!(run(&open).0, Some(0), "{theirs}");
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
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
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
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

    /// The pids a check wrote to `pids` in its clone, one per line
    #[cfg(target_os = "macos")]
    fn written_pids(dir: &Path) -> Vec<i32> {
        fs::read_to_string(dir.join("pids"))
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim().parse().unwrap())
            .collect()
    }

    #[cfg(target_os = "macos")]
    fn alive(pid: i32) -> bool {
        // Safe: signal 0 only checks that the process exists
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn processes_that_leave_the_checks_group_are_reaped() {
        let cases = [
            ("set -m", "set -m; ( exec sleep 30 ) & echo $! >> pids"),
            (
                "double fork with setsid",
                "/usr/bin/perl -e 'use POSIX; if (fork() == 0) { POSIX::setsid(); if (fork() == 0) { open(my $f, \">>\", \"pids\"); print $f \"$$\\n\"; close $f; exec \"/bin/sleep\", \"30\" } exit 0 } wait'; sleep 0.3",
            ),
            (
                "nohup env -i",
                "nohup /usr/bin/env -i /bin/sleep 30 >/dev/null 2>&1 & echo $! >> pids",
            ),
            (
                "hopping chain",
                "/usr/bin/perl -e 'use POSIX; for (1..300) { exit 0 if fork(); POSIX::setsid(); open(my $f, \">>\", \"pids\"); print $f \"$$\\n\"; close $f; select(undef, undef, undef, 0.01) } sleep 30' & sleep 0.3",
            ),
        ];

        for (name, cmd) in cases {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("log");
            let exit = run_command(
                cmd,
                dir.path(),
                &jail(dir.path()),
                &[],
                &log,
                Duration::from_secs(10),
            )
            .unwrap();
            assert_eq!(
                exit,
                Some(0),
                "{name}: {}",
                fs::read_to_string(&log).unwrap()
            );
            let pids = written_pids(dir.path());
            assert!(!pids.is_empty(), "{name}");
            std::thread::sleep(Duration::from_millis(200));

            for pid in pids {
                assert!(!alive(pid), "{name}: {pid} outlived its check");
            }
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_reap_signals_nothing_outside_the_checks_coalition() {
        let dir = tempfile::tempdir().unwrap();
        let jail = jail(dir.path());
        let log = dir.path().join("log");
        // Sandboxed with the same profile, as other checks and apps on the machine are
        let mut others: Vec<std::process::Child> = (0..2)
            .map(|_| {
                jail.shell("exec sleep 30", dir.path(), &[])
                    .unwrap()
                    .spawn()
                    .unwrap()
            })
            .collect();
        let all = || {
            let mut pids: Vec<libc::pid_t> = vec![0; 16384];
            let size = std::mem::size_of_val(pids.as_slice()) as libc::c_int;
            // Safe: the kernel writes at most `size` bytes into `pids`
            let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
            pids.truncate(n.max(0) as usize);
            pids
        };

        for (cmd, leaves) in [("echo hi", false), ("sleep 30 & echo $! > pids", true)] {
            let before = all();
            let shell = jail.shell(cmd, dir.path(), &[]).unwrap();
            let (exit, signaled) =
                job::reaped(&shell, dir.path(), &log, Duration::from_secs(10)).unwrap();
            assert_eq!(exit, Some(0), "{}", fs::read_to_string(&log).unwrap());
            let outside: Vec<_> = signaled.iter().filter(|p| before.contains(p)).collect();
            assert!(outside.is_empty(), "{cmd}: signaled {outside:?}");

            if leaves {
                let left = written_pids(dir.path());
                assert!(left.iter().all(|p| signaled.contains(p)), "{cmd}");
            }
        }

        for other in &mut others {
            assert!(
                other.try_wait().unwrap().is_none(),
                "a process outside the check was killed"
            );
            other.kill().unwrap();
            other.wait().unwrap();
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_reap_refuses_shipcheck_s_own_coalition() {
        // Safe: getpid cannot fail
        let own = job::coalition(unsafe { libc::getpid() }).unwrap();
        assert!(job::reap(own).is_err());
        assert!(job::reap(0).is_err());
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_check_cannot_run_a_setuid_binary() {
        assert!(setuid().contains(&PathBuf::from("/bin/ps")));
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let exit = run_command(
            "/bin/ps -p $$",
            dir.path(),
            &jail(dir.path()),
            &[],
            &log,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_ne!(exit, Some(0), "{}", fs::read_to_string(&log).unwrap());
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_check_signals_only_its_own_processes() {
        let dir = tempfile::tempdir().unwrap();
        let jail = jail(dir.path());
        let log = dir.path().join("log");
        let run = |cmd: &str| {
            run_command(cmd, dir.path(), &jail, &[], &log, Duration::from_secs(10)).unwrap()
        };
        // This test's own processes: one outside any sandbox, one in a sandbox of its own
        let mut outside = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let mut other = jail
            .shell("exec sleep 30", dir.path(), &[])
            .unwrap()
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));

        for (name, pid) in [("outside", outside.id()), ("another sandbox", other.id())] {
            assert_ne!(run(&format!("kill -0 {pid}")), Some(0), "{name}");
        }

        let own = "sleep 30 & kill $! || exit 1; set -m; ( exec sleep 30 ) & kill $! || exit 1; \
            /usr/bin/perl -e 'use POSIX; my $p = fork(); if (!$p) { POSIX::setsid(); sleep 30; exit 0 } select(undef, undef, undef, 0.2); exit(kill(\"KILL\", $p) ? 0 : 1)'";
        assert_eq!(run(own), Some(0), "{}", fs::read_to_string(&log).unwrap());

        for child in [&mut outside, &mut other] {
            assert!(child.try_wait().unwrap().is_none());
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_check_cannot_reach_launchd_jobs() {
        let dir = tempfile::tempdir().unwrap();
        // Safe: getuid cannot fail
        let uid = unsafe { libc::getuid() };
        let label = format!("run.shipcheck.probe.test-{}", std::process::id());
        let target = format!("gui/{uid}/{label}");
        let plist = dir.path().join("job.plist");
        fs::write(
            &plist,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>/bin/sleep</string><string>60</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><false/></dict></plist>\n"
            ),
        )
        .unwrap();
        let launchctl = |args: &[&str]| Command::new(LAUNCHCTL).args(args).output().unwrap();
        assert!(launchd().is_ok(), "{:?}", launchd());
        assert!(
            launchctl(&["bootstrap", &format!("gui/{uid}"), plist.to_str().unwrap()])
                .status
                .success()
        );
        let log = dir.path().join("log");
        let jail = jail(dir.path());

        for cmd in [
            format!("{LAUNCHCTL} print {target}"),
            format!("{LAUNCHCTL} kill SIGCONT {target}"),
            format!("{LAUNCHCTL} bootout {target}"),
            format!("cp {LAUNCHCTL} ./lc && ./lc bootout {target}"),
        ] {
            let exit = run_command(&cmd, dir.path(), &jail, &[], &log, Duration::from_secs(10));
            assert_ne!(
                exit.unwrap(),
                Some(0),
                "{cmd}: {}",
                fs::read_to_string(&log).unwrap()
            );
        }

        let loaded = launchctl(&["print", &target]).status.success();

        // A bootout can fail while launchd still stops the job, so it tries until the job is gone
        for _ in 0..50 {
            if !launchctl(&["bootout", &target]).status.success()
                && !launchctl(&["print", &target]).status.success()
            {
                break;
            }

            std::thread::sleep(Duration::from_millis(100));
        }

        assert!(loaded, "a check booted out the test's job");
        assert!(
            !launchctl(&["print", &target]).status.success(),
            "{target} stayed loaded"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_reap_refuses_a_coalition_it_shares_or_the_kernels() {
        assert!(job::reap(1).is_err());
        // This process's coalition holds the test runner and its parents, so it is never a job's own
        // Safe: getpid cannot fail
        assert_eq!(job::own_coalition(unsafe { libc::getpid() }), None);
        assert_eq!(job::own_coalition(1), None);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn setuid_binaries_outside_the_bin_dirs_are_denied_too() {
        for path in [
            "/usr/libexec/authopen",
            "/usr/libexec/security_authtrampoline",
            ARD_AGENT,
        ] {
            if Path::new(path).exists() {
                assert!(setuid().contains(&PathBuf::from(path)), "{path}");
            }
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_versions_command_that_leaves_its_output_open_fails_in_time() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let got = version(
            "echo tool 1.0; set -m; ( exec sleep 30 ) &",
            dir.path(),
            &[],
            Duration::from_secs(2),
        );
        assert!(got.is_err(), "{got:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
