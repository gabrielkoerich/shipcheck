use crate::config::refused_env;
use crate::paths;
use crate::runner;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use sandbox_jail::known::{self, LAUNCHCTL};
#[cfg(target_os = "macos")]
use sandbox_jail::macos::{denies, sandboxed};
use sandbox_jail::{Access, Backend, Policy, Sandbox};

use super::{Outcome, RunRequest, Runner, shard};
use crate::config;

/// The one "workflow" of a host run, its jobs are the checks
pub const HOST: &str = "host";

/// What ran the jobs of the workflow `file`, host checks or act
pub fn runner_of(file: &str) -> config::RunnerKind {
    if file == HOST {
        config::RunnerKind::Host
    } else {
        config::RunnerKind::Act
    }
}

/// Whether a branch run's checks see the base's cache through an overlay, for tests of each way
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Overlay {
    On,
    Off,
}

/// Runs the owner's `[host]` checks natively, as the user, each in its own process group
#[derive(Debug)]
pub struct Host {
    config: config::Host,
    /// Where `versions` run, so a toolchain file in the repository picks the version
    repo: PathBuf,
    /// This run's directory under `runs`, with its copy of the base's cache or its checks' overlays
    copy: std::sync::OnceLock<Result<RunCopy, String>>,
    /// Whether checks of another tree see the base's cache through an overlay instead of a copy
    overlay: std::sync::OnceLock<bool>,
    shards: config::ShardTests,
}

impl Host {
    pub fn new(config: config::Host, repo: &Path) -> Self {
        Host {
            config,
            repo: repo.to_path_buf(),
            copy: std::sync::OnceLock::new(),
            overlay: std::sync::OnceLock::new(),
            shards: config::ShardTests::default(),
        }
    }

    #[must_use]
    pub fn with_shards(self, shards: config::ShardTests) -> Self {
        Host { shards, ..self }
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
                fs::create_dir(copy.dir()).map_err(|e| format!("{}: {e}", copy.dir().display()))?;
            } else {
                clone_dir(&base, copy.dir())?;
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
    fn with_overlay(self, overlay: Overlay) -> Self {
        self.overlay.set(overlay == Overlay::On).unwrap();
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
#[derive(Debug)]
struct RunCopy {
    dir: PathBuf,
    lock: PathBuf,
    // Closed after `drop` removes both paths, which releases the lock
    _file: fs::File,
}

impl RunCopy {
    fn dir(&self) -> &Path {
        &self.dir
    }

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
    match (file.metadata(), fs::symlink_metadata(path)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Takes an exclusive lock on `file` without waiting, held until the file is closed
fn locked(file: &fs::File) -> bool {
    // SAFETY: flock only takes a lock on a descriptor this file owns
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
        let from = std::ffi::CString::new(src.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        let to = std::ffi::CString::new(dst.as_os_str().as_bytes()).map_err(|e| e.to_string())?;

        // SAFETY: both strings outlive the call, which only reads them
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
    let home = paths::user_dir();
    // A sandbox that passes these is reused only where results never post, since any can pass them
    paths::home_overridden()
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
#[derive(Debug)]
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
        let user = paths::user_dir();

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
        let home = paths::home();
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
                    .map(std::string::ToString::to_string)
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
            // libgit2's TLS on macOS needs securityd, which holds the keychain and the jail denies. git does not
            cmd.env("CARGO_NET_GIT_FETCH_WITH_CLI", "true");
        }

        if self.home.join(SBF_SDK).is_dir() {
            cmd.env("SBF_SDK_PATH", self.home.join(SBF_SDK));
        }

        if let Some(sdk) = sdk_root() {
            cmd.env("SDKROOT", sdk);
        }

        let hook = || {
            // SAFETY: setsid changes only this child's session
            if unsafe { libc::setsid() } == -1 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(())
        };
        // SAFETY: the hook only calls setsid, which is async-signal-safe
        unsafe {
            cmd.pre_exec(hook);
        }
        Ok(cmd)
    }
}

/**
 * The tool caches of host checks, per repository, written only by checks. Runs of the base's tree
 * write `base`, and every other run writes an overlay of it or a copy of it of its own, under `runs`
 **/
fn cache_dir(repo_id: Option<&str>) -> PathBuf {
    let dir = paths::home().join("hostcache");
    repo_id.map_or(dir.clone(), |id| dir.join(id))
}

/// The canonical path, which `sandbox-exec` matches against, of a directory shipcheck owns
fn real_dir(dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(dir)
        .and_then(|()| dir.canonicalize())
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
    toolchains(&path, &paths::user_dir())?;
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
    // SAFETY: confstr writes at most `buf.len()` bytes into a buffer this function owns
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
        &paths::user_dir(),
    )
}

/// A toolchain manager's home from its variable when that is trusted, else the default in `~`
fn user_tool(var: &str, dir: &str) -> PathBuf {
    let user = paths::user_dir();
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

        // SAFETY: pipe writes two descriptors into `fds`
        if unsafe { libc::pipe(fds.as_mut_ptr()) } == 0 {
            let read = fds[0];
            WAKE.store(fds[1], Ordering::SeqCst);
            std::thread::spawn(move || {
                let mut sig = 0u8;

                // SAFETY: one byte into `sig`, which outlives the call
                if unsafe { libc::read(read, (&raw mut sig).cast(), 1) } == 1 {
                    #[cfg(target_os = "macos")]
                    let _held = job::stop_all();
                    die(libc::c_int::from(sig));
                }
            });
        }

        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            #[expect(clippy::as_conversions, reason = "libc takes a handler as an integer")]
            let handler = stop as *const () as libc::sighandler_t;
            // SAFETY: the handler calls only async-signal-safe functions
            unsafe {
                libc::signal(sig, handler);
            }
        }
    });
}

fn register(pgid: u32) -> Option<usize> {
    let pgid = i32::try_from(pgid).ok()?;
    install();
    GROUPS.iter().position(|g| {
        g.compare_exchange(0, pgid, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    })
}

fn kill_registered(groups: &[AtomicI32]) {
    for g in groups {
        let pgid = g.load(Ordering::SeqCst);

        if pgid > 0 {
            // SAFETY: killpg only sends a signal
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

extern "C" fn stop(sig: libc::c_int) {
    let wake = WAKE.load(Ordering::SeqCst);
    let byte = u8::try_from(sig).unwrap_or(0);

    // SAFETY: write is async-signal-safe, and the byte outlives the call
    if wake >= 0 && unsafe { libc::write(wake, (&raw const byte).cast(), 1) } == 1 {
        return;
    }

    die(sig);
}

fn die(sig: libc::c_int) {
    kill_registered(&GROUPS);
    // SAFETY: back to the default action
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
    }
    // SAFETY: then die of the same signal
    unsafe {
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
    use crate::paths;
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
    use std::{io, mem, ptr, thread};

    use sha2::{Digest, Sha256};

    use super::{
        BTreeMap, Command, Duration, Instant, LAUNCHCTL, Path, PathBuf, Stdio, fs, real_dir,
    };
    use crate::cache;

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
    pub(super) fn coalition(pid: libc::pid_t) -> Option<u64> {
        let mut info = CoalitionInfo {
            id: [0; 2],
            reserved: [0; 3],
        };
        let size = libc::c_int::try_from(mem::size_of::<CoalitionInfo>()).ok()?;
        // SAFETY: the kernel writes at most `size` bytes into `info`
        let n = unsafe {
            libc::proc_pidinfo(pid, PROC_PIDCOALITIONINFO, 0, (&raw mut info).cast(), size)
        };
        (n == size && info.id[0] != 0 && state(pid).is_some_and(|s| s != SZOMB))
            .then_some(info.id[0])
    }

    fn state(pid: libc::pid_t) -> Option<u32> {
        // SAFETY: a plain C struct of integers, which zero is a valid value of
        let mut info: libc::proc_bsdshortinfo = unsafe { mem::zeroed() };
        let size = libc::c_int::try_from(mem::size_of::<libc::proc_bsdshortinfo>()).ok()?;
        // SAFETY: the kernel writes at most `size` bytes into `info`
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDT_SHORTBSDINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        (n == size).then_some(info.pbsi_status)
    }

    /// Every pid on the machine, in a buffer doubled until the list fits with room to spare
    fn all_pids() -> Vec<libc::pid_t> {
        // SAFETY: a null buffer only asks for the count
        let count = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
        let mut cap = usize::try_from(count).unwrap_or(0) + 256;

        loop {
            let mut pids: Vec<libc::pid_t> = vec![0; cap];
            let size = libc::c_int::try_from(mem::size_of_val(pids.as_slice()))
                .expect("a buffer for every pid on the machine fits in a C int");
            // SAFETY: the kernel writes at most `size` bytes into `pids`
            let listed = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
            let n = usize::try_from(listed).unwrap_or(0);

            if n < cap {
                pids.truncate(n);
                return pids;
            }

            cap *= 2;
        }
    }

    pub(super) fn members(coal: u64) -> Vec<libc::pid_t> {
        let mut pids = all_pids();
        pids.retain(|&p| p > 1 && coalition(p) == Some(coal));
        pids
    }

    /**
     * Stops every member of the coalition until a scan finds all of them stopped, since a stopped
     * process cannot fork, then kills them. Each pid is checked again right before its signal.
     * Returns every pid signaled, an error when a member cannot be stopped or keeps forking
     **/
    pub(super) fn reap(coal: u64) -> Result<Vec<libc::pid_t>, String> {
        // SAFETY: getpid cannot fail
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

                // SAFETY: kill only sends a signal
                if unsafe { libc::kill(pid, sig) } == -1
                    && io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
                {
                    return Err(format!(
                        "process {pid} of a host check runs as another user, so shipcheck cannot stop it"
                    ));
                }

                signaled.push(pid);
            }

            thread::sleep(Duration::from_millis(1));
        }

        Err("a host check's processes kept forking faster than shipcheck could stop them".into())
    }

    /// The launchd domain of the user's jobs: the login session's, else the background one over ssh
    fn domain() -> &'static str {
        static DOMAIN: OnceLock<String> = OnceLock::new();
        DOMAIN.get_or_init(|| {
            // SAFETY: getuid cannot fail
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
        // SAFETY: the kernel writes at most `len` bytes into `buf`
        let ok = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buf.as_mut_ptr().cast(),
                &raw mut len,
                ptr::null_mut(),
                0,
            )
        } == 0;
        let id = String::from_utf8_lossy(&buf[..if ok { len } else { 0 }])
            .trim_end_matches('\0')
            .to_string();
        (!id.is_empty() && !id.contains(' ')).then_some(id)
    }

    /// Each job this process started and has not booted out, with its coalition and record once known
    static RUNNING: Mutex<BTreeMap<String, Option<(u64, PathBuf)>>> = Mutex::new(BTreeMap::new());

    fn running() -> MutexGuard<'static, BTreeMap<String, Option<(u64, PathBuf)>>> {
        RUNNING.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /**
     * What a signal does before shipcheck dies: reap every running check and boot out its job.
     * Returns the lock, which the caller holds until it dies, so no check starts meanwhile
     **/
    pub(super) fn stop_all() -> MutexGuard<'static, BTreeMap<String, Option<(u64, PathBuf)>>> {
        let mut held = running();

        for (target, known) in mem::take(&mut *held) {
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
        let id = cache::hex(&Sha256::digest(dir.as_os_str().as_bytes())[..16]);
        paths::home().join("reap").join(id)
    }

    pub(super) fn reap_slot(slot: &Path) -> Result<(), String> {
        let dir = paths::home().join("reap");

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
    pub(super) fn probe() -> Result<String, String> {
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
    pub(super) fn run(
        cmd: &Command,
        dir: &Path,
        log: &Path,
        timeout: Duration,
    ) -> Result<Option<i32>, String> {
        reaped(cmd, dir, log, timeout).map(|(exit, _)| exit)
    }

    /// [`run`], and every pid the reap signaled
    pub(super) fn reaped(
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

        // SAFETY: the path is a valid C string that outlives the call
        if unsafe { libc::mkfifo(go.as_ptr(), 0o600) } != 0 {
            return Err(format!("mkfifo: {}", io::Error::last_os_error()));
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

            thread::sleep(Duration::from_millis(5));
        };
        // The script's `mv` of its pid file may not have exited yet
        let coal = loop {
            if let Some(c) = own_coalition(pid) {
                break c;
            }

            if Instant::now() >= started + START {
                return Err("launchd did not give the check a coalition of its own".into());
            }

            thread::sleep(Duration::from_millis(5));
        };
        let record = record(dir);
        let parent = record.parent().ok_or("no reap dir")?;
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        let new = record.with_extension("new");
        let line = format!("{coal} {} {target} ", boot().ok_or(NO_BOOT)?);
        let mut text = line.into_bytes();
        text.extend_from_slice(dir.as_os_str().as_bytes());
        fs::write(&new, text)
            .and_then(|()| fs::rename(&new, &record))
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

                thread::sleep(Duration::from_millis(20));
            }
        });

        // The record stays when the reap fails, so the slot's next run tries again before its checkout
        let signaled = reap(coal)?;
        fs::remove_file(&record).map_err(|e| format!("{}: {e}", record.display()))?;
        exit.map(|e| (e, signaled))
    }

    /// The coalition of a job's waiting script, None unless it is the only member, so never a shared one
    pub(super) fn own_coalition(pid: libc::pid_t) -> Option<u64> {
        // SAFETY: getpid cannot fail
        let own = coalition(unsafe { libc::getpid() });
        coalition(pid).filter(|&c| c > 1 && Some(c) != own && members(c) == [pid])
    }

    /// Lets the job's script run the check, false when the deadline passed first
    fn release(go: &CStr, deadline: Instant) -> Result<bool, String> {
        loop {
            // SAFETY: the path is a valid C string, and a nonblocking open fails until the script reads
            let fd = unsafe {
                libc::open(
                    go.as_ptr(),
                    libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };

            if fd >= 0 {
                // SAFETY: fd is ours, and the byte outlives the call
                unsafe { libc::write(fd, b"\n".as_ptr().cast(), 1) };
                // SAFETY: fd is ours and closed once
                unsafe { libc::close(fd) };
                return Ok(true);
            }

            if io::Error::last_os_error().raw_os_error() != Some(libc::ENXIO) {
                return Err(format!("job fifo: {}", io::Error::last_os_error()));
            }

            if Instant::now() >= deadline {
                return Ok(false);
            }

            thread::sleep(Duration::from_millis(2));
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
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(home)
        .and_then(|dir| shard::open_at(&dir, STEP_SUMMARY, shard::Kind::File));
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
        .take(u64::try_from(runner::MAX_SUMMARY).expect("the limit fits a u64") + 1)
        .read_to_end(&mut body)
        .is_err()
    {
        return REFUSED.into();
    }

    runner::cap_summary(&String::from_utf8_lossy(&body))
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

    if paths::user_dir().starts_with(&repo) {
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
            let _ = write!(id, "\0{command}\0{}", out.trim());
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
    fn run(&self, request: &RunRequest<'_>, log: &Path) -> Result<Outcome, String> {
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
            readable.push(base);
        }

        let jail = Jail {
            writable,
            readable,
            cache,
            home,
            overlay,
            sandboxed: command.sandbox,
        };
        // As many shards as this machine's CPUs allow, since host checks share them with nothing else
        let shards = (request.shard && self.shards.enabled())
            .then(|| {
                let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
                shard::count(
                    self.shards,
                    Some((u32::try_from(cpus).unwrap_or(u32::MAX), u64::MAX)),
                )
            })
            .filter(|n| *n > 0);
        let sharded = match shards {
            Some(n) => shard::write_host(request.run_dir, command, n)?.map(|c| (n, c)),
            None => None,
        };
        let exit = run_command(
            sharded.as_ref().map_or(&**command, |(_, c)| c.as_str()),
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
            shards: sharded.map(|(n, _)| n),
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
mod tests;
