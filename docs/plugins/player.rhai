// Play a video in VLC, mpv or whatever your system uses - while it downloads.
//
// Right-click a file in a torrent's details panel and pick "Play". The plugin
// asks NanoTorrent for a streaming URL for that one file and hands it to a
// player. The web interface has the same item, under the ... button on a file's
// row or on a right-click, so this works from a phone too.
//
// Nothing waits for the download to finish. NanoTorrent serves byte ranges out
// of a partial file and asks the swarm for the pieces a player seeks to, so a
// film usually starts within seconds of being added.
//
// ---------------------------------------------------------------------------
// SETTING IT UP
//
// Preferences > Plugins, tick "player", approve what it asks for. The cog on
// its row picks the player. That is the whole setup.
//
// The stream is served by the web interface, so that has to be ON - it is what
// listens on the port the player connects to. Preferences > Web interface.
// Nothing is exposed by this: the URL points at 127.0.0.1, and it carries a
// one-file token rather than your password, so it cannot read anything else
// and dies half an hour after its last use.
//
// ---------------------------------------------------------------------------
// PERMISSIONS
//
//   read     the file list, and the stream URL for one file
//   ui       the settings form, and the item on the file menu
//   data     remember which player you picked
//   execute  START THE PLAYER. This is the one to think about: it means this
//            script can run programs as you. Read it before you approve it -
//            all it runs is the player named in its settings, on the next line
//            but one, and every launch is written to the log.
//   notify   say why nothing happened when nothing happens
//
// Every string this plugin shows lives in `player.json` beside it, and `t()`
// reads it - no permission needed, it is the plugin's own file. Adding a
// language means adding one block to that file and nothing else. A key with no
// translation falls back to English, then to the key itself.
//
//! permissions: read, ui, data, execute, notify

// Every player this plugin knows, where it lives, and which systems it exists
// on. Paths are tried in order until one starts: a path that is not there
// fails to spawn and the next is tried, so nothing has to look first - which is
// just as well, because a plugin cannot read the file system.
//
// `os` is what keeps the dropdown honest. Offering IINA on Linux or Celluloid
// on Windows is an option that can only disappoint, so the list is filtered to
// what could actually be there before it is ever shown.
const PLAYERS = [
    #{ id: "system", os: ["windows", "linux", "macos"], paths: [] },
    #{ id: "wmp", os: ["windows"], paths: [
        "C:\\Program Files\\Windows Media Player\\wmplayer.exe",
        "C:\\Program Files (x86)\\Windows Media Player\\wmplayer.exe",
    ] },
    #{ id: "vlc", os: ["windows", "linux", "macos"], paths: [
        "vlc",                                              // PATH: Linux, or installed that way
        "C:\\Program Files\\VideoLAN\\VLC\\vlc.exe",
        "C:\\Program Files (x86)\\VideoLAN\\VLC\\vlc.exe",
        "/Applications/VLC.app/Contents/MacOS/VLC",
        "/snap/bin/vlc",
    ] },
    #{ id: "mpv", os: ["windows", "linux", "macos"], paths: [
        "mpv",
        "C:\\Program Files\\mpv\\mpv.exe",
        "/Applications/mpv.app/Contents/MacOS/mpv",
        "/opt/homebrew/bin/mpv",
        "/usr/local/bin/mpv",
    ] },
    #{ id: "mpchc", os: ["windows"], paths: [
        "C:\\Program Files\\MPC-HC\\mpc-hc64.exe",
        "C:\\Program Files\\MPC-HC\\mpc-hc.exe",
        "C:\\Program Files (x86)\\MPC-HC\\mpc-hc.exe",
    ] },
    #{ id: "potplayer", os: ["windows"], paths: [
        "C:\\Program Files\\DAUM\\PotPlayer\\PotPlayerMini64.exe",
        "C:\\Program Files (x86)\\DAUM\\PotPlayer\\PotPlayerMini.exe",
        "C:\\Program Files\\PotPlayer\\PotPlayerMini64.exe",
    ] },
    #{ id: "smplayer", os: ["windows", "linux"], paths: [
        "smplayer",
        "C:\\Program Files\\SMPlayer\\smplayer.exe",
        "C:\\Program Files (x86)\\SMPlayer\\smplayer.exe",
    ] },
    #{ id: "celluloid", os: ["linux"], paths: [
        "celluloid",
        "/usr/bin/celluloid",
    ] },
    #{ id: "iina", os: ["macos"], paths: [
        "/Applications/IINA.app/Contents/MacOS/IINA",
        "iina-cli",
    ] },
    #{ id: "elmedia", os: ["macos"], paths: [
        "/Applications/Elmedia Player.app/Contents/MacOS/Elmedia Player",
    ] },
    #{ id: "custom", os: ["windows", "linux", "macos"], paths: [] },
];

// What is worth handing to a player. The same list the web interface uses for
// its own Play button, so the two agree about what is playable.
const PLAYABLE = [
    ".mp4", ".m4v", ".mkv", ".webm", ".avi", ".mov", ".wmv", ".flv",
    ".mpg", ".mpeg", ".m2ts", ".mts", ".ts", ".ogv",
    ".mp3", ".flac", ".aac", ".m4a", ".opus", ".ogg", ".oga", ".wav",
];

// What to use when nothing has been picked.
//
// Windows gets Media Player, which is what is actually there on a stock
// install. Linux and macOS get the system default instead of a named player:
// nothing is reliably preinstalled on Linux, and the one that is on macOS -
// QuickTime - cannot open Matroska, which is most of what gets downloaded.
// Handing those two to the desktop's own association at least respects
// whatever the user has already chosen for video.
const DEFAULT_PLAYER = #{
    "windows": "wmp",
    "linux": "system",
    "macos": "system",
};

// What the setting used to hold, when the label WAS the value. Read on the way
// in so an existing profile keeps the player it had.
const LEGACY_IDS = #{
    "System default": "system",
    "VLC": "vlc",
    "mpv": "mpv",
    "Custom": "custom",
};

fn on_session_start() {
    // A window, because the settings form needs somewhere to be drawn, and a
    // cog in Preferences because there is genuinely something to set up: until
    // a player is picked this does the least useful of the four things it can.
    ui_window(t("title"));
    ui_configurable(true);

    // Its mark in the plugin lists, as SVG path data on a 16x16 grid. The same
    // string is drawn by the desktop and by the web interface, so this is
    // declared once and is sharp in both. Straight lines only: Slint's Path
    // does not lean on SVG arc support.
    ui_icon("M 4 2 L 14 8 L 4 14 Z");

    // The item itself. One is enough - which file it was used on comes back
    // with the click, so there is no need for one item per file or any state
    // held between drawing the menu and it being used.
    ui_file_menu([#{ id: "play", label: t("play") }]);

    // And a place on the menu bar, so the settings are reachable without going
    // by way of Preferences > Plugins > the cog.
    ui_menu(t("menu_title"), [#{ id: "settings", label: t("menu_settings") }]);

    // The log is not translated: it is for whoever reads the log file, which is
    // as often the person being asked for it as the person who wrote it.
    log("player plugin ready, using " + player_id());
}

// ---------------------------------------------------------------------------
// The click
// ---------------------------------------------------------------------------

fn on_ui_menu(id) {
    if id == "settings" {
        fill_settings();
        ui_show();
    }
}

fn on_file_menu(id, hash, index, name) {
    if id != "play" { return; }

    // The menu is the same on every file - the host draws it before it knows
    // which file it will be used on - so the check happens here rather than by
    // hiding the item. Saying why is better than a menu item that does nothing.
    if !playable(name) {
        notify(t("title"), t("not_playable", name));
        return;
    }

    // `read`: a URL for this one file, carrying a token minted for it. Empty
    // when the web interface is off, because then there is nothing listening
    // to stream from.
    let url = stream_url(hash, index);
    if url == "" {
        notify(t("title"), t("web_off"));
        return;
    }

    launch(url, name);
}

fn launch(url, name) {
    let choice = player_id();

    // System default: hand the URL to whatever the desktop opens links with.
    // That is usually the browser, which plays MP4 and WebM perfectly well and
    // is the honest answer for someone who has not picked a player.
    if choice == "system" {
        if !open(url) {
            notify(t("title"), t("nothing_opened"));
        }
        return;
    }

    if choice == "custom" {
        let program = setting("custom_program", "");
        if program == "" {
            notify(t("title"), t("no_custom"));
            fill_settings();
            ui_show();
            return;
        }
        // Arguments are separate from the program so a path with a space in it
        // - which on Windows is most of them - needs no quoting, and nothing
        // here builds a command line out of a string a torrent could influence.
        if run(program, custom_args(url)) { return; }
        notify(t("title"), t("wont_start", program));
        fill_settings();
        ui_show();
        return;
    }

    // A path pointed at by hand comes first: it is the answer to "it is
    // installed, just not where you looked", and it is what the Browse button
    // in the settings fills in.
    let candidates = [];
    let pointed = setting("path." + choice, "");
    if pointed != "" { candidates.push(pointed); }
    for guess in paths_for(choice) { candidates.push(guess); }

    if first_that_runs(candidates, [url]) { return; }

    // Nowhere to be found. Rather than a dead end, open the settings with a
    // Browse button already labelled for this player - "it is installed
    // somewhere else" is by far the likeliest reason to be here, and there is
    // nothing else the user could usefully do with the message on its own.
    notify(t("title"), t("not_found", t("player_" + choice)));
    fill_settings();
    ui_show();
}

// The search paths for one player id, or an empty list for one that has none.
fn paths_for(id) {
    for entry in global::PLAYERS {
        if entry.id == id { return entry.paths; }
    }
    return [];
}

// The player ids that exist on this system, in the order they are listed.
fn available_ids() {
    let here = os();
    let out = [];
    for entry in global::PLAYERS {
        if entry.os.contains(here) { out.push(entry.id); }
    }
    return out;
}

// Arguments for a custom player: whitespace-separated, with {url} replaced.
// An empty setting means the URL on its own, which is what nearly every player
// wants and saves anyone typing "{url}" to get the obvious behaviour.
fn custom_args(url) {
    let raw = setting("custom_args", "");
    if raw == "" { return [url]; }

    let out = [];
    for word in raw.split(" ") {
        if word == "" { continue; }
        let arg = word;
        arg.replace("{url}", url);
        out.push(arg);
    }
    if out.is_empty() { return [url]; }
    return out;
}

// Try each candidate until one spawns. `run` is false for a program that is not
// there, which is how a list of guesses works without being able to look.
fn first_that_runs(candidates, args) {
    for program in candidates {
        if run(program, args) { return true; }
    }
    return false;
}

fn playable(name) {
    let lower = name;
    lower.make_lower();
    for ext in global::PLAYABLE {
        if lower.ends_with(ext) { return true; }
    }
    return false;
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

// The cog on this plugin's row in Preferences. The window IS the settings,
// so this fills it and puts it on screen.
fn on_ui_configure() {
    fill_settings();
    ui_show();
}

// The window was opened - by the cog above, or by the user choosing it. Fill
// it, and do NOT call `ui_show()` here: showing the window is what raised this
// handler, so asking to be shown again from inside it is a loop with no way
// out. NanoTorrent no longer re-raises this for a window already on screen, but
// a plugin should not be leaning on that.
fn on_ui_open() {
    fill_settings();
}

fn fill_settings() {
    // Labels are built from the ids, so the dropdown is in the reader's
    // language while the stored value stays the id. Only the ids that exist on
    // this system are offered.
    let chosen = player_id();
    // Hoisted: the host caps how deeply a Rhai expression may nest, and
    // `t("path_to", t("player_" + chosen))` is one level past it.
    let chosen_name = t("player_" + chosen);
    let labels = [];
    for id in available_ids() { labels.push(t("player_" + id)); }

    // One path field, for whichever player is currently chosen, rather than ten
    // that are blank. It is labelled with that player's name so the Browse
    // dialog says what it is asking for.
    let fields = [
        #{
            id: "player", label: t("play_with"), kind: "choice",
            options: join_lines(labels),
            value: chosen_name,
            hint: t("hint_player"),
        },
    ];
    if chosen != "system" && chosen != "custom" {
        fields.push(#{
            id: "player_path", label: t("path_to", chosen_name),
            kind: "file",
            value: setting("path." + chosen, ""),
            hint: t("hint_path"),
        });
    }
    fields.push(#{
        id: "custom_program", label: t("custom_player"), kind: "file",
        value: setting("custom_program", ""),
        hint: t("hint_custom_player"),
    });
    fields.push(#{
        id: "custom_args", label: t("custom_args"), kind: "text",
        value: setting("custom_args", ""),
        hint: t("hint_custom_args"),
    });

    ui_form("settings", t("title"), fields);
    ui_show();
}

fn on_ui_form(form_id, values) {
    if form_id != "settings" { return; }

    // Back from the label to the id. A form hands back the line that was
    // chosen, and the lines are translated - so the id is whichever one
    // produced that label.
    let chosen = default_player();
    for id in available_ids() {
        if t("player_" + id) == values["player"] { chosen = id; }
    }

    data_set("set.player", chosen);
    data_set("set.custom_program", values["custom_program"]);
    data_set("set.custom_args", values["custom_args"]);
    // The path field is only drawn for a real player, and belongs to the one
    // that is selected NOW - changing both in one go points the new player at
    // the path just typed, which is the only reading that is not surprising.
    if values.contains("player_path") {
        data_set("set.path." + chosen, values["player_path"]);
    }
    ui_form_close();
    ui_status(t("now_playing_with", t("player_" + chosen)));
}

// `ui_form` takes its dropdown as one string, a line per option.
fn join_lines(list) {
    let out = "";
    for line in list {
        if out != "" { out += "\n"; }
        out += line;
    }
    return out;
}

fn on_ui_form_cancel(form_id) {
    ui_form_close();
}

// The chosen player, as an id. Accepts what older versions stored - the
// English label - so nobody's setting is lost to this change.
fn player_id() {
    let fallback = default_player();
    let raw = setting("player", fallback);
    if global::LEGACY_IDS.contains(raw) { raw = global::LEGACY_IDS[raw]; }
    // A player that does not exist on this system is not a player: a profile
    // carried from a Windows machine to a Linux one would otherwise keep
    // trying to start PotPlayer forever.
    if available_ids().contains(raw) { return raw; }
    return fallback;
}

fn default_player() {
    let here = os();
    if global::DEFAULT_PLAYER.contains(here) { return global::DEFAULT_PLAYER[here]; }
    return "system";
}

fn setting(key, fallback) {
    let raw = data_get("set." + key);
    if raw == () || raw == "" { return fallback; }
    return raw;
}

// ---------------------------------------------------------------------------
// SELF-SIGNED CERTIFICATES
//
// With TLS on, the web interface uses a certificate it generated itself, and a
// player has no way to know it is yours.
//
//   VLC   asks once, showing the certificate. "Accept permanently" remembers
//         it and it never asks again.
//   mpv   plays it. Its HTTP comes from FFmpeg, which does not verify by
//         default.
//
// Neither is a hole this plugin opens: the certificate is the one NanoTorrent
// already serves the web interface with, on 127.0.0.1, and the alternative -
// no TLS - is worse. Preferences > Web interface can turn TLS off if a
// particular player will not be talked round.
// ---------------------------------------------------------------------------
