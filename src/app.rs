//! The review app: a GitHub App whose installation tokens reviewer sessions
//! use. GitHub doesn't count an approval from a PR's author, and issuewright's
//! sessions open PRs under the user's account, so an approval that branch
//! protection accepts has to come from another identity.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::ReviewAppConfig;

const API: &str = "https://api.github.com";

pub struct ReviewApp {
    app_id: u64,
    key: EncodingKey,
    /// The app's slug; its reviews are signed `<slug>[bot]`.
    pub slug: String,
    /// The app's `contents` permission (`read`, `write`). GitHub counts an
    /// app's approval toward required reviews only with `write`.
    pub contents: String,
    http: ureq::Agent,
}

#[derive(Serialize)]
struct Claims {
    iat: i64,
    exp: i64,
    iss: String,
}

impl ReviewApp {
    /// Reads the private key and asks GitHub who the app is, which also
    /// proves the key and the app id belong together.
    pub fn load(cfg: &ReviewAppConfig) -> Result<Self> {
        let pem = read_key(cfg)?;
        let key = EncodingKey::from_rsa_pem(pem.as_bytes())
            .context("review_app: the private key is not an RSA PEM key")?;
        let http = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .user_agent("issuewright")
            .build();
        let mut app = Self {
            app_id: cfg.app_id,
            key,
            slug: String::new(),
            contents: String::new(),
            http,
        };
        let me = app
            .get("/app", &app.jwt()?)
            .context("review_app: GitHub refused the app's credentials")?
            .context("review_app: GitHub doesn't know this app id")?;
        app.slug = me["slug"]
            .as_str()
            .context("review_app: GET /app returned no slug")?
            .to_string();
        app.contents = me["permissions"]["contents"]
            .as_str()
            .unwrap_or("none")
            .to_string();
        Ok(app)
    }

    /// The login the app's reviews carry.
    pub fn bot_login(&self) -> String {
        format!("{}[bot]", self.slug)
    }

    /// A JWT that authenticates as the app itself, valid for nine minutes.
    fn jwt(&self) -> Result<String> {
        let now = chrono::Utc::now().timestamp();
        let claims = Claims {
            // A minute back, against clock drift.
            iat: now - 60,
            exp: now + 9 * 60,
            iss: self.app_id.to_string(),
        };
        Ok(jsonwebtoken::encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &self.key,
        )?)
    }

    /// The app's installation on `repo` (`owner/name`), if it has one.
    pub fn installation(&self, repo: &str) -> Result<Option<u64>> {
        Ok(self
            .get(&format!("/repos/{repo}/installation"), &self.jwt()?)?
            .and_then(|v| v["id"].as_u64()))
    }

    /// An installation token limited to `repo`, valid for one hour.
    pub fn token(&self, repo: &str) -> Result<String> {
        let id = self
            .installation(repo)?
            .with_context(|| format!("the review app isn't installed on {repo}"))?;
        let name = repo.rsplit('/').next().unwrap_or(repo);
        let resp = self
            .http
            .post(&format!("{API}/app/installations/{id}/access_tokens"))
            .set("Authorization", &format!("Bearer {}", self.jwt()?))
            .set("Accept", "application/vnd.github+json")
            .send_json(json!({ "repositories": [name] }))
            .map_err(http_error)
            .with_context(|| format!("creating an installation token for {repo}"))?;
        let v: Value = resp.into_json()?;
        Ok(v["token"]
            .as_str()
            .context("no token in the response")?
            .to_string())
    }

    /// GET with the app's JWT. `None` on 404.
    fn get(&self, path: &str, jwt: &str) -> Result<Option<Value>> {
        match self
            .http
            .get(&format!("{API}{path}"))
            .set("Authorization", &format!("Bearer {jwt}"))
            .set("Accept", "application/vnd.github+json")
            .call()
        {
            Ok(r) => Ok(Some(r.into_json()?)),
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(http_error(e)).with_context(|| format!("GET {path}")),
        }
    }
}

fn http_error(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            anyhow::anyhow!("HTTP {code}: {}", body.trim())
        }
        other => other.into(),
    }
}

fn read_key(cfg: &ReviewAppConfig) -> Result<String> {
    match (&cfg.private_key, &cfg.private_key_command) {
        (Some(p), None) => read_key_file(p),
        (None, Some(cmd)) => {
            let out = std::process::Command::new("sh")
                .args(["-c", cmd])
                .output()?;
            if !out.status.success() {
                bail!(
                    "review_app.private_key_command failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(String::from_utf8(out.stdout)?)
        }
        _ => bail!("review_app needs exactly one of private_key and private_key_command"),
    }
}

fn read_key_file(p: &Path) -> Result<String> {
    std::fs::read_to_string(p)
        .with_context(|| format!("reading review_app.private_key {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// jsonwebtoken 12 panics at the first signature unless exactly one
    /// crypto backend feature is enabled.
    #[test]
    fn jsonwebtoken_has_a_crypto_backend() {
        let claims = Claims {
            iat: 0,
            exp: 1,
            iss: "1".into(),
        };
        let key = EncodingKey::from_secret(b"test").unwrap();
        assert!(jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &key).is_ok());
    }

    #[test]
    fn needs_exactly_one_key_source() {
        let both = ReviewAppConfig {
            app_id: 1,
            private_key: Some("/k.pem".into()),
            private_key_command: Some("cat /k.pem".into()),
        };
        assert!(read_key(&both).is_err());
        let none = ReviewAppConfig {
            app_id: 1,
            private_key: None,
            private_key_command: None,
        };
        assert!(read_key(&none).is_err());
        let cmd = ReviewAppConfig {
            app_id: 1,
            private_key: None,
            private_key_command: Some("printf pem".into()),
        };
        assert_eq!(read_key(&cmd).unwrap(), "pem");
    }
}
