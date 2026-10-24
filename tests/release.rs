// The release logic of ci.yml, run from scripts/ against temp repos, with a fake crates.io
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(dir: &Path, subject: &str) {
    git(dir, &["commit", "-q", "--allow-empty", "-m", subject]);
}

fn tag(dir: &Path, name: &str) {
    git(dir, &["tag", "-a", name, "-m", name]);
}

// A repo with the real Cargo.toml and Cargo.lock in its first commit
fn repo(tmp: &Path) -> PathBuf {
    let dir = tmp.join("origin");
    fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    fs::copy(root().join("Cargo.toml"), dir.join("Cargo.toml")).unwrap();
    fs::copy(root().join("Cargo.lock"), dir.join("Cargo.lock")).unwrap();
    set_toml_version(&dir, "0.1.0");
    git(&dir, &["add", "."]);
    commit(&dir, "Start");
    dir
}

fn set_toml_version(dir: &Path, v: &str) {
    let t = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
    let old = t.lines().find(|l| l.starts_with("version = ")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        t.replacen(old, &format!("version = \"{v}\""), 1),
    )
    .unwrap();
}

// The outputs next-version.sh gives the check-release job, as `key=value`
fn next_version(dir: &Path) -> Vec<String> {
    let out = Command::new(root().join("scripts/next-version.sh"))
        .current_dir(dir)
        .env_remove("GITHUB_OUTPUT")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(String::from)
        .collect()
}

fn version(dir: &Path) -> String {
    let out = next_version(dir);
    assert!(out.contains(&"skip=false".to_string()), "{out:?}");
    out.iter()
        .find_map(|l| l.strip_prefix("version="))
        .unwrap()
        .to_string()
}

fn stamp(dir: &Path, v: &str) -> Output {
    Command::new("python3")
        .arg(root().join("scripts/stamp-version.py"))
        .arg(v)
        .current_dir(dir)
        .output()
        .unwrap()
}

#[test]
fn a_fix_bumps_the_patch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = repo(tmp.path());
    tag(&dir, "v0.1.0");
    commit(&dir, "fix: a");
    commit(&dir, "Plain subject");
    let out = next_version(&dir);
    assert_eq!(out, ["last_tag=v0.1.0", "skip=false", "version=v0.1.1"]);
}

#[test]
fn a_feat_bumps_the_minor() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = repo(tmp.path());
    tag(&dir, "v0.1.3");
    commit(&dir, "fix: a");
    commit(&dir, "feat(cli): b");
    assert_eq!(version(&dir), "v0.2.0");
}

#[test]
fn breaking_bumps_the_major() {
    for subject in ["breaking: a", "breaking(daemon): a", "BREAKING CHANGE: a"] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = repo(tmp.path());
        tag(&dir, "v0.4.2");
        commit(&dir, "feat: b");
        commit(&dir, subject);
        assert_eq!(version(&dir), "v1.0.0", "{subject}");
    }
}

#[test]
fn only_chore_and_docs_skip_the_release() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = repo(tmp.path());
    tag(&dir, "v0.1.0");
    commit(&dir, "chore: a");
    commit(&dir, "docs(readme): b");
    assert_eq!(next_version(&dir), ["last_tag=v0.1.0", "skip=true"]);

    commit(&dir, "fix: c");
    assert_eq!(version(&dir), "v0.1.1");
}

#[test]
fn no_commit_since_the_tag_skips_the_release() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = repo(tmp.path());
    tag(&dir, "v0.1.0");
    assert_eq!(next_version(&dir), ["last_tag=v0.1.0", "skip=true"]);
}

#[test]
fn the_first_release_bumps_the_cargo_toml_version() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = repo(tmp.path());
    set_toml_version(&dir, "0.3.4");
    git(&dir, &["commit", "-q", "-am", "fix: a"]);
    assert_eq!(
        next_version(&dir),
        ["last_tag=", "skip=false", "version=v0.3.5"]
    );
}

// A tag on a commit HEAD does not reach, as a run that tagged and then failed leaves
#[test]
fn a_taken_tag_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = repo(tmp.path());
    tag(&dir, "v0.1.0");
    git(&dir, &["checkout", "-q", "-b", "side"]);
    commit(&dir, "fix: side");
    tag(&dir, "v0.1.1");
    git(&dir, &["checkout", "-q", "main"]);
    commit(&dir, "fix: a");
    assert_eq!(
        next_version(&dir),
        ["last_tag=v0.1.0", "skip=false", "version=v0.1.2"]
    );
}

#[test]
fn the_stamp_writes_cargo_toml_and_cargo_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = repo(tmp.path());
    let lock = fs::read_to_string(dir.join("Cargo.lock")).unwrap();
    let out = stamp(&dir, "v7.8.9");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let toml = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
    assert_eq!(
        toml.lines().find(|l| l.starts_with("version")),
        Some("version = \"7.8.9\"")
    );
    let stamped = fs::read_to_string(dir.join("Cargo.lock")).unwrap();
    assert!(stamped.contains("name = \"shipcheck\"\nversion = \"7.8.9\"\n"));
    let changed: Vec<_> = lock
        .lines()
        .zip(stamped.lines())
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(changed.len(), 1, "{changed:?}");

    // A lock that no longer matches the manifest fails `cargo build --locked` in the build job
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/lib.rs"), "").unwrap();
    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--locked", "--offline", "--format-version", "1"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_stamp_refuses_an_empty_version() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = repo(tmp.path());
    let toml = fs::read_to_string(dir.join("Cargo.toml")).unwrap();
    for v in ["", "v"] {
        let out = stamp(&dir, v);
        assert!(!out.status.success(), "{v:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("produced no version"));
        assert_eq!(fs::read_to_string(dir.join("Cargo.toml")).unwrap(), toml);
    }
}

const TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
];

// Formula/shipcheck.rb filled from the archives, as the formula step does
fn fill_formula(template: &str) -> (tempfile::TempDir, Output) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::create_dir_all(dir.join("Formula")).unwrap();
    fs::create_dir_all(dir.join("artifacts")).unwrap();
    fs::write(dir.join("Formula/shipcheck.rb"), template).unwrap();
    for target in TARGETS {
        fs::write(
            dir.join(format!("artifacts/shipcheck-{target}.tar.gz")),
            target,
        )
        .unwrap();
    }

    let out = Command::new(root().join("scripts/fill-formula.sh"))
        .arg("v1.2.3")
        .current_dir(dir)
        .env("GITHUB_REPOSITORY", "owner/repo")
        .output()
        .unwrap();
    (tmp, out)
}

#[test]
fn the_formula_gets_the_version_and_every_archive() {
    let template = fs::read_to_string(root().join("Formula/shipcheck.rb")).unwrap();
    let (tmp, out) = fill_formula(&template);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let formula = fs::read_to_string(tmp.path().join("formula.rb")).unwrap();
    assert!(formula.contains("\n  version \"1.2.3\"\n"), "{formula}");
    assert!(!formula.contains("0.0.0"));
    let base = "https://github.com/owner/repo/releases/download/v1.2.3";
    for target in TARGETS {
        assert!(formula.contains(&format!("url \"{base}/shipcheck-{target}.tar.gz\"")));
        let sum = Command::new("sha256sum")
            .arg(format!("artifacts/shipcheck-{target}.tar.gz"))
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let sum = String::from_utf8_lossy(&sum.stdout);
        let sum = sum.split_whitespace().next().unwrap();
        assert!(formula.contains(&format!("sha256 \"{sum}\"")), "{target}");
    }
}

// The version sed matches only two spaces of indent, and the placeholder check greps only URL_ and SHA_
#[test]
fn a_formula_left_at_version_0_0_0_is_caught() {
    let template = fs::read_to_string(root().join("Formula/shipcheck.rb")).unwrap();
    let template = template.replace("  version \"0.0.0\"", "\tversion \"0.0.0\"");
    let (tmp, out) = fill_formula(&template);
    let formula = fs::read_to_string(tmp.path().join("formula.rb")).unwrap();
    assert!(
        !out.status.success(),
        "the formula step passed with a formula that installs as 0.0.0:\n{}",
        formula.lines().find(|l| l.contains("version")).unwrap()
    );
}

// A fake `cargo` that answers `publish` as crates.io does, from a file of the versions it holds
const FAKE_CARGO: &str = r#"#!/bin/sh
[ "$1" = publish ] || exit 2
v=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
if grep -qx "$v" "$FAKE_CRATES_IO" 2>/dev/null; then
  echo "error: failed to publish to registry at https://crates.io" >&2
  echo "the remote server responded with an error (status 400 Bad Request): crate shipcheck@$v already exists on crates.io index" >&2
  exit 101
fi
echo "$v" >> "$FAKE_CRATES_IO"
echo "Published shipcheck v$v at registry crates-io"
"#;

// One release job up to the tag: a new clone, the version, the stamp and the publish
fn release_until_tag(tmp: &Path, origin: &Path, n: u32) -> (String, Output) {
    let dir = tmp.join(format!("run{n}"));
    git(
        tmp,
        &[
            "clone",
            "-q",
            &origin.to_string_lossy(),
            &dir.to_string_lossy(),
        ],
    );
    let v = version(&dir);
    assert!(stamp(&dir, &v).status.success());
    let path = format!(
        "{}:{}",
        tmp.join("bin").display(),
        std::env::var("PATH").unwrap()
    );
    let out = Command::new(root().join("scripts/publish-crate.sh"))
        .current_dir(&dir)
        .env("FAKE_CRATES_IO", tmp.join("crates-io"))
        .env("PATH", path)
        .output()
        .unwrap();
    (v, out)
}

// The publish step claims a rerun then tags it, but with no tag the rerun repeats the version
#[test]
fn a_half_release_is_finished_by_the_next_run() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = repo(tmp.path());
    tag(&origin, "v0.1.0");
    commit(&origin, "fix: a");
    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("cargo"), FAKE_CARGO).unwrap();
    fs::set_permissions(
        bin.join("cargo"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();

    let (first, out) = release_until_tag(tmp.path(), &origin, 1);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(first, "v0.1.1");
    // Cancelled here, before `Create tag`

    let (second, out) = release_until_tag(tmp.path(), &origin, 2);
    assert_eq!(
        second, first,
        "the next run picks the version again, since no tag holds it"
    );
    assert!(
        out.status.success(),
        "the next run cannot publish {second}, so it never reaches the tag:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
