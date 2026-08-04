//! Free-tier enrolment against the gateway.
//!
//! A fresh install has no API key. Rather than dropping the user on a wall of
//! configuration, it registers anonymously with the gateway and gets a device
//! token good for a small daily budget on one model. No sign-in, no email, no
//! account -- the only thing sent is a hash of a hardware id, so that
//! reinstalling does not read as a new device.
//!
//! Every failure here is non-fatal. The app must still work for someone who
//! brings their own API key, is offline, or is behind a proxy that blocks the
//! gateway entirely.

use crate::config::Config;
use crate::device;
use serde::Deserialize;
use std::time::Duration;

/// Where a fresh install registers. Overridden by `free_tier.gateway` in
/// config.toml or `TUISAMPLE_GATEWAY`, which is how staging gets exercised.
pub const DEFAULT_GATEWAY: &str = "https://free.tuisample.dev";

#[derive(Deserialize)]
struct RegisterResponse {
    device_token: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    daily_limit_usd: f64,
    #[serde(default)]
    resets_at: String,
}

/// What enrolment produced, for the welcome screen.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Enrolment {
    pub model: String,
    pub daily_limit_usd: f64,
    pub resets_at: String,
}

/// Whether this install should try to use the free tier.
///
/// A user who has configured their own key is never redirected to it: their key
/// is a deliberate choice, and silently proxying their traffic through our
/// servers would be a surprise they did not ask for.
pub fn should_register(config: &Config) -> bool {
    config.free_tier.enabled
        && config.free_tier.device_token.is_empty()
        && config.llm.api_key.is_empty()
}

/// True when the configured key is a free-tier device token rather than a
/// provider key.
pub fn is_free_tier(config: &Config) -> bool {
    config.free_tier.enabled
        && !config.free_tier.device_token.is_empty()
        && config.llm.api_key == config.free_tier.device_token
}

/// Register with the gateway and write the result into `config`.
///
/// Returns `Err` with a human-readable reason on any failure; callers show it
/// on the welcome screen and carry on without the free tier.
pub async fn register(config: &mut Config) -> Result<Enrolment, String> {
    let gateway = config.free_tier.gateway.trim_end_matches('/').to_string();
    if gateway.is_empty() {
        return Err("No free-tier gateway configured.".to_string());
    }

    // Persisted so a machine with no readable hardware id still presents the
    // same identity next launch, instead of drawing a new budget each start.
    if config.free_tier.fallback_id.is_empty() {
        config.free_tier.fallback_id = device::random_fallback_id();
    }
    let device_id_hash = device::device_id_hash(&config.free_tier.fallback_id);

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("Could not create HTTP client: {e}"))?;

    let response = client
        .post(format!("{gateway}/register"))
        .json(&serde_json::json!({
            "device_id_hash": device_id_hash,
            "client_version": env!("CARGO_PKG_VERSION"),
            "platform": std::env::consts::OS,
        }))
        .send()
        .await
        .map_err(|e| format!("Could not reach the free-tier gateway: {e}"))?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "Free tier unavailable ({status}). {}",
            summarise_error(&body)
        ));
    }

    let parsed: RegisterResponse = serde_json::from_str(&body)
        .map_err(|e| format!("Free-tier gateway returned an unexpected response: {e}"))?;
    if parsed.device_token.trim().is_empty() {
        return Err("Free-tier gateway returned an empty device token.".to_string());
    }

    config.free_tier.device_token = parsed.device_token.clone();
    config.llm.endpoint = gateway;
    config.llm.api_key = parsed.device_token;
    config.llm.provider = "free-tier".to_string();
    if !parsed.model.is_empty() {
        config.llm.model = parsed.model.clone();
    }

    Ok(Enrolment {
        model: parsed.model,
        daily_limit_usd: parsed.daily_limit_usd,
        resets_at: parsed.resets_at,
    })
}

/// Pull the human-readable part out of an OpenAI-shaped error body.
pub fn summarise_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                String::new()
            } else {
                trimmed.chars().take(200).collect()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn fresh() -> Config {
        Config::default()
    }

    #[test]
    fn a_fresh_install_with_no_key_registers() {
        assert!(should_register(&fresh()));
    }

    /// Someone who configured their own key made a deliberate choice; their
    /// traffic must never be quietly routed through our gateway.
    #[test]
    fn an_install_with_its_own_api_key_is_left_alone() {
        let mut config = fresh();
        config.llm.api_key = "sk-the-users-own-key".to_string();
        assert!(!should_register(&config));
    }

    #[test]
    fn an_already_registered_device_does_not_register_again() {
        let mut config = fresh();
        config.free_tier.device_token = "dt_abc.def".to_string();
        assert!(!should_register(&config));
    }

    #[test]
    fn disabling_the_free_tier_stops_registration() {
        let mut config = fresh();
        config.free_tier.enabled = false;
        assert!(!should_register(&config));
    }

    #[test]
    fn is_free_tier_recognises_a_device_token_in_use() {
        let mut config = fresh();
        config.free_tier.device_token = "dt_abc.def".to_string();
        config.llm.api_key = "dt_abc.def".to_string();
        assert!(is_free_tier(&config));

        // A user who later pastes their own key is no longer on the free tier.
        config.llm.api_key = "sk-their-own".to_string();
        assert!(!is_free_tier(&config));
    }

    #[test]
    fn error_bodies_are_summarised_for_humans() {
        assert_eq!(
            summarise_error(r#"{"error":{"code":"x","message":"Too many devices."}}"#),
            "Too many devices."
        );
        assert_eq!(summarise_error("plain text failure"), "plain text failure");
        assert_eq!(summarise_error(""), "");
    }

    #[test]
    fn an_enormous_error_body_is_truncated() {
        let huge = "x".repeat(10_000);
        assert!(summarise_error(&huge).chars().count() <= 200);
    }

    // ---- registration against a live socket ---------------------------------

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve one canned HTTP response and hand back the request body it received.
    async fn serve(
        status: &str,
        body: &str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
            let _ = tx.send(request);
        });
        (format!("http://{addr}"), rx)
    }

    #[tokio::test]
    async fn registering_stores_the_token_and_points_the_client_at_the_gateway() {
        let (addr, request) = serve(
            "200 OK",
            r#"{"device_token":"dt_ref.secret","model":"free-model","daily_limit_usd":1.0,"resets_at":"2026-08-05T00:00:00Z"}"#,
        )
        .await;

        let mut config = fresh();
        config.free_tier.gateway = addr.clone();
        let enrolment = register(&mut config).await.expect("registration should succeed");

        assert_eq!(enrolment.model, "free-model");
        assert_eq!(enrolment.daily_limit_usd, 1.0);
        // The token becomes the bearer credential, and traffic goes to the gateway.
        assert_eq!(config.free_tier.device_token, "dt_ref.secret");
        assert_eq!(config.llm.api_key, "dt_ref.secret");
        assert_eq!(config.llm.endpoint, addr);
        assert_eq!(config.llm.model, "free-model");
        assert!(is_free_tier(&config));

        // Only a hash is sent: no raw hardware id, and nothing identifying.
        let sent = request.await.unwrap();
        assert!(sent.contains("device_id_hash"), "{sent}");
        let body = sent.split("\r\n\r\n").nth(1).unwrap_or("");
        let json: serde_json::Value = serde_json::from_str(body).expect("valid JSON body");
        let hash = json["device_id_hash"].as_str().unwrap();
        assert_eq!(hash.len(), 64, "must be a hex sha256");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// A gateway that is down, blocked, or slow must not stop the app starting.
    #[tokio::test]
    async fn an_unreachable_gateway_reports_an_error_rather_than_panicking() {
        let mut config = fresh();
        config.free_tier.gateway = "http://127.0.0.1:1".to_string(); // refuses instantly
        let err = register(&mut config).await.expect_err("should fail");
        assert!(err.contains("Could not reach"), "{err}");
        // Nothing half-written: the app still has no key and no token.
        assert!(config.free_tier.device_token.is_empty());
        assert!(config.llm.api_key.is_empty());
    }

    #[tokio::test]
    async fn a_rejected_registration_surfaces_the_reason() {
        let (addr, _req) = serve(
            "429 Too Many Requests",
            r#"{"error":{"code":"registration_rate_limited","message":"Too many new devices from this network today."}}"#,
        )
        .await;

        let mut config = fresh();
        config.free_tier.gateway = addr;
        let err = register(&mut config).await.expect_err("should fail");
        assert!(err.contains("Too many new devices"), "{err}");
        assert!(config.free_tier.device_token.is_empty());
    }

    #[tokio::test]
    async fn an_empty_token_is_rejected_rather_than_stored() {
        let (addr, _req) = serve("200 OK", r#"{"device_token":"   "}"#).await;
        let mut config = fresh();
        config.free_tier.gateway = addr;
        assert!(register(&mut config).await.is_err());
        assert!(config.llm.api_key.is_empty());
    }

    #[tokio::test]
    async fn a_garbled_response_is_an_error_not_a_panic() {
        let (addr, _req) = serve("200 OK", "this is not json").await;
        let mut config = fresh();
        config.free_tier.gateway = addr;
        assert!(register(&mut config).await.is_err());
    }

    /// The fallback id is persisted so a machine with no hardware id presents the
    /// same identity next launch instead of drawing a fresh budget each start.
    #[tokio::test]
    async fn a_fallback_id_is_generated_once_and_kept() {
        let (addr, _req) = serve("200 OK", r#"{"device_token":"dt_a.b","model":"m"}"#).await;
        let mut config = fresh();
        config.free_tier.gateway = addr;
        assert!(config.free_tier.fallback_id.is_empty());

        register(&mut config).await.expect("should succeed");
        let first = config.free_tier.fallback_id.clone();
        assert!(!first.is_empty());

        let (addr2, _req2) = serve("200 OK", r#"{"device_token":"dt_c.d","model":"m"}"#).await;
        config.free_tier.gateway = addr2;
        register(&mut config).await.expect("should succeed");
        assert_eq!(config.free_tier.fallback_id, first, "must not be regenerated");
    }
}
