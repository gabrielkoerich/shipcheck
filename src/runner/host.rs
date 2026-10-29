use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use sandbox_jail::known::{self, LAUNCHCTL};
use sandbox_jail::{Access, Backend, Policy, Sandbox};

use super::{Outcome, RunRequest, Runner};
use crate::config;

/// The one "workflow" of a host run, its jobs are the checks
pub const HOST: &str = "host";

/// Runs the owner's `[host]` checks natively, as the user, each in its own process group
pub struct Host {
    config: config::Host,
    /// Where `versions` run, so a toolchain file in the repository picks the version
    repo: PathBuf,
    /// This run's directory under `runs`, with its copy of the base's cache or its checks' overlays
    copy: std::sync::OnceLock<Result<RunCopy, String>>,
    /// Whether checks of another tree see the base's cache through an overlay instead of a copy
    overlay: std::sync::OnceLock<bool>,
}

impl Host {
    pub fn new(config: config::Host, repo: &Path) -> Self {
        Host {
            config,
            repo: repo.to_path_buf(),
            copy: std::sync::OnceLock::new(),
            overlay: std::sync::OnceLock::new(),
        }
    }

    /**
     * This run's directory under `runs`, made once per run and removed with it, so no branch leaves
     * a file another branch's run reads. A shared cache let one branch plant registry sources that
     * cargo trusts once extracted. With an overlay it holds each check's upper layer, otherwise a
     * copy of the base's cache, which shares its blocks on APFS, btrfs or XFS and is a full copy on ext4
     **/
    fn run_cache(&self, repo_id: &str) -> Result<PathBuf, String> {
        let copy = self.copy.get_or_init(|| {
            let dir = cache_dir(Some(repo_id));
            // The cache every branch once shared, which a branch may have written
            remove_tree(&dir.join("branches"));
            let base = real_dir(&dir.join("base"))?;
            let runs = real_dir(&dir.join("runs"))?;
            RunCopy::remove_unused(&runs);
            let copy = RunCopy::new(&runs)?;

            if *self
                .overlay
                .get_or_init(|| sandbox_jail::overlay_supported(&runs).is_ok())
            {
                fs::create_dir(&copy.dir).map_err(|e| format!("{}: {e}", copy.dir.display()))?;
            } else {
                clone_dir(&base, &copy.dir)?;
            }

            Ok(copy)
        });

        let dir = match copy {
            Ok(copy) => &copy.dir,
            Err(e) => return Err(e.clone()),
        };

        // A check may write inside it, so before each check it must still be the directory made
        if !fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
            return Err(format!(
                "{} is no longer this run's directory",
                dir.display()
            ));
        }

        Ok(dir.clone())
    }

    /// This host with the overlay forced on or off, for tests of each way
    #[cfg(test)]
    fn with_overlay(self, on: bool) -> Self {
        self.overlay.set(on).unwrap();
        self
    }

    /// Whether this run's checks of another tree got overlays, known once `run_cache` ran
    fn overlays(&self) -> bool {
        self.overlay.get().copied().unwrap_or(false)
    }
}

/**
 * One run's copy of the base cache, `runs/run-N`, beside `runs/run-N.lock`, which the run holds
 * locked while it lives. A copy whose lock another run can take is unused, as when its run died
 **/
struct RunCopy {
    dir: PathBuf,
    lock: PathBuf,
    // Closed after `drop` removes both paths, which releases the lock
    _file: fs::File,
}

impl RunCopy {
    fn new(runs: &Path) -> Result<Self, String> {
        let fail = |e: std::io::Error| format!("run cache in {}: {e}", runs.display());

        // A fixed name per slot, since cargo keys a build by its sources' absolute paths
        for n in 0..64 {
            let lock = runs.join(format!("run-{n}.lock"));
            let file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&lock)
                .map_err(fail)?;

            // The lock must still be the one at that path, not one a run or cleanup removed meanwhile
            if !locked(&file) || !same_file(&file, &lock) {
                continue;
            }

            let dir = lock.with_extension("");
            remove_tree(&dir);

            // What a check left that cannot be removed must never reach another branch's run
            if fs::symlink_metadata(&dir).is_ok() {
                continue;
            }

            return Ok(RunCopy {
                dir,
                lock,
                _file: file,
            });
        }

        Err(format!("all 64 run caches in {} are busy", runs.display()))
    }

    fn remove_unused(runs: &Path) {
        for entry in fs::read_dir(runs).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // A live run links its lock and ends its probe within milliseconds of making them
            let old = || {
                fs::symlink_metadata(&path)
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t.elapsed().is_ok_and(|e| e > Duration::from_secs(60)))
            };

            // The crate's overlay probe, and the one older shipcheck versions made
            if name.starts_with(".sandbox-jail-probe-") || name.starts_with(".probe-") {
                if old() {
                    remove_tree(&path);
                }

                continue;
            }

            // A lock its run took and never linked, as when it died between the two
            if name.starts_with(".new-") {
                if old() && fs::File::open(&path).is_ok_and(|f| locked(&f) && same_file(&f, &path))
                {
                    let _ = fs::remove_file(&path);
                }

                continue;
            }

            if path.extension().is_none_or(|e| e != "lock")
                || !entry.file_type().is_ok_and(|t| t.is_file())
            {
                continue;
            }

            let Ok(file) = fs::File::open(&path) else {
                continue;
            };

            // The lock must still be the one at that path, not one another cleanup removed meanwhile
            if locked(&file) && same_file(&file, &path) {
                remove_tree(&path.with_extension(""));
                let _ = fs::remove_file(&path);
            }

            drop(file);
        }
    }
}

impl Drop for RunCopy {
    fn drop(&mut self) {
        remove_tree(&self.dir);
        let _ = fs::remove_file(&self.lock);
    }
}

/// The regular `runs/run-X.lock` files of one repository's host cache, or of every repository's
pub fn run_locks(repo_id: Option<&str>) -> Vec<PathBuf> {
    // Real directories and files only, so a symlink a check left never leads elsewhere
    let entries = |dir: &Path, dirs: bool| -> Vec<PathBuf> {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.file_type()
                    .is_ok_and(|t| if dirs { t.is_dir() } else { t.is_file() })
            })
            .map(|e| e.path())
            .collect()
    };
    let repos = match repo_id {
        Some(id) => vec![cache_dir(Some(id))],
        None => entries(&cache_dir(None), true)
            .iter()
            .flat_map(|owner| entries(owner, true))
            .collect(),
    };
    repos
        .iter()
        .flat_map(|repo| entries(&repo.join("runs"), false))
        .filter(|p| p.extension().is_some_and(|e| e == "lock"))
        .collect()
}

/// Whether `path` still names the inode `file` has open
fn same_file(file: &fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    match (file.metadata(), fs::symlink_metadata(path)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Takes an exclusive lock on `file` without waiting, held until the file is closed
fn locked(file: &fs::File) -> bool {
    use std::os::fd::AsRawFd;

    // Safe: flock only takes a lock on a descriptor this file owns
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Removes a cache tree, read-only directories included, as Go's module cache and overlayfs make them
fn remove_tree(dir: &Path) {
    // A symlink or a file is removed itself, never what it points at
    match fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            let _ = fs::remove_file(dir);
            return;
        }
        Err(_) => return,
    }

    // The system's chmod, never one earlier on PATH such as coreutils' gnubin on macOS
    let chmod = ["/bin/chmod", "/usr/bin/chmod"]
        .into_iter()
        .find(|c| Path::new(c).exists())
        .unwrap_or("chmod");
    let _ = Command::new(chmod)
        .arg("-R")
        .arg("u+rwx")
        .arg(dir)
        .stderr(Stdio::null())
        .status();
    let _ = fs::remove_dir_all(dir);
}

/// Copies `src` to the new path `dst`, sharing blocks where the file system can
fn clone_dir(src: &Path, dst: &Path) -> Result<(), String> {
    // One clonefile call copies the whole tree on APFS
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let from = std::ffi::CString::new(src.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        let to = std::ffi::CString::new(dst.as_os_str().as_bytes()).map_err(|e| e.to_string())?;

        // Safe: both strings outlive the call, which only reads them
        if unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), 0) } == 0 {
            return Ok(());
        }
    }

    let copied = Command::new("cp")
        .args(if cfg!(target_os = "linux") {
            &["-a", "--reflink=auto"][..]
        } else {
            &["-pR"][..]
        })
        .arg(src)
        .arg(dst)
        .status()
        .map_err(|e| format!("cp: {e}"))?;

    if !copied.success() {
        return Err(format!(
            "could not copy {} to {}",
            src.display(),
            dst.display()
        ));
    }

    Ok(())
}

/// What every check gets from the user's environment, besides the names `[host] env` adds.
/// `HOME` and `TMPDIR` are replaced by the jail's
pub const BASE_ENV: &[&str] = &["PATH", "USER", "LOGNAME", "LANG", "LC_ALL", "SHELL", "TERM"];

/// Names `[host] env` may not pass, since they carry credentials or point git at another repository
pub fn refused_env(name: &str) -> bool {
    known::secret_env(name) || name.to_ascii_uppercase().starts_with("SHIPCHECK_")
}

/// Whether a variable from the user's environment reaches a check: the base list and the owner's
/// names, never a refused one
pub fn keeps_var(name: &str, extra: &[String]) -> bool {
    (BASE_ENV.contains(&name) || extra.iter().any(|e| e == name)) && !refused_env(name)
}

/**
 * Whether a check may find tools in `dir`. Not in a temp dir or a git checkout, where an agent can
 * write a fake `cargo` that exits 0 and put it first on the `PATH` it runs `shipcheck run` with.
 * The user's home is not a checkout here, so a dotfiles repository leaves `~/.cargo/bin` usable
 **/
fn trusted_dir(dir: &Path, user: &Path) -> bool {
    let mut temp: Vec<PathBuf> = known::TEMP_DIRS.iter().map(PathBuf::from).collect();
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

/// How a branch run gets the base's cache, for doctor
pub fn branch_cache() -> String {
    if cfg!(target_os = "macos") {
        return "Branch runs get a clonefile copy of the base's tool cache, which shares its blocks"
            .into();
    }

    let overlay = real_dir(&cache_dir(None))
        .and_then(|d| sandbox_jail::overlay_supported(&d).map_err(|e| e.to_string()));

    match overlay {
        Ok(()) => "Branch runs see the base's tool cache through an overlay, and their writes go to a layer of their own that the run removes".into(),
        Err(e) => format!("Branch runs get a copy of the base's tool cache, a full one on ext4 at about 4 s per GB, since {e}. bubblewrap 0.10 or later mounts an overlay instead, at no cost per GB"),
    }
}

/// The OS sandbox host checks run in, an error naming the fix when this machine has none
pub fn sandbox() -> Result<&'static str, String> {
    match Backend::detect() {
        Ok(Backend::Seatbelt) => Ok("sandbox-exec"),
        Ok(Backend::Bwrap) => Ok("bwrap"),
        // macOS refuses a sandbox inside a sandbox, such as an agent's, unless that one is as strict
        Err(sandbox_jail::Error::Nested) if in_a_jail() => Ok("sandbox-exec"),
        Err(sandbox_jail::Error::Nested) => Err(NESTED.into()),
        Err(e) => Err(format!("host mode needs an OS sandbox: {e}")),
    }
}

const NESTED: &str = "the host checks' sandbox cannot start inside another sandbox, such as an agent's, so nothing ran: run shipcheck outside it, for example by exempting the `shipcheck` command in the agent's sandbox settings. A push still gets its checks from the hook's run and the daemon, which run outside it";

/**
 * Whether this process already runs in a jail as strict as a host check's, as shipcheck's own
 * tests do inside its host checks. The kernel answers through `sandbox_check`, which job code
 * cannot fake, for what the profile denies: reading and writing the home, and Docker's socket.
 * An agent's looser sandbox fails one of them, so its checks still get their own jail
 **/
#[cfg(target_os = "macos")]
pub fn in_a_jail() -> bool {
    use sandbox_jail::macos::{denies, sandboxed};

    let home = crate::run::user_dir();
    // A sandbox that passes these is reused only where results never post, since any can pass them
    crate::run::home_overridden()
        && sandboxed()
        && denies("file-read-data", &home)
        && denies("file-write-data", &home.join(".shipcheck-probe"))
        && denies("network-outbound", &home.join(".docker/run/docker.sock"))
        && denies("network-outbound", Path::new("/var/run/docker.sock"))
}

#[cfg(not(target_os = "macos"))]
pub fn in_a_jail() -> bool {
    false
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

/// What a check may touch: it writes its clone, a home made new for each run and the repository's
/// tool caches, and reads the system, its toolchains and `readable`
pub struct Jail {
    pub writable: Vec<PathBuf>,
    pub readable: Vec<PathBuf>,
    pub home: PathBuf,
    pub cache: PathBuf,
    /// The upper and work directories of an overlay mounted on `cache`, which stays unchanged
    pub overlay: Option<(PathBuf, PathBuf)>,
    /// False for a check with `sandbox = false`, which runs with the user's own access
    pub sandboxed: bool,
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
        let mut hidden = known::credentials(user);
        hidden.extend(["token", "app.pem", "secrets"].map(|h| home.join(h)));
        hidden.extend(std::env::var_os("SHIPCHECK_APP_KEY_FILE").map(PathBuf::from));
        hidden.into_iter().map(resolved).collect()
    }

    /**
     * Writes only to the jail. The user's home is unreadable but for the toolchains and the jail.
     * Unix sockets connect only in the jail and to the DNS resolver, so Docker's or an agent's socket
     * is out of reach. Mach services only from the basic list, and no Apple events, so no copy of
     * `open` or `osascript` reaches Launch Services or another app, which would start a process
     * outside the sandbox. `launchctl` talks to launchd over the bootstrap port, which no sandbox rule
     * covers, so it may not run at all. Signals reach only the check's own processes, and terminals
     * only the ones it opened. On macOS nothing in the temp dirs but the jail's own paths, on Linux
     * new empty `/tmp`, `/run` and `/var/tmp`, so no Unix socket of the host is in reach by path
     **/
    fn policy(&self, backend: Backend) -> Result<Policy, String> {
        let user = Self::user()?;
        let seatbelt = backend == Backend::Seatbelt;
        let mut readable = self.readable(&user);
        // Without its cache every `/usr/bin` shim, git and cc among them, asks xcodebuild for a second
        readable.extend(xcrun_db());
        let mut unreadable = vec![user.clone()];
        let (mut pinned, mut no_exec, mut scratch) = (Vec::new(), Vec::new(), Vec::new());

        if seatbelt {
            unreadable.extend(known::temp_dirs());

            // Writes go inside the cache, never to the dir itself, so no check swaps it for a symlink
            if self.cache != self.home && self.writable.contains(&self.cache) {
                pinned.push(self.cache.clone());
            }

            // A setuid binary runs as root, which shipcheck cannot stop when it outlives its check
            no_exec.push(PathBuf::from(LAUNCHCTL));
            no_exec.extend(known::setuid_binaries().iter().cloned());
        } else {
            scratch = ["/tmp", "/run", "/var/run", "/var/tmp"]
                .map(PathBuf::from)
                .to_vec();
        }

        Ok(Policy {
            writable: self.writable.clone(),
            read_only: Vec::new(),
            pinned,
            unreadable,
            readable,
            scratch,
            hidden: Self::hidden(&user),
            network: true,
            unix_sockets: Access::Only(self.writable.clone()),
            mach_services: Access::Only(
                known::BASIC_MACH_SERVICES
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            apple_events: false,
            own_signals: true,
            own_terminals: true,
            no_exec,
            overlays: self
                .overlay
                .iter()
                .map(|(upper, work)| sandbox_jail::Overlay {
                    lower: self.cache.clone(),
                    upper: upper.clone(),
                    work: work.clone(),
                })
                .collect(),
        })
    }

    /// What goes before the check's command: the sandbox, or `env` when it runs without one
    fn wrapper(&self) -> Result<Vec<std::ffi::OsString>, String> {
        let backend = match sandbox()? {
            _ if !self.sandboxed => None,
            // macOS refuses a sandbox inside a sandbox, and the one around this process is as strict
            "sandbox-exec" if in_a_jail() => None,
            "sandbox-exec" => Some(Backend::Seatbelt),
            _ => Some(Backend::Bwrap),
        };

        let Some(backend) = backend else {
            return Ok(vec!["/usr/bin/env".into()]);
        };

        let sandbox = Sandbox::new(backend, self.policy(backend)?).map_err(|e| e.to_string())?;
        Ok(sandbox.argv().to_vec())
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
        let wrapper = self.wrapper()?;
        let mut cmd = Command::new(&wrapper[0]);
        cmd.args(&wrapper[1..]);
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
            .stdin(Stdio::null());

        // Tests of the jail read it, since macOS refuses a sandbox inside a sandbox
        if self.sandboxed {
            cmd.env("SHIPCHECK_JAIL", "1");
        }

        if self.home.join(SBF_SDK).is_dir() {
            cmd.env("SBF_SDK_PATH", self.home.join(SBF_SDK));
        }

        if let Some(sdk) = sdk_root() {
            cmd.env("SDKROOT", sdk);
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
 * The tool caches of host checks, per repository, written only by checks. Runs of the base's tree
 * write `base`, and every other run writes an overlay of it or a copy of it of its own, under `runs`
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
        shims(&path)?;
    }

    Ok((dir, path))
}

/// The macOS SDK, found once outside the jail, so a build in it never asks xcrun to cache a lookup
#[cfg(target_os = "macos")]
fn sdk_root() -> Option<&'static Path> {
    static SDK: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    SDK.get_or_init(|| {
        let out = Command::new("/usr/bin/xcrun")
            .arg("--show-sdk-path")
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        Some(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())).filter(|p| p.is_dir())
    })
    .as_deref()
}

#[cfg(not(target_os = "macos"))]
fn sdk_root() -> Option<&'static Path> {
    None
}

/// xcrun's lookup cache, in the user's temp dir whatever `TMPDIR` says, read-only to a check
#[cfg(target_os = "macos")]
fn xcrun_db() -> Option<PathBuf> {
    let mut buf = vec![0u8; 1024];
    // Safe: confstr writes at most `buf.len()` bytes into a buffer this function owns
    let n = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };

    if n == 0 || n > buf.len() {
        return None;
    }

    buf.truncate(n - 1);
    let dir = PathBuf::from(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(buf));
    Some(resolved(dir.join("xcrun_db")))
}

#[cfg(not(target_os = "macos"))]
fn xcrun_db() -> Option<PathBuf> {
    None
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

/// The setuid binaries a check would run by name from `path`, the first match of each name
fn denied_by_name(path: &std::ffi::OsStr) -> Vec<String> {
    let dirs: Vec<PathBuf> = std::env::split_paths(path).collect();

    known::setuid_binaries()
        .iter()
        .filter_map(|p| p.file_name()?.to_str().map(|n| (n.to_string(), p)))
        .filter(|(name, denied)| {
            dirs.iter()
                .map(|d| d.join(name))
                .find(|c| c.is_file())
                .is_some_and(|first| resolved(first) == **denied)
        })
        .map(|(name, _)| name)
        .collect()
}

/// The jail's `mktemp`, and a command for each setuid binary that says why the jail denies it
fn shims(home: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.join(SHIMS);
    fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
    let mut files = vec![("mktemp".to_string(), MKTEMP.to_string())];
    // Only where the name finds the setuid binary first, so a user's own `ps` still runs
    files.extend(denied_by_name(&jail_path()).into_iter().map(|name| {
        let text = format!("#!/bin/sh\necho \"shipcheck: {name} is setuid root, which a macOS host check cannot run, since shipcheck could not stop it once it outlived the check. Run this job under act instead\" >&2\nexit 126\n");
        (name, text)
    }));

    for (name, text) in files {
        let file = bin.join(name);
        fs::write(&file, text).map_err(|e| e.to_string())?;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }

    // The compilers themselves, since each `/usr/bin` shim asks xcrun, whose cache a check cannot write
    for (name, real) in compilers() {
        std::os::unix::fs::symlink(real, bin.join(name)).map_err(|e| e.to_string())?;
    }

    Ok(())
}

/// Where `xcrun -f` finds each C compiler, asked once outside the jail
fn compilers() -> &'static [(&'static str, PathBuf)] {
    static FOUND: std::sync::OnceLock<Vec<(&'static str, PathBuf)>> = std::sync::OnceLock::new();
    FOUND.get_or_init(|| {
        ["cc", "c++", "clang", "clang++"]
            .into_iter()
            .filter_map(|name| {
                let out = Command::new("/usr/bin/xcrun")
                    .args(["-f", name])
                    .output()
                    .ok()?;
                let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
                (out.status.success() && path.is_file()).then_some((name, path))
            })
            .collect()
    })
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
        overlay: None,
        sandboxed: true,
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
        let base = real_dir(&cache_dir(Some(request.repo_id)).join("base"))?;
        let (cache, overlay) = if request.on_base {
            (base.clone(), None)
        } else {
            let run = self.run_cache(request.repo_id)?;

            // bwrap mounts the overlay, so a check outside the sandbox gets a copy
            if self.overlays() && command.sandbox {
                // Each check its own upper layer, since two overlays must not share one
                let layer = tempfile::Builder::new()
                    .prefix("check-")
                    .tempdir_in(&run)
                    .map_err(|e| format!("{}: {e}", run.display()))?
                    .keep();
                let (upper, work) = (layer.join("upper"), layer.join("work"));
                (base.clone(), Some((real_dir(&upper)?, real_dir(&work)?)))
            } else {
                (run, None)
            }
        };
        let mut writable = vec![real_dir(request.run_dir)?, home.clone()];
        let mut readable = Vec::new();

        if overlay.is_none() {
            writable.push(cache.clone());
        }

        // A copy still holds the absolute paths into the base that tools such as pre-commit saved
        if cache != base {
            readable.push(base.clone());
        }

        let jail = Jail {
            writable,
            readable,
            cache,
            home,
            overlay,
            sandboxed: command.sandbox,
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

        if !dir.exists() {
            return Vec::new();
        }

        let held = run_locks(repo_id)
            .into_iter()
            .find(|lock| fs::File::open(lock).is_ok_and(|f| !locked(&f)));

        if let Some(lock) = held {
            return vec![format!(
                "kept {}, a run holds {}",
                dir.display(),
                lock.display()
            )];
        }

        remove_tree(&dir);

        if dir.exists() {
            return vec![format!("could not remove {}", dir.display())];
        }

        vec![format!("removed {}", dir.display())]
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

    /// The ways a branch run can get the base's cache here: a copy, and an overlay where bwrap mounts one
    fn modes() -> Vec<bool> {
        let dir = tempfile::tempdir().unwrap();
        let mut modes = vec![false];

        if sandbox_jail::overlay_supported(dir.path()).is_ok() {
            modes.push(true);
        }

        modes
    }

    /// A jail whose clone is `dir`, home and cache inside it
    fn jail(dir: &Path) -> Jail {
        let dir = dir.canonicalize().unwrap();
        Jail {
            writable: vec![dir.clone()],
            readable: Vec::new(),
            home: real_dir(&dir.join("home")).unwrap(),
            cache: real_dir(&dir.join("cache")).unwrap(),
            overlay: None,
            sandboxed: true,
        }
    }

    impl Jail {
        /// The `sandbox-exec` profile, on any OS
        fn profile(&self) -> Result<String, String> {
            let sandbox = Sandbox::seatbelt(self.policy(Backend::Seatbelt)?);
            let sandbox = sandbox.map_err(|e| e.to_string())?;
            Ok(sandbox.argv()[2].to_string_lossy().into_owned())
        }
    }

    #[test]
    fn a_check_with_sandbox_false_runs_outside_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut open = jail(dir.path());
        open.sandboxed = false;
        let cmd = open.shell("true", dir.path(), &[]).unwrap();
        assert_eq!(cmd.get_program(), "/usr/bin/env");
        assert!(!cmd.get_envs().any(|(k, _)| k == "SHIPCHECK_JAIL"));

        let file = outside.path().join("written");
        let status = open
            .shell(&format!("echo x > '{}'", file.display()), dir.path(), &[])
            .unwrap()
            .status()
            .unwrap();
        assert!(
            status.success() && file.exists(),
            "it writes outside its clone"
        );

        // Its home and caches still go to the run's own directories
        let home = cmd
            .get_envs()
            .find(|(k, _)| *k == "HOME")
            .and_then(|(_, v)| v)
            .unwrap();
        assert_eq!(Path::new(home), open.home);
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
            overlay: None,
            sandboxed: true,
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
        let xcrun = xcrun_db().map_or(String::new(), |p| format!(" (subpath \"{}\")", p.display()));
        assert!(profile.contains(&format!("(subpath \"/j/repo\"){xcrun} {paths})")));
        assert_eq!(xcrun.ends_with("/xcrun_db\")"), cfg!(target_os = "macos"));
        assert!(profile.contains("(deny network-outbound (remote unix-socket))\n"));
        assert!(profile.contains(
            r#"(allow network-outbound (remote unix-socket (subpath "/j/clone \"x\"")) (remote unix-socket (subpath "/j/home")))"#
        ));
        assert!(profile.contains("(deny appleevent-send)"));
        assert!(profile.contains("(deny mach-lookup)\n(allow mach-lookup (global-name"));
        assert!(!profile.contains("SecurityServer") && !profile.contains("launchservicesd"));
        let bad = Jail {
            writable: vec![PathBuf::from(std::ffi::OsString::from_vec(
                b"/\xff".to_vec(),
            ))],
            readable: Vec::new(),
            home: "/j".into(),
            cache: "/j".into(),
            overlay: None,
            sandboxed: true,
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
        let dir = tempfile::tempdir().unwrap();
        let clone = real_dir(&dir.path().join("clone")).unwrap();
        let home = real_dir(&dir.path().join("home")).unwrap();
        let jail = Jail {
            writable: vec![clone.clone(), home.clone()],
            readable: Vec::new(),
            home: home.clone(),
            cache: home.clone(),
            overlay: None,
            sandboxed: true,
        };
        let sandbox = Sandbox::bwrap(jail.policy(Backend::Bwrap).unwrap()).unwrap();
        let args: Vec<String> = sandbox
            .argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let all = args.join(" ");
        assert_eq!(args[..4], ["bwrap", "--ro-bind", "/", "/"]);

        for flag in ["--unshare-pid", "--new-session"] {
            assert!(all.contains(flag), "{flag}");
        }

        // New empty temp dirs, so no Unix socket of the host is in reach by path
        for dir in ["/tmp", "/run", "/var/tmp"].map(PathBuf::from) {
            if dir.is_dir() {
                let dir = resolved(dir);
                assert!(all.contains(&format!("--tmpfs {}", dir.display())), "{all}");
            }
        }

        let user = resolved(crate::run::user_dir());
        assert!(all.contains(&format!("--tmpfs {}", user.display())));
        assert!(!all.contains("--share-net") && !all.contains("--unshare-net"));

        for path in [&clone, &home] {
            assert!(all.contains(&format!("--bind {0} {0}", path.display())));
        }

        assert!(all.ends_with(&format!("--remount-ro {} --", user.display())));
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
            overlay: None,
            sandboxed: true,
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
            overlay: None,
            sandboxed: true,
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
            overlay: None,
            sandboxed: true,
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
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_branch_run_never_sees_what_another_branch_run_left_in_the_cache() {
        for overlay in modes() {
            a_branch_run_sees_only_the_base(overlay);
        }
    }

    fn a_branch_run_sees_only_the_base(overlay: bool) {
        let dir = tempfile::tempdir().unwrap();
        let repo_id = format!("test/{}", dir.path().file_name().unwrap().to_string_lossy());
        let checks = BTreeMap::from([
            (
                "base".into(),
                "mkdir -p \"$CARGO_HOME\" && touch \"$CARGO_HOME/from-base\"".into(),
            ),
            (
                "branch".into(),
                "mkdir -p \"$CARGO_HOME\" && touch \"$CARGO_HOME/from-branch\"".into(),
            ),
            (
                "other".into(),
                "test -e \"$CARGO_HOME/from-base\" && test ! -e \"$CARGO_HOME/from-branch\"".into(),
            ),
        ]);
        // Each call is a run of its own, as each run makes its own runners
        let run = |job: &str, on_base: bool| {
            let host = Host::new(
                config::Host {
                    checks: checks.clone(),
                    ..Default::default()
                },
                dir.path(),
            )
            .with_overlay(overlay);
            let clone = tempfile::tempdir().unwrap();
            let log = dir.path().join(format!("{job}.log"));
            let (needs, artifacts) = (BTreeMap::new(), dir.path().join("artifacts"));
            let request = RunRequest {
                run_dir: clone.path(),
                file: HOST,
                workflow: Path::new("/dev/null"),
                job,
                needs: &needs,
                artifacts: &artifacts,
                repo_id: &repo_id,
                timeout: Duration::from_secs(30),
                shard: false,
                on_base,
                slot_lock: None,
            };
            let exit = host.run(&request, &log).unwrap().exit;
            (exit, fs::read_to_string(&log).unwrap())
        };
        assert_eq!(run("base", true).0, Some(0));
        assert_eq!(run("branch", false).0, Some(0));
        let (exit, log) = run("other", false);
        let cache = cache_dir(Some(&repo_id));
        let planted = cache.join("base/cargo/from-branch").exists();
        let left: Vec<_> = fs::read_dir(cache.join("runs"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        Host::new(config::Host::default(), dir.path()).clean(Some(&repo_id));
        assert_eq!(
            exit,
            Some(0),
            "overlay {overlay}: saw the other branch's file or missed the base's: {log}"
        );
        assert!(!planted, "overlay {overlay}: a branch run wrote the base");
        assert!(left.is_empty(), "overlay {overlay}: {left:?}");
    }

    /// A branch run's request for `job` in `clone`
    fn branch_request<'a>(
        clone: &'a Path,
        job: &'a str,
        repo_id: &'a str,
        empty: &'a (BTreeMap<String, BTreeMap<String, String>>, PathBuf),
    ) -> RunRequest<'a> {
        RunRequest {
            run_dir: clone,
            file: HOST,
            workflow: Path::new("/dev/null"),
            job,
            needs: &empty.0,
            artifacts: &empty.1,
            repo_id,
            timeout: Duration::from_secs(30),
            shard: false,
            on_base: false,
            slot_lock: None,
        }
    }

    #[test]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_check_that_swaps_its_runs_dir_for_a_symlink_never_gets_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let target_path = target.path().canonicalize().unwrap();
        let repo_id = format!("test/{}", dir.path().file_name().unwrap().to_string_lossy());
        let checks = BTreeMap::from([
            (
                "swap".into(),
                format!(
                    "d=$(dirname \"$CARGO_HOME\"); rm -rf \"$d\" && ln -s '{}' \"$d\"; true",
                    target_path.display()
                )
                .into(),
            ),
            (
                "write".into(),
                "mkdir -p \"$CARGO_HOME\" && touch \"$CARGO_HOME/../x\"".into(),
            ),
        ]);
        let host = Host::new(
            config::Host {
                checks,
                ..Default::default()
            },
            dir.path(),
        )
        .with_overlay(false);
        let empty = (BTreeMap::new(), dir.path().join("artifacts"));
        // One run, so both checks share the run's directory, as a retry does
        let run = |job: &str| {
            let clone = tempfile::tempdir().unwrap();
            let log = dir.path().join(format!("{job}.log"));
            let exit = host
                .run(&branch_request(clone.path(), job, &repo_id, &empty), &log)
                .map(|o| o.exit);
            (exit, fs::read_to_string(&log).unwrap_or_default())
        };
        let _ = run("swap");
        let (exit, log) = run("write");
        drop(host);
        let left: Vec<_> = fs::read_dir(&target_path)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        Host::new(config::Host::default(), dir.path()).clean(Some(&repo_id));
        assert!(
            left.is_empty(),
            "a check wrote the symlink's target: {left:?}"
        );
        // The sandbox kept the directory, so the next check still had it
        assert_eq!(exit, Ok(Some(0)), "{log}");
    }

    #[test]
    fn a_runs_dir_replaced_by_a_symlink_is_refused_before_the_next_check() {
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let repo_id = format!("test/{}", dir.path().file_name().unwrap().to_string_lossy());
        let host = Host::new(config::Host::default(), dir.path()).with_overlay(false);
        let run = host.run_cache(&repo_id).unwrap();
        remove_tree(&run);
        std::os::unix::fs::symlink(target.path(), &run).unwrap();
        let again = host.run_cache(&repo_id);
        drop(host);
        Host::new(config::Host::default(), dir.path()).clean(Some(&repo_id));
        assert!(again.is_err(), "the run's cache became {again:?}");
    }

    #[test]
    fn remove_tree_removes_a_symlink_and_never_touches_its_target() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join("f"), "x").unwrap();
        fs::set_permissions(victim.join("f"), fs::Permissions::from_mode(0o444)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        remove_tree(&link);
        let mode = fs::metadata(victim.join("f")).unwrap().permissions().mode() & 0o777;
        assert!(fs::symlink_metadata(&link).is_err(), "the link stays");
        assert_eq!(mode, 0o444, "chmod followed the link");

        // A read-only tree, as Go's module cache is, goes whole
        let ro = dir.path().join("ro");
        fs::create_dir_all(ro.join("m")).unwrap();
        fs::write(ro.join("m/f"), "x").unwrap();
        fs::set_permissions(ro.join("m"), fs::Permissions::from_mode(0o555)).unwrap();
        remove_tree(&ro);
        assert!(!ro.exists());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_slot_whose_old_copy_survives_removal_is_never_reused() {
        let runs = tempfile::tempdir().unwrap();
        let stuck = runs.path().join("run-0/planted");
        fs::create_dir_all(stuck.parent().unwrap()).unwrap();
        fs::write(&stuck, "x").unwrap();
        let flag = |f: &str| Command::new("chflags").arg(f).arg(&stuck).status().unwrap();
        assert!(flag("uchg").success());
        let copy = RunCopy::new(runs.path());
        flag("nouchg");
        assert_eq!(copy.unwrap().dir, runs.path().join("run-1"));
    }

    #[test]
    fn a_run_removes_dead_probes_unlinked_locks_and_the_old_shared_cache() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let repo_id = format!("test/{}", dir.path().file_name().unwrap().to_string_lossy());
        let cache = cache_dir(Some(&repo_id));
        let runs = cache.join("runs");
        fs::create_dir_all(cache.join("base/cargo")).unwrap();
        fs::create_dir_all(runs.join(".sandbox-jail-probe-old/lower")).unwrap();
        fs::create_dir_all(runs.join(".sandbox-jail-probe-new/lower")).unwrap();
        fs::write(runs.join(".new-dead"), "").unwrap();
        fs::write(runs.join(".new-young"), "").unwrap();
        let live = fs::File::create(runs.join(".new-live")).unwrap();
        assert!(locked(&live));
        assert!(
            Command::new("touch")
                .args(["-t", "200001010000"])
                .arg(runs.join(".sandbox-jail-probe-old"))
                .arg(runs.join(".new-dead"))
                .arg(runs.join(".new-live"))
                .status()
                .unwrap()
                .success()
        );
        fs::create_dir_all(cache.join("branches/go/pkg/mod/m")).unwrap();
        fs::write(cache.join("branches/go/pkg/mod/m/f"), "x").unwrap();
        fs::set_permissions(
            cache.join("branches/go/pkg/mod/m"),
            fs::Permissions::from_mode(0o555),
        )
        .unwrap();

        let host = Host::new(config::Host::default(), dir.path()).with_overlay(false);
        host.run_cache(&repo_id).unwrap();
        let gone = |p: &str| fs::symlink_metadata(cache.join(p)).is_err();
        let (branches, old, new, dead, kept) = (
            gone("branches"),
            gone("runs/.sandbox-jail-probe-old"),
            gone("runs/.sandbox-jail-probe-new"),
            gone("runs/.new-dead"),
            !gone("runs/.new-live") && !gone("runs/.new-young"),
        );
        drop((host, live));
        Host::new(config::Host::default(), dir.path()).clean(Some(&repo_id));
        assert!(branches, "the old shared cache stays");
        assert!(old, "a dead probe stays");
        assert!(!new, "a probe that may still run was removed");
        assert!(dead, "an unlinked lock no run holds stays");
        assert!(
            kept,
            "a lock a live run holds, or one just made, was removed"
        );
    }

    /// One host check as a run of a branch's tree, in a run of its own, its exit and its log
    fn branch_check(dir: &Path, repo_id: &str, command: &str) -> (Option<i32>, String) {
        let checks = BTreeMap::from([("probe".into(), command.into())]);
        let host = Host::new(
            config::Host {
                checks,
                ..Default::default()
            },
            dir,
        );
        let clone = tempfile::tempdir().unwrap();
        let log = dir.join("probe.log");
        let (needs, artifacts) = (BTreeMap::new(), dir.join("artifacts"));
        let request = RunRequest {
            run_dir: clone.path(),
            file: HOST,
            workflow: Path::new("/dev/null"),
            job: "probe",
            needs: &needs,
            artifacts: &artifacts,
            repo_id,
            timeout: Duration::from_secs(30),
            shard: false,
            on_base: false,
            slot_lock: None,
        };
        let exit = host.run(&request, &log).unwrap().exit;
        (exit, fs::read_to_string(&log).unwrap_or_default())
    }

    #[test]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_branch_run_reads_paths_into_the_base_cache_and_cannot_write_them() {
        let dir = tempfile::tempdir().unwrap();
        let repo_id = format!("test/{}", dir.path().file_name().unwrap().to_string_lossy());
        let base = real_dir(&cache_dir(Some(&repo_id)).join("base/xdg/pre-commit")).unwrap();
        fs::write(base.join("hooks.yaml"), "hooks").unwrap();
        // Under bwrap's overlay the touch succeeds in the run's own layer, so only the base on disk counts
        let command = format!(
            "cat {0}/hooks.yaml && {{ touch {0}/planted; true; }}",
            base.display()
        );
        let (exit, log) = branch_check(dir.path(), &repo_id, &command);
        let planted = base.join("planted").exists();
        Host::new(config::Host::default(), dir.path()).clean(Some(&repo_id));
        assert_eq!(exit, Some(0), "{log}");
        assert!(!planted, "a branch run wrote the base's cache");
    }

    #[test]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn consecutive_branch_runs_get_the_same_cargo_home_so_builds_stay_warm() {
        let dir = tempfile::tempdir().unwrap();
        let repo_id = format!("test/{}", dir.path().file_name().unwrap().to_string_lossy());
        fs::create_dir_all(cache_dir(Some(&repo_id)).join("base/cargo")).unwrap();
        let runs: Vec<_> = (0..2)
            .map(|_| branch_check(dir.path(), &repo_id, "echo \"home=$CARGO_HOME\""))
            .collect();
        Host::new(config::Host::default(), dir.path()).clean(Some(&repo_id));
        assert!(runs.iter().all(|(exit, _)| *exit == Some(0)), "{runs:?}");
        let homes: Vec<_> = runs
            .iter()
            .filter_map(|(_, log)| log.lines().find_map(|l| l.strip_prefix("home=")))
            .collect();
        assert_eq!(homes.len(), 2, "{runs:?}");
        assert_eq!(
            homes[0], homes[1],
            "cargo keys a build by its sources' paths"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_file_one_run_makes_immutable_never_reaches_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let repo_id = format!("test/{}", dir.path().file_name().unwrap().to_string_lossy());
        fs::create_dir_all(cache_dir(Some(&repo_id)).join("base/cargo")).unwrap();
        let plant = "mkdir -p \"$CARGO_HOME/registry\" && touch \"$CARGO_HOME/registry/planted\" && chflags uchg \"$CARGO_HOME/registry/planted\"";
        let (planted, log) = branch_check(dir.path(), &repo_id, plant);
        let (next, next_log) = branch_check(
            dir.path(),
            &repo_id,
            "test ! -e \"$CARGO_HOME/registry/planted\"",
        );
        let runs = cache_dir(Some(&repo_id)).join("runs");
        let _ = Command::new("chflags")
            .args(["-R", "nouchg"])
            .arg(&runs)
            .status();
        Host::new(config::Host::default(), dir.path()).clean(Some(&repo_id));
        assert_eq!(planted, Some(0), "{log}");
        assert_eq!(
            next,
            Some(0),
            "the next run saw the planted file: {next_log}"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_rust_build_in_a_check_links_without_asking_xcrun_to_write_its_cache() {
        let dir = tempfile::tempdir().unwrap();
        let repo_id = format!("test/{}", dir.path().file_name().unwrap().to_string_lossy());
        let build = "cargo new -q --vcs none hello && cd hello && cargo build -q --offline";
        let (exit, log) = branch_check(dir.path(), &repo_id, build);
        Host::new(config::Host::default(), dir.path()).clean(Some(&repo_id));
        assert_eq!(exit, Some(0), "{log}");
        assert!(!log.contains("couldn't create cache file"), "{log}");
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_check_reads_xcruns_cache_and_cannot_write_it() {
        let Some(db) = xcrun_db().filter(|p| p.is_file()) else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let repo_id = format!("test/{}", dir.path().file_name().unwrap().to_string_lossy());
        let q = db.display();
        let command = format!("cat '{q}' > /dev/null && ! touch '{q}' 2>/dev/null");
        let (exit, log) = branch_check(dir.path(), &repo_id, &command);
        Host::new(config::Host::default(), dir.path()).clean(Some(&repo_id));
        assert_eq!(
            exit,
            Some(0),
            "without the cache every /usr/bin shim asks xcodebuild first: {log}"
        );
    }

    #[test]
    #[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
    fn a_branch_run_without_an_overlay_writes_a_copy_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let repo_id = format!("test/{}", dir.path().file_name().unwrap().to_string_lossy());
        let base = cache_dir(Some(&repo_id)).join("base");
        fs::create_dir_all(base.join("cargo")).unwrap();
        fs::write(base.join("cargo/from-base"), "").unwrap();
        let checks = BTreeMap::from([(
            "copy".into(),
            "case \"$CARGO_HOME\" in */runs/run-*/cargo) ;; *) exit 3 ;; esac && test -e \"$CARGO_HOME/from-base\" && touch \"$CARGO_HOME/from-branch\"".into(),
        )]);
        let host = Host::new(
            config::Host {
                checks,
                ..Default::default()
            },
            dir.path(),
        )
        .with_overlay(false);
        let clone = tempfile::tempdir().unwrap();
        let log = dir.path().join("copy.log");
        let (needs, artifacts) = (BTreeMap::new(), dir.path().join("artifacts"));
        let request = RunRequest {
            run_dir: clone.path(),
            file: HOST,
            workflow: Path::new("/dev/null"),
            job: "copy",
            needs: &needs,
            artifacts: &artifacts,
            repo_id: &repo_id,
            timeout: Duration::from_secs(30),
            shard: false,
            on_base: false,
            slot_lock: None,
        };
        let exit = host.run(&request, &log).unwrap().exit;
        drop(host);
        let planted = base.join("cargo/from-branch").exists();
        let runs = fs::read_dir(cache_dir(Some(&repo_id)).join("runs"))
            .unwrap()
            .count();
        Host::new(config::Host::default(), dir.path()).clean(Some(&repo_id));
        assert_eq!(exit, Some(0), "{}", fs::read_to_string(&log).unwrap());
        assert!(!planted, "the copy was the base");
        assert_eq!(runs, 0, "the run left its copy");
    }

    #[test]
    fn a_run_leaves_the_copy_of_a_run_going_alone_and_removes_a_dead_runs() {
        for overlay in modes() {
            a_run_leaves_a_live_runs_dir_alone(overlay);
        }
    }

    fn a_run_leaves_a_live_runs_dir_alone(overlay: bool) {
        let dir = tempfile::tempdir().unwrap();
        let repo_id = format!("test/{}", dir.path().file_name().unwrap().to_string_lossy());
        let base = cache_dir(Some(&repo_id)).join("base");
        fs::create_dir_all(base.join("cargo")).unwrap();
        // A copy keeps the base's times, and an old base once made a live copy look abandoned
        assert!(
            Command::new("touch")
                .args(["-t", "200001010000"])
                .arg(&base)
                .status()
                .unwrap()
                .success()
        );
        let host = || Host::new(config::Host::default(), dir.path()).with_overlay(overlay);
        let runs = cache_dir(Some(&repo_id)).join("runs");
        let dead = runs.join("run-dead");
        fs::create_dir_all(dead.join("cargo")).unwrap();
        fs::write(runs.join("run-dead.lock"), "").unwrap();

        let (first, second) = (host(), host());
        let one = first.run_cache(&repo_id).unwrap();
        let two = second.run_cache(&repo_id).unwrap();
        assert_ne!(one, two);
        assert!(
            one.is_dir() && one.join("cargo").is_dir() != overlay,
            "overlay {overlay}: the second run removed the first's copy"
        );
        assert!(!dead.exists(), "a copy no run holds stays");
        drop((first, second));
        let left: Vec<_> = fs::read_dir(&runs)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        Host::new(config::Host::default(), dir.path()).clean(Some(&repo_id));
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn runs_racing_on_one_cache_never_remove_each_others_copies() {
        for overlay in modes() {
            racing_runs_keep_their_dirs(overlay);
        }
    }

    fn racing_runs_keep_their_dirs(overlay: bool) {
        let dir = tempfile::tempdir().unwrap();
        let repo_id = format!("test/{}", dir.path().file_name().unwrap().to_string_lossy());
        fs::create_dir_all(cache_dir(Some(&repo_id)).join("base/cargo")).unwrap();
        let barrier = std::sync::Barrier::new(8);
        let host = || Host::new(config::Host::default(), dir.path()).with_overlay(overlay);
        // Recorded, not asserted, so a failing thread never leaves the others at a barrier
        let removed = std::sync::Mutex::new(Vec::new());

        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    barrier.wait();
                    let mine = host();
                    let copy = mine.run_cache(&repo_id);
                    barrier.wait();

                    // Each cleanup runs while every other copy is in use
                    for _ in 0..5 {
                        let _ = host().run_cache(&repo_id);
                    }

                    barrier.wait();

                    if !copy
                        .as_ref()
                        .is_ok_and(|c| c.is_dir() && c.join("cargo").is_dir() != overlay)
                    {
                        removed.lock().unwrap().push(copy);
                    }
                });
            }
        });

        let runs = cache_dir(Some(&repo_id)).join("runs");
        let left: Vec<_> = fs::read_dir(&runs)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        host().clean(Some(&repo_id));
        assert!(
            removed.lock().unwrap().is_empty(),
            "{:?}",
            removed.lock().unwrap()
        );
        assert!(left.is_empty(), "{left:?}");
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
        assert!(known::setuid_binaries().contains(&PathBuf::from("/bin/ps")));
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let jail = jail(dir.path());
        shims(&jail.home).unwrap();
        let run = |cmd: &str| {
            let exit = run_command(cmd, dir.path(), &jail, &[], &log, Duration::from_secs(10));
            (exit.unwrap(), fs::read_to_string(&log).unwrap())
        };
        let (exit, _) = run("/bin/ps -p $$");
        assert_ne!(exit, Some(0));
        // By name, as test suites and build scripts call it, the error names the cause
        let (exit, text) = run("ps -p $$");
        assert_eq!(exit, Some(126), "{text}");
        assert!(
            text.contains("ps is setuid root") && text.contains("under act"),
            "{text}"
        );
        // pgrep has no process list here, so it cannot show a root process's arguments either
        let (exit, text) = run("pgrep -lf securityd");
        assert_ne!(exit, Some(0), "{text}");
        assert!(!text.contains("/usr/libexec/securityd"), "{text}");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_users_own_tool_of_a_setuid_name_is_not_shadowed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let ps = dir.path().join("ps");
        fs::write(&ps, "#!/bin/sh\necho procps\n").unwrap();
        fs::set_permissions(&ps, fs::Permissions::from_mode(0o755)).unwrap();
        let ahead = std::env::join_paths([dir.path(), Path::new("/bin")]).unwrap();
        assert!(!denied_by_name(&ahead).contains(&"ps".to_string()));
        assert!(denied_by_name("/bin".as_ref()).contains(&"ps".to_string()));
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
        /// Boots out the test's launchd job, also when an assertion fails
        struct BootOut(String);

        impl Drop for BootOut {
            fn drop(&mut self) {
                let launchctl = |a: &str| {
                    Command::new(LAUNCHCTL)
                        .args([a, self.0.as_str()])
                        .output()
                        .is_ok_and(|o| o.status.success())
                };

                // A bootout can fail while launchd still stops the job, so it tries until the job is gone
                for _ in 0..50 {
                    if !launchctl("bootout") && !launchctl("print") {
                        break;
                    }

                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }

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
        let job = BootOut(target.clone());
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
        drop(job);
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
        let dir = tempfile::tempdir().unwrap();
        let no_exec = jail(dir.path()).policy(Backend::Seatbelt).unwrap().no_exec;

        for path in [
            "/usr/libexec/authopen",
            "/usr/libexec/security_authtrampoline",
            "/System/Library/CoreServices/RemoteManagement/ARDAgent.app/Contents/MacOS/ARDAgent",
            LAUNCHCTL,
        ] {
            if Path::new(path).exists() {
                assert!(no_exec.contains(&PathBuf::from(path)), "{path}");
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
