use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::{Value, json};

const API: &str = "https://api.github.com";

pub struct App {
    id: String,
    key: EncodingKey,
}

#[derive(Serialize)]
struct Claims<'a> {
    iat: u64,
    exp: u64,
    iss: &'a str,
}

impl App {
    /// The App ID from `SHIPCHECK_APP_ID`, the key from `SHIPCHECK_APP_KEY_FILE` or `app.pem` in shipcheck's home (`secret::read`)
    pub fn from_env() -> Result<Self, String> {
        let id = std::env::var("SHIPCHECK_APP_ID")
            .map_err(|_| "SHIPCHECK_APP_ID is not set".to_string())?;
        let pem = match std::env::var_os("SHIPCHECK_APP_KEY_FILE").map(PathBuf::from) {
            Some(path) => std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?,
            None => {
                let path = crate::run::home().join("app.pem");
                crate::secret::read(&path)?
                    .ok_or_else(|| format!("no App key: put it at {}", path.display()))?
            }
        };
        let key = EncodingKey::from_rsa_pem(&pem)
            .map_err(|e| format!("the App key is not an RSA private key: {e}"))?;
        Ok(App { id, key })
    }

    fn jwt(&self) -> Result<String, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Starts a minute in the past for clock drift, and GitHub refuses one that lasts ten minutes or more
        let claims = Claims {
            iat: now - 60,
            exp: now + 540,
            iss: &self.id,
        };
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key)
            .map_err(|e| format!("signing the App token: {e}"))
    }

    /// A token for the App's installation on `repo`, as `owner/name`
    pub fn installation_token(&self, repo: &str) -> Result<String, String> {
        // Only what the daemon calls, on this repository, not all the installation can do
        self.token(repo, json!({"checks": "write", "pull_requests": "read"}))
    }

    /// A token that writes the PR comment, which GitHub refuses until the App has Pull requests write
    pub fn comment_token(&self, repo: &str) -> Result<String, String> {
        self.token(repo, json!({"pull_requests": "write"}))
    }

    fn token(&self, repo: &str, permissions: Value) -> Result<String, String> {
        let jwt = self.jwt()?;
        let installation = get(&format!("{API}/repos/{repo}/installation"), &jwt).map_err(|e| {
            if e.contains("404") {
                format!("the shipcheck App is not installed on {repo}")
            } else {
                e
            }
        })?;
        let id = installation["id"]
            .as_u64()
            .ok_or("GitHub returned no installation id")?;
        let name = repo.rsplit('/').next().unwrap_or(repo);
        let token = post(
            &format!("{API}/app/installations/{id}/access_tokens"),
            &jwt,
            &json!({"repositories": [name], "permissions": permissions}),
        )?;
        token["token"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "GitHub returned no installation token".to_string())
    }
}

impl App {
    pub fn id(&self) -> &str {
        &self.id
    }
}

pub struct OpenPr {
    pub number: u64,
    pub sha: String,
    pub base: String,
    /// `owner/name` of the repository the head commit lives in, another one for a fork
    pub head_repo: String,
    /// The head branch, which the daemon finds the head's push on
    pub branch: String,
    /// The login that opened the PR, for the run records only, never for whether a head runs
    pub author: String,
}

/// One PR, open or not, to see whether one that left the open list was merged
pub fn pull(token: &str, repo: &str, number: u64) -> Result<Value, String> {
    get(&format!("{API}/repos/{repo}/pulls/{number}"), token)
}

/// The open PRs whose head is `sha` in `repo` itself, never a fork, into `base`
pub fn prs_for(token: &str, repo: &str, sha: &str, base: &str) -> Result<Vec<u64>, String> {
    let prs = get(&format!("{API}/repos/{repo}/commits/{sha}/pulls"), token)?;
    Ok(prs
        .as_array()
        .into_iter()
        .flatten()
        .filter(|pr| {
            pr["state"] == "open"
                && pr["head"]["sha"] == sha
                && pr["head"]["repo"]["full_name"] == repo
                && pr["base"]["ref"] == base
        })
        .filter_map(|pr| pr["number"].as_u64())
        .collect())
}

/// This App's comment on a PR that holds `marker`, as its id and body
// ponytail: first 100 comments, follow the Link header if a PR has more
pub fn find_comment(
    token: &str,
    repo: &str,
    pr: u64,
    app_id: &str,
    marker: &str,
) -> Result<Option<(u64, String)>, String> {
    let comments = get(
        &format!("{API}/repos/{repo}/issues/{pr}/comments?per_page=100"),
        token,
    )?;
    Ok(comments.as_array().into_iter().flatten().find_map(|c| {
        let body = c["body"].as_str().filter(|b| b.starts_with(marker))?;

        // Only a comment this App wrote, never one a person or another App wrote with the marker
        let by = c["performed_via_github_app"]["id"].as_u64();

        if by.map(|id| id.to_string()).as_deref() != Some(app_id) {
            return None;
        }

        Some((c["id"].as_u64()?, body.to_string()))
    }))
}

/// Creates the PR comment, or changes the one with `id`, and returns its page on GitHub
pub fn put_comment(
    token: &str,
    repo: &str,
    pr: u64,
    id: Option<u64>,
    body: &str,
) -> Result<String, String> {
    let res = match id {
        Some(id) => patch(
            &format!("{API}/repos/{repo}/issues/comments/{id}"),
            token,
            &json!({"body": body}),
        )?,
        None => post(
            &format!("{API}/repos/{repo}/issues/{pr}/comments"),
            token,
            &json!({"body": body}),
        )?,
    };
    Ok(res["html_url"].as_str().unwrap_or_default().to_string())
}

// ponytail: first 100 open PRs only, follow the Link header if a repository has more
pub fn open_prs(token: &str, repo: &str) -> Result<Vec<OpenPr>, String> {
    let prs = get(
        &format!("{API}/repos/{repo}/pulls?state=open&per_page=100"),
        token,
    )?;
    Ok(parse_prs(&prs))
}

/// Open PRs from GitHub's list, which the hosted service passes on in the same shape
pub fn parse_prs(prs: &Value) -> Vec<OpenPr> {
    prs.as_array()
        .map(|list| {
            list.iter()
                .filter_map(|pr| {
                    Some(OpenPr {
                        number: pr["number"].as_u64()?,
                        sha: pr["head"]["sha"].as_str()?.to_string(),
                        base: pr["base"]["ref"].as_str()?.to_string(),
                        head_repo: pr["head"]["repo"]["full_name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        branch: pr["head"]["ref"].as_str().unwrap_or_default().to_string(),
                        author: pr["user"]["login"].as_str().unwrap_or_default().to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Check {
    pub id: u64,
    pub status: String,
    /// As GitHub writes it, `2026-10-23T12:00:00Z`, empty when unknown
    pub started_at: String,
}

pub type Checks = BTreeMap<String, Check>;

/// This App's latest check run per name on a commit, in one call
// ponytail: first 100 check runs, follow the Link header if a commit has more jobs
pub fn find_checks(token: &str, repo: &str, sha: &str, app_id: &str) -> Result<Checks, String> {
    let runs = get(
        &format!(
            "{API}/repos/{repo}/commits/{sha}/check-runs?app_id={app_id}&filter=latest&per_page=100"
        ),
        token,
    )?;
    Ok(parse_checks(&runs))
}

/// The latest run per name from GitHub's list, which the hosted service passes on in the same shape
pub fn parse_checks(runs: &Value) -> Checks {
    let mut found = Checks::new();

    for run in runs["check_runs"].as_array().into_iter().flatten() {
        if let (Some(name), Some(id), Some(status)) = (
            run["name"].as_str(),
            run["id"].as_u64(),
            run["status"].as_str(),
        ) {
            // `filter=latest` already gives one run per name
            found.entry(name.to_string()).or_insert(Check {
                id,
                status: status.to_string(),
                started_at: run["started_at"].as_str().unwrap_or_default().to_string(),
            });
        }
    }

    found
}

/**
 * Who pushed `sha` to `branch`, from the repository's events, which need only Metadata read. An
 * "Update branch" or a rebase on GitHub is a push by the person who clicked it. GitHub lists
 * events late, from 30 seconds to hours, so None can mean not yet
 **/
// ponytail: first 100 events, the activity API is exact and immediate but needs Contents read
pub fn find_pusher(
    token: &str,
    repo: &str,
    branch: &str,
    sha: &str,
) -> Result<Option<String>, String> {
    let events = get(&format!("{API}/repos/{repo}/events?per_page=100"), token)?;
    Ok(pusher(&events, branch, sha))
}

/// The login of the push event that moved `branch` to `sha`
pub fn pusher(events: &Value, branch: &str, sha: &str) -> Option<String> {
    let full = format!("refs/heads/{branch}");
    events.as_array()?.iter().find_map(|e| {
        let push = e["type"] == "PushEvent"
            && e["payload"]["ref"] == full.as_str()
            && e["payload"]["head"] == sha;
        push.then(|| e["actor"]["login"].as_str().map(String::from))?
    })
}

/// Creates a check run from a body built in `check`, and returns its id and page on GitHub
pub fn post_check(token: &str, repo: &str, body: &Value) -> Result<(u64, String), String> {
    let run = post(&format!("{API}/repos/{repo}/check-runs"), token, body)?;
    let id = run["id"]
        .as_u64()
        .ok_or("GitHub returned no check run id")?;
    Ok((id, run["html_url"].as_str().unwrap_or_default().to_string()))
}

/// Changes a check run, such as `in_progress` to `completed`, and returns its page on GitHub
pub fn update_check(token: &str, repo: &str, id: u64, body: &Value) -> Result<String, String> {
    let mut body = body.clone();

    // The commit of a check run is fixed when it is created
    if let Some(fields) = body.as_object_mut() {
        fields.remove("head_sha");
    }

    let run = patch(&format!("{API}/repos/{repo}/check-runs/{id}"), token, &body)?;
    Ok(run["html_url"].as_str().unwrap_or_default().to_string())
}

fn patch(url: &str, auth: &str, body: &Value) -> Result<Value, String> {
    ureq::patch(url)
        .header("Authorization", format!("Bearer {auth}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "shipcheck")
        .send_json(body)
        .map_err(|e| format!("PATCH {url}: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("PATCH {url}: {e}"))
}

fn get(url: &str, auth: &str) -> Result<Value, String> {
    ureq::get(url)
        .header("Authorization", format!("Bearer {auth}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "shipcheck")
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("GET {url}: {e}"))
}

fn post(url: &str, auth: &str, body: &Value) -> Result<Value, String> {
    ureq::post(url)
        .header("Authorization", format!("Bearer {auth}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "shipcheck")
        .send_json(body)
        .map_err(|e| format!("POST {url}: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("POST {url}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_who_pushed_the_head() {
        let events = json!([
            {"type": "PushEvent", "actor": {"login": "bob"}, "payload": {"ref": "refs/heads/fix", "head": "b"}},
            {"type": "PullRequestEvent", "actor": {"login": "eve"}, "payload": {}},
            {"type": "PushEvent", "actor": {"login": "ann"}, "payload": {"ref": "refs/heads/fix", "head": "a"}},
            {"type": "PushEvent", "actor": {"login": "eve"}, "payload": {"ref": "refs/heads/other", "head": "c"}},
        ]);
        assert_eq!(pusher(&events, "fix", "a").as_deref(), Some("ann"));
        assert_eq!(pusher(&events, "fix", "b").as_deref(), Some("bob"));
        assert_eq!(
            pusher(&events, "fix", "c"),
            None,
            "a push of the same commit to another branch"
        );
        assert_eq!(pusher(&events, "fix", "d"), None, "not listed yet");
    }
}
