//! `boxcode login` / `boxcode logout` — Cursor-style device auth via boxcode.sh.

use crate::config::Config;
use serde::Deserialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_AUTH_BASE: &str = "https://boxcode.sh";

fn auth_base() -> String {
    std::env::var("BOXCODE_AUTH_URL")
        .or_else(|_| std::env::var("BOXCODE_SITE_URL"))
        .unwrap_or_else(|_| DEFAULT_AUTH_BASE.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn boxcode_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".boxcode")
}

fn account_token_path() -> PathBuf {
    boxcode_dir().join("account.token")
}

fn account_email_path() -> PathBuf {
    boxcode_dir().join("account.email")
}

/// Snapshot of the local boxcode.sh device-login session (no network).
pub struct LoginStatus {
    pub has_session_token: bool,
    pub email: Option<String>,
    pub endpoint: String,
    pub model: String,
    pub key_prefix: String,
    pub via_boxcode_proxy: bool,
}

impl LoginStatus {
    pub fn load(config: &Config) -> Self {
        let email = std::fs::read_to_string(account_email_path())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let key = config.llm.api_key.trim();
        let key_prefix = if key.is_empty() {
            String::new()
        } else if key.len() > 12 {
            format!("{}…", &key[..12])
        } else {
            key.to_string()
        };
        let endpoint = config.llm.endpoint.trim().to_string();
        let via_boxcode_proxy = endpoint.to_lowercase().contains("llm.boxcode.sh");
        Self {
            has_session_token: account_token_path().is_file(),
            email,
            endpoint,
            model: config.llm.model.clone(),
            key_prefix,
            via_boxcode_proxy,
        }
    }

    /// Human-readable status for `/login` in the TUI.
    pub fn readout(&self) -> String {
        if self.has_session_token && self.via_boxcode_proxy && !self.key_prefix.is_empty() {
            let who = self.email.as_deref().unwrap_or("(email on next login)");
            format!(
                "Signed in via boxcode.sh\n\n\
                 Account:  {who}\n\
                 Endpoint: {}\n\
                 Model:    {}\n\
                 Key:      {}\n\n\
                 Session:  ~/.boxcode/account.token\n\
                 Proof:    heartbeat hits https://boxcode.sh/api/heartbeat\n\n\
                 To link another machine: exit (^c) then run `boxcode login`\n\
                 and confirm the Device ID on https://boxcode.sh/login/device.",
                self.endpoint, self.model, self.key_prefix
            )
        } else if self.via_boxcode_proxy && !self.key_prefix.is_empty() {
            format!(
                "Using llm.boxcode.sh, but no device session yet.\n\n\
                 Endpoint: {}\n\
                 Key:      {}\n\n\
                 Exit (^c) and run:\n\
                   boxcode login\n\n\
                 That signs in with Google on boxcode.sh and links this machine.",
                self.endpoint, self.key_prefix
            )
        } else {
            "Not signed in via boxcode.sh.\n\n\
             Exit (^c) and run:\n\
               boxcode login\n\n\
             Browser opens https://boxcode.sh — Google sign-in, then this CLI\n\
             receives your promo key. Same file the IDE reads:\n\
               ~/.boxcode/config.toml"
                .to_string()
        }
    }
}

fn write_account_email(email: &str) {
    let path = account_email_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, email.trim());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

#[derive(Debug, Deserialize)]
struct StartResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PollResponse {
    status: Option<String>,
    endpoint: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    session_token: Option<String>,
    email: Option<String>,
    error: Option<String>,
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).status();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).status();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .status();
    }
}

pub async fn login() -> Result<(), Box<dyn std::error::Error>> {
    let base = auth_base();
    let client = reqwest::Client::new();

    let start: StartResponse = client
        .post(format!("{base}/api/auth/device/start"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let verify = start
        .verification_uri_complete
        .unwrap_or_else(|| format!("{}?code={}", start.verification_uri, start.user_code));

    println!("boxcode login");
    println!();
    println!("  Device ID:  {}", start.user_code);
    println!("  Open:       {verify}");
    println!();
    println!("Sign in with Google in the browser, then approve this device.");
    println!("The website will show the same Device ID — confirm it matches.");
    print!("Waiting");
    let _ = io::stdout().flush();
    open_browser(&verify);

    let interval = Duration::from_secs(start.interval.unwrap_or(2).max(1));
    loop {
        tokio::time::sleep(interval).await;
        print!(".");
        let _ = io::stdout().flush();

        let res = client
            .post(format!("{base}/api/auth/device/poll"))
            .json(&serde_json::json!({ "device_code": start.device_code }))
            .send()
            .await?;

        if res.status() == reqwest::StatusCode::GONE {
            println!();
            return Err("device code expired — run boxcode login again".into());
        }

        let body: PollResponse = res.json().await?;
        if body.api_key.is_some() {
            println!();
            let mut config = Config::load().unwrap_or_default();
            if let Some(endpoint) = body.endpoint {
                config.llm.endpoint = endpoint.trim_end_matches('/').to_string();
            }
            if let Some(model) = body.model {
                config.llm.model = model;
            }
            if let Some(api_key) = body.api_key {
                config.llm.api_key = api_key;
            }
            config.llm.provider = "deepseek".to_string();
            config.save()?;

            if let Some(token) = body.session_token {
                let path = account_token_path();
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&path, token.trim())?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
            }
            if let Some(ref email) = body.email {
                write_account_email(email);
            }

            println!(
                "✓ Signed in as {}. Credentials saved to ~/.boxcode/config.toml",
                body.email.unwrap_or_else(|| "you".into())
            );
            println!("  Endpoint: {}", config.llm.endpoint);
            println!("  Model:    {}", config.llm.model);
            println!("  IDE uses the same config file.");
            // Immediate heartbeat so DAU counts this login.
            heartbeat_once().await;
            return Ok(());
        }

        match body.status.as_deref() {
            Some("pending") | None => continue,
            Some("expired") => {
                println!();
                return Err("device code expired — run boxcode login again".into());
            }
            _ => {
                if let Some(err) = body.error {
                    println!();
                    return Err(err.into());
                }
                continue;
            }
        }
    }
}

pub async fn logout() -> Result<(), Box<dyn std::error::Error>> {
    let msg = logout_local()?;
    println!("{msg}");
    Ok(())
}

/// Clear device session (+ promo key when pointed at llm.boxcode.sh). Used by
/// both `boxcode logout` and the `/logout` slash command.
pub fn logout_local() -> Result<String, Box<dyn std::error::Error>> {
    let token_path = account_token_path();
    if token_path.exists() {
        let _ = std::fs::remove_file(&token_path);
    }
    let email_path = account_email_path();
    if email_path.exists() {
        let _ = std::fs::remove_file(&email_path);
    }
    let mut config = Config::load().unwrap_or_default();
    let endpoint = config.llm.endpoint.to_lowercase();
    if endpoint.contains("llm.boxcode.sh") {
        config.llm.api_key.clear();
        config.save()?;
        Ok("✓ Cleared llm.boxcode.sh credentials from ~/.boxcode/config.toml".into())
    } else {
        Ok("✓ Cleared local session token (left custom endpoint/api_key untouched)".into())
    }
}

/// Fire-and-forget heartbeat while the TUI is running.
pub async fn heartbeat_once() {
    let Ok(token) = std::fs::read_to_string(account_token_path()) else {
        return;
    };
    let token = token.trim();
    if token.is_empty() {
        return;
    }
    let base = auth_base();
    let client = reqwest::Client::new();
    let _ = client
        .post(format!("{base}/api/heartbeat"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "client": "cli" }))
        .send()
        .await;
}

/// Spawn a background task that heartbeats every five minutes.
pub fn spawn_heartbeat_loop() {
    tokio::spawn(async {
        loop {
            heartbeat_once().await;
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });
}
