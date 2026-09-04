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

/// What a finished check found.
///
/// The check used to write the slot only when there was something newer, which
/// is all the startup check needs. A check the user asked for has to be able to
/// say "you are up to date" and "I could not reach GitHub" as well - a menu
/// item that silently does nothing four times out of five is worse than none.
pub struct Report {
    pub update: Option<UpdateInfo>,
    pub error: Option<String>,
    /// True when this came from Help > Check for update. Only then are the
    /// quiet outcomes worth showing: nobody wants "no update available" every
    /// time the application opens.
    pub manual: bool,
}

/// Where a finished check leaves its [`Report`]. The UI polls it.
pub type Slot = Arc<Mutex<Option<Report>>>;

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

/// Where "Download" should send whoever is running *this* build.
///
/// A Microsoft Store copy must not be pointed at the GitHub release page. The
/// NSIS installer does not upgrade an MSIX install - it cannot see it - so
/// running it leaves two NanoTorrents on the machine, each with its own
/// settings folder, and the one on the Start menu is then a coin toss. Windows
/// tells a packaged process its own package family name, so send that one to
/// its own Store page and leave every other install pointed at the release it
/// came from.
///
/// Nothing is suppressed either way: a Store user still learns that a new
/// version exists, they are just sent somewhere that upgrades what they have.
fn download_url(release_url: String, package_family: Option<String>) -> String {
    match package_family {
        Some(pfn) => format!("ms-windows-store://pdp/?PFN={pfn}"),
        None => release_url,
    }
}

/// Spawns the update check on the session's tokio runtime; the result is
/// delivered through the shared slot which the UI polls.
pub fn check(handle: &tokio::runtime::Handle, cfg: &Configuration, slot: Slot, manual: bool) {
    // The preference governs the check at startup. Asking for one from the
    // menu is an explicit request and runs either way - otherwise the menu
    // item would appear to do nothing for anyone who turned the setting off.
    if !manual && !cfg.get_bool("update_checks.enabled") {
        return;
    }

    let Some(url) = cfg.get_string("update_checks.url") else {
        report(&slot, None, Some(String::from("no update URL is configured")), manual);
        return;
    };

    // A version dismissed with "Ignore this update" stays dismissed for the
    // automatic check only. Asking directly overrides it.
    let ignored = if manual {
        String::new()
    } else {
        cfg.get_string("update_checks.ignored_version")
            .unwrap_or_default()
    };

    // Built out here, not inside the task: `cfg` is a borrow and the task is
    // 'static. Through the proxy when one is set - someone who routes their
    // torrents through a proxy did not mean to exempt a request to GitHub that
    // goes out from their own address every time the app starts.
    let client = match crate::core::http::client(cfg) {
        Ok(c) => c,
        Err(err) => {
            report(&slot, None, Some(err.to_string()), manual);
            return;
        }
    };

    handle.spawn(async move {

        let response = match client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
        {
            Ok(r) => r,
            Err(err) => {
                report(&slot, None, Some(err.to_string()), manual);
                return;
            }
        };

        // A repo with no releases yet answers 404, and an over-quota client gets
        // 403 - both come back as JSON that would otherwise parse into "no new
        // version", which is right but silent. Say which it was.
        if !response.status().is_success() {
            let status = response.status();
            tracing::info!("update check: {url} returned {status}");
            report(&slot, None, Some(format!("{url} returned {status}")), manual);
            return;
        }

        let json = match response.json::<serde_json::Value>().await {
            Ok(j) => j,
            Err(err) => {
                report(&slot, None, Some(err.to_string()), manual);
                return;
            }
        };

        // Release tags are conventionally "v0.1.2"; carry the bare number so the
        // UI's own "v" prefix doesn't double up.
        let version = json
            .get("tag_name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim_start_matches('v')
            .to_string();
        let dl_url = download_url(
            json.get("html_url")
                .and_then(|v| v.as_str())
                .unwrap_or("https://www.nanotorrent.org")
                .to_string(),
            crate::core::environment::package_family_name(),
        );

        if !version.is_empty()
            && version != ignored
            && is_newer(&version, crate::buildinfo::version())
        {
            tracing::info!("update available: {version} ({dl_url})");
            report(
                &slot,
                Some(UpdateInfo {
                    version,
                    url: dl_url,
                }),
                None,
                manual,
            );
        } else {
            report(&slot, None, None, manual);
        }
    });
}

/// Leave a report in the slot, unless the automatic check found nothing worth
/// saying - the UI would only have to throw that away.
fn report(slot: &Slot, update: Option<UpdateInfo>, error: Option<String>, manual: bool) {
    if !manual && update.is_none() {
        return;
    }
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(Report {
            update,
            error,
            manual,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{download_url, is_newer};

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

    /// The whole point of `download_url`: an MSIX install sent to the GitHub
    /// release page ends up with a second NanoTorrent beside it rather than an
    /// upgraded one.
    #[test]
    fn a_store_install_is_sent_to_the_store() {
        let release = String::from("https://github.com/Power2All/nanotorrent/releases/tag/v0.3.1");

        assert_eq!(download_url(release.clone(), None), release);
        assert_eq!(
            download_url(
                release,
                Some(String::from("Power2All.NanoTorrent_jsrm4ke13n4c4"))
            ),
            "ms-windows-store://pdp/?PFN=Power2All.NanoTorrent_jsrm4ke13n4c4"
        );
    }
}
