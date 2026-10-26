use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

/// A volume unused this long goes, as results and logs do
pub const KEEP: Duration = Duration::from_secs(14 * 24 * 3600);

/// An image only an older release pins goes after a day, unless a config still names it
const KEEP_OLD_PIN: Duration = Duration::from_secs(24 * 3600);

/// Platform and busybox digests that earlier releases pinned and this one does not. Add each digest
/// a release stops pinning, so the image leaves machines that never stamped it
pub const OLD_PINS: &[&str] = &[];

/// A gone home's volume goes only when Docker made it longer ago than this
const ORPHAN_AGE: Duration = Duration::from_secs(24 * 3600);

/// An empty volume no run used this long goes, the wait keeps one a run just made for its mount
const KEEP_EMPTY: Duration = Duration::from_secs(3600);

/// A volume as `docker system df -v` lists it
#[derive(Clone, Debug, Default)]
pub struct Volume {
    pub name: String,
    /// The `shipcheck.home` and `shipcheck.host` labels, None on a volume from before labels
    pub home: Option<String>,
    pub host: Option<String>,
    /// None when Docker gave no size it can read
    pub bytes: Option<u64>,
}

/// Where this process runs, so the prune can tell its own volumes from another home's
pub struct Here<'a> {
    pub home: &'a Path,
    pub prefix: &'a str,
    pub host: &'a str,
    /// Whether this home may remove the volumes of gone homes. Only the default home may, so a
    /// test's or an agent's home never removes another home's volume
    pub sweep: bool,
}

/// The Docker calls the prune makes, so its decisions can be tested without Docker
pub trait Engine {
    /// Every volume on the machine, None when Docker cannot list them
    fn volumes(&self) -> Option<Vec<Volume>>;
    /// The names of the volumes whose name contains `part`, None when Docker cannot list them
    fn named(&self, part: &str) -> Option<Vec<String>>;
    /// When Docker made the volume, None when it cannot say
    fn created(&self, name: &str) -> Option<SystemTime>;
    /// Whether a container, running or stopped, matches the `docker ps` filter. True on any doubt
    fn used(&self, filter: &str) -> bool;
    /// The image's id, None when it is not on this machine
    fn image_id(&self, reference: &str) -> Option<String>;
    /// `docker <kind> rm` without `-f`, so Docker still refuses what a container uses
    fn remove(&self, kind: &str, name: &str) -> bool;
}

pub struct Docker;

fn docker(args: &[&str]) -> Option<String> {
    let out = Command::new("docker").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl Engine for Docker {
    fn volumes(&self) -> Option<Vec<Volume>> {
        let out = docker(&["system", "df", "-v", "--format", "{{json .Volumes}}"])?;
        let listed: Vec<serde_json::Value> = serde_json::from_str(&out).ok()?;
        Some(listed.iter().map(parse_volume).collect())
    }

    fn named(&self, part: &str) -> Option<Vec<String>> {
        let out = docker(&["volume", "ls", "-q", "--filter", &format!("name={part}")])?;
        Some(out.split_whitespace().map(String::from).collect())
    }

    fn created(&self, name: &str) -> Option<SystemTime> {
        parse_created(&docker(&[
            "volume",
            "inspect",
            "-f",
            "{{.CreatedAt}}",
            name,
        ])?)
    }

    fn used(&self, filter: &str) -> bool {
        docker(&["ps", "-a", "-q", "--filter", filter]).is_none_or(|ids| !ids.is_empty())
    }

    fn image_id(&self, reference: &str) -> Option<String> {
        docker(&["image", "inspect", "--format", "{{.Id}}", reference]).filter(|id| !id.is_empty())
    }

    fn remove(&self, kind: &str, name: &str) -> bool {
        docker(&[kind, "rm", name]).is_some()
    }
}

/// Docker's `CreatedAt`, RFC 3339 such as `2026-10-25T23:40:46Z` or with an offset
fn parse_created(text: &str) -> Option<SystemTime> {
    let (date, rest) = text.trim().split_once('T')?;
    let mut ymd = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (ymd.next()??, ymd.next()??, ymd.next()??);
    let (clock, offset) = rest.split_at(rest.find(['Z', '+', '-'])?);
    let mut hms = clock
        .split('.')
        .next()?
        .splitn(3, ':')
        .map(|p| p.parse::<i64>().ok());
    let (h, mi, sec) = (hms.next()??, hms.next()??, hms.next()??);
    let offset = match offset {
        "Z" => 0,
        o => {
            let (oh, om) = o[1..].split_once(':')?;
            let secs = oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60;
            if o.starts_with('-') { -secs } else { secs }
        }
    };
    // Days since 1970 from a civil date, as in Howard Hinnant's `days_from_civil`
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset;
    Some(std::time::UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).ok()?))
}

/// Docker joins labels as `k=v,k=v`, and `create_volume` refuses a value with a comma
fn parse_volume(v: &serde_json::Value) -> Volume {
    let labels = v["Labels"].as_str().unwrap_or_default();
    let label = |key: &str| {
        labels
            .split(',')
            .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
            .map(String::from)
    };
    Volume {
        name: v["Name"].as_str().unwrap_or_default().to_string(),
        home: label("shipcheck.home"),
        host: label("shipcheck.host"),
        bytes: v["Size"].as_str().and_then(size),
    }
}

/// Creates the volume with labels that name its home, host, repository and path. Docker keeps the
/// labels of a volume that exists already
pub fn create_volume(name: &str, home: &Path, repo_id: &str, path: &str) -> Result<(), String> {
    let mut cmd = Command::new("docker");
    cmd.args(["volume", "create"]);

    for label in labels(home, &hostname(), repo_id, path)? {
        cmd.arg("--label").arg(label);
    }

    let out = cmd
        .arg(name)
        .output()
        .map_err(|e| format!("docker volume create: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "docker volume create {name}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    Ok(())
}

fn labels(home: &Path, host: &str, repo_id: &str, path: &str) -> Result<Vec<String>, String> {
    let home = home.display().to_string();
    let labels = [
        ("home", &*home),
        ("host", host),
        ("repo", repo_id),
        ("path", path),
    ];

    if let Some((key, value)) = labels.iter().find(|(_, v)| v.contains(',')) {
        return Err(format!(
            "the volume label shipcheck.{key} would be {value}, shipcheck refuses a comma in it"
        ));
    }

    Ok(labels
        .iter()
        .map(|(k, v)| format!("shipcheck.{k}={v}"))
        .collect())
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer outlives the call, and one byte stays for the NUL a long name lacks
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
    String::from_utf8_lossy(if ok { &buf[..end] } else { &[] }).into_owned()
}

/// Volume names are file names already, image references hold `/`
fn encode(name: &str) -> String {
    name.replace('%', "%25").replace('/', "%2F")
}

fn decode(file: &str) -> String {
    file.replace("%2F", "/").replace("%25", "%")
}

/// Marks a volume (`kind` "volumes") or image ("images") as used now, host side
pub fn stamp(home: &Path, kind: &str, name: &str) {
    let dir = home.join(kind);
    let _ = fs::create_dir_all(&dir);

    if let Ok(file) = fs::File::create(dir.join(encode(name))) {
        let _ = file.set_modified(SystemTime::now());
    }
}

fn unstamp(home: &Path, kind: &str, name: &str) {
    let _ = fs::remove_file(home.join(kind).join(encode(name)));
}

fn age(home: &Path, kind: &str, name: &str, now: SystemTime) -> Option<Duration> {
    let used = fs::metadata(home.join(kind).join(encode(name)))
        .and_then(|m| m.modified())
        .ok()?;
    Some(now.duration_since(used).unwrap_or_default())
}

fn stamps(home: &Path, kind: &str) -> Vec<String> {
    fs::read_dir(home.join(kind))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| decode(&e.file_name().to_string_lossy()))
        .collect()
}

/// `shipcheck-` and the six hex digits of `home_prefix`, so a user's `shipcheck-review-cargo` never matches
fn shipcheck_named(name: &str) -> bool {
    name.strip_prefix("shipcheck-")
        .and_then(|rest| rest.get(..7))
        .is_some_and(|h| {
            h.ends_with('-')
                && h[..6]
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
}

fn ours(name: &str, prefix: &str) -> bool {
    shipcheck_named(prefix) && name.starts_with(prefix)
}

/**
 * A volume of this home goes when no run used it for `KEEP`, or when it has labels, is empty and
 * no run used it for `KEEP_EMPTY`. An older binary sharing this home makes them without labels and
 * keeps them empty until its job writes, so those wait the full `KEEP`. One without a stamp starts
 * its days now
 **/
fn volume_due(v: &Volume, prefix: &str, age: Option<Duration>) -> bool {
    let Some(age) = age.filter(|_| ours(&v.name, prefix)) else {
        return false;
    };
    age > KEEP || (v.home.is_some() && v.bytes == Some(0) && age > KEEP_EMPTY)
}

/**
 * A volume of another home goes only when its labels name this host and a home that is gone: an
 * absolute path without `.` or `..`, missing, under a parent that is there. A parent that is gone
 * too may be an unmounted disk or another mount namespace. A volume without labels never goes, it
 * may be a live home's that an older binary made
 **/
fn orphan(v: &Volume, here: &Here) -> bool {
    if !shipcheck_named(&v.name) || v.name.starts_with(here.prefix) {
        return false;
    }

    let (Some(home), Some(host)) = (&v.home, &v.host) else {
        return false;
    };
    host == here.host && gone(Path::new(home))
}

fn gone(home: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    // `components` drops a `.` inside the path, so the bytes are read
    let bytes = home.as_os_str().as_bytes();
    let plain = home.is_absolute() && !bytes.split(|&b| b == b'/').any(|c| c == b"." || c == b"..");
    let missing =
        matches!(fs::symlink_metadata(home), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
    plain && missing && home.parent().is_some_and(Path::is_dir)
}

/// Only an image an older release pinned can go, a day after a config last named it. Any other
/// image, even one a workflow made shipcheck pull, may be shared with the user's own stacks
fn image_due(reference: &str, age: Option<Duration>, old_pins: &[&str]) -> bool {
    old_pins.contains(&reference) && age.is_none_or(|a| a > KEEP_OLD_PIN)
}

fn remove_volume(engine: &dyn Engine, name: &str) -> bool {
    !engine.used(&format!("volume={name}")) && engine.remove("volume", name)
}

/// Some(true) when removed, Some(false) when kept, None when the image is not on this machine
fn remove_image(engine: &dyn Engine, reference: &str) -> Option<bool> {
    let id = engine.image_id(reference)?;
    let used =
        engine.used(&format!("ancestor={reference}")) || engine.used(&format!("ancestor={id}"));
    Some(!used && engine.remove("image", reference))
}

/**
 * Removes this home's volumes that `volume_due` picks, the volumes of other homes `orphan` picks,
 * the act volumes of this home's run dirs as `tags` names them, and every image of `old_pins` no
 * config uses. Whatever a container references, running or stopped, stays
 **/
pub fn prune(
    engine: &dyn Engine,
    here: &Here,
    tags: &[String],
    old_pins: &[&str],
    now: SystemTime,
) -> Vec<String> {
    let (home, prefix) = (here.home, here.prefix);
    let mut done = Vec::new();

    if let Some(listed) = engine.volumes() {
        for v in &listed {
            let age = age(home, "volumes", &v.name, now);

            // A volume from before stamps, or from a run that crashed before its stamp, starts its days now
            if age.is_none() && ours(&v.name, prefix) {
                stamp(home, "volumes", &v.name);
            }

            if volume_due(v, prefix, age) && remove_volume(engine, &v.name) {
                unstamp(home, "volumes", &v.name);
                done.push(format!("removed volume {}", v.name));
            }
        }

        let names: BTreeSet<&str> = listed.iter().map(|v| v.name.as_str()).collect();

        for name in stamps(home, "volumes") {
            if !names.contains(name.as_str()) {
                unstamp(home, "volumes", &name);
            }
        }

        done.extend(remove_orphans(engine, &listed, here, now));

        for tag in tags {
            done.extend(
                env_volumes(&listed, tag)
                    .filter(|n| remove_volume(engine, n))
                    .map(|n| format!("removed volume {n}")),
            );
        }
    }

    for reference in old_pins.iter().map(|r| r.to_string()) {
        if !image_due(&reference, age(home, "images", &reference, now), old_pins) {
            continue;
        }

        match remove_image(engine, &reference) {
            Some(false) => {}
            Some(true) => {
                unstamp(home, "images", &reference);
                done.push(format!("removed image {reference}"));
            }
            None => unstamp(home, "images", &reference),
        }
    }

    done
}

/// A home in another mount namespace, such as a sandbox's private /tmp, looks gone from here, and
/// its runs make volumes, so only a volume older than a day can be a gone home's
fn remove_orphans(
    engine: &dyn Engine,
    listed: &[Volume],
    here: &Here,
    now: SystemTime,
) -> Vec<String> {
    let old = |name: &str| {
        engine
            .created(name)
            .is_some_and(|t| now.duration_since(t).is_ok_and(|a| a > ORPHAN_AGE))
    };
    listed
        .iter()
        .filter(|v| here.sweep && orphan(v, here) && old(&v.name) && remove_volume(engine, &v.name))
        .map(|v| format!("removed volume {} of another home", v.name))
        .collect()
}

/// Removes the volumes that start with `prefix`, and with `orphans` also those `orphan` picks when
/// `here` may sweep. Whatever a container references stays
pub fn clean(engine: &dyn Engine, here: &Here, prefix: &str, orphans: bool) -> Vec<String> {
    let listed = engine.volumes().unwrap_or_default();
    let mut done = Vec::new();

    for v in &listed {
        if ours(&v.name, prefix) && remove_volume(engine, &v.name) {
            unstamp(here.home, "volumes", &v.name);
            done.push(format!("removed volume {}", v.name));
        }
    }

    if orphans {
        done.extend(remove_orphans(engine, &listed, here, SystemTime::now()));
    }

    done
}

/// act's `act-<workflow>-<job>-<hash>-env` volumes of the jobs whose name carries this tag. act's
/// shared `act-toolcache` never matches
fn env_volume(name: &str, tag: &str) -> bool {
    name.starts_with("act-") && name.ends_with("-env") && name.contains(&format!("-{tag}-"))
}

fn env_volumes<'a>(listed: &'a [Volume], tag: &'a str) -> impl Iterator<Item = &'a str> {
    listed
        .iter()
        .map(|v| v.name.as_str())
        .filter(move |n| env_volume(n, tag))
}

/// The act volumes of a job, which a killed act leaves behind
pub fn remove_env_volumes(engine: &dyn Engine, tag: &str) {
    let named = engine.named(&format!("-{tag}-")).unwrap_or_default();

    for name in named.iter().filter(|n| env_volume(n, tag)) {
        remove_volume(engine, name);
    }
}

/// The images act used, from its own `🐳  docker <verb> image=` lines. A step's output has `| `
/// after the job's prefix, so a job cannot print a line that counts
pub fn images_in_log(log: &str) -> BTreeSet<String> {
    log.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix('[')?.split_once("] ")?.1.trim_start();
            let rest = rest.strip_prefix("🐳  docker ")?;
            let (_, image) = rest.split_once(' ')?;
            let image = image.strip_prefix("image=")?.split_whitespace().next()?;
            (!image.is_empty()).then(|| image.to_string())
        })
        .collect()
}

/// Count and size of this home's volumes, of the volumes other live homes keep, of act's that no
/// run dir of this home names, and of this home's stamped images
pub fn usage(here: &Here, tags: &[String]) -> Result<String, String> {
    let listed = Docker.volumes().ok_or("docker system df failed")?;
    let tally = |keep: &dyn Fn(&Volume) -> bool| {
        (listed.iter().filter(|v| keep(v)))
            .fold((0, 0), |(n, b), v| (n + 1, b + v.bytes.unwrap_or(0)))
    };
    let (count, bytes) = tally(&|v| ours(&v.name, here.prefix));
    let (others, other_bytes) =
        tally(&|v| shipcheck_named(&v.name) && !ours(&v.name, here.prefix) && !orphan(v, here));
    let (gone, gone_bytes) = tally(&|v| orphan(v, here));
    let mine: BTreeSet<&str> = tags.iter().flat_map(|t| env_volumes(&listed, t)).collect();
    let (act, act_bytes) =
        tally(&|v| v.name.starts_with("act-") && !mine.contains(v.name.as_str()));
    let home = here.home;
    let refs = stamps(home, "images");
    let mut images = BTreeSet::new();
    let mut image_bytes = 0;

    if !refs.is_empty() {
        // Exits 1 when one reference is missing, and still prints the others
        let mut args = vec!["image", "inspect", "--format", "{{.Id}} {{.Size}}"];
        args.extend(refs.iter().map(String::as_str));
        let out = Command::new("docker")
            .args(&args)
            .output()
            .map_err(|e| e.to_string())?;

        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some((id, n)) = line.split_once(' ')
                && images.insert(id.to_string())
            {
                image_bytes += n.parse::<u64>().unwrap_or(0);
            }
        }
    }

    let mut text = format!(
        "Docker volumes of this home: {count}, {}, removed after a run once unused for 14 days, empty ones after an hour, or now with `shipcheck clean`. Images its runs used: {}, {}, kept",
        gigabytes(bytes),
        images.len(),
        gigabytes(image_bytes)
    );

    if gone > 0 {
        text += &format!(
            ". Volumes of gone homes: {gone}, {}, removed after a run of the default home, or with `shipcheck clean --all` there",
            gigabytes(gone_bytes)
        );
    }

    if others > 0 {
        text += &format!(
            ". Volumes of other homes, hosts or older releases: {others}, {}, kept",
            gigabytes(other_bytes)
        );
    }

    if act > 0 {
        text += &format!(
            ". {act} act volumes, {}, are not from a run of this home and stay",
            gigabytes(act_bytes)
        );
    }

    Ok(text)
}

/// Docker's sizes are decimal: `4.3GB`, `512kB`, `0B`. None for `N/A` or anything else
fn size(text: &str) -> Option<u64> {
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (n, unit) = text.split_at(split);
    let scale = match unit {
        "B" => 1e0,
        "kB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return None,
    };
    Some((n.parse::<f64>().ok()? * scale) as u64)
}

fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const P: &str = "shipcheck-abc123-";
    const DAY: Duration = Duration::from_secs(24 * 3600);

    /// Volumes and images on a fake machine, the filters of its containers, and what Docker refuses
    #[derive(Default)]
    struct Fake {
        volumes: Vec<Volume>,
        images: Vec<String>,
        containers: Vec<String>,
        refuse: Vec<String>,
        /// Volumes Docker made an hour ago, every other one is a month old
        new: Vec<String>,
        removed: RefCell<Vec<String>>,
    }

    impl Engine for Fake {
        fn volumes(&self) -> Option<Vec<Volume>> {
            Some(self.volumes.clone())
        }

        fn named(&self, part: &str) -> Option<Vec<String>> {
            Some(
                (self.volumes.iter())
                    .filter(|v| v.name.contains(part))
                    .map(|v| v.name.clone())
                    .collect(),
            )
        }

        fn created(&self, name: &str) -> Option<SystemTime> {
            let ago = if self.new.iter().any(|n| n == name) {
                DAY / 24
            } else {
                30 * DAY
            };
            Some(SystemTime::now() - ago)
        }

        fn used(&self, filter: &str) -> bool {
            self.containers.iter().any(|c| c == filter)
        }

        fn image_id(&self, reference: &str) -> Option<String> {
            self.images
                .contains(&reference.to_string())
                .then(|| format!("sha256:id-{reference}"))
        }

        fn remove(&self, _: &str, name: &str) -> bool {
            let ok = !self.refuse.contains(&name.to_string());

            if ok {
                self.removed.borrow_mut().push(name.to_string());
            }

            ok
        }
    }

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// A volume with the labels of `home` on host `h1`, 1 kB
    fn labeled(name: &str, home: &Path) -> Volume {
        Volume {
            name: name.into(),
            home: Some(home.display().to_string()),
            host: Some("h1".into()),
            bytes: Some(1000),
        }
    }

    fn bare(name: &str) -> Volume {
        Volume {
            name: name.into(),
            bytes: Some(1000),
            ..Volume::default()
        }
    }

    fn here(home: &Path, sweep: bool) -> Here<'_> {
        Here {
            home,
            prefix: P,
            host: "h1",
            sweep,
        }
    }

    fn stamp_at(home: &Path, kind: &str, name: &str, ago: Duration) {
        stamp(home, kind, name);
        let file = fs::File::options()
            .write(true)
            .open(home.join(kind).join(encode(name)))
            .unwrap();
        file.set_modified(SystemTime::now() - ago).unwrap();
    }

    #[test]
    fn volumes_go_by_stamp_age_and_prefix() {
        let home = Path::new("/h");
        let v = labeled(&format!("{P}x"), home);
        assert!(!volume_due(&v, P, None));
        assert!(volume_due(&v, P, Some(15 * DAY)));
        assert!(!volume_due(&v, P, Some(13 * DAY)));
        assert!(!volume_due(
            &labeled("shipcheck-other-x", home),
            P,
            Some(15 * DAY)
        ));
        // A prefix that is not shipcheck's never matches, even an empty one
        assert!(!volume_due(&bare("pg-data"), "", Some(15 * DAY)));
        assert!(!volume_due(&bare("act-toolcache"), "act-", Some(15 * DAY)));
        // An empty one with labels goes after an hour
        let empty = Volume {
            bytes: Some(0),
            ..v.clone()
        };
        assert!(!volume_due(&empty, P, Some(DAY / 48)));
        assert!(volume_due(&empty, P, Some(DAY / 12)));
        // One without labels, as an older binary in this home makes them, waits the full 14 days
        let old = Volume {
            bytes: Some(0),
            ..bare(&format!("{P}x"))
        };
        assert!(!volume_due(&old, P, Some(Duration::ZERO)));
        assert!(!volume_due(&old, P, Some(DAY)));
        assert!(volume_due(&old, P, Some(15 * DAY)));
        // Docker gave no size, so it is not known to be empty
        let unknown = Volume { bytes: None, ..v };
        assert!(!volume_due(&unknown, P, Some(DAY)));
    }

    #[test]
    fn only_labeled_volumes_of_a_gone_home_on_this_host_are_orphans() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        fs::create_dir(&live).unwrap();
        let gone = tmp.path().join("gone");
        let h = here(&live, true);
        assert!(orphan(&labeled("shipcheck-111111-a", &gone), &h));
        assert!(!orphan(&labeled("shipcheck-111111-a", &live), &h));
        // Another host's home may exist there, so its volumes stay
        let other_host = Volume {
            host: Some("h2".into()),
            ..labeled("shipcheck-111111-a", &gone)
        };
        assert!(!orphan(&other_host, &h));

        // A volume without labels may be a live home's that an older binary made
        for name in ["shipcheck-111111-a", "shipcheck-4691d2-a", &format!("{P}a")] {
            assert!(!orphan(&bare(name), &h), "{name}");
        }

        // This home's go by `volume_due`
        assert!(!orphan(&labeled(&format!("{P}a"), &gone), &h));
        // A missing home proves nothing when its parent is missing too, or its path is not plain
        let unplain = [
            tmp.path().join("no-parent/gone"),
            tmp.path().join("live/../gone"),
            tmp.path().join("./gone"),
            std::path::PathBuf::from("relative/gone"),
        ];

        for home in &unplain {
            let v = labeled("shipcheck-111111-a", home);
            assert!(!orphan(&v, &h), "{}", home.display());
        }

        // Names that are not shipcheck's, even with shipcheck's labels
        for name in [
            "shipcheck-review-gaps-cargo",
            "cvm_redis-data",
            "act-toolcache",
            "act-ci-e2e-1",
            "sc-cargo",
            "shipcheck-ABCDEF-a",
        ] {
            assert!(!orphan(&bare(name), &h), "{name}");
            assert!(!orphan(&labeled(name, &gone), &h), "{name}");
        }
    }

    #[test]
    fn only_a_home_that_may_sweep_removes_another_homes_volumes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let gone = labeled("shipcheck-111111-x", &tmp.path().join("gone"));
        let fake = Fake {
            volumes: vec![gone.clone(), labeled(&format!("{P}x"), &home)],
            ..Fake::default()
        };

        // A test's or an agent's home, as `home_overridden` says
        prune(&fake, &here(&home, false), &[], &[], SystemTime::now());
        clean(&fake, &here(&home, false), P, true);
        assert_eq!(*fake.removed.borrow(), [format!("{P}x")]);

        fake.removed.borrow_mut().clear();
        clean(&fake, &here(&home, true), P, false);
        assert!(!fake.removed.borrow().contains(&gone.name));
        clean(&fake, &here(&home, true), P, true);
        assert!(fake.removed.borrow().contains(&gone.name));
        fake.removed.borrow_mut().clear();
        prune(&fake, &here(&home, true), &[], &[], SystemTime::now());
        assert_eq!(*fake.removed.borrow(), [gone.name]);
    }

    #[test]
    fn only_an_old_pin_is_ever_due() {
        let old = ["catthehacker/ubuntu@sha256:old"];
        assert!(!image_due("postgres:16", None, &old));
        assert!(!image_due("postgres:16", Some(100 * DAY), &old));
        assert!(image_due(old[0], None, &old));
        assert!(image_due(old[0], Some(2 * DAY), &old));
        assert!(!image_due(old[0], Some(DAY / 2), &old));
    }

    #[test]
    fn a_gone_homes_volume_goes_only_once_a_day_old() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let gone = tmp.path().join("gone");
        // A sandbox's home behind a private /tmp on this host, whose run just made its volume
        let fake = Fake {
            volumes: vec![
                labeled("shipcheck-111111-new", &gone),
                labeled("shipcheck-111111-old", &gone),
            ],
            new: strings(&["shipcheck-111111-new"]),
            ..Fake::default()
        };

        prune(&fake, &here(&home, true), &[], &[], SystemTime::now());
        assert_eq!(*fake.removed.borrow(), ["shipcheck-111111-old"]);
        fake.removed.borrow_mut().clear();
        clean(&fake, &here(&home, true), P, true);
        assert_eq!(*fake.removed.borrow(), ["shipcheck-111111-old"]);
    }

    #[test]
    fn parses_dockers_created_at() {
        let at = |t: &str| {
            parse_created(t).map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs())
        };
        assert_eq!(at("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(at("2026-10-25T23:40:46Z\n"), Some(1_792_971_646));
        assert_eq!(at("2024-02-29T12:00:00.123+01:00"), Some(1_709_204_400));
        assert_eq!(at("2024-02-29T10:00:00-01:00"), Some(1_709_204_400));
        assert_eq!(at("yesterday"), None);
        assert_eq!(at(""), None);
    }

    #[test]
    fn prune_removes_only_unused_old_pins() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let (old_pin, used_pin) = ("catthehacker/ubuntu@sha256:old", "busybox@sha256:old");
        // A workflow's `services:` image and a docker action act built, old and unused
        stamp_at(home, "images", "postgres:16", 100 * DAY);
        stamp_at(home, "images", "act-x-dockeraction:latest", 100 * DAY);
        let fake = Fake {
            images: strings(&[
                "postgres:16",
                "act-x-dockeraction:latest",
                old_pin,
                used_pin,
            ]),
            // A stopped container made from the image's id
            containers: vec![format!("ancestor=sha256:id-{used_pin}")],
            ..Fake::default()
        };

        prune(
            &fake,
            &here(home, true),
            &[],
            &[old_pin, used_pin],
            SystemTime::now(),
        );
        assert_eq!(*fake.removed.borrow(), [old_pin]);
        assert!(home.join("images").join(encode("postgres:16")).exists());
    }

    #[test]
    fn docker_refusing_an_image_keeps_its_stamp_quietly() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        stamp_at(home, "images", "a/b:1", 20 * DAY);
        let fake = Fake {
            images: strings(&["a/b:1"]),
            refuse: strings(&["a/b:1"]),
            ..Fake::default()
        };

        assert!(prune(&fake, &here(home, true), &[], &["a/b:1"], SystemTime::now()).is_empty());
        assert!(home.join("images").join(encode("a/b:1")).exists());
    }

    #[test]
    fn prune_removes_only_unused_shipcheck_volumes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        let home = home.as_path();
        let gone = tmp.path().join("gone");
        let (old, fresh, unstamped, stopped, refused, empty, unlabeled) = (
            format!("{P}old"),
            format!("{P}fresh"),
            format!("{P}unstamped"),
            format!("{P}stopped"),
            format!("{P}refused"),
            format!("{P}empty"),
            format!("{P}unlabeled"),
        );
        stamp_at(home, "volumes", &old, 20 * DAY);
        stamp_at(home, "volumes", &fresh, DAY);
        stamp_at(home, "volumes", &stopped, 20 * DAY);
        stamp_at(home, "volumes", &refused, 20 * DAY);
        stamp_at(home, "volumes", &empty, DAY);
        stamp_at(home, "volumes", &unlabeled, DAY);
        stamp_at(home, "volumes", &format!("{P}gone"), DAY);
        let fake = Fake {
            volumes: [&old, &fresh, &unstamped, &stopped, &refused]
                .map(|n| labeled(n, home))
                .into_iter()
                .chain([
                    Volume {
                        bytes: Some(0),
                        ..labeled(&empty, home)
                    },
                    Volume {
                        bytes: Some(0),
                        ..bare(&unlabeled)
                    },
                ])
                .chain(
                    [
                        "act-toolcache",
                        "sbpf-cargo-registry",
                        "compose_db",
                        "cvm_cvm-data",
                        "sc-cargo",
                        "shipcheck-review-gaps-cargo",
                        "shipcheck-4691d2-home-runner--npm",
                        "act-ci-yml-a-85f7-env",
                        "act-CI-test-ffffffff-0abc-env",
                    ]
                    .map(bare),
                )
                .chain([
                    labeled("shipcheck-111111-x", &gone),
                    labeled("shipcheck-333333-x", &tmp.path().join("no/gone")),
                    bare("act-CI-test-1-13f69a23-0abc-env"),
                ])
                .collect(),
            // A stopped container still references it, as `docker ps -a` shows
            containers: vec![format!("volume={stopped}")],
            refuse: vec![refused.clone()],
            ..Fake::default()
        };

        let tags = strings(&["13f69a23"]);
        let done = prune(&fake, &here(home, true), &tags, &[], SystemTime::now());
        assert_eq!(
            *fake.removed.borrow(),
            [
                old.as_str(),
                empty.as_str(),
                "shipcheck-111111-x",
                "act-CI-test-1-13f69a23-0abc-env"
            ]
        );
        assert_eq!(done.len(), 4);
        // An unstamped volume gets a stamp dated now, so it lasts 14 more days
        let unstamped_age = age(home, "volumes", &unstamped, SystemTime::now()).unwrap();
        assert!(unstamped_age < DAY);
        assert!(!home.join("volumes").join(&old).exists());
        assert!(home.join("volumes").join(&fresh).exists());
        assert!(home.join("volumes").join(&stopped).exists());
        assert!(home.join("volumes").join(&refused).exists());
        // The stamp of a volume Docker no longer has goes
        assert!(!home.join("volumes").join(format!("{P}gone")).exists());
    }

    /// Docker, but it lists and removes only the volumes whose name holds the test's id
    struct Only<'a>(&'a str);

    impl Engine for Only<'_> {
        fn volumes(&self) -> Option<Vec<Volume>> {
            let all = Docker.volumes()?;
            Some(
                all.into_iter()
                    .filter(|v| v.name.contains(self.0))
                    .collect(),
            )
        }

        fn named(&self, part: &str) -> Option<Vec<String>> {
            let all = Docker.named(part)?;
            Some(all.into_iter().filter(|n| n.contains(self.0)).collect())
        }

        fn created(&self, name: &str) -> Option<SystemTime> {
            Docker.created(name)
        }

        fn used(&self, filter: &str) -> bool {
            Docker.used(filter)
        }

        fn image_id(&self, _: &str) -> Option<String> {
            None
        }

        fn remove(&self, kind: &str, name: &str) -> bool {
            kind == "volume" && name.contains(self.0) && Docker.remove(kind, name)
        }
    }

    /// Every volume name on the machine but this test's
    fn others(id: &str) -> BTreeSet<String> {
        let out = docker(&["volume", "ls", "-q"]).unwrap();
        (out.split_whitespace())
            .filter(|n| !n.contains(id))
            .map(String::from)
            .collect()
    }

    #[test]
    #[ignore = "needs Docker"]
    fn docker_prune_touches_only_what_it_may() {
        let tmp = tempfile::tempdir().unwrap();
        let (live, gone) = (tmp.path().join("live"), tmp.path().join("gone"));
        fs::create_dir(&live).unwrap();
        let id = &crate::cache::hex(&rand_bytes())[..16];
        let before = others(id);
        // `shipcheck-` and six hex digits make a name shipcheck's, the whole id makes it this test's
        let p = &id[..6];
        let names = [
            format!("shipcheck-{p}-{id}-gone"),
            format!("shipcheck-{p}-{id}-bare"),
            format!("shipcheck-{p}-{id}-live"),
            format!("shipcheck-review-{id}"),
            format!("act-CI-t-{id}-0a-env"),
        ];
        create_volume(&names[0], &gone, "a/b", "/x").unwrap();
        assert!(docker(&["volume", "create", &names[1]]).is_some());
        create_volume(&names[2], &live, "a/b", "/x").unwrap();
        assert!(docker(&["volume", "create", &names[3]]).is_some());
        assert!(docker(&["volume", "create", &names[4]]).is_some());
        let host = hostname();
        let here = Here {
            home: &live,
            prefix: "shipcheck-ffffff-",
            host: &host,
            sweep: true,
        };

        prune(&Only(id), &here, &[], &[], SystemTime::now());
        remove_env_volumes(&Only(id), id);
        let left = Only(id).volumes().unwrap();
        let kept = names.each_ref().map(|n| left.iter().any(|v| &v.name == n));

        for name in &names {
            Only(id).remove("volume", name);
        }

        // The gone home's volume is new, so it stays a day, as a sandbox's would
        assert_eq!(kept, [true, true, true, true, false]);
        assert_eq!(
            others(id),
            before,
            "the test changed volumes it did not make"
        );
    }

    fn rand_bytes() -> [u8; 8] {
        let mut b = [0u8; 8];
        fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
            .unwrap();
        b
    }

    #[test]
    fn parses_labels_from_docker_system_df() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"Name":"shipcheck-abc123-x","Labels":"shipcheck.repo=a/b,shipcheck.home=/Users/a/.local/state/shipcheck,shipcheck.host=mac","Size":"0B"}"#,
        )
        .unwrap();
        let v = parse_volume(&v);
        assert_eq!(v.home.as_deref(), Some("/Users/a/.local/state/shipcheck"));
        assert_eq!(v.host.as_deref(), Some("mac"));
        assert_eq!(v.bytes, Some(0));
        let v = parse_volume(&serde_json::json!({"Name": "x", "Labels": "", "Size": "N/A"}));
        assert_eq!((v.home, v.host, v.bytes), (None, None, None));
    }

    #[test]
    fn labels_refuse_a_comma() {
        let ok = labels(Path::new("/h"), "mac", "a/b", "/x").unwrap();
        assert_eq!(
            ok,
            [
                "shipcheck.home=/h",
                "shipcheck.host=mac",
                "shipcheck.repo=a/b",
                "shipcheck.path=/x"
            ]
        );
        assert!(labels(Path::new("/h,x=1"), "mac", "a/b", "/x").is_err());
        assert!(labels(Path::new("/h"), "mac", "a/b", "/x,y").is_err());
    }

    #[test]
    fn clean_removes_only_its_volumes() {
        let home = tempfile::tempdir().unwrap();
        stamp(home.path(), "images", "a:1");
        let fake = Fake {
            images: strings(&["a:1"]),
            volumes: vec![
                labeled(&format!("{P}x"), home.path()),
                bare("pg-data"),
                bare("shipcheck-222222-x"),
            ],
            ..Fake::default()
        };

        clean(&fake, &here(home.path(), true), P, true);
        assert_eq!(*fake.removed.borrow(), [format!("{P}x")]);
    }

    #[test]
    fn env_volumes_match_the_tag_only() {
        let fake = Fake {
            volumes: [
                "act-CI-test-1-13f69a23-0abc-env",
                "act-CI-test-1-13f69a23-0abc",
                "other-13f69a23-env",
                "act-CI-test-ffffffff-0abc-env",
                "act-toolcache",
            ]
            .map(bare)
            .to_vec(),
            ..Fake::default()
        };
        remove_env_volumes(&fake, "13f69a23");
        assert_eq!(*fake.removed.borrow(), ["act-CI-test-1-13f69a23-0abc-env"]);
        assert!(!env_volume("act-toolcache", "toolcache"));
    }

    #[test]
    fn reads_images_from_act_lines_only() {
        let log = "\
[CI/test 13f69a23]   🐳  docker pull image=catthehacker/ubuntu@sha256:c5 platform= username= forcePull=false
[CI/test 13f69a23]   🐳  docker create image=postgres:16 platform= entrypoint=[] cmd=[] network=\"x\"
[CI/test 13f69a23]   🐳  docker run image=act-x-dockeraction:latest platform= entrypoint=[] cmd=[]
[CI/test 13f69a23]   🐳  docker build -t act-y-dockeraction:latest /dir
[CI/test 13f69a23]   | 🐳  docker run image=mine:latest platform=
🐳  docker run image=bare:latest
";
        assert_eq!(
            images_in_log(log).into_iter().collect::<Vec<_>>(),
            [
                "act-x-dockeraction:latest",
                "catthehacker/ubuntu@sha256:c5",
                "postgres:16"
            ]
        );
    }

    #[test]
    fn stamps_round_trip_image_references() {
        let home = tempfile::tempdir().unwrap();
        stamp(home.path(), "images", "ghcr.io/a/b@sha256:1%");
        assert_eq!(stamps(home.path(), "images"), ["ghcr.io/a/b@sha256:1%"]);
    }

    #[test]
    fn parses_docker_sizes() {
        assert_eq!(size("4.3GB"), Some(4_300_000_000));
        assert_eq!(size("512kB"), Some(512_000));
        assert_eq!(size("0B"), Some(0));
        assert_eq!(size("N/A"), None);
    }
}
