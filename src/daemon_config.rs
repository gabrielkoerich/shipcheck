use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::{self, MovedHome};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    #[serde(default = "ten")]
    pub interval_seconds: u64,
    #[serde(default = "yes")]
    pub run_misses: bool,
    /// Whose PRs this daemon runs, for an App key, where no sign-in names the user
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    #[serde(default)]
    pub repos: BTreeMap<String, RepoEntry>,
    /// A person's `--home-ok` on the daemon, passed on to the runs it starts
    #[serde(skip)]
    pub home_ok: MovedHome,
    /// How shipcheck's own fetches reach a GitHub remote, `https` to skip an SSH key that asks for a touch
    #[serde(default, skip_serializing_if = "Fetch::is_origin")]
    pub fetch: Fetch,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Fetch {
    /// The remote's URL as the person set it up
    #[default]
    Origin,
    /// HTTPS with git's credential helper, such as `gh auth git-credential`
    Https,
}

impl Fetch {
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's skip_serializing_if passes a reference"
    )]
    fn is_origin(&self) -> bool {
        *self == Fetch::Origin
    }

    /// The `-c` options that send a fetch of a GitHub SSH URL over HTTPS. Remotes, pushes and the
    /// person's own git stay as they are
    pub(crate) fn options(self) -> Vec<String> {
        if self == Fetch::Origin {
            return Vec::new();
        }

        ["git@github.com:", "ssh://git@github.com/"]
            .iter()
            .flat_map(|from| {
                [
                    "-c".to_string(),
                    format!("url.https://github.com/.insteadOf={from}"),
                ]
            })
            .collect()
    }
}

/// The options for every fetch and `ls-remote` shipcheck runs, from `fetch` in daemon.toml
pub fn fetch_options() -> Vec<String> {
    DaemonConfig::load().map_or_default(|c| c.fetch).options()
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepoEntry {
    pub path: PathBuf,
    #[serde(default = "origin_main")]
    pub base: String,
}

fn ten() -> u64 {
    10
}

fn yes() -> bool {
    true
}

pub(crate) fn origin_main() -> String {
    "origin/main".into()
}

impl DaemonConfig {
    pub fn path() -> PathBuf {
        paths::home().join("daemon.toml")
    }

    pub fn load() -> Result<Self, String> {
        Self::load_from(&Self::path())
    }

    /// No file is a daemon with no repositories yet
    pub(crate) fn load_from(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Adds a repository or changes its entry, and says whether the file changed
    pub fn add(repo: &str, path: &Path, base: &str) -> Result<bool, String> {
        Self::add_to(&Self::path(), repo, path, base)
    }

    pub(crate) fn add_to(file: &Path, repo: &str, path: &Path, base: &str) -> Result<bool, String> {
        let mut config = Self::load_from(file)?;
        let entry = RepoEntry {
            path: path.to_path_buf(),
            base: base.to_string(),
        };

        if config.repos.get(repo) == Some(&entry) {
            return Ok(false);
        }

        config.repos.insert(repo.to_string(), entry);
        let text = toml::to_string(&config).map_err(|e| e.to_string())?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }

        std::fs::write(file, text).map_err(|e| format!("{}: {e}", file.display()))?;
        Ok(true)
    }
}
