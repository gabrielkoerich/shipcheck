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
    Ok(same_repo_prs(&prs, repo, sha, base))
}

fn same_repo_prs(prs: &Value, repo: &str, sha: &str, base: &str) -> Vec<u64> {
    prs.as_array()
        .into_iter()
        .flatten()
        .filter(|pr| {
            pr["state"] == "open"
                && pr["head"]["sha"] == sha
                && pr["head"]["repo"]["full_name"]
                    .as_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case(repo))
                && pr["base"]["ref"] == base
        })
        .filter_map(|pr| pr["number"].as_u64())
        .collect()
}

/// This App's comment on a PR that holds `marker`, as its id and body
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
 * Who pushed head `sha` of PR `pr`, and which source said so, None while none does. Exact sources
 * first: the PR timeline lists a force-push, such as a rebase "Update branch", at once. A head a
 * `trusted` login signed counts as theirs (`signed`). A merge "Update branch" is a commit GitHub
 * signs with the clicker as author. A PR merged into the branch, such as a stacked PR, names who
 * merged it. The repository's events cover a plain push, and GitHub lists them from 30 seconds to
 * hours late
 **/
pub fn find_pusher(
    token: &str,
    repo: &str,
    pr: u64,
    branch: &str,
    sha: &str,
    trusted: &[String],
) -> Result<Option<(String, &'static str)>, String> {
    let timeline = pages(
        &format!("{API}/repos/{repo}/issues/{pr}/timeline"),
        token,
        10,
    )?;

    if let Some(login) = forced(&timeline, sha) {
        return Ok(Some((login, "timeline")));
    }

    // GitHub lists at most 250 commits of a PR
    let commits = pages(&format!("{API}/repos/{repo}/pulls/{pr}/commits"), token, 3)?;

    if let Some(login) = signed(&commits, sha, trusted) {
        return Ok(Some((login, "signature")));
    }

    if let Some(login) = web_merge(&commits, sha) {
        return Ok(Some((login, "merge-commit")));
    }

    let closed = get(
        &format!(
            "{API}/repos/{repo}/pulls?state=closed&base={branch}&sort=updated&direction=desc&per_page=100"
        ),
        token,
    )?;

    if let Some(number) = merged_into(&closed, branch, sha) {
        let merged = get(&format!("{API}/repos/{repo}/pulls/{number}"), token)?;

        if let Some(login) = merged["merged_by"]["login"].as_str() {
            return Ok(Some((login.to_string(), "merged-pr")));
        }
    }

    let events = get(&format!("{API}/repos/{repo}/events?per_page=100"), token)?;
    Ok(pusher(&events, branch, sha).map(|login| (login, "events")))
}

/// Every item of a GitHub list, 100 a page, up to `cap` pages
fn pages(url: &str, token: &str, cap: u32) -> Result<Vec<Value>, String> {
    let mut all = Vec::new();

    for page in 1..=cap {
        let list = get(&format!("{url}?per_page=100&page={page}"), token)?;
        let list = list.as_array().cloned().unwrap_or_default();
        let last = list.len() < 100;
        all.extend(list);

        if last {
            break;
        }
    }

    Ok(all)
}

/// The actor of the latest force-push of the PR's head to `sha`, from its timeline
pub fn forced(timeline: &[Value], sha: &str) -> Option<String> {
    timeline.iter().rev().find_map(|e| {
        let push = e["event"] == "head_ref_force_pushed" && e["commit_id"] == sha;
        push.then(|| e["actor"]["login"].as_str().map(String::from))?
    })
}

/**
 * The `trusted` login whose verified signature is on commit `sha`. GitHub verifies a signature only
 * with a key on the committer's account, and the commit names its tree, so the tree is what that
 * user signed, whoever pushed it. Never web-flow's signature: a rebase on GitHub keeps the author
 * the commit had, and anyone can set that
 **/
pub fn signed(commits: &[Value], sha: &str, trusted: &[String]) -> Option<String> {
    let c = commits.iter().find(|c| c["sha"] == sha)?;
    let login = c["committer"]["login"].as_str()?;
    let ok = c["commit"]["verification"]["verified"] == true
        && login != "web-flow"
        && trusted.iter().any(|t| t.eq_ignore_ascii_case(login));
    ok.then(|| login.to_string())
}

/**
 * The login that clicked a merge "Update branch" whose result is `sha`: GitHub commits it as
 * web-flow, signs it, and names the clicker as author. Its first parent is the PR's previous head
 * and its second comes from the base
 **/
pub fn web_merge(commits: &[Value], sha: &str) -> Option<String> {
    let c = commits.iter().find(|c| c["sha"] == sha)?;
    let ours = |p: &Value| commits.iter().any(|c| c["sha"] == p["sha"]);
    let parents = c["parents"].as_array()?;
    let by_github = c["committer"]["login"] == "web-flow"
        && c["commit"]["committer"]["email"] == "noreply@github.com"
        && c["commit"]["verification"]["verified"] == true;

    if !by_github || parents.len() != 2 || !ours(&parents[0]) || ours(&parents[1]) {
        return None;
    }

    c["author"]["login"].as_str().map(String::from)
}

/// The number of the PR into `branch` whose merge made `sha`
pub fn merged_into(pulls: &Value, branch: &str, sha: &str) -> Option<u64> {
    pulls.as_array()?.iter().find_map(|p| {
        let made =
            p["merge_commit_sha"] == sha && p["base"]["ref"] == branch && !p["merged_at"].is_null();
        made.then(|| p["number"].as_u64())?
    })
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

    #[test]
    fn finds_a_force_push_on_the_timeline() {
        let timeline = vec![
            json!({"event": "head_ref_force_pushed", "actor": {"login": "eve"}, "commit_id": "h"}),
            json!({"event": "committed", "sha": "h"}),
            json!({"event": "head_ref_force_pushed", "actor": {"login": "ann"}, "commit_id": "h"}),
            json!({"event": "head_ref_force_pushed", "actor": {"login": "bob"}, "commit_id": "g"}),
        ];
        assert_eq!(
            forced(&timeline, "h").as_deref(),
            Some("ann"),
            "the latest push of h"
        );
        assert_eq!(
            forced(&timeline, "x"),
            None,
            "a plain push is only a commit"
        );
    }

    #[test]
    fn finds_the_pr_whose_merge_made_the_head() {
        let sha = "7f32acaf1e3db3e0192f492f9c7d3cf2ba0f432f";
        let pulls = serde_json::json!([
            {"number": 58, "merge_commit_sha": sha, "base": {"ref": "cloud-only"}, "merged_at": null},
            {"number": 59, "merge_commit_sha": sha, "base": {"ref": "main"}, "merged_at": "2026-10-24T11:08:29Z"},
            {"number": 57, "merge_commit_sha": sha, "base": {"ref": "cloud-only"}, "merged_at": "2026-10-24T11:08:29Z"}
        ]);
        assert_eq!(
            merged_into(&pulls, "cloud-only", sha),
            Some(57),
            "a closed PR that never merged, or one into another branch, names no one"
        );
        assert_eq!(merged_into(&pulls, "cloud-only", &"0".repeat(40)), None);
    }

    #[test]
    fn finds_who_clicked_a_merge_update_branch() {
        let merge = |committer: &str, email: &str, verified: bool, second: &str| {
            json!({
                "sha": "m",
                "author": {"login": "ann"},
                "committer": {"login": committer},
                "commit": {"committer": {"email": email}, "verification": {"verified": verified}},
                "parents": [{"sha": "a"}, {"sha": second}],
            })
        };
        let pr = |m: Value| vec![json!({"sha": "a", "parents": [{"sha": "base"}]}), m];
        let gh = "noreply@github.com";
        assert_eq!(
            web_merge(&pr(merge("web-flow", gh, true, "main")), "m").as_deref(),
            Some("ann")
        );
        assert_eq!(
            web_merge(&pr(merge("ann", gh, true, "main")), "m"),
            None,
            "a merge made on a laptop"
        );
        assert_eq!(
            web_merge(&pr(merge("web-flow", "ann@x", true, "main")), "m"),
            None
        );
        assert_eq!(
            web_merge(&pr(merge("web-flow", gh, false, "main")), "m"),
            None,
            "unsigned"
        );
        assert_eq!(
            web_merge(&pr(merge("web-flow", gh, true, "a")), "m"),
            None,
            "both parents from the PR"
        );
        assert_eq!(
            web_merge(&pr(merge("web-flow", gh, true, "main")), "a"),
            None,
            "not a merge"
        );
    }

    #[test]
    fn a_head_a_trusted_login_signed_is_theirs() {
        let commit = |sha: &str, author: &str, committer: &str, verified: bool| {
            json!({
                "sha": sha,
                "author": {"login": author},
                "committer": {"login": committer},
                "commit": {"verification": {"verified": verified, "reason": if verified { "valid" } else { "unsigned" }}},
                "parents": [{"sha": "base"}],
            })
        };
        let trusted = ["Ann".to_string(), "bob".to_string()];
        let commits = vec![
            commit("h", "eve", "ann", true),
            commit("u", "ann", "ann", false),
            commit("e", "ann", "eve", true),
            commit("w", "ann", "web-flow", true),
        ];
        assert_eq!(
            signed(&commits, "h", &trusted).as_deref(),
            Some("ann"),
            "the committer signed it, whoever the author email names"
        );
        assert_eq!(signed(&commits, "u", &trusted), None, "unsigned");
        assert_eq!(
            signed(&commits, "e", &trusted),
            None,
            "signed by a login main does not list"
        );
        assert_eq!(
            signed(&commits, "w", &trusted),
            None,
            "GitHub signed it, and a rebase keeps any author"
        );
        assert_eq!(signed(&commits, "h", &[]), None);
        assert_eq!(signed(&commits, "x", &trusted), None, "not in the PR");
    }

    #[test]
    fn a_same_repo_pr_matches_its_repository_in_any_case() {
        let prs = json!([
            {"number": 5, "state": "open", "head": {"sha": "a", "repo": {"full_name": "acme/app"}}, "base": {"ref": "main"}},
            {"number": 6, "state": "open", "head": {"sha": "a", "repo": {"full_name": "eve/app"}}, "base": {"ref": "main"}},
        ]);
        assert_eq!(same_repo_prs(&prs, "acme/app", "a", "main"), vec![5]);
        assert_eq!(
            same_repo_prs(&prs, "Acme/App", "a", "main"),
            vec![5],
            "an origin URL of Acme/App is GitHub's acme/app"
        );
    }
}
