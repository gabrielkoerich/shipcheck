use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    // A relative path would name another folder from every clone and git command that changes directory
    let absolute =
        |dir: std::ffi::OsString| std::path::absolute(&dir).unwrap_or_else(|_| dir.into());

    // An empty value is unset, as XDG says, not the current directory
    let set = |name| std::env::var_os(name).filter(|v| !v.is_empty());

    if let Some(dir) = set("SHIPCHECK_HOME") {
        return absolute(dir);
    }

    if let Some(state) = set("XDG_STATE_HOME") {
        return absolute(state).join("shipcheck");
    }

    env_home().join(".local/state/shipcheck")
}

fn env_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// Where the home was on macOS before it moved to `~/.local/state`
pub fn legacy_home() -> PathBuf {
    env_home().join("Library/Application Support/shipcheck")
}

#[derive(Debug, PartialEq)]
enum Migration {
    Nothing,
    Moved,
    Both,
}

/// Renames `old` to `new` when only `old` exists. A rename, since the home holds clones a copy
/// would inflate
fn move_home(old: &Path, new: &Path) -> Result<Migration, String> {
    match (old.is_dir(), new.exists()) {
        (false, _) => return Ok(Migration::Nothing),
        (true, true) => return Ok(Migration::Both),
        (true, false) => {}
    }

    if let Some(parent) = new.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }

    fs::rename(old, new)
        .and_then(|()| fs::set_permissions(new, fs::Permissions::from_mode(0o700)))
        .map_err(|e| format!("moving {} to {}: {e}", old.display(), new.display()))?;
    Ok(Migration::Moved)
}

/// Moves the macOS home out of `~/Library` on first use. Leaves both alone when both exist, which
/// `shipcheck doctor` reports
pub fn migrate_home() {
    if !cfg!(target_os = "macos") || std::env::var_os("SHIPCHECK_HOME").is_some() {
        return;
    }

    let (old, new) = (legacy_home(), home());

    match move_home(&old, &new) {
        Ok(Migration::Moved) => {
            eprintln!("shipcheck: moved {} to {}", old.display(), new.display());
        }
        Ok(_) => {}
        Err(e) => eprintln!("shipcheck: {e}"),
    }
}

/// The home of a user who set nothing, from the passwd entry, since an agent can set any variable
pub fn default_home() -> PathBuf {
    user_dir().join(".local/state/shipcheck")
}

/// Whether the results live somewhere else than the default, where an agent may have written them
pub fn home_overridden() -> bool {
    home() != default_home()
}

/// Whether the results live in the default home or in one `SHIPCHECK_HOME` moved
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Home {
    Default,
    Moved,
}

/// [`home_overridden`] as a [`Home`]
pub fn which_home() -> Home {
    if home_overridden() {
        Home::Moved
    } else {
        Home::Default
    }
}

/// Whether a person's `--home-ok` lets a moved home post
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MovedHome {
    Allowed,
    #[default]
    Refused,
}

/// The user's home directory from the passwd entry, empty when there is none, so a check against
/// it fails closed
pub fn user_dir() -> PathBuf {
    passwd().map_or_default(|(_, dir)| dir)
}

/// The user's login name from the passwd entry, empty when there is none
pub fn user_name() -> String {
    passwd().map_or_default(|(name, _)| name)
}

fn passwd() -> Option<(String, PathBuf)> {
    let mut buf: Vec<libc::c_char> = vec![0; 16384];
    // SAFETY: passwd is plain C data, for which all zeroes is a valid value
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    // SAFETY: getuid cannot fail and touches no memory
    let uid = unsafe { libc::getuid() };
    // SAFETY: getpwuid_r writes only into pwd and buf, and its strings point into buf on success
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &raw mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut found,
        )
    };

    if rc != 0 || found.is_null() || pwd.pw_dir.is_null() || pwd.pw_name.is_null() {
        return None;
    }

    // SAFETY: on success pw_name is a NUL-terminated string in buf, which outlives this borrow
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
    // SAFETY: as pw_name
    let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
    let dir = PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()));
    Some((name.to_string_lossy().into_owned(), dir))
}

pub fn log_dir(key: &str) -> PathBuf {
    home().join("logs").join(key)
}

pub fn secrets_file(repo_id: &str) -> PathBuf {
    home().join("secrets").join(format!("{repo_id}.env"))
}

pub fn partial_log_dir(key: &str) -> PathBuf {
    home().join("logs").join(format!("{key}.partial"))
}

pub(crate) fn git_lock_in(home: &Path, repo_id: &str) -> Result<fs::File, String> {
    let dir = home.join("locks").join("git");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name = repo_id.replace('/', "%2F");
    let lock = fs::File::create(dir.join(format!("{name}.lock"))).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_moves_once_and_never_merges() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("Library/Application Support/shipcheck");
        let new = dir.path().join(".local/state/shipcheck");
        assert_eq!(move_home(&old, &new).unwrap(), Migration::Nothing);

        fs::create_dir_all(old.join("logs")).unwrap();
        fs::write(old.join("logs/k"), "log").unwrap();
        assert_eq!(move_home(&old, &new).unwrap(), Migration::Moved);
        assert!(!old.exists());
        assert_eq!(fs::read_to_string(new.join("logs/k")).unwrap(), "log");
        assert_eq!(
            fs::metadata(&new).unwrap().permissions().mode() & 0o777,
            0o700
        );

        fs::create_dir_all(&old).unwrap();
        assert_eq!(move_home(&old, &new).unwrap(), Migration::Both);
        assert!(old.exists() && new.join("logs/k").exists());
    }
}
