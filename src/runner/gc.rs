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
    /// The default home's prefix. Only that home removes its own volumes from before labels
    pub default_prefix: &'a str,
    pub host: &'a str,
}

/// The Docker calls the prune makes, so its decisions can be tested without Docker
pub trait Engine {
    /// Every volume on the machine, None when Docker cannot list them
    fn volumes(&self) -> Option<Vec<Volume>>;
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

/// Docker joins labels as `k=v,k=v`. A home with a comma never runs, `venv_mount` refuses it
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
    let out = Command::new("docker")
        .args(["volume", "create"])
        .arg("--label")
        .arg(format!("shipcheck.home={}", home.display()))
        .arg("--label")
        .arg(format!("shipcheck.host={}", hostname()))
        .arg("--label")
        .arg(format!("shipcheck.repo={repo_id}"))
        .arg("--label")
        .arg(format!("shipcheck.path={path}"))
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
 * A volume of this home goes when no run used it for `KEEP`, when it is empty and no run used it
 * for `KEEP_EMPTY`, or when it has no labels: the next run that needs it makes it again with them.
 * One without a stamp starts its days now
 **/
fn volume_due(v: &Volume, prefix: &str, age: Option<Duration>) -> bool {
    let Some(age) = age.filter(|_| ours(&v.name, prefix)) else {
        return false;
    };
    age > KEEP || v.home.is_none() || (v.bytes == Some(0) && age > KEEP_EMPTY)
}

/// A volume of another home goes when that home is gone from this host, or when it has no labels
/// and is not the default home's, which removes its own
fn orphan(v: &Volume, here: &Here) -> bool {
    if !shipcheck_named(&v.name) || v.name.starts_with(here.prefix) {
        return false;
    }

    match (&v.home, &v.host) {
        (Some(home), Some(host)) => {
            host == here.host
                && matches!(fs::symlink_metadata(home), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        }
        (None, None) => !v.name.starts_with(here.default_prefix),
        _ => false,
    }
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

        done.extend(remove_orphans(engine, &listed, here));

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

fn remove_orphans(engine: &dyn Engine, listed: &[Volume], here: &Here) -> Vec<String> {
    listed
        .iter()
        .filter(|v| orphan(v, here) && remove_volume(engine, &v.name))
        .map(|v| format!("removed volume {} of another home", v.name))
        .collect()
}

/// Removes the volumes that start with `prefix`, and with `orphans` also those `orphan` picks.
/// Whatever a container references stays
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
        done.extend(remove_orphans(engine, &listed, here));
    }

    done
}

/// act's `act-<workflow>-<job>-<hash>-env` volumes of the jobs whose name carries this tag
fn env_volumes<'a>(listed: &'a [Volume], tag: &str) -> impl Iterator<Item = &'a str> {
    let part = format!("-{tag}-");
    listed
        .iter()
        .map(|v| v.name.as_str())
        .filter(move |n| n.starts_with("act-") && n.ends_with("-env") && n.contains(&part))
}

/// The act volumes of a job, which a killed act leaves behind
pub fn remove_env_volumes(engine: &dyn Engine, tag: &str) {
    for name in env_volumes(&engine.volumes().unwrap_or_default(), tag) {
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
            ". Volumes of gone homes: {gone}, {}, removed after the next run, or now with `shipcheck clean --all`",
            gigabytes(gone_bytes)
        );
    }

    if others > 0 {
        text += &format!(
            ". Volumes of other live homes or hosts: {others}, {}, kept",
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
        removed: RefCell<Vec<String>>,
    }

    impl Engine for Fake {
        fn volumes(&self) -> Option<Vec<Volume>> {
            Some(self.volumes.clone())
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

    fn here(home: &Path) -> Here<'_> {
        Here {
            home,
            prefix: P,
            default_prefix: "shipcheck-def456-",
            host: "h1",
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
        // Empty goes after an hour, one from before labels on the first prune that stamped it
        let empty = Volume {
            bytes: Some(0),
            ..v.clone()
        };
        assert!(!volume_due(&empty, P, Some(DAY / 48)));
        assert!(volume_due(&empty, P, Some(DAY / 12)));
        assert!(volume_due(&bare(&format!("{P}x")), P, Some(Duration::ZERO)));
        assert!(!volume_due(&bare(&format!("{P}x")), P, None));
    }

    #[test]
    fn only_volumes_of_gone_homes_and_unlabeled_strangers_are_orphans() {
        let live = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let (live, gone) = (live.path(), tmp.path().join("gone"));
        let h = here(live);
        assert!(orphan(&labeled("shipcheck-111111-a", &gone), &h));
        assert!(!orphan(&labeled("shipcheck-111111-a", live), &h));
        // Another host's home may exist there, so its volumes stay
        let other_host = Volume {
            host: Some("h2".into()),
            ..labeled("shipcheck-111111-a", &gone)
        };
        assert!(!orphan(&other_host, &h));
        assert!(orphan(&bare("shipcheck-111111-a"), &h));
        // The default home removes its own, this home's go by `volume_due`
        assert!(!orphan(&bare("shipcheck-def456-a"), &h));
        assert!(!orphan(&bare(&format!("{P}a")), &h));
        assert!(!orphan(&labeled(&format!("{P}a"), &gone), &h));
        // Names that are not shipcheck's, even with shipcheck's labels
        for name in [
            "shipcheck-review-gaps-cargo",
            "cvm_redis-data",
            "act-ci-e2e-1",
            "shipcheck-ABCDEF-a",
        ] {
            assert!(!orphan(&bare(name), &h), "{name}");
            assert!(!orphan(&labeled(name, &gone), &h), "{name}");
        }
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
    fn prune_removes_only_unused_shipcheck_volumes() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let gone = home.join("gone");
        let (old, fresh, bare_name, stopped, refused) = (
            format!("{P}old"),
            format!("{P}fresh"),
            format!("{P}bare"),
            format!("{P}stopped"),
            format!("{P}refused"),
        );
        stamp_at(home, "volumes", &old, 20 * DAY);
        stamp_at(home, "volumes", &fresh, DAY);
        stamp_at(home, "volumes", &stopped, 20 * DAY);
        stamp_at(home, "volumes", &format!("{P}gone"), DAY);
        let fake = Fake {
            volumes: [&old, &fresh, &bare_name, &stopped, &refused]
                .map(|n| labeled(n, home))
                .into_iter()
                .chain(
                    [
                        "act-toolcache",
                        "sbpf-cargo-registry",
                        "myapp-cargo",
                        "compose_db",
                        "cvm_cvm-data",
                        "shipcheck-review-gaps-cargo",
                        "shipcheck-def456-x",
                        "act-ci-yml-a-85f7-env",
                        "act-CI-test-ffffffff-0abc-env",
                    ]
                    .map(bare),
                )
                .chain([
                    labeled("shipcheck-111111-x", &gone),
                    bare("shipcheck-222222-x"),
                ])
                .chain([bare("act-CI-test-1-13f69a23-0abc-env")])
                .collect(),
            // A stopped container still references it, as `docker ps -a` shows
            containers: vec![format!("volume={stopped}")],
            refuse: vec![refused.clone()],
            ..Fake::default()
        };

        let done = prune(
            &fake,
            &here(home),
            &strings(&["13f69a23"]),
            &[],
            SystemTime::now(),
        );
        assert_eq!(
            *fake.removed.borrow(),
            [
                old.as_str(),
                "shipcheck-111111-x",
                "shipcheck-222222-x",
                "act-CI-test-1-13f69a23-0abc-env"
            ]
        );
        assert_eq!(done.len(), 4);
        // An unstamped volume gets a stamp dated now, so it lasts 14 more days
        let bare_age = age(home, "volumes", &bare_name, SystemTime::now()).unwrap();
        assert!(bare_age < DAY);
        assert!(!home.join("volumes").join(&old).exists());
        assert!(home.join("volumes").join(&fresh).exists());
        assert!(home.join("volumes").join(&stopped).exists());
        // The stamp of a volume Docker no longer has goes
        assert!(!home.join("volumes").join(format!("{P}gone")).exists());
    }

    #[test]
    #[ignore = "needs Docker"]
    fn docker_prune_removes_the_volumes_of_a_gone_home_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (live, gone) = (tmp.path().join("live"), tmp.path().join("gone"));
        fs::create_dir(&live).unwrap();
        let id = &crate::cache::hex(&rand_bytes())[..6];
        let names = [
            format!("shipcheck-{id}-gone"),
            format!("shipcheck-{id}-bare"),
            format!("shipcheck-{id}-live"),
            format!("shipcheck-review-{id}"),
        ];
        create_volume(&names[0], &gone, "a/b", "/x").unwrap();
        assert!(docker(&["volume", "create", &names[1]]).is_some());
        create_volume(&names[2], &live, "a/b", "/x").unwrap();
        assert!(docker(&["volume", "create", &names[3]]).is_some());
        let host = hostname();
        let here = Here {
            home: &live,
            prefix: "shipcheck-ffffff-",
            default_prefix: "shipcheck-eeeeee-",
            host: &host,
        };

        // Docker, but it lists only this test's volumes, so the prune never sees anyone else's
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

            fn used(&self, filter: &str) -> bool {
                Docker.used(filter)
            }

            fn image_id(&self, reference: &str) -> Option<String> {
                Docker.image_id(reference)
            }

            fn remove(&self, kind: &str, name: &str) -> bool {
                name.contains(self.0) && Docker.remove(kind, name)
            }
        }

        prune(&Only(id), &here, &[], &[], SystemTime::now());
        let left = Only(id).volumes().unwrap();
        let has = |n: &str| left.iter().any(|v| v.name == n);
        let kept = [
            has(&names[0]),
            has(&names[1]),
            has(&names[2]),
            has(&names[3]),
        ];

        for name in &names {
            let _ = docker(&["volume", "rm", name]);
        }

        assert_eq!(kept, [false, false, true, true]);
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
            &here(home),
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

        assert!(prune(&fake, &here(home), &[], &["a/b:1"], SystemTime::now()).is_empty());
        assert!(home.join("images").join(encode("a/b:1")).exists());
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

        clean(&fake, &here(home.path()), P, false);
        assert_eq!(*fake.removed.borrow(), [format!("{P}x")]);
        clean(&fake, &here(home.path()), P, true);
        assert_eq!(fake.removed.borrow().last().unwrap(), "shipcheck-222222-x");
    }

    #[test]
    fn env_volumes_match_the_tag_only() {
        let fake = Fake {
            volumes: [
                "act-CI-test-1-13f69a23-0abc-env",
                "act-CI-test-1-13f69a23-0abc",
                "other-13f69a23-env",
                "act-CI-test-ffffffff-0abc-env",
            ]
            .map(bare)
            .to_vec(),
            ..Fake::default()
        };
        remove_env_volumes(&fake, "13f69a23");
        assert_eq!(*fake.removed.borrow(), ["act-CI-test-1-13f69a23-0abc-env"]);
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
