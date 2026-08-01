//! `tuisample-code --upgrade` — pull the latest build without having to
//! remember (or re-paste) the curl one-liner.
//!
//! The install itself is delegated to `install.sh` fetched from the repo, not
//! reimplemented here. That script already knows how to pick an install
//! directory, escalate with sudo, sweep stale copies off `$PATH`, and verify
//! what the shell actually resolves to. A second install path written in Rust
//! would only drift away from it.

use std::error::Error;
use std::process::Command;
use std::time::Duration;

const REPO: &str = "HolboxAI/tuisample-code";
const BRANCH: &str = "main";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Where to fetch `Cargo.toml` and `install.sh` from. Overridable so a fork or
/// an internal mirror (this tool is often run somewhere with no route to
/// github.com) can serve its own builds.
const URL_BASE_ENV: &str = "TUISAMPLE_UPGRADE_URL_BASE";

fn join_url(base: &str, path: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), path)
}

fn base_url() -> String {
    std::env::var(URL_BASE_ENV)
        .ok()
        .filter(|base| !base.trim().is_empty())
        .unwrap_or_else(|| format!("https://raw.githubusercontent.com/{REPO}/{BRANCH}"))
}

fn raw_url(path: &str) -> String {
    join_url(&base_url(), path)
}

pub async fn run(force: bool) -> Result<(), Box<dyn Error>> {
    println!("🔎 Checking {} for a newer version...", base_url());

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .user_agent(format!("tuisample-code/{CURRENT}"))
        .build()?;

    match fetch_latest_version(&client).await {
        Ok(latest) if version_is_newer(&latest, CURRENT) => {
            println!("⬆️  {CURRENT} → {latest}");
        }
        Ok(latest) if force => {
            println!("✓ Already on {latest}. Reinstalling anyway (--force).");
        }
        Ok(_) => {
            println!("✓ Already up to date ({CURRENT}). Nothing to do.");
            println!();
            println!("main can also carry changes that haven't been given a new version");
            println!("number yet. To rebuild from the latest source regardless, run:");
            println!("    tuisample-code --upgrade --force");
            return Ok(());
        }
        Err(e) => {
            // An unreachable network or a moved file shouldn't block an
            // explicit upgrade request — go ahead and let the installer itself
            // fail loudly if it genuinely can't run.
            eprintln!("⚠️  Could not determine the latest version ({e}). Installing anyway.");
        }
    }

    println!("📥 Fetching the installer...");
    let script = fetch_installer(&client).await?;

    let script_path =
        std::env::temp_dir().join(format!("tuisample-code-install-{}.sh", std::process::id()));
    std::fs::write(&script_path, script)?;
    println!();

    let status = Command::new("bash").arg(&script_path).status();
    let _ = std::fs::remove_file(&script_path);

    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("installer exited with {status}").into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err("--upgrade needs `bash` on PATH, and it wasn't found. \
                 Reinstall manually: https://github.com/HolboxAI/tuisample-code"
                .into())
        }
        Err(e) => Err(Box::new(e)),
    }
}

async fn fetch_latest_version(client: &reqwest::Client) -> Result<String, Box<dyn Error>> {
    let response = client.get(raw_url("Cargo.toml")).send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} fetching Cargo.toml").into());
    }
    let body = response.text().await?;
    parse_package_version(&body).ok_or_else(|| "no [package] version in Cargo.toml".into())
}

async fn fetch_installer(client: &reqwest::Client) -> Result<String, Box<dyn Error>> {
    let url = raw_url("install.sh");
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("could not download {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} fetching {url}").into());
    }
    let body = response.text().await?;
    if !looks_like_installer(&body) {
        return Err("downloaded install.sh does not look like the installer".into());
    }
    Ok(body)
}

/// Never hand an unexpected body to bash — a captive-portal login page or an
/// error blob would otherwise be executed as a shell script.
fn looks_like_installer(body: &str) -> bool {
    body.starts_with("#!") && body.contains("tuisample-code")
}

/// Reads `version` from the `[package]` table only — a `version = "..."` under
/// `[dependencies]` must not be mistaken for the crate's own version.
fn parse_package_version(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = line.strip_prefix("version") {
            if let Some(value) = rest.trim_start().strip_prefix('=') {
                return Some(value.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

fn parse_semver(version: &str) -> Option<(u64, u64, u64)> {
    // Ignore any pre-release/build suffix: 1.2.3-rc1 compares as 1.2.3.
    let core = version.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

fn version_is_newer(candidate: &str, current: &str) -> bool {
    match (parse_semver(candidate), parse_semver(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        // Unparseable on either side: treat "a different string" as newer, so an
        // unusual version scheme still offers an upgrade instead of pinning the
        // user to their current build forever.
        _ => candidate.trim() != current.trim(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn reads_the_version_from_the_package_table() {
        let manifest = "\
[package]
name = \"tuisample-code\"
version = \"0.3.0\"
edition = \"2021\"
";
        assert_eq!(parse_package_version(manifest).as_deref(), Some("0.3.0"));
    }

    #[test]
    fn ignores_dependency_versions() {
        // The bug this guards: `version` under [dependencies] appears *after*
        // the package one in a real manifest, and matching greedily would
        // report a dependency's version as the app's.
        let manifest = "\
[package]
name = \"tuisample-code\"
version = \"0.3.0\"

[dependencies]
tokio = { version = \"1.35\" }
version = \"9.9.9\"
";
        assert_eq!(parse_package_version(manifest).as_deref(), Some("0.3.0"));
    }

    #[test]
    fn missing_package_version_is_none() {
        assert_eq!(parse_package_version("[dependencies]\nversion = \"1.0\"\n"), None);
        assert_eq!(parse_package_version(""), None);
    }

    #[test]
    fn parses_semver_variants() {
        assert_eq!(parse_semver("0.2.0"), Some((0, 2, 0)));
        assert_eq!(parse_semver("v1.4.9"), Some((1, 4, 9)));
        assert_eq!(parse_semver("1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_semver("2.1"), Some((2, 1, 0)));
        assert_eq!(parse_semver("not-a-version"), None);
    }

    #[test]
    fn compares_versions_numerically() {
        assert!(version_is_newer("0.3.0", "0.2.0"));
        assert!(version_is_newer("0.2.1", "0.2.0"));
        assert!(version_is_newer("1.0.0", "0.99.99"));
        // 10 > 9 numerically, but "0.10.0" < "0.9.0" as strings.
        assert!(version_is_newer("0.10.0", "0.9.0"));
    }

    #[test]
    fn same_or_older_version_is_not_newer() {
        assert!(!version_is_newer("0.2.0", "0.2.0"));
        assert!(!version_is_newer("0.1.0", "0.2.0"));
        assert!(!version_is_newer("0.9.0", "0.10.0"));
    }

    #[test]
    fn unparseable_versions_fall_back_to_string_inequality() {
        assert!(version_is_newer("nightly-2", "nightly-1"));
        assert!(!version_is_newer("nightly-1", "nightly-1"));
    }

    #[test]
    fn urls_join_without_doubling_the_slash() {
        let github = format!("https://raw.githubusercontent.com/{REPO}/{BRANCH}");
        assert_eq!(
            join_url(&github, "install.sh"),
            "https://raw.githubusercontent.com/HolboxAI/tuisample-code/main/install.sh"
        );
        assert_eq!(join_url("http://mirror.internal/", "Cargo.toml"), "http://mirror.internal/Cargo.toml");
    }

    #[test]
    fn only_a_real_installer_reaches_bash() {
        assert!(looks_like_installer("#!/bin/bash\ninstall tuisample-code\n"));
        // What a captive portal or an error page would return.
        assert!(!looks_like_installer("<html><body>Sign in to continue</body></html>"));
        assert!(!looks_like_installer("404: Not Found"));
        // Right shebang, wrong script.
        assert!(!looks_like_installer("#!/bin/bash\nrm -rf /\n"));
        assert!(!looks_like_installer(""));
    }

    /// Serve `Cargo.toml` and `install.sh` on an ephemeral port.
    async fn serve_repo(manifest: String, installer: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let Ok(n) = socket.read(&mut buf).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if request.contains("/Cargo.toml") {
                    manifest.clone()
                } else {
                    installer.clone()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    /// Every phase lives in one test on purpose: it sets a process-wide env var,
    /// and cargo runs tests in thread-parallel, so two tests racing on it would
    /// be flaky.
    #[tokio::test]
    async fn upgrade_end_to_end() {
        let marker =
            std::env::temp_dir().join(format!("tuisample-upgrade-test-{}", std::process::id()));
        let installer = format!(
            "#!/bin/bash\n# stand-in for the tuisample-code installer\necho ran > \"{}\"\n",
            marker.display()
        );
        let manifest = |version: &str| format!("[package]\nname = \"tuisample-code\"\nversion = \"{version}\"\n");
        let ran = || marker.exists();
        let reset = || {
            let _ = std::fs::remove_file(&marker);
        };

        // A newer version upstream: fetch the installer and run it.
        reset();
        let base = serve_repo(manifest("99.0.0"), installer.clone()).await;
        std::env::set_var(URL_BASE_ENV, &base);
        run(false).await.expect("upgrade should succeed");
        assert!(ran(), "a newer version should have run the installer");

        // Already current: nothing should be installed.
        reset();
        let base = serve_repo(manifest(CURRENT), installer.clone()).await;
        std::env::set_var(URL_BASE_ENV, &base);
        run(false).await.expect("up-to-date check should succeed");
        assert!(!ran(), "an up-to-date build must not reinstall");

        // ...unless --force is given.
        reset();
        let base = serve_repo(manifest(CURRENT), installer.clone()).await;
        std::env::set_var(URL_BASE_ENV, &base);
        run(true).await.expect("forced upgrade should succeed");
        assert!(ran(), "--force should reinstall even when up to date");

        // A body that isn't the installer must never be executed.
        reset();
        let base = serve_repo(manifest("99.0.0"), "<html>Sign in</html>".to_string()).await;
        std::env::set_var(URL_BASE_ENV, &base);
        let result = run(false).await;
        assert!(result.is_err(), "a non-installer body should be rejected");
        assert!(!ran(), "a rejected body must not be executed");

        reset();
        std::env::remove_var(URL_BASE_ENV);
    }
}
