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
    /// The App ID from `SHIPCHECK_APP_ID`, the key from `SHIPCHECK_APP_KEY_FILE` or `app.pem` in shipcheck's home
    pub fn from_env() -> Result<Self, String> {
        let id = std::env::var("SHIPCHECK_APP_ID")
            .map_err(|_| "SHIPCHECK_APP_ID is not set".to_string())?;
        let path = std::env::var_os("SHIPCHECK_APP_KEY_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::run::home().join("app.pem"));
        let pem = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let key = EncodingKey::from_rsa_pem(&pem)
            .map_err(|e| format!("{} is not an RSA private key: {e}", path.display()))?;
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
        // Only what the daemon calls, on this repository, not all the installation can do
        let token = post(
            &format!("{API}/app/installations/{id}/access_tokens"),
            &jwt,
            &json!({
                "repositories": [name],
                "permissions": {"checks": "write", "pull_requests": "read"},
            }),
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
}

// ponytail: first 100 open PRs only, follow the Link header if a repository has more
pub fn open_prs(token: &str, repo: &str) -> Result<Vec<OpenPr>, String> {
    let prs = get(
        &format!("{API}/repos/{repo}/pulls?state=open&per_page=100"),
        token,
    )?;
    Ok(prs
        .as_array()
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
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Whether the commit already has a `shipcheck` check from this App
/// This App's `shipcheck` check run on a commit, as its id and status, if there is one
pub fn find_check(
    token: &str,
    repo: &str,
    sha: &str,
    app_id: &str,
) -> Result<Option<(u64, String)>, String> {
    let runs = get(
        &format!(
            "{API}/repos/{repo}/commits/{sha}/check-runs?check_name=shipcheck&app_id={app_id}"
        ),
        token,
    )?;
    let first = &runs["check_runs"][0];

    Ok(first["id"]
        .as_u64()
        .zip(first["status"].as_str().map(String::from)))
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

    let url = format!("{API}/repos/{repo}/check-runs/{id}");
    let run: Value = ureq::patch(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "shipcheck")
        .send_json(&body)
        .map_err(|e| format!("PATCH {url}: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("PATCH {url}: {e}"))?;
    Ok(run["html_url"].as_str().unwrap_or_default().to_string())
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
