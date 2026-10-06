//! New-version notifications: the one network call calvin makes.
//!
//! Only the background process checks, at most once a day, by asking GitHub for the latest
//! release of calvin. The request carries no data about you or your usage. The answer is
//! cached in the data directory, so `calvin status`, `calvin doctor` and the dashboard can
//! show it without going online themselves.
//!
//! Turn it off with `[updates] check = false` in config.toml, or `CALVIN_NO_UPDATE_CHECK=1`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const REPO: &str = "jmelosegui/calvin";
const CHECK_EVERY: chrono::Duration = chrono::Duration::hours(24);
/// A failed check (offline, VPN, DNS) is retried sooner, so it can't hide a release for a day.
const RETRY_AFTER_ERROR: chrono::Duration = chrono::Duration::hours(1);
const CACHE_FILE: &str = "update-check.json";

pub fn enabled(cfg_check: bool) -> bool {
    cfg_check && std::env::var_os("CALVIN_NO_UPDATE_CHECK").is_none()
}

fn latest_release_url() -> String {
    std::env::var("CALVIN_UPDATE_URL")
        .unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}/releases/latest"))
}

/// What the last check found. Stored as `update-check.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckResult {
    pub checked_at: DateTime<Utc>,
    pub latest: Option<String>,
    pub url: Option<String>,
    pub error: Option<String>,
}

/// An available update, for display.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Available {
    pub current: String,
    pub latest: String,
    pub url: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `v0.2.0` → `0.2.0`; anything that isn't a version is ignored.
fn parse_tag(tag: &str) -> Option<semver::Version> {
    semver::Version::parse(tag.trim().trim_start_matches('v')).ok()
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_tag(latest), parse_tag(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// The update to show, if the cached check found a newer version than this binary.
pub fn available(data_dir: &Path) -> Option<Available> {
    let cached = read_cache(data_dir)?;
    let (latest, url) = (cached.latest?, cached.url?);
    is_newer(&latest, current_version()).then(|| Available {
        current: current_version().to_string(),
        latest: latest.trim_start_matches('v').to_string(),
        url,
    })
}

pub fn read_cache(data_dir: &Path) -> Option<CheckResult> {
    let text = std::fs::read_to_string(data_dir.join(CACHE_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_cache(data_dir: &Path, result: &CheckResult) -> Result<()> {
    std::fs::write(
        data_dir.join(CACHE_FILE),
        serde_json::to_vec_pretty(result)?,
    )?;
    Ok(())
}

fn is_due(cached: &CheckResult, now: DateTime<Utc>) -> bool {
    let wait = if cached.error.is_some() {
        RETRY_AFTER_ERROR
    } else {
        CHECK_EVERY
    };
    now - cached.checked_at >= wait
}

/// Check GitHub if the last check is older than a day (an hour if it failed).
/// Blocking; call from a worker thread.
pub fn check_if_due(data_dir: &Path) -> Result<CheckResult> {
    if let Some(cached) = read_cache(data_dir)
        && !is_due(&cached, Utc::now())
    {
        return Ok(cached);
    }
    let result = match fetch_latest() {
        Ok(release) => CheckResult {
            checked_at: Utc::now(),
            latest: Some(release.tag_name),
            url: Some(release.html_url),
            error: None,
        },
        // Keep the last known version so a flaky network doesn't hide a real update.
        Err(e) => {
            let previous = read_cache(data_dir);
            CheckResult {
                checked_at: Utc::now(),
                latest: previous.as_ref().and_then(|p| p.latest.clone()),
                url: previous.and_then(|p| p.url),
                error: Some(format!("{e:#}")),
            }
        }
    };
    write_cache(data_dir, &result)?;
    Ok(result)
}

fn fetch_latest() -> Result<Release> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let release: Release = agent
        .get(&latest_release_url())
        .header("User-Agent", &format!("calvin/{}", current_version()))
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("asking GitHub for the latest release")?
        .body_mut()
        .read_json()
        .context("reading GitHub's answer")?;
    anyhow::ensure!(
        !release.draft && !release.prerelease,
        "latest release is not a final release"
    );
    Ok(release)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_not_strings() {
        assert!(is_newer("v0.10.0", "0.9.0"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
        assert!(!is_newer("nightly", "0.1.0"));
        // A pre-release of the current version is older than the release.
        assert!(!is_newer("v0.2.0-rc.1", "0.2.0"));
    }

    #[test]
    fn failed_checks_retry_sooner() {
        let now = Utc::now();
        let ok = CheckResult {
            checked_at: now - chrono::Duration::hours(2),
            latest: Some("v0.1.0".into()),
            url: None,
            error: None,
        };
        assert!(!is_due(&ok, now));
        assert!(is_due(
            &CheckResult {
                checked_at: now - chrono::Duration::hours(25),
                ..ok.clone()
            },
            now
        ));
        let failed = CheckResult {
            error: Some("no such host".into()),
            ..ok.clone()
        };
        assert!(is_due(&failed, now));
        assert!(!is_due(
            &CheckResult {
                checked_at: now - chrono::Duration::minutes(30),
                ..failed
            },
            now
        ));
    }

    #[test]
    fn available_reads_the_cache() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(available(tmp.path()), None);
        let result = CheckResult {
            checked_at: Utc::now(),
            latest: Some("v99.0.0".into()),
            url: Some("https://github.com/jmelosegui/calvin/releases/tag/v99.0.0".into()),
            error: None,
        };
        write_cache(tmp.path(), &result).unwrap();
        let a = available(tmp.path()).unwrap();
        assert_eq!(a.latest, "99.0.0");
        assert_eq!(a.current, current_version());

        write_cache(
            tmp.path(),
            &CheckResult {
                latest: Some(format!("v{}", current_version())),
                ..result
            },
        )
        .unwrap();
        assert_eq!(available(tmp.path()), None);
    }
}
