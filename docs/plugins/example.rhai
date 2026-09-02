// Example NanoTorrent plugin.
//
// A copy of this file is placed in your plugins folder the first time
// NanoTorrent runs, switched OFF. Preferences ▸ Plugins lists it; tick it,
// approve what it asks for, and restart. Every handler below is optional -
// delete the ones you do not want. It is compiled by the test suite, so it
// stays valid.
//
// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------
// Everything a script may reach is declared here, on one line, and NanoTorrent
// reads it WITHOUT running the script - so it can tell you what this wants
// before any of it executes. Approve it in Preferences ▸ Plugins.
//
//   read     see your torrents and transfer rates
//   control  pause, resume and re-check torrents
//   add      add new torrents
//   labels   change torrent labels
//   storage  move downloaded files to another folder
//   remove   remove torrents AND delete their files
//   notify   show desktop notifications
//
// Ask for the least you need. A function you did not declare does not exist:
// calling it fails with "function not found" rather than being refused at
// runtime. `log` is always available and needs no permission.
//
// This example only watches and reports, so it asks for two:
//! permissions: read, notify

// Top-level statements run once, when the plugin loads. Anything kept here is
// visible to this plugin's handlers, and to no other plugin - each script gets
// its own scope and its own engine.
let large_torrent = 8 * 1024 * 1024 * 1024;   // 8 GiB
let completed_this_run = 0;

fn on_session_start() {
    log("example plugin loaded");

    // `read`: the whole torrent list, as maps. Field names match the web API's
    // JSON, so a plugin and a web client describe a torrent the same way.
    let all = torrents();
    let seeding = 0;
    for t in all {
        if t.progress >= 1.0 { seeding += 1; }
    }
    log("watching " + all.len() + " torrent(s), " + seeding + " already complete");

    // `read`: session-wide rates, in bytes per second.
    let rates = session_rates();
    log("current rates: down " + rates.download + " B/s, up " + rates.upload + " B/s");
}

fn on_torrent_added(hash, name) {
    // `read`: look one up by hash. Returns () rather than an error for a
    // torrent that vanished between the event and this call, so check the type
    // before indexing into it.
    let t = torrent(hash);
    if type_of(t) != "map" {
        log("added, but already gone: " + name);
        return;
    }
    if t.size > large_torrent {
        log("large torrent added: " + name + " (" + t.size + " bytes)");
    }
}

fn on_torrent_completed(hash, name) {
    completed_this_run += 1;
    log("finished: " + name + " (" + completed_this_run + " this run)");

    // `notify`: the same desktop notification a finished download uses.
    notify("Download complete", name);
}

fn on_torrent_removed(hash, name) {
    log("removed: " + name);
}

fn on_error(message) {
    log("session error: " + message);
}

// ---------------------------------------------------------------------------
// Doing something, rather than just watching
// ---------------------------------------------------------------------------
// Uncomment the handler below to pause anything that reaches a 2.0 ratio, and
// add `control` to the permissions line at the top:
//
//     //! permissions: read, control, notify
//
// NanoTorrent will notice that the script now asks for more than you approved
// and hold it until you approve the new set - consent is to a specific list of
// permissions, not a one-time yes. That is the point: an edited plugin cannot
// quietly widen its own reach.
//
// fn on_torrent_completed(hash, name) {
//     let t = torrent(hash);
//     if type_of(t) == "map" && t.ratio >= 2.0 {
//         log("ratio reached, pausing " + name);
//         pause(hash);        // needs `control`
//     }
// }
