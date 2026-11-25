use crate::paths;
use std::ffi::CStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::{io, mem, ptr, thread};

use sha2::{Digest, Sha256};

use super::{BTreeMap, Command, Duration, Instant, LAUNCHCTL, Path, PathBuf, Stdio, fs, real_dir};
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
    let n =
        unsafe { libc::proc_pidinfo(pid, PROC_PIDCOALITIONINFO, 0, (&raw mut info).cast(), size) };
    (n == size && info.id[0] != 0 && state(pid).is_some_and(|s| s != SZOMB)).then_some(info.id[0])
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

        fs::remove_file(entry.path()).map_err(|e| format!("{}: {e}", entry.path().display()))?;
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
    let go =
        std::ffi::CString::new(ctl.join("go").as_os_str().as_bytes()).map_err(|e| e.to_string())?;

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
