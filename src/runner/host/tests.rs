use super::*;
use crate::run;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;

/// The ways a branch run can get the base's cache here: a copy, and an overlay where bwrap mounts one
fn modes() -> Vec<Overlay> {
    let dir = tempfile::tempdir().unwrap();
    let mut modes = vec![Overlay::Off];

    if sandbox_jail::overlay_supported(dir.path()).is_ok() {
        modes.push(Overlay::On);
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
    assert!(
        !cmd.get_envs()
            .any(|(k, _)| k == "CARGO_NET_GIT_FETCH_WITH_CLI")
    );

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
    assert_eq!(env("CARGO_NET_GIT_FETCH_WITH_CLI"), "true");
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
    std::os::unix::fs::symlink("/user/.cache/solana/v1.52", deps.join("platform-tools")).unwrap();
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
    let pid = i32::try_from(child.id()).expect("a pid fits an i32");
    let groups = [AtomicI32::new(pid), AtomicI32::new(0)];
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
        let exit = run_command(&cmd, dir.path(), &jail(dir.path()), &[], &log, timeout).unwrap();
        assert_eq!(exit, None);
        assert!(started.elapsed() < timeout + Duration::from_secs(5));

        if pid.exists() {
            break;
        }
    }

    let pid: i32 = fs::read_to_string(&pid).unwrap().trim().parse().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    // SAFETY: signal 0 only checks that the process exists
    let alive = unsafe { libc::kill(pid, 0) };
    assert_ne!(alive, 0, "the grandchild still runs");
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
    // SAFETY: signal 0 only checks that the process exists
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
}

#[test]
#[cfg_attr(jail, ignore = "macOS refuses a sandbox inside shipcheck's jail")]
fn the_check_sees_no_token() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    // SAFETY: tests in this module do not read these variables concurrently
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
    let path = std::ffi::CString::new(fifo.join(STEP_SUMMARY).into_os_string().into_vec()).unwrap();
    // SAFETY: path is a NUL-terminated string that outlives the call
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
    let user = resolved(run::user_dir());
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

    let user = resolved(run::user_dir());
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
    let mut name: [libc::c_char; 128] = [0; 128];
    // SAFETY: openpty writes the two descriptors and at most 128 bytes of name
    let opened = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            name.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0);
    // SAFETY: openpty wrote a NUL-terminated name into the buffer
    let theirs = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_str()
        .unwrap()
        .to_string();
    // Another terminal of the same user, such as the one shipcheck was started from
    let open = format!("python3 -c 'import os; os.open(\"{theirs}\", os.O_WRONLY | os.O_NOCTTY)'");
    assert_ne!(run(&open).0, Some(0), "{theirs}");
    // SAFETY: both descriptors came from openpty and nothing else owns them
    unsafe {
        libc::close(master);
    }
    // SAFETY: as above
    unsafe {
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
    let secret = tempfile::NamedTempFile::new_in(run::user_dir()).unwrap();
    let read = run(&format!("cat '{}'", secret.path().display()));
    assert_ne!(read.0, Some(0), "{}", read.1);
    let planted = run::user_dir().join(format!(".shipcheck-test-{}", std::process::id()));
    assert_ne!(run(&format!("touch '{}'", planted.display())).0, Some(0));
    assert!(!planted.exists());

    // Credentials, also beside a readable toolchain dir, whether or not this machine has them
    let user = run::user_dir();
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
    // SAFETY: getuid cannot fail
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

fn a_branch_run_sees_only_the_base(overlay: Overlay) {
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
        "overlay {overlay:?}: saw the other branch's file or missed the base's: {log}"
    );
    assert!(!planted, "overlay {overlay:?}: a branch run wrote the base");
    assert!(left.is_empty(), "overlay {overlay:?}: {left:?}");
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
    .with_overlay(Overlay::Off);
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
    let host = Host::new(config::Host::default(), dir.path()).with_overlay(Overlay::Off);
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

    let host = Host::new(config::Host::default(), dir.path()).with_overlay(Overlay::Off);
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
    .with_overlay(Overlay::Off);
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

fn a_run_leaves_a_live_runs_dir_alone(overlay: Overlay) {
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
        one.is_dir() && one.join("cargo").is_dir() != (overlay == Overlay::On),
        "overlay {overlay:?}: the second run removed the first's copy"
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

fn racing_runs_keep_their_dirs(overlay: Overlay) {
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

                if !copy.as_ref().is_ok_and(|c| {
                    c.is_dir() && c.join("cargo").is_dir() != (overlay == Overlay::On)
                }) {
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
                versions: versions
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect(),
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
    // SAFETY: signal 0 only checks that the process exists
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
        let size = libc::c_int::try_from(std::mem::size_of_val(pids.as_slice())).unwrap();
        // SAFETY: the kernel writes at most `size` bytes into `pids`
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
        pids.truncate(usize::try_from(n).unwrap_or(0));
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
    // SAFETY: getpid cannot fail
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
    // SAFETY: getuid cannot fail
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
    // SAFETY: getpid cannot fail
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
