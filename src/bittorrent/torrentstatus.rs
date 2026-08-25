// Port of src/picotorrent/bittorrent/torrentstatus.hpp

use chrono::{DateTime, Local};

// Some variants have no librqbit equivalent (queueing, resume-data checks)
// but are kept to mirror the original enum.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Unknown,
    Error,
    CheckingFiles,
    CheckingResumeData,
    Downloading,
    DownloadingChecking,
    DownloadingMetadata,
    DownloadingPaused,
    DownloadingQueued,
    Uploading,
    UploadingPaused,
    UploadingQueued,
}

/// Snapshot of a torrent's status - mirrors the C++ TorrentStatus struct.
#[derive(Clone, Debug)]
pub struct TorrentStatus {
    pub added_on: DateTime<Local>,
    pub all_time_download: i64,
    pub all_time_upload: i64,
    pub availability: f32,
    pub completed_on: Option<DateTime<Local>>,
    pub download_payload_rate: i64,
    pub error: String,
    pub eta: Option<std::time::Duration>,
    pub info_hash: String,
    pub label_id: Option<i32>,
    pub label_name: String,
    pub name: String,
    pub paused: bool,
    pub peers_current: i64,
    pub peers_total: i64,
    pub progress: f32,
    pub queue_position: i64,
    pub ratio: f32,
    pub save_path: String,
    pub seeds_current: i64,
    pub seeds_total: i64,
    pub state: State,
    pub total_wanted: i64,
    pub total_wanted_remaining: i64,
    pub upload_payload_rate: i64,
}
