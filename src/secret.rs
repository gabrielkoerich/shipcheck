use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Where a secret lives on this OS: a keychain item on macOS, the file itself elsewhere
#[derive(Debug, PartialEq)]
enum Step {
    ReadFile,
    MoveFileToKeychain,
    ReadKeychain,
}

fn step(keychain: bool, file_exists: bool) -> Step {
    match (keychain, file_exists) {
        (false, _) => Step::ReadFile,
        (true, true) => Step::MoveFileToKeychain,
        (true, false) => Step::ReadKeychain,
    }
}

/**
 * The keychain account for `path`, and the one a shipcheck from before the home left `~/Library`
 * used. Under the default home it is the path relative to it, so the items follow the home when it
 * moves. Elsewhere, such as a throwaway `SHIPCHECK_HOME`, it is the full path, so that home gets
 * items of its own
 **/
fn accounts(path: &Path, default: Option<&Path>, legacy: &Path) -> (String, Option<String>) {
    match default.and_then(|d| path.strip_prefix(d).ok()) {
        Some(rel) => (
            rel.to_string_lossy().into_owned(),
            Some(legacy.join(rel).to_string_lossy().into_owned()),
        ),
        None => (path.to_string_lossy().into_owned(), None),
    }
}

fn accounts_for(path: &Path) -> (String, Option<String>) {
    let default = (!crate::run::home_overridden()).then(crate::run::default_home);
    accounts(path, default.as_deref(), &crate::run::legacy_home())
}

/// The item at `account`, else the one at `legacy`, which moves to `account` on this read
fn get_or_rekey(account: &str, legacy: Option<&str>) -> Result<Option<Vec<u8>>, String> {
    if let Some(data) = keychain::get(account)? {
        return Ok(Some(data));
    }

    let Some(old) = legacy else {
        return Ok(None);
    };

    let Some(data) = keychain::get(old)? else {
        return Ok(None);
    };

    keychain::set(account, &data)?;
    keychain::delete(old)?;
    eprintln!("shipcheck: moved the keychain item {old} to {account}");
    Ok(Some(data))
}

// Parallel jobs after an upgrade ask the user once. A miss is not kept and a hit only for a minute,
// so the daemon sees a later `shipcheck login`
static READ: Mutex<Option<HashMap<PathBuf, Hit>>> = Mutex::new(None);
type Hit = (Instant, Vec<u8>);
const KEEP: Duration = Duration::from_secs(60);

/**
 * The secret that lives at `path` in shipcheck's home. On macOS it is a login keychain item named
 * by [`accounts`], which only the shipcheck binary that wrote it reads without asking the user. A file
 * found there is moved into the item first, and the file to the Trash. Elsewhere it is the file
 **/
pub fn read(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let mut cache = READ.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);

    let (account, legacy) = accounts_for(path);

    match step(cfg!(target_os = "macos"), path.exists()) {
        Step::ReadFile => read_file(path),
        Step::MoveFileToKeychain => {
            let data = read_file(path)?.unwrap_or_default();
            keychain::set(&account, &data)?;
            trash(path);
            cache.insert(path.to_path_buf(), (Instant::now(), data.clone()));
            Ok(Some(data))
        }
        Step::ReadKeychain => {
            if let Some((at, data)) = cache.get(path)
                && at.elapsed() < KEEP
            {
                return Ok(Some(data.clone()));
            }

            let data = get_or_rekey(&account, legacy.as_deref())?;

            match &data {
                Some(d) => cache.insert(path.to_path_buf(), (Instant::now(), d.clone())),
                None => cache.remove(path),
            };

            Ok(data)
        }
    }
}

/// Whether the secret at `path` is there, without reading it, so the keychain never asks
pub fn exists(path: &Path) -> bool {
    if path.exists() {
        return true;
    }

    let (account, legacy) = accounts_for(path);
    cfg!(target_os = "macos")
        && (keychain::exists(&account) || legacy.is_some_and(|old| keychain::exists(&old)))
}

/// Stores a secret for `path`: the keychain item on macOS, a `0600` file elsewhere
pub fn write(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Ok(mut cache) = READ.lock() {
        cache.get_or_insert_with(HashMap::new).remove(path);
    }

    if cfg!(target_os = "macos") {
        let (account, legacy) = accounts_for(path);
        keychain::set(&account, data)?;
        return legacy.map_or(Ok(()), |old| keychain::delete(&old));
    }

    write_file(path, data)
}

/// Deletes the secret for `path`, the file and the keychain item
pub fn remove(path: &Path) -> Result<(), String> {
    if let Ok(mut cache) = READ.lock() {
        cache.get_or_insert_with(HashMap::new).remove(path);
    }

    if path.exists() {
        std::fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    }

    if cfg!(target_os = "macos") {
        let (account, legacy) = accounts_for(path);
        keychain::delete(&account)?;

        if let Some(old) = legacy {
            keychain::delete(&old)?;
        }
    }

    Ok(())
}

fn read_file(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(data) => Ok(Some(data)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// A `0600` file, for a secret off macOS and for private state that is no secret
pub fn write_file(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // `mode` applies only when the file is created
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .and_then(|_| file.write_all(data))
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn trash(path: &Path) {
    let moved = std::process::Command::new("/usr/bin/trash")
        .arg(path)
        .output()
        .is_ok_and(|o| o.status.success());

    if moved {
        eprintln!(
            "shipcheck: moved {} into the login keychain and the file to the Trash",
            path.display()
        );
    } else {
        eprintln!(
            "shipcheck: moved {} into the login keychain, but could not move the file to the Trash, delete it yourself",
            path.display()
        );
    }
}

#[cfg(target_os = "macos")]
mod keychain {
    use std::ffi::c_void;

    const SERVICE: &str = "shipcheck";
    const NOT_FOUND: i32 = -25300;
    type Item = *const c_void;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecKeychainFindGenericPassword(
            keychain: *const c_void,
            service_len: u32,
            service: *const u8,
            account_len: u32,
            account: *const u8,
            data_len: *mut u32,
            data: *mut *mut c_void,
            item: *mut *const c_void,
        ) -> i32;
        fn SecKeychainAddGenericPassword(
            keychain: *const c_void,
            service_len: u32,
            service: *const u8,
            account_len: u32,
            account: *const u8,
            data_len: u32,
            data: *const c_void,
            item: *mut *const c_void,
        ) -> i32;
        fn SecKeychainItemFreeContent(attrs: *mut c_void, data: *mut c_void) -> i32;
        fn SecKeychainItemDelete(item: *const c_void) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    fn fail(what: &str, account: &str, status: i32) -> String {
        let why = match status {
            -128 => "you denied shipcheck access to it".to_string(),
            // After an upgrade the new binary is not on the item's list yet, and nobody can answer the prompt
            -25308 | -25293 => "the keychain asks you to allow this shipcheck binary, which happens after an upgrade: run `shipcheck doctor` at the Mac and choose Always Allow".to_string(),
            s => format!("error {s}"),
        };
        format!("{what} the keychain item for {account}: {why}")
    }

    /// Finds the item for `account`, with its data when `data` is set, which is what the item's list guards
    fn find(account: &str, data: bool) -> Result<Option<(Item, Vec<u8>)>, String> {
        let mut len = 0u32;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut item: *const c_void = std::ptr::null();
        // SAFETY: the lengths match the buffers, and the outputs are freed below
        let status = unsafe {
            SecKeychainFindGenericPassword(
                std::ptr::null(),
                SERVICE.len() as u32,
                SERVICE.as_ptr(),
                account.len() as u32,
                account.as_ptr(),
                if data { &mut len } else { std::ptr::null_mut() },
                if data { &mut ptr } else { std::ptr::null_mut() },
                &mut item,
            )
        };

        if status == NOT_FOUND {
            return Ok(None);
        }

        if status != 0 {
            return Err(fail("reading", account, status));
        }

        let mut bytes = Vec::new();

        if !ptr.is_null() {
            // SAFETY: the keychain returned `len` bytes at `ptr`
            bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) }.to_vec();
            unsafe { SecKeychainItemFreeContent(std::ptr::null_mut(), ptr) };
        }

        Ok(Some((item, bytes)))
    }

    pub fn get(account: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(find(account, true)?.map(|(item, data)| {
            unsafe { CFRelease(item) };
            data
        }))
    }

    pub fn exists(account: &str) -> bool {
        let Ok(Some((item, _))) = find(account, false) else {
            return false;
        };
        unsafe { CFRelease(item) };
        true
    }

    pub fn delete(account: &str) -> Result<(), String> {
        let Some((item, _)) = find(account, false)? else {
            return Ok(());
        };
        let status = unsafe { SecKeychainItemDelete(item) };
        unsafe { CFRelease(item) };

        if status != 0 {
            return Err(fail("deleting", account, status));
        }

        Ok(())
    }

    /// Replaces the item, so its list trusts only this binary, which created it
    pub fn set(account: &str, data: &[u8]) -> Result<(), String> {
        delete(account)?;
        // SAFETY: the lengths match the buffers, and no item is asked for
        let status = unsafe {
            SecKeychainAddGenericPassword(
                std::ptr::null(),
                SERVICE.len() as u32,
                SERVICE.as_ptr(),
                account.len() as u32,
                account.as_ptr(),
                data.len() as u32,
                data.as_ptr() as *const c_void,
                std::ptr::null_mut(),
            )
        };

        if status != 0 {
            return Err(fail("writing", account, status));
        }

        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod keychain {
    pub fn get(_: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(None)
    }

    pub fn exists(_: &str) -> bool {
        false
    }

    pub fn delete(_: &str) -> Result<(), String> {
        Ok(())
    }

    pub fn set(_: &str, _: &[u8]) -> Result<(), String> {
        Err("no keychain on this OS".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_only_on_macos_and_a_file_moves_into_it() {
        assert_eq!(step(false, true), Step::ReadFile);
        assert_eq!(step(false, false), Step::ReadFile);
        assert_eq!(step(true, true), Step::MoveFileToKeychain);
        assert_eq!(step(true, false), Step::ReadKeychain);
    }

    #[test]
    fn accounts_follow_the_default_home_and_keep_the_old_name() {
        let default = Path::new("/u/.local/state/shipcheck");
        let legacy = Path::new("/u/Library/Application Support/shipcheck");

        assert_eq!(
            accounts(&default.join("token"), Some(default), legacy),
            (
                "token".into(),
                Some("/u/Library/Application Support/shipcheck/token".into())
            )
        );
        assert_eq!(
            accounts(&default.join("secrets/o-r.env"), Some(default), legacy).0,
            "secrets/o-r.env"
        );
        // A throwaway home never shares the real one's items
        assert_eq!(
            accounts(Path::new("/tmp/h/token"), None, legacy),
            ("/tmp/h/token".into(), None)
        );
        assert_eq!(
            accounts(Path::new("/tmp/h/token"), Some(default), legacy),
            ("/tmp/h/token".into(), None)
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "writes throwaway items to the login keychain"]
    fn an_item_under_its_old_name_moves_to_the_new_one() {
        let new = format!("shipcheck-test-{}-new", std::process::id());
        let old = format!("shipcheck-test-{}-old", std::process::id());
        keychain::set(&old, b"v").unwrap();

        assert_eq!(get_or_rekey(&new, Some(&old)).unwrap(), Some(b"v".to_vec()));
        assert!(keychain::exists(&new) && !keychain::exists(&old));
        assert_eq!(get_or_rekey(&new, Some(&old)).unwrap(), Some(b"v".to_vec()));
        keychain::delete(&new).unwrap();
        assert_eq!(get_or_rekey(&new, Some(&old)).unwrap(), None);
    }

    #[test]
    fn secret_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home/token");
        write_file(&path, b"a").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_file(&path, b"b").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
