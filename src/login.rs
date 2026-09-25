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

fn account_token_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".boxcode").join("account.token")
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
    println!("  Code:  {}", start.user_code);
    println!("  Open:  {verify}");
    println!();
    println!("Sign in with Google in the browser, then approve this device.");
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
    let token_path = account_token_path();
    if token_path.exists() {
        let _ = std::fs::remove_file(&token_path);
    }
    let mut config = Config::load().unwrap_or_default();
    let endpoint = config.llm.endpoint.to_lowercase();
    if endpoint.contains("llm.boxcode.sh") {
        config.llm.api_key.clear();
        config.save()?;
        println!("✓ Cleared llm.boxcode.sh credentials from ~/.boxcode/config.toml");
    } else {
        println!("✓ Cleared local session token (left custom endpoint/api_key untouched)");
    }
    Ok(())
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
