use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::RepoConfig;
use crate::daemon::DaemonConfig;
use crate::run;

const LABEL: &str = "run.shipcheck.daemon";
const UNIT: &str = "shipcheck.service";
const RUNNING: &str = "daemon running, it reruns PRs whose history changed on GitHub";

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The launchd agent, with the installer's PATH, since launchd's own finds neither act nor docker
pub fn plist(exe: &Path, path: &str, log: &Path) -> String {
    let (exe, path, log) = (
        xml(&exe.to_string_lossy()),
        xml(path),
        xml(&log.to_string_lossy()),
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>daemon</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

/// The systemd user unit, with the installer's PATH
pub fn unit(exe: &Path, path: &str) -> String {
    let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "[Unit]\nDescription=shipcheck daemon\n\n[Service]\nExecStart=\"{}\" daemon\nEnvironment=\"PATH={}\"\nRestart=always\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n",
        quote(&exe.to_string_lossy()),
        quote(path)
    )
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn service_file() -> PathBuf {
    if cfg!(target_os = "macos") {
        home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
    } else {
        home().join(format!(".config/systemd/user/{UNIT}"))
    }
}

fn ok(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Writes `text` unless the file holds it already, and says whether it changed
fn write(file: &Path, text: &str) -> Result<bool, String> {
    if fs::read_to_string(file).is_ok_and(|t| t == text) {
        return Ok(false);
    }

    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }

    fs::write(file, text).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(true)
}

/// The checkout the daemon fetches into: the main one, not a worktree that may go away
pub(crate) fn main_checkout(root: &Path) -> PathBuf {
    Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(root)
        .output()
        .ok()
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .filter(|dir| dir.file_name().is_some_and(|n| n == ".git"))
        .and_then(|dir| dir.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| root.to_path_buf())
}

/// Whether `shipcheck.toml` on the base branch leaves the daemon on, which it does when there is none yet
fn wanted_on_main(root: &Path) -> bool {
    Command::new("git")
        .args([
            "show",
            &format!("{}:shipcheck.toml", run::default_base(root)),
        ])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| toml::from_str::<RepoConfig>(&String::from_utf8_lossy(&o.stdout)).ok())
        .is_none_or(|c| c.daemon)
}

/// Adds the repository to the daemon and installs or updates the service. Never fails, one line
pub fn install(root: &Path) -> String {
    match try_install(root) {
        Ok(line) => line,
        Err(e) => format!("daemon not installed: {e}"),
    }
}

/// Adds the checkout to the daemon at the base the remote names. Offline it stores no guess
fn register(file: &Path, checkout: &Path) -> Result<bool, String> {
    let base = run::remote_base(checkout)
        .ok_or("could not ask origin for its default branch, run init again when online")?;
    DaemonConfig::add_to(file, &run::repo_id(checkout)?, checkout, &base)
}

fn try_install(root: &Path) -> Result<String, String> {
    if std::env::var_os("SHIPCHECK_NO_DAEMON").is_some_and(|v| v == "1") {
        return Ok("daemon not installed, SHIPCHECK_NO_DAEMON=1".into());
    }

    if !owns_service(run::home_overridden()) {
        return Ok("daemon not installed, SHIPCHECK_HOME is not the default".into());
    }

    if !wanted_on_main(root) {
        return Ok("daemon not installed, daemon = false on main".into());
    }

    register(&DaemonConfig::path(), &main_checkout(root))?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // The service runs outside any sandbox for weeks, so it keeps only the PATH entries a jail would
    let path = crate::runner::host::jail_path()
        .to_string_lossy()
        .into_owned();
    let file = service_file();

    if cfg!(target_os = "macos") {
        let log = run::home().join("logs/daemon.log");
        fs::create_dir_all(run::home().join("logs")).map_err(|e| e.to_string())?;
        let changed = write(&file, &plist(&exe, &path, &log))?;
        let target = format!("gui/{}/{LABEL}", unsafe { libc::getuid() });

        if changed || !ok("launchctl", &["print", &target]) {
            let domain = target.rsplit_once('/').map_or("", |(d, _)| d);
            ok("launchctl", &["bootout", &target]);

            if !ok("launchctl", &["bootstrap", domain, &file.to_string_lossy()]) {
                return Err(format!("launchctl bootstrap {} failed", file.display()));
            }
        }
    } else {
        let changed = write(&file, &unit(&exe, &path))?;
        ok("systemctl", &["--user", "daemon-reload"]);

        if !ok("systemctl", &["--user", "enable", "--now", UNIT]) {
            return Err(format!("systemctl --user enable --now {UNIT} failed"));
        }

        if changed {
            ok("systemctl", &["--user", "restart", UNIT]);
        }
    }

    Ok(RUNNING.into())
}

/// Whether the LaunchAgent still logs into the home from before it left `~/Library`
pub fn logs_to_legacy_home() -> bool {
    let old = xml(&run::legacy_home().to_string_lossy());
    fs::read_to_string(service_file()).is_ok_and(|t| t.contains(&old))
}

/// The one service belongs to the default home, so a moved SHIPCHECK_HOME, such as a test's,
/// never installs or removes it
fn owns_service(home_overridden: bool) -> bool {
    !home_overridden
}

/// Stops the service and removes its file
pub fn uninstall() -> Result<String, String> {
    if !owns_service(run::home_overridden()) {
        return Ok("daemon service left alone, SHIPCHECK_HOME is not the default".into());
    }

    let file = service_file();

    if cfg!(target_os = "macos") {
        ok(
            "launchctl",
            &[
                "bootout",
                &format!("gui/{}/{LABEL}", unsafe { libc::getuid() }),
            ],
        );
    } else {
        ok("systemctl", &["--user", "disable", "--now", UNIT]);
    }

    match fs::remove_file(&file) {
        Ok(()) => Ok(format!("removed the daemon service {}", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok("no daemon service installed".into())
        }
        Err(e) => Err(format!("{}: {e}", file.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(at: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(at)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    #[test]
    fn an_unreachable_remote_registers_no_guessed_base() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, origin) = (dir.path().join("app"), dir.path().join("acme/app.git"));
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "master"]);
        git(&repo, &["init", "-q", "-b", "master"]);
        git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "a",
            ],
        );
        git(&repo, &["push", "-q", "origin", "master"]);
        let file = dir.path().join("daemon.toml");
        fs::write(&file, "").unwrap();

        assert_eq!(register(&file, &repo), Ok(true));
        let before = fs::read_to_string(&file).unwrap();
        assert!(before.contains("base = \"origin/master\""), "{before}");

        let gone = dir.path().join("gone/app.git");
        git(
            &repo,
            &["remote", "set-url", "origin", gone.to_str().unwrap()],
        );
        let e = register(&file, &repo).unwrap_err();
        assert!(
            e.contains("could not ask origin for its default branch"),
            "{e}"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), before);
    }

    #[test]
    fn only_the_default_home_installs_or_removes_the_service() {
        assert!(owns_service(false));
        assert!(
            !owns_service(true),
            "a test's SHIPCHECK_HOME once ran `clean --all` and removed the developer's daemon"
        );
    }

    #[test]
    fn launchd_agent() {
        let text = plist(
            Path::new("/opt/bin/shipcheck"),
            "/usr/bin:/a&b",
            Path::new("/h/logs/daemon.log"),
        );
        assert_eq!(
            text,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>run.shipcheck.daemon</string>
  <key>ProgramArguments</key>
  <array>
    <string>/opt/bin/shipcheck</string>
    <string>daemon</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/usr/bin:/a&amp;b</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>/h/logs/daemon.log</string>
  <key>StandardErrorPath</key>
  <string>/h/logs/daemon.log</string>
</dict>
</plist>
"#
        );
    }

    #[test]
    fn systemd_unit() {
        assert_eq!(
            unit(Path::new("/home/u/.cargo/bin/shipcheck"), "/usr/bin:/bin"),
            "[Unit]\nDescription=shipcheck daemon\n\n[Service]\nExecStart=\"/home/u/.cargo/bin/shipcheck\" daemon\nEnvironment=\"PATH=/usr/bin:/bin\"\nRestart=always\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n"
        );
    }

    #[test]
    fn write_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agents/x.plist");
        assert!(write(&file, "a").unwrap());
        assert!(
            !write(&file, "a").unwrap(),
            "the same text must not reload the service"
        );
        assert!(
            write(&file, "b").unwrap(),
            "a new binary path must update it"
        );
    }
}
