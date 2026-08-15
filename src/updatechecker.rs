// Port of the update checker (src/picotorrent/updatechecker.cpp) and
// bittorrent/semver.hpp version comparison.
//
// The original polled api.picotorrent.org for `{version, url}`. That host is
// gone, so the endpoint (`update_checks.url`) now points at this project's
// GitHub releases API and we read GitHub's shape instead: `tag_name` and
// `html_url`. `/releases/latest` never returns drafts or prereleases, so
// anything it hands back is a real release. Pointing the setting at another
// repo's `/releases/latest` works unchanged.

use std::sync::{Arc, Mutex};

use crate::core::configuration::Configuration;

pub struct UpdateInfo {
    pub version: String,
    pub url: String,
}

/// Compare dotted version strings, e.g. "0.26.0" < "0.27.1".
fn is_newer(remote: &str, local: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split('.')
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    };

    let r = parse(remote);
    let l = parse(local);
    let len = r.len().max(l.len());

    for i in 0..len {
        let rv = r.get(i).copied().unwrap_or(0);
        let lv = l.get(i).copied().unwrap_or(0);
        if rv != lv {
            return rv > lv;
        }
    }

    false
}

/// Spawns the update check on the session's tokio runtime; the result is
/// delivered through the shared slot which the UI polls.
pub fn check(
    handle: &tokio::runtime::Handle,
    cfg: &Configuration,
    slot: Arc<Mutex<Option<UpdateInfo>>>,
) {
    if !cfg.get_bool("update_checks.enabled") {
        return;
    }

    let Some(url) = cfg.get_string("update_checks.url") else {
        return;
    };

    let ignored = cfg
        .get_string("update_checks.ignored_version")
        .unwrap_or_default();

    handle.spawn(async move {
        let client = match reqwest::Client::builder()
            .user_agent(crate::buildinfo::user_agent())
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };

        let Ok(response) = client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
        else {
            return;
        };

        // A repo with no releases yet answers 404, and an over-quota client gets
        // 403 - both come back as JSON that would otherwise parse into "no new
        // version", which is right but silent. Say which it was.
        if !response.status().is_success() {
            tracing::info!("update check: {} returned {}", url, response.status());
            return;
        }

        let Ok(json) = response.json::<serde_json::Value>().await else {
            return;
        };

        // Release tags are conventionally "v0.1.2"; carry the bare number so the
        // UI's own "v" prefix doesn't double up.
        let version = json
            .get("tag_name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim_start_matches('v')
            .to_string();
        let dl_url = json
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or("https://www.nanotorrent.org")
            .to_string();

        if !version.is_empty()
            && version != ignored
            && is_newer(&version, crate::buildinfo::version())
        {
            tracing::info!("update available: {version} ({dl_url})");
            *slot.lock().unwrap() = Some(UpdateInfo {
                version,
                url: dl_url,
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn compares_releases() {
        assert!(is_newer("0.1.2", "0.1.1"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));

        // GitHub tags carry a "v"; a shorter tag is not automatically older.
        assert!(is_newer("v0.1.2", "0.1.1"));
        assert!(is_newer("0.2", "0.1.9"));
        assert!(!is_newer("0.1", "0.1.0"));

        // Trailing junk on a component must not read as a bump.
        assert!(!is_newer("0.1.1-rc1", "0.1.1"));
    }
}
