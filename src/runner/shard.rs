use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use yaml_rust2::Yaml;
use yaml_rust2::yaml::Hash;

use crate::cache;
use crate::config::ShardTests;
use crate::workflow::key;

/// The plugin each shard loads, written into the job's own `.git`, never into a shared volume
pub const PLUGIN: &str = include_str!("shipcheck_shard.py");
pub const PLUGIN_DIR: &str = "shipcheck-plugin";
pub const REPORT_DIR: &str = "shipcheck-shards";
/// In every wrapper step and nowhere else, so a job's prepared copy shows whether it shards
pub const MARK: &str = "SHIPCHECK_SHARD=";
const MAX_SHARDS: u32 = 6;
/// The most shards `shard_tests` may fix
pub const MAX_FIXED: u32 = 32;
const MAX_REPORTS: u64 = 64 << 20;

/// What the manifest records about a job that had a sharded step
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShardRun {
    /// None when the sharded run was not accepted
    pub shards: Option<u32>,
    pub items: Option<u64>,
    /// Why the job ran again unsharded, whose result is the job's result
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// The pytest command line of a `run` step shipcheck can shard, or None
pub fn pytest_line(step: &Hash, default_shell: Option<&str>) -> Option<String> {
    if step.contains_key(&key("uses")) {
        return None;
    }

    let shell = match step.get(&key("shell")) {
        Some(Yaml::String(s)) => Some(s.as_str()),
        None => default_shell,
        Some(_) => return None,
    };

    if shell.is_some_and(|s| s != "bash") {
        return None;
    }

    let line = step.get(&key("run"))?.as_str()?.trim();

    if line.contains('\n') || line.contains([';', '&', '|', '<', '>', '$', '`', '(', ')', '\\']) {
        return None;
    }

    let words: Vec<&str> = line.split_whitespace().collect();
    let mut at = match words.as_slice() {
        ["uv", "run", ..] | ["poetry" | "pdm", "run", ..] | ["python" | "python3", "-m", ..] => 2,
        _ => 0,
    };

    if words.first() == Some(&"uv") {
        let flag = |w: &str| {
            w.strip_prefix("--").is_some_and(|f| {
                !f.is_empty()
                    && f.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
            })
        };

        while words.get(at).is_some_and(|w| flag(w)) {
            at += 1;
        }
    }

    // pytest-xdist splits the run itself, which the plugin cannot see into
    let xdist = words[at.min(words.len())..].iter().any(|w| {
        w.starts_with("-n")
            || w.starts_with("--numprocesses")
            || w.starts_with("--dist")
            || w.contains("xdist")
    });

    (words.get(at) == Some(&"pytest") && !xdist).then(|| line.to_string())
}

/// The `defaults.run.shell` of a workflow or job mapping
pub fn default_shell(h: &Hash) -> Option<String> {
    h.get(&key("defaults"))?["run"]["shell"]
        .as_str()
        .map(String::from)
}

/**
 * Replaces the one shardable pytest step of each job with the wrapper. A job with two such steps
 * keeps both, since only one set of reports can be checked
 **/
pub fn shard_steps(jobs: &mut Hash, root_shell: Option<&str>, shards: ShardTests) {
    for job in jobs.values_mut() {
        let Yaml::Hash(job) = job else {
            continue;
        };
        let shell = default_shell(job).or(root_shell.map(String::from));
        let Some(Yaml::Array(steps)) = job.get_mut(&key("steps")) else {
            continue;
        };
        let found: Vec<(usize, String)> = steps
            .iter()
            .enumerate()
            .filter_map(|(i, s)| Some((i, pytest_line(s.as_hash()?, shell.as_deref())?)))
            .collect();

        let [(i, line)] = found.as_slice() else {
            continue;
        };
        let Yaml::Hash(step) = &mut steps[*i] else {
            continue;
        };
        let name = match step.get(&key("name")) {
            Some(Yaml::String(n)) => n.clone(),
            _ => format!("Run {line}"),
        };
        let count = match shards {
            ShardTests::Fixed(n) => format!("{n} shards"),
            _ => format!("up to {MAX_SHARDS} shards"),
        };
        step.insert(key("name"), key(&format!("{name} (shipcheck: {count})")));
        step.insert(key("shell"), key("bash"));
        step.insert(key("run"), Yaml::String(wrapper(line)));
    }
}

/// Whether any job of a workflow has a step shipcheck would shard
pub fn shardable(source: &str) -> bool {
    let Ok(mut root) = crate::workflow::load(source) else {
        return false;
    };
    let shell = default_shell(&root);
    let Ok(jobs) = crate::workflow::jobs_mut(&mut root) else {
        return false;
    };
    shard_steps(jobs, shell.as_deref(), ShardTests::On(true));
    crate::workflow::emit(root).is_ok_and(|w| w.contains(MARK))
}

/**
 * Runs the line once per shard in the background, prints each shard's output in order, then a
 * summary. The host writes the plugin and the shard count into `.git` only for a sharded attempt,
 * without them the line runs as it is. The plugin's digest is in the script, so a new plugin changes
 * the cache key, and the count is not, so the key does not depend on the machine
 **/
fn wrapper(line: &str) -> String {
    let plugin = &cache::hex(&Sha256::digest(PLUGIN))[..16];
    format!(
        r#"set +e
# shipcheck shard plugin {plugin}
plugin="$GITHUB_WORKSPACE/.git/{PLUGIN_DIR}"
n=$(cat "$plugin/shards" 2>/dev/null)
case "$n" in
  ''|*[!0-9]*)
    {line}
    exit $?
    ;;
esac
out="$GITHUB_WORKSPACE/.git/{REPORT_DIR}"
mkdir -p "$out"
for i in $(seq 0 $((n - 1))); do
  (
    export {MARK}"$i/$n" SHIPCHECK_SHARD_DIR="$out" SHIPCHECK_ADDOPTS="${{PYTEST_ADDOPTS:-}}"
    export PYTEST_ADDOPTS="-p shipcheck_shard ${{PYTEST_ADDOPTS:-}}"
    export PYTHONPATH="$plugin${{PYTHONPATH:+:$PYTHONPATH}}"
    {line}
  ) > "$out/$i.log" 2>&1 &
  pids[$i]=$!
done
code=0
for i in $(seq 0 $((n - 1))); do
  wait "${{pids[$i]}}"
  codes[$i]=$?
  if [ "$code" = 0 ]; then
    code=${{codes[$i]}}
  fi
done
for i in $(seq 0 $((n - 1))); do
  echo "::group::shard $i of $n, exit ${{codes[$i]}}"
  cat "$out/$i.log"
  echo "::endgroup::"
done
echo "shipcheck: $n shards"
for i in $(seq 0 $((n - 1))); do
  echo "shard $i: exit ${{codes[$i]}}, $(head -n 1 "$out/$i.summary" 2>/dev/null || echo 'no report')"
  tail -n +2 "$out/$i.summary" 2>/dev/null | sed 's/^/  failed /'
done
exit $code
"#
    )
}

/// Memory one shard of a suite with heavy imports can take
const SHARD_MEMORY: u64 = 3 << 29;

/**
 * How many shards a sharded attempt runs: the fixed number, or as many as the engine's CPUs and
 * memory allow at 1.5 GiB each, at most MAX_SHARDS. Memory runs out before CPUs on a Mac's default
 * Docker VM
 **/
pub fn count(shards: ShardTests, engine: Option<(u32, u64)>) -> u32 {
    let Some((cpus, memory)) = engine else {
        return match shards {
            ShardTests::Fixed(n) => n,
            _ => 1,
        };
    };

    match shards {
        ShardTests::Fixed(n) => n,
        _ => cpus
            .min(u32::try_from(memory / SHARD_MEMORY).unwrap_or(u32::MAX))
            .clamp(1, MAX_SHARDS),
    }
}

/// Puts the plugin and the shard count in the checkout's own `.git`, made for this attempt alone
pub fn write_plugin(run_dir: &Path, n: u32) -> Result<(), String> {
    let dir = run_dir.join(".git").join(PLUGIN_DIR);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    fs::write(dir.join("shards"), n.to_string()).map_err(|e| e.to_string())?;
    fs::write(dir.join("shipcheck_shard.py"), PLUGIN).map_err(|e| e.to_string())
}

/// Opens `name` inside `dir` without following a link at any step, and refuses a FIFO or device
fn open_at(dir: &fs::File, name: &str, directory: bool) -> std::io::Result<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let name = std::ffi::CString::new(name)?;
    let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

    if directory {
        flags |= libc::O_DIRECTORY;
    }

    // SAFETY: both are valid descriptors and a NUL-terminated name, and the new fd gets one owner
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };

    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let file = unsafe { fs::File::from_raw_fd(fd) };
    let kind = file.metadata()?.file_type();

    if kind.is_dir() == directory && (directory || kind.is_file()) {
        Ok(file)
    } else {
        Err(std::io::Error::other("not a regular file"))
    }
}

/**
 * Reports `0.json` to `<n - 1>.json`, None for each that cannot be read. Job code wrote them, so the
 * caller reads them only once no container of the job is left, no link is followed, and the total is
 * capped
 **/
pub fn read_reports(run_dir: &Path, n: u32) -> Vec<Option<Vec<u8>>> {
    let dir = fs::File::open(run_dir)
        .and_then(|d| open_at(&d, ".git", true))
        .and_then(|g| open_at(&g, REPORT_DIR, true));
    let mut left = MAX_REPORTS;

    (0..n.min(MAX_FIXED))
        .map(|i| {
            let file = open_at(dir.as_ref().ok()?, &format!("{i}.json"), false).ok()?;
            let mut body = Vec::new();
            file.take(left + 1).read_to_end(&mut body).ok()?;
            left = left.checked_sub(body.len() as u64)?;
            Some(body)
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    shard: u32,
    shards: u32,
    collected: u64,
    digest: String,
    exit: i32,
    items: BTreeMap<String, String>,
}

/// Why a sharded run was not accepted. It holds only numbers, so it is safe in a public check
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Rejected {
    JobFailed,
    Missing(u32),
    Malformed(u32),
    WrongShard(u32),
    DifferentCollection,
    NoTests,
    ShardFailed(u32, i32),
    TestFailed,
    Duplicate,
    Incomplete { ran: u64, collected: u64 },
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Rejected::JobFailed => write!(f, "the job failed"),
            Rejected::Missing(i) => write!(f, "shard {i} wrote no report"),
            Rejected::Malformed(i) => write!(f, "shard {i}'s report is malformed"),
            Rejected::WrongShard(i) => write!(f, "report {i} is not shard {i}'s"),
            Rejected::DifferentCollection => write!(f, "the shards collected different tests"),
            Rejected::NoTests => write!(f, "no tests collected"),
            Rejected::ShardFailed(i, code) => write!(f, "shard {i} exited {code}"),
            Rejected::TestFailed => write!(f, "a test failed"),
            Rejected::Duplicate => write!(f, "a test ran in two shards"),
            Rejected::Incomplete { ran, collected } => {
                write!(f, "the shards ran {ran} of {collected} collected tests")
            }
        }
    }
}

/**
 * Accepts a sharded run of `n` shards only if the shards together ran exactly what one serial run
 * collects, each item once, and every shard passed. Returns the items run
 **/
pub fn check(reports: &[Option<Vec<u8>>], n: u32) -> Result<u64, Rejected> {
    let mut parsed = Vec::new();

    for i in 0..n {
        let body = reports
            .get(i as usize)
            .and_then(Option::as_ref)
            .ok_or(Rejected::Missing(i))?;
        let r: Report = serde_json::from_slice(body).map_err(|_| Rejected::Malformed(i))?;

        if r.shard != i || r.shards != n {
            return Err(Rejected::WrongShard(i));
        }

        parsed.push(r);
    }

    let first = parsed.first().ok_or(Rejected::Missing(0))?;
    let (collected, digest) = (first.collected, first.digest.clone());

    if parsed
        .iter()
        .any(|r| r.collected != collected || r.digest != digest)
    {
        return Err(Rejected::DifferentCollection);
    }

    if collected == 0 {
        return Err(Rejected::NoTests);
    }

    if let Some(r) = parsed.iter().find(|r| r.exit != 0) {
        return Err(Rejected::ShardFailed(r.shard, r.exit));
    }

    let mut ran = BTreeSet::new();

    for r in &parsed {
        for (id, outcome) in &r.items {
            if !matches!(outcome.as_str(), "passed" | "skipped") {
                return Err(Rejected::TestFailed);
            }

            if !ran.insert(id.as_str()) {
                return Err(Rejected::Duplicate);
            }
        }
    }

    let joined: Vec<&str> = ran.iter().copied().collect();
    let ran_digest = cache::hex(&Sha256::digest(joined.join("\n")));

    if ran.len() as u64 != collected || ran_digest != digest {
        return Err(Rejected::Incomplete {
            ran: ran.len() as u64,
            collected,
        });
    }

    Ok(collected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(run: &str) -> Hash {
        let mut h = Hash::new();
        h.insert(key("run"), key(run));
        h
    }

    #[test]
    fn finds_only_plain_pytest_lines() {
        for line in [
            "pytest",
            "uv run pytest -v",
            "uv run --frozen --all-extras pytest tests/",
            "poetry run pytest -m 'not slow'",
            "pdm run pytest",
            "python -m pytest -q",
            "python3 -m pytest",
            "  pytest -k \"a and not b\"  \n",
        ] {
            assert_eq!(
                pytest_line(&step(line), None).as_deref(),
                Some(line.trim()),
                "{line}"
            );
        }

        for line in [
            "pytest; echo done",
            "pytest && coverage report",
            "pytest | tee out",
            "pytest > out",
            "pytest $ARGS",
            "pytest `ls`",
            "(pytest)",
            "pytest \\\n  -v",
            "pip install pytest\npytest",
            "pytest-watch",
            "npx pytest",
            "uv run --with pytest-xdist pytest",
            "pytest -n 4",
            "pytest -nauto",
            "uv run pytest --numprocesses=2",
            "pytest -p xdist",
            "uv pip install pytest",
            "tox",
            "${{ matrix.cmd }}",
        ] {
            assert_eq!(pytest_line(&step(line), None), None, "{line}");
        }

        let mut sh = step("pytest");
        sh.insert(key("shell"), key("sh"));
        assert_eq!(pytest_line(&sh, None), None);
        assert_eq!(pytest_line(&step("pytest"), Some("pwsh")), None);
        assert!(pytest_line(&step("pytest"), Some("bash")).is_some());
        let mut action = step("pytest");
        action.insert(key("uses"), key("x/y@v1"));
        assert_eq!(pytest_line(&action, None), None);
    }

    fn jobs(src: &str) -> Hash {
        let mut root = crate::workflow::load(src).unwrap();
        let shell = default_shell(&root);
        let jobs = crate::workflow::jobs_mut(&mut root).unwrap();
        shard_steps(jobs, shell.as_deref(), ShardTests::On(true));
        jobs.clone()
    }

    #[test]
    fn wraps_one_step_per_job() {
        let out = jobs(
            "on: push\njobs:\n  t:\n    steps:\n      - run: uv sync\n      - name: Tests\n        run: uv run pytest -v\n  two:\n    steps:\n      - run: pytest a\n      - run: pytest b\n",
        );
        let step = &out[&key("t")]["steps"][1];
        assert_eq!(
            step["name"].as_str(),
            Some("Tests (shipcheck: up to 6 shards)")
        );
        assert_eq!(step["shell"].as_str(), Some("bash"));
        let script = step["run"].as_str().unwrap();
        assert!(script.contains(MARK));
        assert!(script.contains("\n    uv run pytest -v\n    exit $?\n"));
        assert!(!script.contains("${{ "), "act would expand it");
        assert_eq!(out[&key("t")]["steps"][0]["run"].as_str(), Some("uv sync"));
        assert_eq!(
            out[&key("two")]["steps"][0]["run"].as_str(),
            Some("pytest a")
        );

        let pwsh = jobs(
            "on: push\ndefaults:\n  run:\n    shell: pwsh\njobs:\n  t:\n    steps:\n      - run: pytest\n",
        );
        assert_eq!(pwsh[&key("t")]["steps"][0]["run"].as_str(), Some("pytest"));
        assert!(shardable(
            "on: push\njobs:\n  t:\n    steps:\n      - run: pytest\n"
        ));
        assert!(!shardable(
            "on: push\njobs:\n  t:\n    steps:\n      - run: cargo test\n"
        ));
    }

    fn report(
        shard: u32,
        shards: u32,
        all: &[&str],
        items: &[(&str, &str)],
        exit: i32,
    ) -> Option<Vec<u8>> {
        let mut sorted: Vec<&str> = all.to_vec();
        sorted.sort();
        let digest = cache::hex(&Sha256::digest(sorted.join("\n")));
        let items: BTreeMap<_, _> = items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let body = serde_json::json!({
            "shard": shard, "shards": shards, "collected": all.len(),
            "digest": digest, "exit": exit, "items": items,
        });
        Some(body.to_string().into_bytes())
    }

    #[test]
    fn checks_the_reports_the_shards_wrote() {
        let all = ["a.py::x", "a.py::y", "b.py::z"];
        let a = report(
            0,
            2,
            &all,
            &[("a.py::x", "passed"), ("a.py::y", "skipped")],
            0,
        );
        let b = report(1, 2, &all, &[("b.py::z", "passed")], 0);
        assert_eq!(check(&[a.clone(), b.clone()], 2), Ok(3));

        // An empty shard of a suite with tests
        let empty = report(2, 3, &all, &[], 0);
        let a3 = report(
            0,
            3,
            &all,
            &[("a.py::x", "passed"), ("a.py::y", "passed")],
            0,
        );
        let b3 = report(1, 3, &all, &[("b.py::z", "passed")], 0);
        assert_eq!(check(&[a3, b3, empty], 3), Ok(3));

        let fail = |reports: &[Option<Vec<u8>>], n: u32, why: Rejected| {
            assert_eq!(check(reports, n), Err(why));
        };
        fail(std::slice::from_ref(&a), 2, Rejected::Missing(1));
        fail(&[a.clone(), None], 2, Rejected::Missing(1));
        fail(&[], 1, Rejected::Missing(0));
        let missing = report(1, 2, &all, &[], 0);
        fail(
            &[a.clone(), missing],
            2,
            Rejected::Incomplete {
                ran: 2,
                collected: 3,
            },
        );
        let twice = report(
            1,
            2,
            &all,
            &[("b.py::z", "passed"), ("a.py::x", "passed")],
            0,
        );
        fail(&[a.clone(), twice], 2, Rejected::Duplicate);
        let other = report(
            1,
            2,
            &["a.py::x", "a.py::y", "c.py::z"],
            &[("c.py::z", "passed")],
            0,
        );
        fail(&[a.clone(), other], 2, Rejected::DifferentCollection);
        let failed = report(1, 2, &all, &[("b.py::z", "failed")], 1);
        fail(&[a.clone(), failed], 2, Rejected::ShardFailed(1, 1));
        let odd = report(1, 2, &all, &[("b.py::z", "great")], 0);
        fail(&[a.clone(), odd], 2, Rejected::TestFailed);
        fail(
            &[a.clone(), Some(b"{\"shard\": 1".to_vec())],
            2,
            Rejected::Malformed(1),
        );
        fail(&[b.clone(), a.clone()], 2, Rejected::WrongShard(0));
        // A report cannot pick the shard count, however large
        let huge = report(0, 4_000_000_000, &all, &[], 0);
        fail(&[huge], 1, Rejected::WrongShard(0));
        let extra = String::from_utf8(b.clone().unwrap()).unwrap().replacen(
            '{',
            "{\"verdict\": \"pass\", ",
            1,
        );
        fail(
            &[a.clone(), Some(extra.into_bytes())],
            2,
            Rejected::Malformed(1),
        );
        fail(&[report(0, 1, &[], &[], 5)], 1, Rejected::NoTests);
    }

    #[test]
    fn rejections_never_carry_report_text() {
        let planted = "a.py::test_<script>";
        let r = report(0, 1, &[planted], &[(planted, "failed")], 0);
        let why = check(&[r], 1).unwrap_err().to_string();
        assert!(!why.contains("script"), "{why}");
    }

    #[test]
    fn reads_reports_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let reports = dir.path().join(".git").join(REPORT_DIR);
        fs::create_dir_all(&reports).unwrap();
        fs::write(reports.join("0.json"), "{}").unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "mine").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), reports.join("1.json")).unwrap();
        let fifo =
            std::ffi::CString::new(reports.join("2.json").to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        fs::create_dir(reports.join("3.json")).unwrap();
        assert_eq!(
            read_reports(dir.path(), 5),
            [Some(b"{}".to_vec()), None, None, None, None]
        );

        // A link anywhere on the way
        let linked = tempfile::tempdir().unwrap();
        fs::create_dir_all(linked.path().join(".git")).unwrap();
        std::os::unix::fs::symlink(&reports, linked.path().join(".git").join(REPORT_DIR)).unwrap();
        assert_eq!(read_reports(linked.path(), 1), [None]);
        let git_link = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path().join(".git"), git_link.path().join(".git")).unwrap();
        assert_eq!(read_reports(git_link.path(), 1), [None]);
    }

    #[test]
    fn caps_the_shard_count() {
        let gib = 1 << 30;
        assert_eq!(count(ShardTests::On(true), Some((14, 32 * gib))), 6);
        // Docker Desktop's 8 GB VM reports 7.75 GiB, which holds 5 shards of 1.5 GiB
        assert_eq!(count(ShardTests::On(true), Some((14, 8_318_976_000))), 5);
        assert_eq!(count(ShardTests::On(true), Some((14, 6 * gib))), 4);
        assert_eq!(count(ShardTests::On(true), Some((2, 32 * gib))), 2);
        assert_eq!(count(ShardTests::On(true), Some((8, gib))), 1);
        assert_eq!(count(ShardTests::On(true), None), 1);
        assert_eq!(count(ShardTests::Fixed(10), Some((2, gib))), 10);
        assert_eq!(
            read_reports(Path::new("/nonexistent"), 1000).len(),
            MAX_FIXED as usize
        );
    }
}
