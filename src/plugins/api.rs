// The host functions a plugin can call.
//
// Deliberately the same verbs the web API already exposes, and nothing more.
// Every one of these is a method on Session that the UI and the HTTP layer
// already call, so a plugin cannot reach anything a web client could not - the
// plugin surface is not a second, wider way into the session.
//
// Not exposed, on purpose: process execution and filesystem writes. Those are
// the difference between "a script that manages torrents" and "a script that
// owns the machine", and they belong behind an explicit per-plugin permission
// rather than in the default surface. See the note at the end of this file.

use std::sync::Arc;

use rhai::{Array, Dynamic, Engine, Map};

use crate::bittorrent::session::Session;

/// Bind the host API into an engine.
pub fn register(engine: &mut Engine, session: Arc<Session>) {
    // ---- logging -------------------------------------------------------
    // Goes to the same file as everything else, tagged with the plugin's
    // output so a misbehaving script is findable after the fact.
    engine.register_fn("log", |message: &str| {
        tracing::info!(target: "plugin", "{message}");
    });

    // ---- reading the session -------------------------------------------
    let s = session.clone();
    engine.register_fn("torrents", move || -> Array {
        s.torrents(&std::collections::HashMap::new())
            .into_iter()
            .map(|t| Dynamic::from_map(torrent_map(&t)))
            .collect()
    });

    let s = session.clone();
    engine.register_fn("torrent", move |hash: &str| -> Dynamic {
        // A map or unit, rather than a Result: a plugin asking about a torrent
        // that just vanished is ordinary, not an error worth aborting on.
        match s
            .torrents(&std::collections::HashMap::new())
            .into_iter()
            .find(|t| t.info_hash == hash)
        {
            Some(t) => Dynamic::from_map(torrent_map(&t)),
            None => Dynamic::UNIT,
        }
    });

    let s = session.clone();
    engine.register_fn("exists", move |hash: &str| -> bool { s.exists(hash) });

    let s = session.clone();
    engine.register_fn("session_rates", move || -> Map {
        let (down, up) = s.session_rates();
        let mut map = Map::new();
        map.insert("download".into(), Dynamic::from(down));
        map.insert("upload".into(), Dynamic::from(up));
        map
    });

    // ---- driving the session -------------------------------------------
    let s = session.clone();
    engine.register_fn("pause", move |hash: &str| s.pause(hash));

    let s = session.clone();
    engine.register_fn("resume", move |hash: &str| s.resume(hash));

    let s = session.clone();
    engine.register_fn("recheck", move |hash: &str| s.recheck(hash));

    // Two arities rather than a default argument: Rhai has no optional
    // parameters, and `remove(hash)` deleting files by accident is the kind of
    // mistake a plugin author only makes once.
    let s = session.clone();
    engine.register_fn("remove", move |hash: &str| s.remove(hash, false));

    let s = session.clone();
    engine.register_fn("remove", move |hash: &str, delete_files: bool| {
        s.remove(hash, delete_files)
    });

    let s = session.clone();
    engine.register_fn("move_storage", move |hash: &str, folder: &str| {
        s.move_storage(hash, folder)
    });

    let s = session.clone();
    engine.register_fn("set_label", move |hash: &str, label_id: i64| {
        // Rhai integers are i64; the label table is i32. Out-of-range means a
        // label that cannot exist, so clear it rather than truncating into
        // some unrelated label's id.
        s.set_label(hash, i32::try_from(label_id).ok())
    });

    let s = session.clone();
    engine.register_fn("clear_label", move |hash: &str| s.set_label(hash, None));

    let s = session.clone();
    engine.register_fn("add_magnet", move |uri: &str| {
        s.add_torrent(
            crate::bittorrent::session::AddTorrentSource::MagnetUri(uri.to_string()),
            crate::bittorrent::session::AddParams {
                save_path: None,
                start_torrent: true,
                only_files: None,
                label_id: None,
            },
        )
    });

    let s = session.clone();
    engine.register_fn("add_magnet", move |uri: &str, save_path: &str| {
        s.add_torrent(
            crate::bittorrent::session::AddTorrentSource::MagnetUri(uri.to_string()),
            crate::bittorrent::session::AddParams {
                save_path: Some(save_path.to_string()),
                start_torrent: true,
                only_files: None,
                label_id: None,
            },
        )
    });

    // ---- telling the user something ------------------------------------
    // A desktop notification, the same channel a finished download uses. No-op
    // where the platform has none.
    engine.register_fn("notify", |title: &str, body: &str| {
        crate::core::toast::download_complete(title, body);
    });

    // ponytail: no `run()` / `read_file()` / `write_file()`, so post-download
    // processing has to go through a plugin calling out to something else.
    // Adding them means a per-plugin permission prompt and a record of what was
    // granted - do that before exposing them, not after.
}

/// A torrent as a plugin sees it.
///
/// Field names match the web API's JSON, so a plugin and a web client describe
/// the same torrent the same way.
fn torrent_map(t: &crate::bittorrent::torrentstatus::TorrentStatus) -> Map {
    let mut map = Map::new();
    map.insert("hash".into(), Dynamic::from(t.info_hash.clone()));
    map.insert("name".into(), Dynamic::from(t.name.clone()));
    map.insert("save_path".into(), Dynamic::from(t.save_path.clone()));
    map.insert("label".into(), Dynamic::from(t.label_name.clone()));
    map.insert("progress".into(), Dynamic::from(t.progress as f64));
    map.insert("ratio".into(), Dynamic::from(t.ratio as f64));
    map.insert("paused".into(), Dynamic::from(t.paused));
    map.insert("error".into(), Dynamic::from(t.error.clone()));
    map.insert("size".into(), Dynamic::from(t.total_wanted));
    map.insert("remaining".into(), Dynamic::from(t.total_wanted_remaining));
    map.insert("downloaded".into(), Dynamic::from(t.all_time_download));
    map.insert("uploaded".into(), Dynamic::from(t.all_time_upload));
    map.insert("download_rate".into(), Dynamic::from(t.download_payload_rate));
    map.insert("upload_rate".into(), Dynamic::from(t.upload_payload_rate));
    map.insert("peers".into(), Dynamic::from(t.peers_current));
    map.insert("seeds".into(), Dynamic::from(t.seeds_current));
    map.insert("queue_position".into(), Dynamic::from(t.queue_position));
    map.insert("state".into(), Dynamic::from(format!("{:?}", t.state)));
    map
}
