//! Free-tier enrolment against the gateway.
//!
//! A fresh install has no API key. Rather than dropping the user on a wall of
//! configuration, it registers anonymously and gets a device token good for a
//! small daily budget on one model. No sign-in, no email, no account: the only
//! thing sent is a hash of a hardware id, so that reinstalling does not read as
//! a brand-new device with a brand-new budget.
//!
//! The provider key is never shipped in this binary. A key compiled into a
//! distributed binary is a published key -- `strings` finds it in seconds -- so
//! the gateway holds it instead, which is also the only place a per-device
//! budget can be enforced honestly.
//!
//! ## Why this device id is not `telemetry.rs`'s
//!
//! `telemetry.rs` generates a **random** id per install, and deliberately so:
//! it counts installs, and an id a user can reset by deleting a file is the
//! privacy-preserving choice for that job.
//!
//! This one is derived from **hardware**, because a resettable id would mean a
//! fresh budget on every reinstall -- the cheapest possible way to defeat the
//! limit. The two ids answer different questions and neither substitutes for
//! the other, which is why both exist. Nothing links them.
//!
//! Every failure here is non-fatal. The app must still work for someone who
//! brings their own API key, is offline, or is behind a proxy that blocks the
//! gateway entirely.

use crate::config::Config;
use crate::device;
use serde::Deserialize;
use std::time::Duration;

/// Where a fresh install enrols. Overridden by `free_tier.gateway` in
/// config.toml or `TUISAMPLE_GATEWAY` -- the same shape `telemetry.rs` uses for
/// its endpoint, so there is one pattern to learn rather than two.
pub const DEFAULT_GATEWAY: &str = "https://if44ueakglms72wqq4lr6podcy0aezbm.lambda-url.us-east-1.on.aws";

#[derive(Deserialize)]
struct RegisterResponse {
    device_token: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    daily_limit_usd: f64,
}

/// What enrolment produced, for the welcome screen.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Enrolment {
    pub model: String,
    pub daily_limit_usd: f64,
}

/// This device's live budget, as the gateway sees it.
///
/// Queried rather than remembered from enrolment: the limit is a server-side
/// setting that can change at any time, so a locally cached copy would be a
/// number that only looks authoritative.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Budget {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub daily_limit_usd: f64,
    #[serde(default)]
    pub spent_usd: f64,
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub resets_at: String,
    #[serde(default)]
    pub exhausted: bool,
}

impl Budget {
    /// The line `/quota` and the welcome screen show.
    pub fn summary(&self) -> String {
        if self.daily_limit_usd <= 0.0 {
            return format!("Free tier — {} · no daily limit", self.model);
        }
        format!(
            "Free tier — {} · ${:.4} of ${:.2} used today ({} request(s)), resets at UTC midnight",
            self.model, self.spent_usd, self.daily_limit_usd, self.requests
        )
    }
}

/// Whether this install should enrol.
///
/// A user who has configured their own key is never redirected: that key is a
/// deliberate choice, and silently proxying their traffic through our servers
/// would be a surprise they did not ask for.
pub fn should_register(config: &Config) -> bool {
    config.free_tier.enabled
        && config.free_tier.device_token.is_empty()
        && config.llm.api_key.is_empty()
}

/// True when the configured key is a free-tier device token, not a provider key.
pub fn is_free_tier(config: &Config) -> bool {
    config.free_tier.enabled
        && !config.free_tier.device_token.is_empty()
        && config.llm.api_key == config.free_tier.device_token
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("Could not create HTTP client: {e}"))
}

/// Register with the gateway and write the result into `config`.
pub async fn register(config: &mut Config) -> Result<Enrolment, String> {
    let gateway = config.free_tier.gateway.trim_end_matches('/').to_string();
    if gateway.is_empty() {
        return Err("No free-tier gateway configured.".to_string());
    }

    // Persisted so a machine with no readable hardware id still presents the
    // same identity next launch, rather than drawing a new budget each start.
    if config.free_tier.fallback_id.is_empty() {
        config.free_tier.fallback_id = device::random_fallback_id();
    }
    let device_id_hash = device::device_id_hash(&config.free_tier.fallback_id);

    let response = client()?
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
        return Err(format!("Free tier unavailable ({status}). {}", summarise_error(&body)));
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

    Ok(Enrolment { model: parsed.model, daily_limit_usd: parsed.daily_limit_usd })
}

/// Ask the gateway what this device has spent today.
///
/// Best-effort: a failure here must never stop the app starting or break
/// `/quota`, so callers fall back to showing the local counters alone.
pub async fn fetch_budget(config: &Config) -> Result<Budget, String> {
    let gateway = config.free_tier.gateway.trim_end_matches('/');
    if gateway.is_empty() || config.free_tier.device_token.is_empty() {
        return Err("Not enrolled in the free tier.".to_string());
    }

    let response = client()?
        .get(format!("{gateway}/me"))
        .bearer_auth(&config.free_tier.device_token)
        .send()
        .await
        .map_err(|e| format!("Could not reach the gateway: {e}"))?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("Gateway returned {status}. {}", summarise_error(&body)));
    }
    serde_json::from_str(&body).map_err(|e| format!("Unexpected response from the gateway: {e}"))
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn fresh() -> Config {
        Config::default()
    }

    #[test]
    fn a_fresh_install_with_no_key_enrols() {
        assert!(should_register(&fresh()));
    }

    /// Someone who configured their own key made a deliberate choice; their
    /// traffic must never be quietly routed through our gateway.
    #[test]
    fn an_install_with_its_own_api_key_is_left_alone() {
        let mut c = fresh();
        c.llm.api_key = "sk-the-users-own-key".to_string();
        assert!(!should_register(&c));
    }

    #[test]
    fn an_already_enrolled_device_does_not_enrol_again() {
        let mut c = fresh();
        c.free_tier.device_token = "dt_abc.def".to_string();
        assert!(!should_register(&c));
    }

    #[test]
    fn disabling_the_free_tier_stops_enrolment() {
        let mut c = fresh();
        c.free_tier.enabled = false;
        assert!(!should_register(&c));
    }

    #[test]
    fn is_free_tier_distinguishes_a_device_token_from_a_provider_key() {
        let mut c = fresh();
        c.free_tier.device_token = "dt_abc.def".to_string();
        c.llm.api_key = "dt_abc.def".to_string();
        assert!(is_free_tier(&c));
        // Pasting your own key later takes you off the free tier.
        c.llm.api_key = "sk-their-own".to_string();
        assert!(!is_free_tier(&c));
    }

    #[test]
    fn the_budget_line_states_spend_against_the_limit() {
        let b = Budget {
            model: "m".to_string(),
            daily_limit_usd: 0.25,
            spent_usd: 0.0021,
            requests: 3,
            ..Default::default()
        };
        let s = b.summary();
        assert!(s.contains("$0.0021"), "{s}");
        assert!(s.contains("$0.25"), "{s}");
        assert!(s.contains("3 request"), "{s}");
    }

    #[test]
    fn error_bodies_are_summarised_for_humans() {
        assert_eq!(
            summarise_error(r#"{"error":{"code":"x","message":"Too many devices."}}"#),
            "Too many devices."
        );
        assert_eq!(summarise_error("plain text failure"), "plain text failure");
        assert_eq!(summarise_error(""), "");
        assert!(summarise_error(&"x".repeat(10_000)).chars().count() <= 200);
    }

    /// Serve one canned HTTP response and hand back the request it received.
    async fn serve(status: &str, body: &str) -> (String, tokio::sync::oneshot::Receiver<String>) {
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
    async fn enrolling_stores_the_token_and_points_the_client_at_the_gateway() {
        let (addr, request) = serve(
            "200 OK",
            r#"{"device_token":"dt_ref.secret","model":"free-model","daily_limit_usd":0.25}"#,
        )
        .await;

        let mut c = fresh();
        c.free_tier.gateway = addr.clone();
        let e = register(&mut c).await.expect("enrolment should succeed");

        assert_eq!(e.model, "free-model");
        assert_eq!(e.daily_limit_usd, 0.25);
        assert_eq!(c.llm.api_key, "dt_ref.secret");
        assert_eq!(c.llm.endpoint, addr);
        assert!(is_free_tier(&c));

        // Only a hash is sent: no raw hardware id, nothing identifying.
        let sent = request.await.unwrap();
        let body = sent.split("\r\n\r\n").nth(1).unwrap_or("");
        let json: serde_json::Value = serde_json::from_str(body).expect("valid JSON body");
        let hash = json["device_id_hash"].as_str().unwrap();
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    /// A gateway that is down, blocked or slow must not stop the app starting.
    #[tokio::test]
    async fn an_unreachable_gateway_reports_an_error_rather_than_panicking() {
        let mut c = fresh();
        c.free_tier.gateway = "http://127.0.0.1:1".to_string(); // refuses instantly
        assert!(register(&mut c).await.is_err());
        // Nothing half-written.
        assert!(c.free_tier.device_token.is_empty());
        assert!(c.llm.api_key.is_empty());
    }

    #[tokio::test]
    async fn a_rejected_enrolment_surfaces_the_reason() {
        let (addr, _r) = serve(
            "429 Too Many Requests",
            r#"{"error":{"message":"Too many new devices from this network today."}}"#,
        )
        .await;
        let mut c = fresh();
        c.free_tier.gateway = addr;
        let err = register(&mut c).await.expect_err("should fail");
        assert!(err.contains("Too many new devices"), "{err}");
        assert!(c.free_tier.device_token.is_empty());
    }

    #[tokio::test]
    async fn an_empty_or_garbled_response_is_rejected_rather_than_stored() {
        for body in [r#"{"device_token":"   "}"#, "not json at all"] {
            let (addr, _r) = serve("200 OK", body).await;
            let mut c = fresh();
            c.free_tier.gateway = addr;
            assert!(register(&mut c).await.is_err(), "{body}");
            assert!(c.llm.api_key.is_empty());
        }
    }

    #[tokio::test]
    async fn the_budget_is_read_from_the_gateway() {
        let (addr, _r) = serve(
            "200 OK",
            r#"{"model":"m","daily_limit_usd":0.25,"spent_usd":0.0021,"requests":3,"tokens":500,"exhausted":false}"#,
        )
        .await;
        let mut c = fresh();
        c.free_tier.gateway = addr;
        c.free_tier.device_token = "dt_a.b".to_string();

        let b = fetch_budget(&c).await.expect("budget");
        assert_eq!(b.daily_limit_usd, 0.25);
        assert_eq!(b.requests, 3);
        assert!(!b.exhausted);
    }

    #[tokio::test]
    async fn a_budget_lookup_that_fails_is_an_error_not_a_panic() {
        let mut c = fresh();
        c.free_tier.gateway = "http://127.0.0.1:1".to_string();
        c.free_tier.device_token = "dt_a.b".to_string();
        assert!(fetch_budget(&c).await.is_err());

        // ...and an install that never enrolled has nothing to ask about.
        let plain = fresh();
        assert!(fetch_budget(&plain).await.is_err());
    }

    /// The fallback id is persisted so a machine with no hardware id keeps one
    /// identity instead of drawing a fresh budget every launch.
    #[tokio::test]
    async fn a_fallback_id_is_generated_once_and_kept() {
        let (addr, _r) = serve("200 OK", r#"{"device_token":"dt_a.b","model":"m"}"#).await;
        let mut c = fresh();
        c.free_tier.gateway = addr;
        register(&mut c).await.expect("ok");
        let first = c.free_tier.fallback_id.clone();
        assert!(!first.is_empty());

        let (addr2, _r2) = serve("200 OK", r#"{"device_token":"dt_c.d","model":"m"}"#).await;
        c.free_tier.gateway = addr2;
        register(&mut c).await.expect("ok");
        assert_eq!(c.free_tier.fallback_id, first, "must not be regenerated");
    }
}
