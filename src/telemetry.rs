//! Anonymous "an install exists" / "an install was used today" pings.
//!
//! There is no login in this app, so there is no user identity to attach
//! anything to. What this sends instead is `device_id`: a random ID
//! generated once per install, written to disk, and reused from then on --
//! it labels a machine, not a person. Two events only:
//!
//! - `install`: fired once by `install.sh` itself (see that script's copy of
//!   this device-id logic, in bash, since it runs before this binary exists
//!   on disk at all) -- both on a fresh install and on every `--upgrade`.
//! - `active`: fired by this binary at most once per UTC calendar day, so a
//!   long-running session doesn't inflate the count and a normal day of use
//!   pings exactly once.
//!
//! Nothing else leaves the machine from here: no conversation content, no
//! file paths, no command text, no prompts. See `usage.rs` for the separate,
//! purely local (never transmitted) per-install usage log.
//!
//! Points at a Cloudflare Worker (see `telemetry-worker.js` in the repo root)
//! that logs each ping to Workers KV and serves a public HTML view of the
//! aggregate counts at the same URL over GET -- no login collects it, so
//! nothing here is more sensitive than what that page already shows anyone.
//! `BOXCODE_TELEMETRY_URL` overrides `DEFAULT_TELEMETRY_URL` below, e.g. to
//! point a fork or a local test run at a different endpoint. A blank value
//! either way disables sending entirely -- every function in this module is a
//! silent no-op in that case, by construction, not by remembering to check a
//! flag.

use crate::dateutil;
use std::path::PathBuf;
use std::time::Duration;

const TELEMETRY_URL_ENV: &str = "BOXCODE_TELEMETRY_URL";
const DEFAULT_TELEMETRY_URL: &str = "https://tui-telemetry.dhruvm307.workers.dev";

fn telemetry_url() -> Option<String> {
    telemetry_url_given(std::env::var(TELEMETRY_URL_ENV).ok().as_deref())
}

/// The actual decision, taking the env override as a plain `Option<&str>`
/// rather than reading the environment directly so it's testable without
/// mutating real process state -- same reasoning as `theme.rs`'s
/// `supports_truecolor_given`.
///
/// `None` (the env var was never set) and `Some("")` (it was set, but
/// blank) must be handled differently: only the former falls back to
/// `DEFAULT_TELEMETRY_URL`. An explicit blank override is a deliberate
/// "disable this," and must win even over a non-blank default -- a `.filter`
/// after a single `.or_else` can't tell those two cases apart, which is
/// exactly the bug this match avoids.
fn telemetry_url_given(env_override: Option<&str>) -> Option<String> {
    let candidate = match env_override {
        Some(url) => url,
        None => DEFAULT_TELEMETRY_URL,
    };
    let trimmed = candidate.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn state_dir() -> Option<PathBuf> {
    // Via `config`, not derived here: that is the one place that inherits the
    // pre-rename directory, and a module that builds the path itself would
    // create `~/.boxcode` first and quietly strand the old one.
    Some(crate::config::Config::config_dir())
}

fn device_id_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("device_id"))
}

fn last_active_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("last_active"))
}

/// The anonymous per-install ID, creating one if this is the first time this
/// binary (not `install.sh`) has looked for it. Reads the existing file
/// first rather than generating a second, conflicting ID -- `install.sh` may
/// already have created one for its own `install` ping.
fn device_id() -> Option<String> {
    let path = device_id_path()?;
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let id = random_id();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &id);
    Some(id)
}

/// 128 bits as hex. Not cryptographically secured, and doesn't need to be --
/// this only ever labels an anonymous count, never protects anything.
fn random_id() -> String {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let mut bytes = [0u8; 16];
            // read_exact, not std::fs::read: /dev/urandom never reaches EOF,
            // so a whole-file read would hang forever.
            if f.read_exact(&mut bytes).is_ok() {
                return hex(&bytes);
            }
        }
    }
    // Windows, or the unlikely case /dev/urandom couldn't be opened: not
    // random in a security sense, but unique enough to count installs by --
    // the same "doesn't need to be secure" reasoning as above.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ (std::process::id() as u128);
    hex(&seed.to_le_bytes())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fires the `active` ping if (and only if) it hasn't already fired today.
/// Every failure mode -- no telemetry URL configured, no `$HOME`, no
/// network, endpoint unreachable or erroring -- is swallowed silently.
/// Telemetry must never be something a user notices, let alone something
/// that slows down or blocks actually using the app; call this as a detached
/// background task, never awaited on the startup path.
pub async fn ping_active_if_new_day(version: &str) {
    let Some(url) = telemetry_url() else { return };
    let Some(id) = device_id() else { return };
    let Some(path) = last_active_path() else { return };

    let today = dateutil::today_string();
    let already_pinged = std::fs::read_to_string(&path)
        .map(|s| s.trim() == today)
        .unwrap_or(false);
    if already_pinged {
        return;
    }

    let body = serde_json::json!({
        "anon_id": id,
        "event": "active",
        "version": version,
        "os": std::env::consts::OS,
        "date": today,
    });

    let Ok(client) = reqwest::Client::builder().timeout(Duration::from_secs(5)).build() else {
        return;
    };
    if client.post(&url).json(&body).send().await.is_ok() {
        let _ = std::fs::write(&path, &today);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_id_produces_32_hex_characters_and_is_not_constant() {
        let a = random_id();
        let b = random_id();
        assert_eq!(a.len(), 32, "{a:?} should be 16 bytes as hex");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a:?}");
        assert_ne!(a, b, "two calls must not collide");
    }

    #[test]
    fn the_default_endpoint_is_a_well_formed_https_url() {
        assert!(DEFAULT_TELEMETRY_URL.starts_with("https://"), "{DEFAULT_TELEMETRY_URL}");
    }

    #[test]
    fn with_no_env_override_the_default_endpoint_is_used() {
        assert_eq!(telemetry_url_given(None), Some(DEFAULT_TELEMETRY_URL.to_string()));
    }

    #[test]
    fn an_env_override_takes_precedence_over_the_default() {
        assert_eq!(
            telemetry_url_given(Some("https://example.test/other")),
            Some("https://example.test/other".to_string())
        );
    }

    /// A fork or a local test run has to be able to turn this off even
    /// though the shipped default is now a real endpoint -- an explicitly
    /// blank override must win, not fall back to the default.
    #[test]
    fn an_explicit_blank_env_override_disables_sending_despite_a_real_default() {
        assert_eq!(telemetry_url_given(Some("")), None);
        assert_eq!(telemetry_url_given(Some("   ")), None);
    }
}
