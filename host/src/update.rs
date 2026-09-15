//! Optional update checker.
//!
//! Fetches a small JSON manifest from `settings.update.check_url` on a
//! timer, compares its `version` against the running build, and — if newer —
//! surfaces it in the tray. It never downloads or runs anything itself: the
//! user clicks through to the download page/installer in their browser, so
//! there is no self-modifying code and no silent binary replacement.
//!
//! Disabled by default (`check_url: None`): a host that hasn't opted in makes
//! no network calls for this at all.

use rc_common::AppPaths;
use serde::Deserialize;
use tokio::sync::watch;

use crate::settings::Settings;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// e.g. `"0.2.0"`.
    pub version: String,
    /// Where the user goes to get it (installer download or a releases page).
    pub url: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    pub notes: String,
}

/// Spawn the background checker. Returns a [`watch`] that publishes `Some`
/// the first time a newer version is seen (and stays there — we only ever
/// notify once per newer version per process lifetime).
///
/// Re-reads `config.toml` itself on every tick rather than sharing state with
/// the core engine, so a config edit (or a fresh `check_url`) takes effect
/// without any extra plumbing.
pub fn spawn(paths: AppPaths) -> watch::Receiver<Option<UpdateInfo>> {
    let (tx, rx) = watch::channel(None);

    tokio::spawn(async move {
        let current = env!("CARGO_PKG_VERSION");
        loop {
            let settings = Settings::load(&paths.config_file()).unwrap_or_default();
            let Some(url) = settings.update.check_url.clone().filter(|u| !u.trim().is_empty()) else {
                // Not configured — check again only if settings change later
                // (e.g. the user sets a URL), polling slowly in the meantime.
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                continue;
            };
            let interval_hours = settings.update.check_interval_hours.max(1);

            match tokio::task::spawn_blocking(move || check_once(&url)).await {
                Ok(Ok(Some(info))) if is_newer(&info.version, current) => {
                    tracing::info!(new_version = %info.version, current, "update available");
                    let _ = tx.send(Some(info));
                }
                Ok(Ok(_)) => tracing::debug!("update check: already current"),
                Ok(Err(e)) => tracing::debug!(error = %e, "update check failed"),
                Err(e) => tracing::debug!(error = %e, "update check task panicked"),
            }

            tokio::time::sleep(std::time::Duration::from_secs(
                u64::from(interval_hours) * 3600,
            ))
            .await;
        }
    });

    rx
}

fn check_once(url: &str) -> anyhow::Result<Option<UpdateInfo>> {
    let manifest: Manifest = ureq::get(url)
        .timeout(std::time::Duration::from_secs(10))
        .call()?
        .into_json()?;
    Ok(Some(UpdateInfo {
        version: manifest.version,
        url: manifest.url,
        notes: manifest.notes,
    }))
}

/// Simple `major.minor.patch` comparison — good enough for our own releases,
/// and never panics on a malformed string (treats missing/non-numeric
/// components as `0`).
fn is_newer(candidate: &str, current: &str) -> bool {
    parse_version(candidate) > parse_version(current)
}

fn parse_version(s: &str) -> (u32, u32, u32) {
    let mut parts = s.trim_start_matches('v').split('.');
    let mut next = || parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (next(), next(), next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }
}
