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
//! Disabled by default. `telemetry_url()` returns `None` -- and every
//! function here silently does nothing -- until either `DEFAULT_TELEMETRY_URL`
//! is filled in at build time or `TUISAMPLE_TELEMETRY_URL` is set at runtime.

use crate::dateutil;
use std::path::PathBuf;
use std::time::Duration;

const TELEMETRY_URL_ENV: &str = "TUISAMPLE_TELEMETRY_URL";
/// Filled in once a real endpoint exists. Blank means disabled: every
/// caller in this module treats "unset env var and blank default" as "don't
/// send anything", so builds before an endpoint exists are silent by
/// construction, not by remembering to flip a flag.
const DEFAULT_TELEMETRY_URL: &str = "";

fn telemetry_url() -> Option<String> {
    let from_env = std::env::var(TELEMETRY_URL_ENV).ok();
    from_env
        .filter(|u| !u.trim().is_empty())
        .or_else(|| Some(DEFAULT_TELEMETRY_URL.to_string()))
        .filter(|u| !u.trim().is_empty())
}

fn state_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".tuisample-code"))
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
    fn a_blank_default_and_unset_env_var_means_disabled() {
        // This is the state every build ships in until DEFAULT_TELEMETRY_URL
        // is filled in -- locking it in as a test, not just a doc comment,
        // so a future accidental non-empty default gets caught by CI rather
        // than silently starting to send pings.
        assert!(DEFAULT_TELEMETRY_URL.trim().is_empty());
    }
}
