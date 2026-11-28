//! Signed commits from the sessions, with a key they can't read.
//!
//! The signing key lives under `~/.ssh`, which every session's sandbox hides.
//! With `session.signing_key` set, issuewright runs an `ssh-agent` of its own
//! that holds only that key, at a fixed socket under its home, and gives the
//! sessions the agent and the key's public half inline
//! (`user.signingkey = "key::ssh-ed25519 …"`), so git signs through the agent
//! and never opens a file under `~/.ssh`.
//!
//! The socket's path stays the same across restarts, so sessions that outlive
//! a restart sign again as soon as the next issuewright has loaded the key.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::Config;

/// Where the agent listens: `<home>/signing/agent.sock`.
pub fn socket_path(cfg: &Config) -> PathBuf {
    cfg.home.join("signing").join("agent.sock")
}

/// The git settings that make a commit signed with `public_key` through an
/// agent, as `(key, value)`.
pub fn git_config(public_key: &str) -> [(&'static str, String); 3] {
    [
        ("gpg.format", "ssh".to_string()),
        ("user.signingkey", format!("key::{public_key}")),
        ("commit.gpgsign", "true".to_string()),
    ]
}

/// The public half of the private key at `key`, as `ssh-ed25519 AAAA…`
/// without the comment.
pub fn public_key(key: &Path) -> Result<String> {
    let out = Command::new("ssh-keygen")
        .args(["-y", "-P", "", "-f"])
        .arg(key)
        .stdin(Stdio::null())
        .output()
        .context("running ssh-keygen")?;
    if !out.status.success() {
        bail!(
            "reading the public key of {}: {} (the key must have no passphrase)",
            key.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(kind), Some(blob)) => Ok(format!("{kind} {blob}")),
        _ => bail!("ssh-keygen printed no public key for {}", key.display()),
    }
}

/// The running agent. Dropping it stops the agent and removes its socket.
pub struct Agent {
    pub socket: PathBuf,
    pub public_key: String,
    child: Child,
}

impl Agent {
    /// Starts the agent for `cfg.session.signing_key`, if one is set, and
    /// loads the key.
    pub fn start(cfg: &Config) -> Result<Option<Agent>> {
        let Some(key) = &cfg.session.signing_key else {
            return Ok(None);
        };
        let public_key = public_key(key)?;
        let socket = socket_path(cfg);
        let dir = socket.parent().context("socket without a directory")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        // A previous issuewright that didn't exit cleanly leaves its socket.
        match std::fs::remove_file(&socket) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", socket.display()));
            }
            _ => {}
        }
        let child = Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting ssh-agent")?;
        let mut agent = Agent {
            socket,
            public_key,
            child,
        };
        let since = Instant::now();
        while !agent.socket.exists() {
            if since.elapsed() > Duration::from_secs(5) {
                bail!("ssh-agent didn't create {}", agent.socket.display());
            }
            if let Some(status) = agent.child.try_wait()? {
                bail!("ssh-agent exited at start: {status}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let out = Command::new("ssh-add")
            .arg(key)
            .env("SSH_AUTH_SOCK", &agent.socket)
            .stdin(Stdio::null())
            .output()
            .context("running ssh-add")?;
        if !out.status.success() {
            bail!(
                "loading {} into the signing agent: {}",
                key.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(Some(agent))
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_with_the_key_inline() {
        let c = git_config("ssh-ed25519 AAAAC3");
        assert_eq!(c[0], ("gpg.format", "ssh".into()));
        assert_eq!(c[1], ("user.signingkey", "key::ssh-ed25519 AAAAC3".into()));
        assert_eq!(c[2], ("commit.gpgsign", "true".into()));
    }

    /// A commit signed through the agent, in a repository whose own config
    /// knows nothing of the key: what a session's clone looks like.
    #[test]
    #[ignore = "needs ssh-keygen, ssh-agent and git"]
    fn a_commit_is_signed_through_the_agent() {
        let root = std::env::temp_dir().join(format!("issuewright-signing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let key = root.join("key");
        let ok = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap();
        assert!(ok.success());
        let mut cfg: Config = toml::from_str("[[repo]]\nname = \"o/a\"\n").unwrap();
        cfg.home = root.join("home");
        cfg.session.signing_key = Some(key.clone());
        let agent = Agent::start(&cfg).unwrap().unwrap();
        let repo = root.join("repo");
        let git = |args: &[&str]| {
            let mut c = Command::new("git");
            c.arg("-C").arg(&repo).args(args);
            c.env("HOME", &root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("SSH_AUTH_SOCK", &agent.socket);
            for (i, (k, v)) in git_config(&agent.public_key).iter().enumerate() {
                c.env(format!("GIT_CONFIG_KEY_{i}"), k)
                    .env(format!("GIT_CONFIG_VALUE_{i}"), v);
            }
            c.env("GIT_CONFIG_COUNT", "3");
            c.output().unwrap()
        };
        std::fs::create_dir_all(&repo).unwrap();
        assert!(git(&["init", "-q"]).status.success());
        let out = git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "signed",
        ]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let raw = git(&["cat-file", "commit", "HEAD"]);
        assert!(String::from_utf8_lossy(&raw.stdout).contains("-----BEGIN SSH SIGNATURE-----"));
        let socket = agent.socket.clone();
        drop(agent);
        assert!(!socket.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
