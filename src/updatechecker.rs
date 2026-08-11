// Port of the update checker (src/picotorrent/updatechecker.cpp) and
// bittorrent/semver.hpp version comparison.

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

        let Ok(response) = client.get(&url).send().await else {
            return;
        };

        let Ok(json) = response.json::<serde_json::Value>().await else {
            return;
        };

        let version = json
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let dl_url = json
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("https://www.nanotorrent.org")
            .to_string();

        if !version.is_empty()
            && version != ignored
            && is_newer(&version, crate::buildinfo::version())
        {
            *slot.lock().unwrap() = Some(UpdateInfo {
                version,
                url: dl_url,
            });
        }
    });
}
