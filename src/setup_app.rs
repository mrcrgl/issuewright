//! `issuewright setup-app`: creates the review app with GitHub's manifest
//! flow, so nobody has to pick its permissions by hand.
//!
//! 1. A short-lived server on 127.0.0.1 serves a page that posts the app's
//!    manifest to GitHub's "new app" page. You click "Create GitHub App".
//! 2. GitHub redirects back to the server with a one-time code. Without a
//!    browser on this machine, the redirected URL can be pasted instead.
//! 3. The code is exchanged for the app id and private key. The key is saved
//!    next to the config, readable only by you, and `[review_app]` is added
//!    to the config, unless the config already has one.
//! 4. The app's install page opens, to pick the repositories.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// How long to wait for the app to be created.
const WAIT: Duration = Duration::from_secs(30 * 60);

pub struct Options {
    pub org: Option<String>,
    pub name: Option<String>,
    pub port: u16,
}

/// The permissions a reviewer session needs; see "The review app" in the
/// README for why each one.
fn manifest(name: &str, redirect: &str) -> Value {
    json!({
        "name": name,
        "url": "https://github.com/mrcrgl/issuewright",
        "description": "Reviews and approves the pull requests of issuewright's sessions.",
        "hook_attributes": { "url": "https://github.com/mrcrgl/issuewright", "active": false },
        "redirect_url": redirect,
        "public": false,
        "default_permissions": {
            "pull_requests": "write",
            // Approvals count toward required reviews only from a reviewer
            // with write access.
            "contents": "write",
            "issues": "write",
            "checks": "read",
            "statuses": "read",
            "actions": "read",
            "metadata": "read"
        },
        "default_events": []
    })
}

/// GitHub's page that takes the manifest.
fn new_app_url(org: Option<&str>, state: &str) -> String {
    match org {
        Some(o) => format!("https://github.com/organizations/{o}/settings/apps/new?state={state}"),
        None => format!("https://github.com/settings/apps/new?state={state}"),
    }
}

/// The page that posts the manifest to GitHub as soon as it loads.
fn form_page(action: &str, manifest: &Value) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>issuewright setup-app</title>\
<body style=\"font-family:sans-serif\"><p>Sending the app's manifest to GitHub…</p>\
<form id=f method=post action=\"{}\"><input type=hidden name=manifest value=\"{}\">\
<noscript><button>Continue to GitHub</button></noscript></form>\
<script>document.getElementById('f').submit()</script>",
        html_escape(action),
        html_escape(&manifest.to_string())
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The value of `key` in a URL's query, percent-decoded.
fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The code from what someone pasted: the redirected URL, or the code alone.
fn pasted_code(line: &str, state: &str) -> Option<Result<String>> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if line.contains('?') {
        let code = query_param(line, "code")?;
        if query_param(line, "state").as_deref() != Some(state) {
            return Some(Err(anyhow::anyhow!(
                "the pasted URL belongs to another setup-app run"
            )));
        }
        return Some(Ok(code));
    }
    line.chars()
        .all(|c| c.is_ascii_alphanumeric())
        .then(|| Ok(line.to_string()))
}

fn respond(mut s: &TcpStream, status: &str, body: &str) {
    let _ = write!(
        s,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Serves the form page and waits for GitHub's redirect; sends its code.
fn serve(listener: TcpListener, page: String, state: String, tx: mpsc::Sender<Result<String>>) {
    for stream in listener.incoming().flatten() {
        let mut line = String::new();
        if BufReader::new(&stream).read_line(&mut line).is_err() {
            continue;
        }
        let path = line.split_whitespace().nth(1).unwrap_or("/");
        if path == "/" {
            respond(&stream, "200 OK", &page);
        } else if path.starts_with("/callback") {
            let result = match (query_param(path, "code"), query_param(path, "state")) {
                (Some(code), Some(s)) if s == state => Ok(code),
                (Some(_), _) => Err(anyhow::anyhow!("GitHub redirected with a wrong state")),
                _ => Err(anyhow::anyhow!("GitHub redirected without a code")),
            };
            let ok = result.is_ok();
            respond(
                &stream,
                "200 OK",
                if ok {
                    "<!doctype html><meta charset=utf-8><body style=\"font-family:sans-serif\"><p>GitHub created the app. issuewright finishes the setup in the terminal.</p>"
                } else {
                    "<!doctype html><meta charset=utf-8><body style=\"font-family:sans-serif\"><p>Something went wrong; see the terminal.</p>"
                },
            );
            let _ = tx.send(result);
            if ok {
                return;
            }
        } else {
            respond(&stream, "404 Not Found", "not found");
        }
    }
}

fn open_browser(url: &str) {
    for opener in ["xdg-open", "open"] {
        let started = Command::new(opener)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if started.is_ok() {
            return;
        }
    }
}

/// The created app, as GitHub's conversion returns it.
struct Created {
    id: u64,
    slug: String,
    pem: String,
    html_url: String,
}

fn convert(code: &str) -> Result<Created> {
    let http = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .user_agent("issuewright")
        .build();
    let v: Value = http
        .post(&format!(
            "https://api.github.com/app-manifests/{code}/conversions"
        ))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(c, r) => {
                anyhow::anyhow!("HTTP {c}: {}", r.into_string().unwrap_or_default().trim())
            }
            other => other.into(),
        })
        .context("exchanging the code for the app's credentials")?
        .into_json()?;
    let s = |k: &str| v[k].as_str().map(String::from);
    Ok(Created {
        id: v["id"].as_u64().context("no app id in GitHub's answer")?,
        slug: s("slug").context("no slug in GitHub's answer")?,
        pem: s("pem").context("no private key in GitHub's answer")?,
        html_url: s("html_url").unwrap_or_default(),
    })
}

/// Writes the key, readable only by the user; never replaces a file.
fn write_key(path: &Path, pem: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(pem.as_bytes())?;
    Ok(())
}

fn snippet(id: u64, key: &Path) -> String {
    format!(
        "[review_app]\napp_id = {id}\nprivate_key = \"{}\"\n",
        key.display()
    )
}

/// The config with `[review_app]` added, or `None` when it has one already.
fn with_review_app(config: &str, snippet: &str) -> Result<Option<String>> {
    let table: toml::Table = toml::from_str(config).context("parsing the config")?;
    if table.contains_key("review_app") {
        return Ok(None);
    }
    let mut out = config.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(snippet);
    Ok(Some(out))
}

/// Your GitHub login, for the default app name.
fn login(gh_user: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("gh");
    cmd.args(["api", "user", "--jq", ".login"]);
    if let Some(u) = gh_user {
        let token = crate::github::Gh::new(Some(u))?.token().to_string();
        cmd.env("GH_TOKEN", token);
    }
    let out = cmd.output().context("running gh api user")?;
    if !out.status.success() {
        bail!(
            "gh api user failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

pub fn run(config_path: &Path, opts: Options) -> Result<()> {
    let config = match std::fs::read_to_string(config_path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", config_path.display())),
    };
    let table: toml::Table = match &config {
        Some(t) => toml::from_str(t).context("parsing the config")?,
        None => toml::Table::new(),
    };
    if table.contains_key("review_app") {
        bail!(
            "{} already has a [review_app]; remove it first to create a new app",
            config_path.display()
        );
    }
    let gh_user = table.get("gh_user").and_then(|v| v.as_str());
    let name = match (opts.name, &opts.org) {
        (Some(n), _) => n,
        (None, Some(o)) => format!("{o}-issuewright"),
        (None, None) => format!("{}-issuewright", login(gh_user)?),
    };

    let listener = TcpListener::bind(("127.0.0.1", opts.port))
        .with_context(|| format!("listening on 127.0.0.1:{}", opts.port))?;
    let port = listener.local_addr()?.port();
    let state = crate::session::new_session_id()?.replace('-', "");
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let page = form_page(
        &new_app_url(opts.org.as_deref(), &state),
        &manifest(&name, &redirect),
    );
    let (tx, rx) = mpsc::channel();
    {
        let (tx, state) = (tx.clone(), state.clone());
        std::thread::spawn(move || serve(listener, page, state, tx));
    }
    {
        let state = state.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                if let Some(r) = pasted_code(&line, &state)
                    && tx.send(r).is_err()
                {
                    return;
                }
            }
        });
    }

    let start = format!("http://127.0.0.1:{port}/");
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let host = host.trim();
    println!(
        "Creating the GitHub App `{name}`. Open this page and click \"Create GitHub App\":\n\n  {start}\n"
    );
    println!(
        "If your browser runs on another machine, forward the port first:\n  ssh -L {port}:127.0.0.1:{port} {}\n\
and if the browser can't reach the redirect at the end, paste the URL it shows here.\n",
        if host.is_empty() { "<this host>" } else { host }
    );
    open_browser(&start);

    let code = rx
        .recv_timeout(WAIT)
        .context("no app was created within 30 minutes")??;
    let app = convert(&code)?;
    println!(
        "Created `{}` (app id {}): {}",
        app.slug, app.id, app.html_url
    );

    let dir = config_path.parent().unwrap_or(Path::new("."));
    let key: PathBuf = dir.join(format!("{}.private-key.pem", app.slug));
    write_key(&key, &app.pem)?;
    println!("Saved its private key to {} (mode 600).", key.display());

    let snip = snippet(app.id, &key);
    match with_review_app(config.as_deref().unwrap_or(""), &snip)? {
        Some(text) => {
            std::fs::write(config_path, text)
                .with_context(|| format!("writing {}", config_path.display()))?;
            println!("Added [review_app] to {}.", config_path.display());
        }
        None => println!(
            "{} got a [review_app] meanwhile; add this yourself:\n\n{snip}",
            config_path.display()
        ),
    }

    let install = format!("https://github.com/apps/{}/installations/new", app.slug);
    println!("\nLast step: install the app on the repositories with auto_merge:\n\n  {install}\n");
    open_browser(&install);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_asks_for_what_the_reviewer_needs() {
        let m = manifest("acme-issuewright", "http://127.0.0.1:1/callback");
        let p = &m["default_permissions"];
        assert_eq!(p["contents"], "write");
        assert_eq!(p["pull_requests"], "write");
        assert_eq!(p["issues"], "write");
        assert_eq!(m["hook_attributes"]["active"], false);
        assert_eq!(m["public"], false);
        assert_eq!(m["redirect_url"], "http://127.0.0.1:1/callback");
    }

    #[test]
    fn form_page_escapes_the_manifest() {
        let page = form_page(
            &new_app_url(Some("acme"), "s1"),
            &json!({"name": "a\"<b>&"}),
        );
        assert!(page.contains(
            "action=\"https://github.com/organizations/acme/settings/apps/new?state=s1\""
        ));
        assert!(page.contains("&quot;name&quot;:&quot;a\\&quot;&lt;b&gt;&amp;&quot;"));
        assert!(!page.contains("<b>"));
    }

    #[test]
    fn codes_come_from_the_redirect_or_a_paste() {
        let url = "http://127.0.0.1:5/callback?code=abc123&state=s1";
        assert_eq!(query_param(url, "code").as_deref(), Some("abc123"));
        assert_eq!(query_param("/x?a=b%20c+d", "a").as_deref(), Some("b c d"));
        assert_eq!(pasted_code(url, "s1").unwrap().unwrap(), "abc123");
        assert!(pasted_code(url, "other").unwrap().is_err());
        assert_eq!(pasted_code(" abc123 ", "s1").unwrap().unwrap(), "abc123");
        assert!(pasted_code("", "s1").is_none());
        assert!(pasted_code("not a code!", "s1").is_none());
    }

    #[test]
    fn review_app_is_added_but_never_replaced() {
        let s = snippet(7, Path::new("/k/a.pem"));
        let added = with_review_app("home = \"/x\"\n[[repo]]\nname = \"a/b\"", &s)
            .unwrap()
            .unwrap();
        let t: toml::Table = toml::from_str(&added).unwrap();
        assert_eq!(t["review_app"]["app_id"].as_integer(), Some(7));
        assert_eq!(t["repo"].as_array().unwrap().len(), 1);
        assert!(with_review_app(&added, &s).unwrap().is_none());
        assert_eq!(with_review_app("", &s).unwrap().unwrap(), s);
    }

    #[test]
    fn the_key_is_private_and_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("issuewright-setup-{}", std::process::id()));
        let k = d.join("app.pem");
        let _ = std::fs::remove_dir_all(&d);
        write_key(&k, "PEM").unwrap();
        assert_eq!(
            std::fs::metadata(&k).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_key(&k, "OTHER").is_err());
        assert_eq!(std::fs::read_to_string(&k).unwrap(), "PEM");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
