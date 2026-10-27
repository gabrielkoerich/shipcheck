use std::fs;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const DEFAULT_API: &str = "https://api.shipcheck.run";

/// The hosted service, `SHIPCHECK_API` or `https://api.shipcheck.run`
pub fn api() -> String {
    std::env::var("SHIPCHECK_API")
        .unwrap_or_else(|_| DEFAULT_API.into())
        .trim_end_matches('/')
        .to_string()
}

fn token_path() -> PathBuf {
    crate::run::home().join("token")
}

fn login_path() -> PathBuf {
    crate::run::home().join("login")
}

/// The GitHub login `shipcheck login` signed in, None for a sign-in from before it was kept
pub fn signed_in_login() -> Option<String> {
    fs::read_to_string(login_path())
        .ok()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

/// How `shipcheck post` reaches GitHub
#[derive(Debug, PartialEq)]
pub enum Poster {
    /// The App's key on this machine, as the daemon uses it
    Key,
    /// The hosted service, with this machine token
    Service(String),
    None,
}

pub fn poster() -> Poster {
    let has_key = has_key();
    // Read only when it is used, since on a Mac a read can ask the user
    let file_token = (!env_token_set() && !has_key).then(login_token).flatten();
    choose(std::env::var("SHIPCHECK_TOKEN").ok(), has_key, file_token)
}

/// Whether `poster` finds a way to post, without reading a secret, so a git hook never waits on a
/// keychain prompt
pub fn can_post() -> bool {
    env_token_set() || has_key() || crate::secret::exists(&token_path())
}

// An own App needs its id as well as its key, a key file alone leaves the login in charge
fn has_key() -> bool {
    std::env::var_os("SHIPCHECK_APP_ID").is_some()
        && (std::env::var_os("SHIPCHECK_APP_KEY_FILE").is_some()
            || crate::secret::exists(&crate::run::home().join("app.pem")))
}

/// The `shipcheck login` token, from the keychain on macOS (`secret::read`)
fn login_token() -> Option<String> {
    let token = crate::secret::read(&token_path()).ok()??;
    Some(String::from_utf8_lossy(&token).into_owned())
}

/// The machine token: `SHIPCHECK_TOKEN`, else the `shipcheck login` token
pub fn machine_token() -> Option<String> {
    std::env::var("SHIPCHECK_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .or_else(login_token)
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn env_token_set() -> bool {
    std::env::var("SHIPCHECK_TOKEN").is_ok_and(|t| !t.trim().is_empty())
}

/// Who posts the checks, named without the secret, or None when this machine cannot post
pub fn identity() -> Option<&'static str> {
    match poster() {
        Poster::Key => Some("this machine's own App key"),
        Poster::Service(_) if env_token_set() => Some("the SHIPCHECK_TOKEN machine token"),
        Poster::Service(_) => Some("the `shipcheck login` token"),
        Poster::None => None,
    }
}

/// What `install` and `init` do about signing in
#[derive(Debug, PartialEq)]
pub enum SignIn {
    Already,
    Ask,
    Skip,
}

/// The device flow needs a person, so only a terminal gets it
pub fn sign_in(can_post: bool, terminal: bool) -> SignIn {
    match (can_post, terminal) {
        (true, _) => SignIn::Already,
        (false, true) => SignIn::Ask,
        (false, false) => SignIn::Skip,
    }
}

fn choose(env_token: Option<String>, has_key: bool, file_token: Option<String>) -> Poster {
    let env_token = env_token.filter(|t| !t.trim().is_empty());
    let file_token = file_token.filter(|t| !t.trim().is_empty());

    match (env_token, has_key, file_token) {
        (Some(t), _, _) => Poster::Service(t.trim().to_string()),
        (None, true, _) => Poster::Key,
        (None, false, Some(t)) => Poster::Service(t.trim().to_string()),
        _ => Poster::None,
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

fn send(
    what: &str,
    res: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<(u16, Value), String> {
    let mut res = res.map_err(|e| format!("{what}: {e}"))?;
    let body = res.body_mut().read_json().unwrap_or(Value::Null);
    Ok((res.status().as_u16(), body))
}

fn ok(what: &str, (status, body): (u16, Value)) -> Result<Value, String> {
    if (200..300).contains(&status) {
        return Ok(body);
    }

    Err(match body["error"].as_str() {
        Some(e) => format!("{what}: {e}"),
        None => format!("{what}: HTTP {status}"),
    })
}

/// Signs this machine in, keeping the machine token as `<home>/token` (`secret::write`). Returns the GitHub login
pub fn login() -> Result<String, String> {
    let machine = std::process::Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let res = grant(&machine)?;
    let token = res["token"]
        .as_str()
        .ok_or("the service returned no token")?;
    crate::secret::write(&token_path(), token.as_bytes())?;
    let login = res["login"].as_str().unwrap_or_default().to_string();
    // The login names the user and is no secret, so it stays a file
    crate::secret::write_file(&login_path(), login.as_bytes())?;
    Ok(login)
}

/// A machine token for another machine, from a new GitHub approval, so a stolen token mints none
pub fn new_token(machine: &str) -> Result<String, String> {
    grant(machine)?["token"]
        .as_str()
        .map(String::from)
        .ok_or("the service returned no token".into())
}

/**
 * GitHub's device flow on the App's client id gives a user token, and the service reads who it
 * belongs to and returns a machine token of its own. The code goes to stderr, so `shipcheck token`
 * keeps stdout for the token
 **/
fn grant(machine: &str) -> Result<Value, String> {
    let agent = agent();
    let api = api();
    let config = ok(
        "GET /v1/login",
        send("GET /v1/login", agent.get(format!("{api}/v1/login")).call())?,
    )?;
    let client_id = config["client_id"]
        .as_str()
        .ok_or("the service returned no client id")?;
    let code = ok(
        "GitHub device code",
        send(
            "GitHub device code",
            agent
                .post("https://github.com/login/device/code")
                .header("Accept", "application/json")
                .send_form([("client_id", client_id)]),
        )?,
    )?;
    let device_code = code["device_code"]
        .as_str()
        .ok_or("GitHub returned no device code")?;
    let url = code["verification_uri"]
        .as_str()
        .unwrap_or("https://github.com/login/device")
        .to_string();
    let user_code = code["user_code"].as_str().unwrap_or_default();

    if std::io::stdin().is_terminal() {
        eprintln!("your one-time code: {user_code}");
        eprintln!("press Enter to open {url} in your browser");
        // Polling goes on meanwhile, so approving from another device works without Enter
        std::thread::spawn(move || {
            if std::io::stdin().read_line(&mut String::new()).is_ok() {
                let opener = if cfg!(target_os = "macos") {
                    "open"
                } else {
                    "xdg-open"
                };
                let _ = std::process::Command::new(opener)
                    .arg(&url)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        });
    } else {
        eprintln!("open {url} and enter {user_code}");
    }

    let deadline = Instant::now() + Duration::from_secs(code["expires_in"].as_u64().unwrap_or(900));
    let mut interval = code["interval"].as_u64().unwrap_or(5);
    let user_token = loop {
        if Instant::now() > deadline {
            return Err("the code expired, run `shipcheck login` again".into());
        }

        std::thread::sleep(Duration::from_secs(interval));
        let res = ok(
            "GitHub access token",
            send(
                "GitHub access token",
                agent
                    .post("https://github.com/login/oauth/access_token")
                    .header("Accept", "application/json")
                    .send_form([
                        ("client_id", client_id),
                        ("device_code", device_code),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ]),
            )?,
        )?;

        if let Some(token) = res["access_token"].as_str() {
            break token.to_string();
        }

        match res["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => {
                interval = res["interval"].as_u64().unwrap_or(interval + 5);
            }
            Some(e) => {
                return Err(format!("GitHub refused the sign-in: {e}"));
            }
            None => {
                return Err("GitHub returned neither a token nor an error".into());
            }
        }
    };

    ok(
        "POST /v1/login",
        send(
            "POST /v1/login",
            agent
                .post(format!("{api}/v1/login"))
                .send_json(json!({"token": user_token, "machine": machine})),
        )?,
    )
}

/// Revokes the machine token on the service, or with `all` every token of its user, and deletes
/// the stored login token unless the token came from `SHIPCHECK_TOKEN`. Returns what to tell the user
pub fn logout(all: bool) -> Result<String, String> {
    let path = token_path();
    let from_env = env_token_set();
    let token = machine_token().ok_or("not signed in")?;
    let route = if all { "/v1/tokens" } else { "/v1/login" };
    let res = send(
        &format!("DELETE {route}"),
        agent()
            .delete(format!("{}{route}", api()))
            .header("Authorization", format!("Bearer {}", token.trim()))
            .call(),
    )?;
    let said = revoked(all, from_env, res)?;

    if !from_env {
        let _ = fs::remove_file(login_path());
        crate::secret::remove(&path)?;
    }

    Ok(said)
}

fn revoked(all: bool, from_env: bool, (status, body): (u16, Value)) -> Result<String, String> {
    if all {
        // A token the service does not know revokes nothing, so the others stay alive
        if status == 401 {
            return Err("this machine's token is no longer valid, so nothing was revoked: run `shipcheck login`, then `shipcheck logout --all`".into());
        }

        let n = ok("DELETE /v1/tokens", (status, body))?["revoked"]
            .as_u64()
            .ok_or("the service returned no count")?;
        return Ok(format!("signed out, revoked {n} machine tokens"));
    }

    // 401 means the service no longer knows the token, so it is revoked already
    if status != 401 {
        ok("DELETE /v1/login", (status, body))?;
    }

    Ok(format!(
        "signed out, {} is revoked",
        if from_env {
            "SHIPCHECK_TOKEN"
        } else {
            "this machine's token"
        }
    ))
}

/// Posts a check body from `check` through the service, or with `check_id` updates that run of the
/// same name and commit, and returns its id and page on GitHub
pub fn post_check(
    token: &str,
    repo: &str,
    body: &Value,
    check_id: Option<u64>,
) -> Result<(u64, String), String> {
    let mut request = json!({"repo": repo, "sha": body["head_sha"], "check": body});

    if let Some(id) = check_id {
        request["check_id"] = id.into();
    }

    let res = ok(
        "POST /v1/checks",
        send(
            "POST /v1/checks",
            agent()
                .post(format!("{}/v1/checks", api()))
                .header("Authorization", format!("Bearer {token}"))
                .send_json(request),
        )?,
    )?;
    let id = res["id"]
        .as_u64()
        .ok_or("the service returned no check id")?;
    Ok((id, res["html_url"].as_str().unwrap_or_default().to_string()))
}

/// Sends one run record to `api`, which stores it only for an account with a plan for team features.
/// The repository goes in the query, so the service checks the plan before it reads the record
pub fn post_record(api: &str, token: &str, repo: &str, record: &Value) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into();
    ok(
        "POST /v1/records",
        send(
            "POST /v1/records",
            agent
                .post(format!("{api}/v1/records"))
                .query("repo", repo)
                .header("Authorization", format!("Bearer {token}"))
                .send_json(record),
        )?,
    )?;
    Ok(())
}

fn get_json(token: &str, path: &str, query: &[(&str, &str)]) -> Result<Value, String> {
    let mut req = agent()
        .get(format!("{}{path}", api()))
        .header("Authorization", format!("Bearer {token}"));

    for (k, v) in query {
        req = req.query(*k, *v);
    }

    ok(
        &format!("GET {path}"),
        send(&format!("GET {path}"), req.call())?,
    )
}

pub const NOT_SIGNED_IN: &str = "nothing to post the check with: run `shipcheck login`, or set SHIPCHECK_APP_ID and put the key of an App of your own at app.pem in shipcheck's home";

/// Reads and writes shipcheck's checks on one repository, with what `poster` finds
pub struct Sender {
    repo: String,
    how: How,
}

enum How {
    /// The App key, its installation token, and this machine's `shipcheck login` token if any
    Key(
        crate::github::App,
        Mutex<Option<(String, Instant)>>,
        OnceLock<Option<String>>,
    ),
    Service(String),
}

impl Sender {
    pub fn new(repo: &str) -> Result<Self, String> {
        let how = match poster() {
            Poster::Key => How::Key(
                crate::github::App::from_env()?,
                Mutex::new(None),
                OnceLock::new(),
            ),
            Poster::Service(token) => How::Service(token),
            Poster::None => return Err(NOT_SIGNED_IN.into()),
        };
        Ok(Sender {
            repo: repo.to_string(),
            how,
        })
    }

    // Installation tokens last an hour, so mint a new one after fifty minutes
    fn installation_token(
        &self,
        app: &crate::github::App,
        cache: &Mutex<Option<(String, Instant)>>,
    ) -> Result<String, String> {
        let mut cache = cache.lock().map_err(|e| e.to_string())?;

        if let Some((token, minted)) = &*cache
            && minted.elapsed() < Duration::from_secs(50 * 60)
        {
            return Ok(token.clone());
        }

        let token = app.installation_token(&self.repo)?;
        *cache = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    /// Creates the check, or updates run `id`, and returns its id and page on GitHub
    pub fn send(&self, body: &Value, id: Option<u64>) -> Result<(u64, String), String> {
        match &self.how {
            How::Service(token) => post_check(token, &self.repo, body, id),
            How::Key(app, cache, _) => {
                let token = self.installation_token(app, cache)?;

                match id {
                    Some(id) => Ok((
                        id,
                        crate::github::update_check(&token, &self.repo, id, body)?,
                    )),
                    None => crate::github::post_check(&token, &self.repo, body),
                }
            }
        }
    }

    pub fn open_prs(&self) -> Result<Vec<crate::github::OpenPr>, String> {
        match &self.how {
            How::Service(token) => {
                let res = get_json(token, "/v1/pulls", &[("repo", &self.repo)])?;
                Ok(crate::github::parse_prs(&res["pulls"]))
            }
            How::Key(app, cache, _) => {
                crate::github::open_prs(&self.installation_token(app, cache)?, &self.repo)
            }
        }
    }

    /// Writes the PR comment of `sha`'s PRs into `base`, or of `prs` with an App key, and returns
    /// each comment's page
    pub fn comment(
        &self,
        prs: Option<&[u64]>,
        base: &str,
        sha: &str,
        entry: &crate::run::Entry,
        secrets: &[String],
    ) -> Result<Vec<String>, String> {
        match &self.how {
            How::Service(token) => {
                crate::comment::post_with_service(token, &self.repo, base, sha, entry, secrets)
            }
            How::Key(app, _, _) => {
                crate::comment::post_with_app(app, &self.repo, prs, base, sha, entry, secrets)
            }
        }
    }

    /// One PR, open or not, with an App key. None through the service, which reads no closed PR
    pub fn pull(&self, number: u64) -> Result<Option<Value>, String> {
        match &self.how {
            How::Service(_) => Ok(None),
            How::Key(app, cache, _) => {
                crate::github::pull(&self.installation_token(app, cache)?, &self.repo, number)
                    .map(Some)
            }
        }
    }

    /// Who pushed head `sha` of PR `pr`, and the source that said so, None while none does.
    /// `trusted` are the logins the daemon runs
    pub fn pusher(
        &self,
        pr: u64,
        branch: &str,
        sha: &str,
        trusted: &[String],
    ) -> Result<Option<(String, String)>, String> {
        let ask = |token: &str| {
            let pr = pr.to_string();
            let query = [
                ("repo", self.repo.as_str()),
                ("pr", &pr),
                ("ref", branch),
                ("sha", sha),
            ];
            get_json(token, "/v1/pusher", &query).map(|res| parse_pusher(&res))
        };

        match &self.how {
            How::Service(token) => ask(token),
            How::Key(app, cache, login) => {
                // Read once, since on a Mac a read can ask the user
                let service = login.get_or_init(machine_token).as_deref().map(ask);
                service_first(service, || {
                    Ok(crate::github::find_pusher(
                        &self.installation_token(app, cache)?,
                        &self.repo,
                        pr,
                        branch,
                        sha,
                        trusted,
                    )?
                    .map(|(login, source)| (login, source.to_string())))
                })
            }
        }
    }

    /// shipcheck's latest check run per name on `sha`
    pub fn find_checks(&self, sha: &str) -> Result<crate::github::Checks, String> {
        match &self.how {
            How::Service(token) => {
                let res = get_json(token, "/v1/checks", &[("repo", &self.repo), ("sha", sha)])?;
                Ok(crate::github::parse_checks(&res))
            }
            How::Key(app, cache, _) => crate::github::find_checks(
                &self.installation_token(app, cache)?,
                &self.repo,
                sha,
                app.id(),
            ),
        }
    }
}

/// The login and source of the service's `GET /v1/pusher`, None while it names no one
fn parse_pusher(res: &Value) -> Option<(String, String)> {
    let source = res["source"].as_str().unwrap_or("events").to_string();
    res["login"].as_str().map(|l| (l.to_string(), source))
}

type Pusher = Result<Option<(String, String)>, String>;

/**
 * The service's pusher when it names one, else the App key's own lookup. Only the service gets
 * GitHub's webhook, so a self-hosted daemon asks it first when this machine is signed in. Its
 * errors stay silent, since the local lookup answers the same question
 **/
fn service_first(service: Option<Pusher>, local: impl FnOnce() -> Pusher) -> Pusher {
    match service {
        Some(Ok(Some(found))) => Ok(Some(found)),
        _ => local(),
    }
}

/// The open PRs whose head is `sha`, never a fork's, with each one's base and the App's comment
pub fn find_comments(
    token: &str,
    repo: &str,
    sha: &str,
) -> Result<Vec<(u64, String, Option<String>)>, String> {
    let res = ok(
        "GET /v1/comments",
        send(
            "GET /v1/comments",
            agent()
                .get(format!("{}/v1/comments", api()))
                .query("repo", repo)
                .query("sha", sha)
                .header("Authorization", format!("Bearer {token}"))
                .call(),
        )?,
    )?;
    Ok(res["prs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some((
                p["pr"].as_u64()?,
                p["base"].as_str()?.to_string(),
                p["body"].as_str().map(str::to_string),
            ))
        })
        .collect())
}

/// Creates or updates the App's comment on a PR through the service, and returns its page
pub fn post_comment(
    token: &str,
    repo: &str,
    sha: &str,
    pr: u64,
    body: &str,
) -> Result<String, String> {
    let res = ok(
        "POST /v1/comments",
        send(
            "POST /v1/comments",
            agent()
                .post(format!("{}/v1/comments", api()))
                .header("Authorization", format!("Bearer {token}"))
                .send_json(json!({"repo": repo, "sha": sha, "pr": pr, "body": body})),
        )?,
    )?;
    Ok(res["html_url"].as_str().unwrap_or_default().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_self_hosted_daemon_takes_the_service_pusher_first_and_falls_back_silently() {
        let hook = parse_pusher(&json!({"login": "ann", "source": "webhook"}));
        assert_eq!(hook, Some(("ann".into(), "webhook".into())));
        assert_eq!(parse_pusher(&Value::Null), None, "no one yet");
        assert_eq!(
            parse_pusher(&json!({"login": "bob"})),
            Some(("bob".into(), "events".into())),
            "an older service names no source"
        );

        let local = || Ok(Some(("bob".to_string(), "signature".to_string())));
        assert_eq!(service_first(Some(Ok(hook.clone())), local), Ok(hook));

        for service in [
            None,
            Some(Ok(None)),
            Some(Err("GET /v1/pusher: HTTP 403".into())),
        ] {
            assert_eq!(service_first(service, local), local());
        }
    }

    #[test]
    fn picks_how_to_post() {
        let t = || Some("shc_x\n".to_string());
        assert_eq!(choose(t(), true, None), Poster::Service("shc_x".into()));
        assert_eq!(choose(None, true, t()), Poster::Key);
        assert_eq!(choose(None, false, t()), Poster::Service("shc_x".into()));
        assert_eq!(
            choose(Some(" ".into()), false, Some("".into())),
            Poster::None
        );
    }

    #[test]
    fn signs_in_only_at_a_terminal_when_it_cannot_post() {
        assert_eq!(sign_in(true, true), SignIn::Already);
        assert_eq!(sign_in(true, false), SignIn::Already);
        assert_eq!(sign_in(false, true), SignIn::Ask);
        assert_eq!(sign_in(false, false), SignIn::Skip);
    }

    #[test]
    fn logout_all_reports_what_it_revoked() {
        let gone = (401, json!({"error": "unknown or revoked machine token"}));
        assert!(
            revoked(true, false, gone.clone())
                .unwrap_err()
                .contains("nothing was revoked")
        );
        assert_eq!(
            revoked(true, false, (200, json!({"revoked": 3}))).unwrap(),
            "signed out, revoked 3 machine tokens"
        );
        assert_eq!(
            revoked(false, true, gone).unwrap(),
            "signed out, SHIPCHECK_TOKEN is revoked"
        );
    }
}
