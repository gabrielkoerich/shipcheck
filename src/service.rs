use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
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
    let has_key = std::env::var_os("SHIPCHECK_APP_KEY_FILE").is_some()
        || crate::run::home().join("app.pem").exists();
    choose(
        std::env::var("SHIPCHECK_TOKEN").ok(),
        has_key,
        fs::read_to_string(token_path()).ok(),
    )
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

fn save(path: &Path, token: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // `mode` applies only when the file is created
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .and_then(|_| file.write_all(token.as_bytes()))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/**
 * Signs this machine in: GitHub's device flow on the App's client id gives a user token, the
 * service reads who it belongs to and returns a machine token of its own, kept at `<home>/token`.
 * Returns the GitHub login
 **/
pub fn login() -> Result<String, String> {
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
    println!(
        "open {} and enter {}",
        code["verification_uri"]
            .as_str()
            .unwrap_or("https://github.com/login/device"),
        code["user_code"].as_str().unwrap_or_default()
    );

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

    let machine = std::process::Command::new("hostname")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let res = ok(
        "POST /v1/login",
        send(
            "POST /v1/login",
            agent
                .post(format!("{api}/v1/login"))
                .send_json(json!({"token": user_token, "machine": machine})),
        )?,
    )?;
    let token = res["token"]
        .as_str()
        .ok_or("the service returned no token")?;
    save(&token_path(), token)?;
    Ok(res["login"].as_str().unwrap_or_default().to_string())
}

/// Revokes this machine's token on the service, then deletes it
pub fn logout() -> Result<(), String> {
    let path = token_path();
    let token = fs::read_to_string(&path).map_err(|_| "not signed in".to_string())?;
    let (status, body) = send(
        "DELETE /v1/login",
        agent()
            .delete(format!("{}/v1/login", api()))
            .header("Authorization", format!("Bearer {}", token.trim()))
            .call(),
    )?;

    // 401 means the service no longer knows the token, so it is revoked already
    if status != 401 {
        ok("DELETE /v1/login", (status, body))?;
    }

    fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Posts a check body from `check` through the service, and returns its id and page on GitHub
pub fn post_check(token: &str, repo: &str, body: &Value) -> Result<(u64, String), String> {
    let res = ok(
        "POST /v1/checks",
        send(
            "POST /v1/checks",
            agent()
                .post(format!("{}/v1/checks", api()))
                .header("Authorization", format!("Bearer {token}"))
                .send_json(json!({"repo": repo, "sha": body["head_sha"], "check": body})),
        )?,
    )?;
    let id = res["id"]
        .as_u64()
        .ok_or("the service returned no check id")?;
    Ok((id, res["html_url"].as_str().unwrap_or_default().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn token_file_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home/token");
        save(&path, "a").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        save(&path, "b").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "b");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
