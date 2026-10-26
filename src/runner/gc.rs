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

/// The Docker calls the prune makes, so its decisions can be tested without Docker
pub trait Engine {
    /// Volume names that contain `part`, None when Docker cannot list them
    fn volumes(&self, part: &str) -> Option<Vec<String>>;
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
    fn volumes(&self, part: &str) -> Option<Vec<String>> {
        let out = docker(&["volume", "ls", "-q", "--filter", &format!("name={part}")])?;
        Some(out.split_whitespace().map(String::from).collect())
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

fn ours(name: &str, prefix: &str) -> bool {
    prefix.starts_with("shipcheck-") && name.starts_with(prefix)
}

/// A volume of this home goes when its stamp is old. One without a stamp gets one first
fn volume_due(name: &str, prefix: &str, age: Option<Duration>) -> bool {
    ours(name, prefix) && age.is_some_and(|a| a > KEEP)
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

/// Removes this home's volumes unused for `KEEP`, and every image of `old_pins` no config uses.
/// Whatever a container references, running or stopped, stays
pub fn prune(
    engine: &dyn Engine,
    home: &Path,
    prefix: &str,
    old_pins: &[&str],
    now: SystemTime,
) -> Vec<String> {
    let mut done = Vec::new();

    if let Some(listed) = engine.volumes(prefix) {
        for name in &listed {
            let age = age(home, "volumes", name, now);

            // A volume from before stamps, or from a run that crashed before its stamp, starts its days now
            if age.is_none() && ours(name, prefix) {
                stamp(home, "volumes", name);
            }

            if volume_due(name, prefix, age) && remove_volume(engine, name) {
                unstamp(home, "volumes", name);
                done.push(format!("removed volume {name}"));
            }
        }

        for name in stamps(home, "volumes") {
            if !listed.contains(&name) {
                unstamp(home, "volumes", &name);
            }
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

/// Removes the volumes that start with `prefix`. Whatever a container references stays
pub fn clean(engine: &dyn Engine, home: &Path, prefix: &str) -> Vec<String> {
    let mut done = Vec::new();

    for name in engine.volumes(prefix).unwrap_or_default() {
        if ours(&name, prefix) && remove_volume(engine, &name) {
            unstamp(home, "volumes", &name);
            done.push(format!("removed volume {name}"));
        }
    }

    done
}

/// act's `act-<workflow>-<job>-<hash>-env` volumes of the jobs whose name carries this tag, which
/// a killed act leaves behind
pub fn remove_env_volumes(engine: &dyn Engine, tag: &str) {
    let part = format!("-{tag}-");

    for name in engine.volumes(&part).unwrap_or_default() {
        if name.starts_with("act-") && name.ends_with("-env") && name.contains(&part) {
            remove_volume(engine, &name);
        }
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

/// Count and size of this home's volumes and stamped images. `docker system df -v` measures volumes
pub fn usage(home: &Path, prefix: &str) -> Result<String, String> {
    let out = docker(&["system", "df", "-v", "--format", "{{json .Volumes}}"])
        .ok_or("docker system df failed")?;
    let volumes: Vec<serde_json::Value> = serde_json::from_str(&out).map_err(|e| e.to_string())?;
    let (count, bytes) = volumes
        .iter()
        .filter(|v| v["Name"].as_str().is_some_and(|n| n.starts_with(prefix)))
        .fold((0, 0), |(n, b), v| {
            (n + 1, b + v["Size"].as_str().map_or(0, size))
        });

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

    Ok(format!(
        "Docker volumes of this home: {count}, {}, removed after a run once unused for 14 days, or now with `shipcheck clean`. Images its runs used: {}, {}, kept",
        gigabytes(bytes),
        images.len(),
        gigabytes(image_bytes)
    ))
}

/// Docker's sizes are decimal: `4.3GB`, `512kB`, `0B`
fn size(text: &str) -> u64 {
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
        _ => 0.0,
    };
    (n.parse::<f64>().unwrap_or(0.0) * scale) as u64
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
        volumes: Vec<String>,
        images: Vec<String>,
        containers: Vec<String>,
        refuse: Vec<String>,
        removed: RefCell<Vec<String>>,
    }

    impl Engine for Fake {
        fn volumes(&self, part: &str) -> Option<Vec<String>> {
            Some(
                self.volumes
                    .iter()
                    .filter(|v| v.contains(part))
                    .cloned()
                    .collect(),
            )
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
        assert!(!volume_due(&format!("{P}x"), P, None));
        assert!(volume_due(&format!("{P}x"), P, Some(15 * DAY)));
        assert!(!volume_due(&format!("{P}x"), P, Some(13 * DAY)));
        assert!(!volume_due("shipcheck-other-x", P, None));
        // A prefix that is not shipcheck's never matches, even an empty one
        assert!(!volume_due("pg-data", "", Some(15 * DAY)));
        assert!(!volume_due("act-toolcache", "act-", Some(15 * DAY)));
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
        let (old, fresh, bare, stopped, refused) = (
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
            volumes: [&old, &fresh, &bare, &stopped, &refused]
                .map(String::clone)
                .into_iter()
                .chain(strings(&[
                    "act-toolcache",
                    "sbpf-cargo-registry",
                    "myapp-cargo",
                    "compose_db",
                ]))
                .collect(),
            // A stopped container still references it, as `docker ps -a` shows
            containers: vec![format!("volume={stopped}")],
            refuse: vec![refused.clone()],
            ..Fake::default()
        };

        let done = prune(&fake, home, P, &[], SystemTime::now());
        assert_eq!(*fake.removed.borrow(), std::slice::from_ref(&old));
        assert_eq!(done.len(), 1);
        // An unstamped volume gets a stamp dated now, so it lasts 14 more days
        let bare_age = age(home, "volumes", &bare, SystemTime::now()).unwrap();
        assert!(bare_age < DAY);
        assert!(!home.join("volumes").join(&old).exists());
        assert!(home.join("volumes").join(&fresh).exists());
        assert!(home.join("volumes").join(&stopped).exists());
        // The stamp of a volume Docker no longer has goes
        assert!(!home.join("volumes").join(format!("{P}gone")).exists());
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

        prune(&fake, home, P, &[old_pin, used_pin], SystemTime::now());
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

        assert!(prune(&fake, home, P, &["a/b:1"], SystemTime::now()).is_empty());
        assert!(home.join("images").join(encode("a/b:1")).exists());
    }

    #[test]
    fn clean_removes_only_its_volumes() {
        let home = tempfile::tempdir().unwrap();
        stamp(home.path(), "images", "a:1");
        let fake = Fake {
            images: strings(&["a:1"]),
            volumes: strings(&[&format!("{P}x"), "pg-data"]),
            ..Fake::default()
        };

        clean(&fake, home.path(), P);
        assert_eq!(*fake.removed.borrow(), [format!("{P}x")]);
    }

    #[test]
    fn env_volumes_match_the_tag_only() {
        let fake = Fake {
            volumes: strings(&[
                "act-CI-test-1-13f69a23-0abc-env",
                "act-CI-test-1-13f69a23-0abc",
                "other-13f69a23-env",
                "act-CI-test-ffffffff-0abc-env",
            ]),
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
        assert_eq!(size("4.3GB"), 4_300_000_000);
        assert_eq!(size("512kB"), 512_000);
        assert_eq!(size("0B"), 0);
        assert_eq!(size("N/A"), 0);
    }
}
