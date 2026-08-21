// Display formatting shared by both UI implementations - ports the
// value-to-string mapping from ui/models/torrentlistmodel.cpp.

use crate::bittorrent::torrentstatus::{State, TorrentStatus};
use crate::core::utils;
use crate::ui::translator::Translator;

/// Port of the status -> display string mapping in torrentlistmodel.cpp.
pub fn state_text(tr: &Translator, status: &TorrentStatus) -> String {
    match status.state {
        State::CheckingFiles | State::DownloadingChecking => {
            tr.i18n("state_downloading_checking")
        }
        State::CheckingResumeData => tr.i18n("state_checking_resume_data"),
        State::Downloading => tr.i18n("state_downloading"),
        State::DownloadingMetadata => tr.i18n("state_downloading_metadata"),
        State::DownloadingPaused => tr.i18n("state_downloading_paused"),
        State::DownloadingQueued => tr.i18n("state_downloading_queued"),
        State::Uploading => tr.i18n("state_uploading"),
        State::UploadingPaused => tr.i18n("state_uploading_paused"),
        State::UploadingQueued => tr.i18n("state_uploading_queued"),
        State::Error => tr.i18n1("state_error", &status.error),
        State::Unknown => tr.i18n("state_unknown"),
    }
}

pub fn eta_text(status: &TorrentStatus) -> String {
    match status.eta {
        Some(eta) if eta.as_secs() > 0 => {
            let secs = eta.as_secs();
            let hours = secs / 3600;
            let minutes = (secs % 3600) / 60;
            let seconds = secs % 60;
            if hours > 0 {
                format!("{hours}h {minutes}m")
            } else if minutes > 0 {
                format!("{minutes}m {seconds}s")
            } else {
                format!("{seconds}s")
            }
        }
        _ => String::from("-"),
    }
}

pub fn speed_text(rate: i64) -> String {
    if rate < 1024 {
        String::from("-")
    } else {
        utils::to_human_speed(rate)
    }
}

#[cfg_attr(not(any(all(feature = "ui-native", windows), feature = "ui-slint")), allow(dead_code))]
pub fn date_text(dt: &chrono::DateTime<chrono::Local>) -> String {
    dt.format("%Y-%m-%d %H:%M").to_string()
}

#[cfg_attr(not(any(all(feature = "ui-native", windows), feature = "ui-slint")), allow(dead_code))]
pub fn opt_date_text(dt: &Option<chrono::DateTime<chrono::Local>>) -> String {
    dt.as_ref().map(date_text).unwrap_or_else(|| String::from("-"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn status_with_eta(eta: Option<Duration>) -> TorrentStatus {
        TorrentStatus {
            added_on: chrono::Local::now(),
            all_time_download: 0,
            all_time_upload: 0,
            availability: 0.0,
            completed_on: None,
            download_payload_rate: 0,
            error: String::new(),
            eta,
            info_hash: String::new(),
            label_id: None,
            label_name: String::new(),
            name: String::new(),
            paused: false,
            peers_current: 0,
            peers_total: 0,
            progress: 0.0,
            queue_position: 0,
            ratio: 0.0,
            save_path: String::new(),
            seeds_current: 0,
            seeds_total: 0,
            state: State::Downloading,
            total_wanted: 0,
            total_wanted_remaining: 0,
            upload_payload_rate: 0,
        }
    }

    #[test]
    fn eta_formats_by_magnitude() {
        assert_eq!(eta_text(&status_with_eta(None)), "-");
        assert_eq!(eta_text(&status_with_eta(Some(Duration::ZERO))), "-");
        assert_eq!(eta_text(&status_with_eta(Some(Duration::from_secs(45)))), "45s");
        assert_eq!(eta_text(&status_with_eta(Some(Duration::from_secs(125)))), "2m 5s");
        assert_eq!(eta_text(&status_with_eta(Some(Duration::from_secs(3700)))), "1h 1m");
    }

    #[test]
    fn speed_hides_sub_kib() {
        assert_eq!(speed_text(0), "-");
        assert_eq!(speed_text(1023), "-");
        assert_eq!(speed_text(1024), "1.00 KB/s");
    }

    #[test]
    fn state_text_uses_translation_key() {
        // No lang dir -> falls back to embedded en-US, so real strings come back.
        let tr = Translator::load(std::path::Path::new("does-not-exist"), "en-US");
        let mut st = status_with_eta(None);
        st.state = State::Downloading;
        assert!(!state_text(&tr, &st).is_empty());
        // Error state threads the message through i18n1.
        st.state = State::Error;
        st.error = "disk full".into();
        assert!(state_text(&tr, &st).contains("disk full"));
    }
}
