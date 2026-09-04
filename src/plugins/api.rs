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
use crate::core::configuration::Configuration;
use super::Permission;
use std::collections::BTreeSet;

/// Ceiling on one HTTP response, in bytes. Without a cap a hostile URL is an
/// out-of-memory kill for the whole client rather than one bad script.
///
/// `super::apply_limits` sizes the engine's string ceiling from this, because
/// `http_get` hands the body back AS a string: a smaller string limit means a
/// fetch the engine then refuses to hold, which is not a limit anyone can act
/// on - it just makes every feed over the smaller number fail.
pub(super) const HTTP_LIMIT: usize = 4 * 1024 * 1024;

/// How long a plugin's HTTP call may take. The plugin thread is shared with
/// event dispatch, so a server that accepts and then stalls would otherwise
/// hold up every other plugin.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Ceiling on one plugin's key/value store, in bytes of stored JSON. The store
/// lives in the settings database, which is not a place to put a cache of
/// every item a feed has ever published.
const DATA_LIMIT: usize = 64 * 1024;

/// Most items one plugin may put in its menu. A plugin gets one dropdown, so
/// this is the whole of the menu bar it can occupy.
const MENU_ITEMS_MAX: usize = 20;

/// How deep `parse_xml` will recurse before giving up. Depth is the one thing
/// the byte cap does not bound - a few hundred bytes of nested tags is enough
/// to blow the stack.
const XML_DEPTH: usize = 64;

/// Bind the host API into an engine, limited to what this plugin was granted.
///
/// A function whose permission is missing is NOT registered, rather than
/// registered and refusing at call time. A script that reaches past its grant
/// therefore fails with "function not found" - loudly, at the call site, with
/// no way to probe for what exists behind a permission it does not hold.
pub fn register(
    engine: &mut Engine,
    session: Arc<Session>,
    cfg: Arc<Configuration>,
    name: &str,
    perms: &BTreeSet<Permission>,
) {
    // ---- logging -------------------------------------------------------
    // Goes to the same file as everything else, tagged with the plugin's
    // output so a misbehaving script is findable after the fact.
    engine.register_fn("log", |message: &str| {
        tracing::info!(target: "plugin", "{message}");
    });

    // ---- reading the session -------------------------------------------
    if perms.contains(&Permission::Read) {
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
    }

        // ---- driving the session -------------------------------------------
    if perms.contains(&Permission::Control) {
    let s = session.clone();
        engine.register_fn("pause", move |hash: &str| s.pause(hash));

        let s = session.clone();
        engine.register_fn("resume", move |hash: &str| s.resume(hash));

        let s = session.clone();
        engine.register_fn("recheck", move |hash: &str| s.recheck(hash));
    }

        // Two arities rather than a default argument: Rhai has no optional
    // parameters, and `remove(hash)` deleting files by accident is the kind of
    // mistake a plugin author only makes once.
    if perms.contains(&Permission::Remove) {
    let s = session.clone();
        engine.register_fn("remove", move |hash: &str| s.remove(hash, false));

        let s = session.clone();
        engine.register_fn("remove", move |hash: &str, delete_files: bool| {
            s.remove(hash, delete_files)
        });
    }

        if perms.contains(&Permission::Storage) {
    let s = session.clone();
        engine.register_fn("move_storage", move |hash: &str, folder: &str| {
            s.move_storage(hash, folder)
        });
    }

        if perms.contains(&Permission::Labels) {
    let s = session.clone();
        engine.register_fn("set_label", move |hash: &str, label_id: i64| {
            // Rhai integers are i64; the label table is i32. Out-of-range means a
            // label that cannot exist, so clear it rather than truncating into
            // some unrelated label's id.
            s.set_label(hash, i32::try_from(label_id).ok())
        });

        let s = session.clone();
        engine.register_fn("clear_label", move |hash: &str| s.set_label(hash, None));
    }

        if perms.contains(&Permission::Add) {
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
    }

        // ---- telling the user something ------------------------------------
    // A desktop notification, the same channel a finished download uses. No-op
    // where the platform has none.
    if perms.contains(&Permission::Notify) {
    engine.register_fn("notify", |title: &str, body: &str| {
            crate::core::toast::download_complete(title, body);
        });
    }

    // ---- reaching the network ------------------------------------------
    // The permission a feed reader needs, and the one that turns `read` into
    // exfiltration: a plugin holding both can post your torrent list anywhere.
    // That pairing is why the approval prompt lists every permission together
    // rather than asking about them one at a time.
    //
    // The client is built ONCE, from the settings, so every plugin request
    // takes whatever route the proxy setting says. If it cannot be built the
    // network functions are simply not registered - the same fail-closed shape
    // permissions use, and better than handing out a client that silently goes
    // direct on a setup where that is the one thing not to do.
    let http = match crate::core::http::client_arc(&cfg) {
        Ok(client) => Some(client),
        Err(err) => {
            tracing::error!("plugin {name}: no HTTP client ({err}); network is unavailable");
            None
        }
    };

    if let Some(http) = http.clone()
        && perms.contains(&Permission::Network)
    {
        let handle = session.handle();
        engine.register_fn("http_get", move |url: &str| -> Map {
            http_get(&handle, &http, url)
        });
    }

    // Both permissions, because it is both actions: fetch bytes from a server
    // and then add them to the session. Feeds that list `.torrent` files
    // rather than magnet links are the ordinary case, and handing a script raw
    // torrent bytes to pass straight back would buy nothing.
    if let Some(http) = http
        && perms.contains(&Permission::Network)
        && perms.contains(&Permission::Add)
    {
        let (handle, s, c) = (session.handle(), session.clone(), http.clone());
        engine.register_fn("add_torrent_url", move |url: &str| -> bool {
            add_url(&handle, &c, &s, url, None)
        });

        let (handle, s, c) = (session.handle(), session.clone(), http);
        engine.register_fn("add_torrent_url", move |url: &str, save_path: &str| -> bool {
            add_url(&handle, &c, &s, url, Some(save_path.to_string()))
        });
    }

    // ---- remembering something between runs ----------------------------
    // Namespaced by plugin name and never by a name the script supplies, so
    // one plugin cannot read or overwrite another's store.
    if perms.contains(&Permission::Data) {
        let (c, key) = (cfg.clone(), store_key(name));
        engine.register_fn("data_get", move |k: &str| -> Dynamic {
            match read_store(&c, &key).remove(k) {
                Some(v) => Dynamic::from(v),
                None => Dynamic::UNIT,
            }
        });

        let (c, key) = (cfg.clone(), store_key(name));
        // Returns false when the store is full rather than throwing: a plugin
        // that has filled it should be able to notice and prune, and a throw
        // here would abort whatever handler was midway through.
        engine.register_fn("data_set", move |k: &str, v: &str| -> bool {
            store_set(&c, &key, k, v)
        });

        let (c, key) = (cfg.clone(), store_key(name));
        engine.register_fn("data_remove", move |k: &str| {
            let mut store = read_store(&c, &key);
            store.remove(k);
            c.set_persistent(&key, &serde_json::to_string(&store).unwrap_or_default());
        });

        let (c, key) = (cfg.clone(), store_key(name));
        engine.register_fn("data_keys", move || -> Array {
            read_store(&c, &key).into_keys().map(Dynamic::from).collect()
        });
    }

    // ---- a window of its own -------------------------------------------
    // Setters, not a layout: the plugin says what goes in the list and the
    // client decides how it looks, so a plugin window cannot drift away from
    // the rest of the application or be made to imitate part of it.
    //
    // Every one of these is a no-op in a headless build. A plugin written for
    // the desktop should degrade to doing nothing visible on a server, not
    // fail on its first line.
    if perms.contains(&Permission::Ui) {
        let plugin = name.to_owned();
        engine.register_fn("ui_window", move |title: &str| {
            let title = title.to_owned();
            super::ui::update(&plugin, move |w| w.title = title);
        });

        let plugin = name.to_owned();
        engine.register_fn("ui_status", move |text: &str| {
            let text = text.to_owned();
            super::ui::update(&plugin, move |w| w.status = text);
        });

        // Empty placeholder means no field, so a plugin that never calls this
        // gets a window with no input rather than an unlabelled box.
        let plugin = name.to_owned();
        engine.register_fn("ui_input", move |placeholder: &str| {
            let placeholder = placeholder.to_owned();
            super::ui::update(&plugin, move |w| w.placeholder = placeholder);
        });

        let plugin = name.to_owned();
        engine.register_fn("ui_buttons", move |buttons: Array| {
            let buttons: Vec<(String, String)> = buttons
                .into_iter()
                .filter_map(|b| b.try_cast::<Map>())
                .map(|b| (field(&b, "id"), field(&b, "label")))
                .collect();
            super::ui::update(&plugin, move |w| w.buttons = buttons);
        });

        let plugin = name.to_owned();
        engine.register_fn("ui_rows", move |rows: Array| {
            let rows = list(rows);
            super::ui::update(&plugin, move |ui| ui.rows = rows);
        });

        // An optional upper list: the things the main list shows the contents
        // OF - feeds, categories, accounts. Empty restores the single-list
        // window, so a plugin that never calls this sees no change.
        let plugin = name.to_owned();
        engine.register_fn("ui_groups", move |rows: Array| {
            let rows = list(rows);
            super::ui::update(&plugin, move |ui| ui.groups = rows);
        });

        // Declaring a window is not showing it: a plugin prepares one at load
        // and opens it when asked, so nothing appears on somebody's screen
        // unbidden.
        let plugin = name.to_owned();
        engine.register_fn("ui_show", move || {
            super::ui::show(&plugin);
        });

        // A dropdown of the plugin's own in the main window's menu bar.
        // Calling this IS the declaration - a plugin that never calls it gets
        // no menu, which is why the bar does not grow for the plugins that
        // have nothing to put there.
        //
        // ONE menu per plugin, and that is structural rather than checked:
        // this overwrites the plugin's single entry, so calling it twice
        // replaces the menu instead of adding a second. There is no shape a
        // script can pass that produces two titles in the bar.
        let plugin = name.to_owned();
        engine.register_fn("ui_menu", move |title: &str, items: Array| {
            let title = title.to_owned();
            let items = menu_items(&plugin, items);
            super::ui::update(&plugin, move |ui| {
                ui.menu_title = title;
                ui.menu_items = items;
            });
        });

        // "This needs setting up before it will do anything useful", which is
        // what puts a Configure button on the plugin's row in Preferences.
        // Not inferred from having a window: plenty of useful windows are
        // somewhere to work rather than somewhere to configure.
        let plugin = name.to_owned();
        engine.register_fn("ui_configurable", move |needed: bool| {
            super::ui::update(&plugin, move |ui| ui.configurable = needed);
        });
    }

    // ---- making sense of what came back --------------------------------
    // No permission: this is arithmetic on a string the plugin already holds.
    // Without it a feed reader means writing an XML parser in Rhai, which is
    // the kind of thing that makes a subsystem technically possible and
    // practically not worth it.
    engine.register_fn("parse_json", |text: &str| -> Dynamic {
        serde_json::from_str::<serde_json::Value>(text)
            .map(|v| json_to_dynamic(&v))
            .unwrap_or(Dynamic::UNIT)
    });

    engine.register_fn("parse_xml", |text: &str| -> Dynamic {
        parse_xml(text).unwrap_or(Dynamic::UNIT)
    });

    // ponytail: still no `run()` / `read_file()` / `write_file()`. Those are
    // the difference between a sandbox with holes in it and no sandbox, and
    // nothing asked for so far has needed them - a plugin that wants to keep
    // something has `data_set`, and one that wants a file has `add_torrent_url`.
}

/// One of a plugin's lists, from the array of maps it handed over.
///
/// A row that is not a map is dropped; a row missing a field gets an empty
/// one. Neither is worth throwing over - a blank line is a mistake the plugin
/// author can see, where an aborted handler is not.
fn list(rows: Array) -> Vec<super::ui::Row> {
    rows.into_iter()
        .filter_map(|r| r.try_cast::<Map>())
        .map(|r| super::ui::Row {
            id: field(&r, "id"),
            title: field(&r, "title"),
            subtitle: field(&r, "subtitle"),
            selected: r
                .get("selected")
                .and_then(|v| v.clone().as_bool().ok())
                .unwrap_or(false),
        })
        .collect()
}

/// A plugin's menu items, bounded.
///
/// A dropdown taller than the screen is not a menu, it is a way to cover the
/// window. Truncated rather than refused, so a plugin that miscounts still
/// works, and logged so its author finds out why the tail vanished.
fn menu_items(plugin: &str, items: Array) -> Vec<(String, String)> {
    let mut items: Vec<(String, String)> = items
        .into_iter()
        .filter_map(|i| i.try_cast::<Map>())
        .map(|i| (field(&i, "id"), field(&i, "label")))
        .collect();
    if items.len() > MENU_ITEMS_MAX {
        tracing::warn!(
            target: "plugin",
            "{plugin}: a menu may have {MENU_ITEMS_MAX} items, {} were given - the rest are ignored",
            items.len()
        );
        items.truncate(MENU_ITEMS_MAX);
    }
    items
}

/// One string field out of a map a plugin built, or "" if it left it out.
///
/// Missing rather than wrong: a row without a subtitle is ordinary, and a
/// throw here would abort a handler over a cosmetic omission.
fn field(map: &Map, key: &str) -> String {
    map.get(key)
        .and_then(|v| v.clone().into_string().ok())
        .unwrap_or_default()
}

/// This plugin's corner of the settings database.
fn store_key(name: &str) -> String {
    format!("plugins.data.{name}")
}

/// The persistent_object table, not the settings table: `Configuration::set`
/// is an UPDATE against a row a migration created, and there is no migration
/// that can name a plugin nobody has written yet.
fn read_store(cfg: &Configuration, key: &str) -> std::collections::BTreeMap<String, String> {
    serde_json::from_str(&cfg.get_persistent(key).unwrap_or_default()).unwrap_or_default()
}

/// Write one key, refusing once the store is over its ceiling.
///
/// The refusal is the whole point, so this is a function rather than four
/// lines inside the closure: an over-full store that silently dropped writes
/// would look like a plugin bug for as long as it took someone to find this.
fn store_set(cfg: &Configuration, key: &str, k: &str, v: &str) -> bool {
    let mut store = read_store(cfg, key);
    store.insert(k.to_owned(), v.to_owned());
    let encoded = serde_json::to_string(&store).unwrap_or_default();
    if encoded.len() > DATA_LIMIT {
        return false;
    }
    cfg.set_persistent(key, &encoded);
    true
}

/// What a plugin sees from `http_get`.
///
/// A function rather than a closure body so a test can drive the same code the
/// engine does: it is the difference between checking that a feed is read and
/// checking that a feed is read *the way plugins read one*.
fn http_get(handle: &tokio::runtime::Handle, client: &reqwest::Client, url: &str) -> Map {
    let mut map = Map::new();
    match fetch(handle, client, url) {
        Ok((status, body)) => {
            map.insert("ok".into(), Dynamic::from((200..300).contains(&status)));
            map.insert("status".into(), Dynamic::from(i64::from(status)));
            map.insert("body".into(), Dynamic::from(body));
            map.insert("error".into(), Dynamic::from(String::new()));
        }
        // An unreachable server is ordinary for a plugin polling the internet
        // on a timer, so it is a field to check rather than a Rhai error that
        // kills the handler.
        Err(err) => {
            map.insert("ok".into(), Dynamic::from(false));
            map.insert("status".into(), Dynamic::from(0_i64));
            map.insert("body".into(), Dynamic::from(String::new()));
            map.insert("error".into(), Dynamic::from(err));
        }
    }
    map
}

/// One HTTP GET, run on the session's runtime because the plugin thread is not
/// one and a second runtime for this would be absurd.
///
/// Capped in both directions - a deadline and a byte ceiling - because the URL
/// comes from a script and the script may have got it from a feed, which is to
/// say from a stranger.
fn fetch(
    handle: &tokio::runtime::Handle,
    client: &reqwest::Client,
    url: &str,
) -> Result<(u16, String), String> {
    let bytes = fetch_bytes(handle, client, url)?;
    let status = bytes.0;
    String::from_utf8(bytes.1)
        .map(|body| (status, body))
        .map_err(|_| String::from("response was not valid UTF-8"))
}

/// Reject anything that is not http(s) BEFORE it reaches the client.
///
/// `file://` is the one that matters: reqwest will not serve it, but this is
/// the boundary where "may contact servers" is defined, and defining it by
/// what a dependency happens not to support is how that stops being true.
fn checked_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(String::from("only http and https URLs can be fetched"));
    }
    Ok(url.to_owned())
}

fn fetch_bytes(
    handle: &tokio::runtime::Handle,
    client: &reqwest::Client,
    url: &str,
) -> Result<(u16, Vec<u8>), String> {
    let url = checked_url(url)?;
    // Cloned in rather than built here: this is the client core::http made,
    // which already carries the proxy setting. Building one at the call site
    // is how the leak happened the first time.
    let client = client.clone();

    handle.block_on(async move {
        let mut response = client
            .get(&url)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();

        // Streamed rather than `.bytes()`, so an endless response is cut off
        // at the ceiling instead of being buffered whole and then measured.
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            if body.len() + chunk.len() > HTTP_LIMIT {
                return Err(format!("response is larger than {HTTP_LIMIT} bytes"));
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))
    })
}

/// Add whatever a feed pointed at: a magnet link as-is, anything else fetched
/// first and added as torrent bytes.
fn add_url(
    handle: &tokio::runtime::Handle,
    client: &reqwest::Client,
    session: &Session,
    url: &str,
    save_path: Option<String>,
) -> bool {
    let params = crate::bittorrent::session::AddParams {
        save_path,
        start_torrent: true,
        only_files: None,
        label_id: None,
    };

    if url.trim().to_ascii_lowercase().starts_with("magnet:") {
        session.add_torrent(
            crate::bittorrent::session::AddTorrentSource::MagnetUri(url.trim().to_owned()),
            params,
        );
        return true;
    }

    match fetch_bytes(handle, client, url) {
        Ok((status, bytes)) if (200..300).contains(&status) => {
            session.add_torrent(
                crate::bittorrent::session::AddTorrentSource::TorrentFileBytes(bytes),
                params,
            );
            true
        }
        Ok((status, _)) => {
            tracing::warn!(target: "plugin", "add_torrent_url: {url} returned {status}");
            false
        }
        Err(err) => {
            tracing::warn!(target: "plugin", "add_torrent_url: {url}: {err}");
            false
        }
    }
}

fn json_to_dynamic(value: &serde_json::Value) -> Dynamic {
    match value {
        serde_json::Value::Null => Dynamic::UNIT,
        serde_json::Value::Bool(b) => Dynamic::from(*b),
        // Rhai has i64 and f64 and no arbitrary precision, so a number that
        // fits neither lands as a string rather than silently losing digits.
        serde_json::Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Dynamic::from(i),
            (None, Some(f)) => Dynamic::from(f),
            (None, None) => Dynamic::from(n.to_string()),
        },
        serde_json::Value::String(s) => Dynamic::from(s.clone()),
        serde_json::Value::Array(items) => {
            Dynamic::from(items.iter().map(json_to_dynamic).collect::<Array>())
        }
        serde_json::Value::Object(fields) => {
            let mut map = Map::new();
            for (k, v) in fields {
                map.insert(k.as_str().into(), json_to_dynamic(v));
            }
            Dynamic::from_map(map)
        }
    }
}

/// XML as nested maps: `#{ tag, attrs, text, children }`.
///
/// Deliberately not a full document model - no namespaces, no comments, no
/// processing instructions. It is enough to walk an RSS or Atom feed, which is
/// what a plugin asking for this is doing.
fn parse_xml(text: &str) -> Option<Dynamic> {
    use quick_xml::events::Event;

    // Text is NOT trimmed as it is read. quick-xml reports `&amp;` as its own
    // event, so trimming here would eat the real spaces on either side of an
    // entity and turn "Ubuntu 24.04 &amp; friends" into "Ubuntu 24.04friends".
    // Each element's text is trimmed once, whole, in `finish`.
    let mut reader = quick_xml::Reader::from_str(text);

    // A stack of part-built elements: the element's own fields, and the
    // children collected for it so far. Children are kept beside the map
    // rather than inside it because a Dynamic holding an Array cannot be
    // appended to in place.
    let mut stack: Vec<(Map, Array)> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if stack.len() >= XML_DEPTH {
                    return None;
                }
                stack.push((new_element(&e), Array::new()));
            }
            Ok(Event::Empty(e)) => {
                let element = finish(new_element(&e), Array::new());
                match stack.last_mut() {
                    Some((_, children)) => children.push(element),
                    None => return Some(element),
                }
            }
            Ok(Event::End(_)) => {
                let (map, children) = stack.pop()?;
                let done = finish(map, children);
                match stack.last_mut() {
                    Some((_, children)) => children.push(done),
                    // Closing the outermost element: that is the whole
                    // document, and anything after it is not our problem.
                    None => return Some(done),
                }
            }
            Ok(Event::Text(e)) => {
                // Literal: anything escaped arrived as a GeneralRef instead.
                if let Ok(raw) = e.decode() {
                    append_text(&mut stack, raw.as_ref());
                }
            }
            // `&amp;`, `&#38;`, `&#x26;`. An entity nobody defined is put back
            // as it was written rather than dropped - a title with a stray
            // ampersand should look wrong, not look shorter.
            Ok(Event::GeneralRef(e)) => {
                if let Ok(name) = e.decode() {
                    append_text(&mut stack, &resolve_entity(name.as_ref()));
                }
            }
            // CDATA is where feeds put the description, so dropping it would
            // make this useless for the exact job it exists for. Its content
            // is literal by definition, so it is decoded but not unescaped.
            Ok(Event::CData(e)) => {
                if let Ok(raw) = e.decode() {
                    append_text(&mut stack, raw.as_ref());
                }
            }
            // EOF with the stack non-empty means tags were left unclosed.
            Ok(Event::Eof) => return None,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

fn append_text(stack: &mut [(Map, Array)], text: &str) {
    let Some((current, _)) = stack.last_mut() else { return };
    let existing = current
        .get("text")
        .and_then(|d| d.clone().into_string().ok())
        .unwrap_or_default();
    current.insert("text".into(), Dynamic::from(existing + text));
}

/// One entity reference, as its name appears between `&` and `;`.
fn resolve_entity(name: &str) -> String {
    if let Some(digits) = name.strip_prefix("#") {
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => digits.parse::<u32>().ok(),
        };
        return match code.and_then(char::from_u32) {
            Some(c) => c.to_string(),
            None => format!("&{name};"),
        };
    }
    match quick_xml::escape::resolve_predefined_entity(name) {
        Some(text) => text.to_owned(),
        None => format!("&{name};"),
    }
}

/// Seal an element: fold its collected children in and hand back the map.
///
/// Where the text is trimmed - once, on the whole accumulated string, so the
/// indentation between child elements does not become an element's "text" and
/// the spaces around an entity survive.
fn finish(mut map: Map, children: Array) -> Dynamic {
    let trimmed = map
        .get("text")
        .and_then(|d| d.clone().into_string().ok())
        .unwrap_or_default()
        .trim()
        .to_owned();
    map.insert("text".into(), Dynamic::from(trimmed));
    map.insert("children".into(), Dynamic::from(children));
    Dynamic::from_map(map)
}

fn new_element(e: &quick_xml::events::BytesStart) -> Map {
    let mut map = Map::new();
    map.insert(
        "tag".into(),
        Dynamic::from(String::from_utf8_lossy(e.local_name().as_ref()).into_owned()),
    );

    let mut attrs = Map::new();
    for attr in e.attributes().flatten() {
        let key = String::from_utf8_lossy(attr.key.local_name().as_ref()).into_owned();
        // Implicit 1.0: feeds are 1.0 and the declaration is not read back
        // here, which is the assumption the specification makes anyway.
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map(|v| v.into_owned())
            .unwrap_or_default();
        attrs.insert(key.as_str().into(), Dynamic::from(value));
    }
    map.insert("attrs".into(), Dynamic::from_map(attrs));
    map.insert("text".into(), Dynamic::from(String::new()));
    map
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The `ui_*` calls every one of these tests wants to be silent about.
    ///
    /// A helper rather than three copies: a plugin that starts declaring a new
    /// surface should break one place, not each test in turn - which is
    /// exactly what adding `ui_menu` did.
    fn stub_surfaces(engine: &mut rhai::Engine) {
        engine.register_fn("ui_window", |_: &str| {});
        engine.register_fn("ui_input", |_: &str| {});
        engine.register_fn("ui_buttons", |_: Array| {});
        engine.register_fn("ui_menu", |_: &str, _: Array| {});
        engine.register_fn("ui_groups", |_: Array| {});
        engine.register_fn("ui_configurable", |_: bool| {});
        engine.register_fn("ui_show", || {});
    }

    fn test_cfg() -> Configuration {
        let db = Arc::new(crate::core::database::Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        Configuration::new(db)
    }

    /// Walk to the first element with this tag, depth first.
    fn find(node: &Map, tag: &str) -> Option<Map> {
        if node.get("tag").and_then(|d| d.clone().into_string().ok()).as_deref() == Some(tag) {
            return Some(node.clone());
        }
        for child in node.get("children")?.clone().cast::<Array>() {
            if let Some(hit) = find(&child.cast::<Map>(), tag) {
                return Some(hit);
            }
        }
        None
    }

    fn text_of(node: &Map, tag: &str) -> String {
        find(node, tag)
            .and_then(|n| n.get("text").cloned())
            .and_then(|d| d.into_string().ok())
            .unwrap_or_default()
    }

    const FEED: &str = r#"<?xml version="1.0"?>
<rss version="2.0">
  <channel>
    <title>Example</title>
    <item>
      <title>Ubuntu 24.04 &amp; friends</title>
      <link>https://example.invalid/one.torrent</link>
      <pubDate>Mon, 01 Sep 2026 10:00:00 GMT</pubDate>
      <description><![CDATA[<b>seeded</b> & ready]]></description>
      <enclosure url="https://example.invalid/one.torrent" type="application/x-bittorrent"/>
    </item>
  </channel>
</rss>"#;

    /// The whole reason `parse_xml` exists: a feed reader has to get the title,
    /// the link and the enclosure URL out of an ordinary RSS document.
    #[test]
    fn an_rss_item_can_be_read_out_of_a_feed() {
        let root = parse_xml(FEED).expect("feed should parse").cast::<Map>();

        let item = find(&root, "item").expect("there is an item");
        // Entities decoded, so a title is what a person would read.
        assert_eq!(text_of(&item, "title"), "Ubuntu 24.04 & friends");
        assert_eq!(text_of(&item, "link"), "https://example.invalid/one.torrent");
        // CDATA kept: this is where feeds put the description.
        assert_eq!(text_of(&item, "description"), "<b>seeded</b> & ready");

        // A self-closing element with the attribute that actually names the
        // torrent - the common case for feeds that do not use <link>.
        let enclosure = find(&item, "enclosure").expect("there is an enclosure");
        let attrs = enclosure.get("attrs").unwrap().clone().cast::<Map>();
        assert_eq!(
            attrs.get("url").unwrap().clone().into_string().unwrap(),
            "https://example.invalid/one.torrent"
        );
    }

    /// Malformed input is a returned unit, not a panic and not a hang: the
    /// document came off the internet.
    #[test]
    fn broken_xml_is_a_unit_rather_than_a_panic() {
        assert!(parse_xml("<a><b></a>").is_none());
        assert!(parse_xml("<a>").is_none(), "unclosed tags are not a document");
        assert!(parse_xml("not xml at all").is_none());
    }

    /// Depth is what the byte ceiling does not bound.
    #[test]
    fn deeply_nested_xml_gives_up_instead_of_blowing_the_stack() {
        let deep = format!("{}{}", "<a>".repeat(XML_DEPTH + 10), "</a>".repeat(XML_DEPTH + 10));
        assert!(parse_xml(&deep).is_none());
    }

    /// The permission is "contact servers", not "read the disk".
    #[test]
    fn only_http_urls_are_fetchable() {
        assert!(checked_url("https://example.invalid/feed.xml").is_ok());
        assert!(checked_url("  http://example.invalid/feed.xml  ").is_ok());
        for bad in ["file:///etc/passwd", "ftp://example.invalid", "/etc/passwd"] {
            assert!(checked_url(bad).is_err(), "{bad} should be refused");
        }
    }

    /// Two plugins with a key in common must not see each other's value.
    #[test]
    fn one_plugins_store_is_not_anothers() {
        let cfg = test_cfg();
        assert!(store_set(&cfg, &store_key("rss"), "seen", "a"));
        assert!(store_set(&cfg, &store_key("other"), "seen", "b"));

        assert_eq!(read_store(&cfg, &store_key("rss")).get("seen").unwrap(), "a");
        assert_eq!(read_store(&cfg, &store_key("other")).get("seen").unwrap(), "b");
    }

    /// A full store refuses the write and says so, rather than accepting it
    /// and quietly dropping the value.
    #[test]
    fn a_full_store_refuses_the_write() {
        let cfg = test_cfg();
        let key = store_key("greedy");
        assert!(store_set(&cfg, &key, "small", "value"));

        assert!(
            !store_set(&cfg, &key, "huge", &"x".repeat(DATA_LIMIT + 1)),
            "a value over the ceiling must be refused"
        );
        // The refused write left the store as it was.
        let store = read_store(&cfg, &key);
        assert_eq!(store.get("small").unwrap(), "value");
        assert!(!store.contains_key("huge"));
    }

    #[test]
    fn json_becomes_something_rhai_can_walk() {
        let value: serde_json::Value =
            serde_json::from_str(r#"{"n": 3, "f": 1.5, "s": "x", "b": true, "a": [1, 2], "z": null}"#)
                .unwrap();
        let map = json_to_dynamic(&value).cast::<Map>();

        assert_eq!(map.get("n").unwrap().clone().as_int().unwrap(), 3);
        assert_eq!(map.get("f").unwrap().clone().as_float().unwrap(), 1.5);
        assert_eq!(map.get("s").unwrap().clone().into_string().unwrap(), "x");
        assert!(map.get("b").unwrap().clone().as_bool().unwrap());
        assert_eq!(map.get("a").unwrap().clone().cast::<Array>().len(), 2);
        assert!(map.get("z").unwrap().is_unit());
    }

    /// The RSS plugin, end to end, against a canned feed.
    ///
    /// The point is not to test Rhai. It is to prove the subsystems compose
    /// into the thing they were added for: `http_get` feeds `parse_xml`,
    /// `parse_xml` feeds `ui_rows`, `data_*` carries the feed list across
    /// calls, and a click reaches `add_torrent_url` with the URL the feed gave.
    /// Only the session-touching functions are stubs - the XML parser under
    /// test here is the real one.
    #[test]
    fn the_rss_plugin_turns_a_feed_into_clickable_rows() {
        use std::sync::Mutex;

        let rows: Arc<Mutex<Vec<String>>> = Arc::default();
        let status: Arc<Mutex<String>> = Arc::default();
        let store: Arc<Mutex<std::collections::BTreeMap<String, String>>> = Arc::default();
        let added: Arc<Mutex<Vec<String>>> = Arc::default();

        store
            .lock()
            .unwrap()
            .insert(String::from("feeds"), String::from("https://example.invalid/feed.xml"));

        // The host's limits, not Rhai's defaults: the point is to check the
        // plugin against the engine it will actually run in.
        let mut engine = rhai::Engine::new();
        crate::plugins::apply_limits(&mut engine);
        engine.register_fn("log", |_: &str| {});
        stub_surfaces(&mut engine);

        let sink = status.clone();
        engine.register_fn("ui_status", move |text: &str| {
            *sink.lock().unwrap() = text.to_owned();
        });

        let sink = rows.clone();
        engine.register_fn("ui_rows", move |items: Array| {
            *sink.lock().unwrap() = items
                .into_iter()
                .filter_map(|i| i.try_cast::<Map>())
                .map(|m| format!("{}|{}|{}", field(&m, "title"), field(&m, "id"), field(&m, "subtitle")))
                .collect();
        });

        let db = store.clone();
        engine.register_fn("data_get", move |key: &str| -> Dynamic {
            match db.lock().unwrap().get(key) {
                Some(value) => Dynamic::from(value.clone()),
                None => Dynamic::UNIT,
            }
        });

        let db = store.clone();
        engine.register_fn("data_set", move |key: &str, value: &str| -> bool {
            db.lock().unwrap().insert(key.to_owned(), value.to_owned());
            true
        });

        engine.register_fn("http_get", |_url: &str| -> Map {
            let mut map = Map::new();
            map.insert("ok".into(), Dynamic::from(true));
            map.insert("status".into(), Dynamic::from(200_i64));
            map.insert("body".into(), Dynamic::from(String::from(FEED)));
            map.insert("error".into(), Dynamic::from(String::new()));
            map
        });

        // The real parser, not a stub.
        engine.register_fn("parse_xml", |text: &str| -> Dynamic {
            parse_xml(text).unwrap_or(Dynamic::UNIT)
        });

        let sink = added.clone();
        engine.register_fn("add_torrent_url", move |url: &str| -> bool {
            sink.lock().unwrap().push(url.to_owned());
            true
        });

        let source = include_str!("../../docs/plugins/rss.rhai");
        let ast = engine
            .compile(source)
            .expect("docs/plugins/rss.rhai must compile");

        let mut scope = rhai::Scope::new();
        engine
            .run_ast_with_scope(&mut scope, &ast)
            .expect("the top level should run");

        let _ = engine
            .call_fn::<Dynamic>(&mut scope, &ast, "on_session_start", ())
            .expect("on_session_start should not fail");
        // Nothing fetched yet: a plugin must not reach the network merely
        // because the application started.
        assert!(rows.lock().unwrap().is_empty());

        let _ = engine
            .call_fn::<Dynamic>(&mut scope, &ast, "on_ui_open", ())
            .expect("on_ui_open should not fail");

        let listed = rows.lock().unwrap().clone();
        assert_eq!(
            listed,
            vec![String::from(
                "Ubuntu 24.04 & friends|https://example.invalid/one.torrent|Mon, 01 Sep 2026 10:00:00 GMT"
            )],
            "the enclosure URL should win over <link>, the title should be unescaped,              and the date should reach the row"
        );

        // Clicking the row adds exactly what the feed pointed at.
        let _ = engine
            .call_fn::<Dynamic>(
                &mut scope,
                &ast,
                "on_ui_row",
                (String::from("https://example.invalid/one.torrent"),),
            )
            .expect("on_ui_row should not fail");
        assert_eq!(
            added.lock().unwrap().clone(),
            vec![String::from("https://example.invalid/one.torrent")]
        );

        // Adding a feed goes through the store, so it survives a restart.
        let _ = engine
            .call_fn::<Dynamic>(
                &mut scope,
                &ast,
                "on_ui_button",
                (String::from("add"), String::from("https://example.invalid/second.xml")),
            )
            .expect("on_ui_button should not fail");
        assert_eq!(
            store.lock().unwrap().get("feeds").unwrap(),
            "https://example.invalid/feed.xml\nhttps://example.invalid/second.xml"
        );
    }

    /// Serve `body` once over HTTP on a loopback port, and hand back the URL.
    ///
    /// A real socket rather than a mock: the point of this test is the parts
    /// that only exist outside the process - reqwest, chunked reads, the
    /// status line - which a stubbed `http_get` cannot exercise.
    fn serve_once(body: String) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            // Read the request line and headers; the body is not our concern.
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 200 OK{sep}Content-Type: application/rss+xml{sep}Content-Length: {len}{sep}Connection: close{sep}{sep}{body}",
                sep = "\r\n",
                len = body.len(),
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });

        (format!("http://127.0.0.1:{port}/feed.xml"), handle)
    }

    /// The shipped RSS plugin against a real HTTP server.
    ///
    /// Everything below the plugin is production code: the registered
    /// `http_get`, reqwest, `parse_xml`, and the real key/value store on a real
    /// (in-memory) settings database. Only the window and the torrent add are
    /// recorders, because neither exists without a UI and a session.
    ///
    /// This is the test that answers "does the RSS plugin actually work".
    #[test]
    fn the_rss_plugin_reads_a_real_feed_over_http() {
        use std::sync::Mutex;

        let (url, server) = serve_once(String::from(FEED));

        // The plugin thread is not a runtime thread in production either, so
        // this mirrors how `fetch` is really called.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime for the fetch");
        let handle = runtime.handle().clone();

        let cfg = Arc::new(test_cfg());
        let rows: Arc<Mutex<Vec<String>>> = Arc::default();
        let status: Arc<Mutex<String>> = Arc::default();
        let added: Arc<Mutex<Vec<String>>> = Arc::default();

        let mut engine = rhai::Engine::new();
        crate::plugins::apply_limits(&mut engine);
        engine.register_fn("log", |message: &str| println!("plugin: {message}"));
        stub_surfaces(&mut engine);

        let sink = status.clone();
        engine.register_fn("ui_status", move |text: &str| {
            *sink.lock().unwrap() = text.to_owned();
        });

        let sink = rows.clone();
        engine.register_fn("ui_rows", move |items: Array| {
            *sink.lock().unwrap() = items
                .into_iter()
                .filter_map(|i| i.try_cast::<Map>())
                .map(|m| format!("{}|{}", field(&m, "title"), field(&m, "id")))
                .collect();
        });

        // The real store, on a real database.
        let (c, key) = (cfg.clone(), store_key("rss"));
        engine.register_fn("data_get", move |k: &str| -> Dynamic {
            match read_store(&c, &key).remove(k) {
                Some(v) => Dynamic::from(v),
                None => Dynamic::UNIT,
            }
        });
        let (c, key) = (cfg.clone(), store_key("rss"));
        engine.register_fn("data_set", move |k: &str, v: &str| -> bool {
            store_set(&c, &key, k, v)
        });

        // The real HTTP path and the real parser.
        let h = handle.clone();
        engine.register_fn("http_get", move |u: &str| -> Map {
            http_get(&h, &reqwest::Client::new(), u)
        });
        engine.register_fn("parse_xml", |text: &str| -> Dynamic {
            parse_xml(text).unwrap_or(Dynamic::UNIT)
        });

        let sink = added.clone();
        engine.register_fn("add_torrent_url", move |u: &str| -> bool {
            sink.lock().unwrap().push(u.to_owned());
            true
        });

        let ast = engine
            .compile(include_str!("../../docs/plugins/rss.rhai"))
            .expect("rss.rhai must compile");
        let mut scope = rhai::Scope::new();
        engine.run_ast_with_scope(&mut scope, &ast).unwrap();
        let _ = engine
            .call_fn::<Dynamic>(&mut scope, &ast, "on_session_start", ())
            .unwrap();

        // Add the feed the way a person would: type the URL, press the button.
        let _ = engine
            .call_fn::<Dynamic>(
                &mut scope,
                &ast,
                "on_ui_button",
                (String::from("add"), url.clone()),
            )
            .expect("adding a feed should not fail");

        server.join().expect("the server thread should finish");

        // It went over the wire, came back, parsed, and became a row.
        assert_eq!(
            rows.lock().unwrap().clone(),
            vec![String::from(
                "Ubuntu 24.04 & friends|https://example.invalid/one.torrent"
            )],
            "status was: {}",
            status.lock().unwrap()
        );
        assert!(
            status.lock().unwrap().contains("1 item"),
            "status was: {}",
            status.lock().unwrap()
        );

        // And the feed was persisted, so a restart would still have it.
        assert_eq!(read_store(&cfg, &store_key("rss")).get("feeds"), Some(&url));

        // Clicking the row reaches the add.
        let _ = engine
            .call_fn::<Dynamic>(
                &mut scope,
                &ast,
                "on_ui_row",
                (String::from("https://example.invalid/one.torrent"),),
            )
            .unwrap();
        assert_eq!(
            added.lock().unwrap().clone(),
            vec![String::from("https://example.invalid/one.torrent")]
        );
    }

    /// A server that is not there is a field to check, not a dead handler.
    #[test]
    fn an_unreachable_server_comes_back_as_an_error_field() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        // Port 1 on loopback: nothing listens there, and it fails fast.
        let response = http_get(
            runtime.handle(),
            &reqwest::Client::new(),
            "http://127.0.0.1:1/feed.xml",
        );
        assert!(!response.get("ok").unwrap().clone().as_bool().unwrap());
        assert_eq!(response.get("status").unwrap().clone().as_int().unwrap(), 0);
        assert!(
            !response.get("error").unwrap().clone().into_string().unwrap().is_empty(),
            "an unreachable server should say why"
        );
    }

    /// A feed whose items carry a magnet link in `<link>` rather than an
    /// `<enclosure>` - which is what most torrent feeds actually look like,
    /// and a different branch of the plugin's `item_link` from the one the
    /// enclosure test covers.
    const MAGNET_FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Test feed</title>
    <item>
      <title>Sintel &amp; friends</title>
      <link>magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&amp;dn=Sintel</link>
      <description><![CDATA[A <b>Blender</b> open movie.]]></description>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn a_magnet_in_the_link_element_is_read_and_unescaped() {
        let root = parse_xml(MAGNET_FEED).expect("feed parses").cast::<Map>();
        let item = find(&root, "item").expect("there is an item");

        // The ampersands separating magnet parameters arrive as entities and
        // must come back as ampersands, or the tracker list is lost.
        assert_eq!(
            text_of(&item, "link"),
            "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel"
        );
        assert_eq!(text_of(&item, "title"), "Sintel & friends");
    }

    /// The same feed, through the shipped plugin and a real socket: the row's
    /// id must be the magnet, ready to hand straight to `add_torrent_url`.
    #[test]
    fn the_rss_plugin_reads_magnet_links_from_a_real_feed() {
        use std::sync::Mutex;

        let (url, server) = serve_once(String::from(MAGNET_FEED));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        let cfg = Arc::new(test_cfg());
        let rows: Arc<Mutex<Vec<String>>> = Arc::default();
        let added: Arc<Mutex<Vec<String>>> = Arc::default();

        let mut engine = rhai::Engine::new();
        crate::plugins::apply_limits(&mut engine);
        engine.register_fn("log", |m: &str| println!("plugin: {m}"));
        stub_surfaces(&mut engine);
        engine.register_fn("ui_status", |_: &str| {});

        let sink = rows.clone();
        engine.register_fn("ui_rows", move |items: Array| {
            *sink.lock().unwrap() = items
                .into_iter()
                .filter_map(|i| i.try_cast::<Map>())
                .map(|m| field(&m, "id"))
                .collect();
        });

        let (c, key) = (cfg.clone(), store_key("rss"));
        engine.register_fn("data_get", move |k: &str| -> Dynamic {
            match read_store(&c, &key).remove(k) {
                Some(v) => Dynamic::from(v),
                None => Dynamic::UNIT,
            }
        });
        let (c, key) = (cfg.clone(), store_key("rss"));
        engine.register_fn("data_set", move |k: &str, v: &str| -> bool {
            store_set(&c, &key, k, v)
        });

        let h = runtime.handle().clone();
        engine.register_fn("http_get", move |u: &str| -> Map {
            http_get(&h, &reqwest::Client::new(), u)
        });
        engine.register_fn("parse_xml", |t: &str| -> Dynamic {
            parse_xml(t).unwrap_or(Dynamic::UNIT)
        });

        let sink = added.clone();
        engine.register_fn("add_torrent_url", move |u: &str| -> bool {
            sink.lock().unwrap().push(u.to_owned());
            true
        });

        let ast = engine
            .compile(include_str!("../../docs/plugins/rss.rhai"))
            .unwrap();
        let mut scope = rhai::Scope::new();
        engine.run_ast_with_scope(&mut scope, &ast).unwrap();
        let _ = engine
            .call_fn::<Dynamic>(&mut scope, &ast, "on_session_start", ())
            .unwrap();
        let _ = engine
            .call_fn::<Dynamic>(&mut scope, &ast, "on_ui_button", (String::from("add"), url))
            .unwrap();
        server.join().unwrap();

        let magnet = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel";
        assert_eq!(rows.lock().unwrap().clone(), vec![String::from(magnet)]);

        let _ = engine
            .call_fn::<Dynamic>(&mut scope, &ast, "on_ui_row", (String::from(magnet),))
            .unwrap();
        assert_eq!(added.lock().unwrap().clone(), vec![String::from(magnet)]);
    }

    /// A plugin gets one dropdown, and it cannot be made arbitrarily tall.
    ///
    /// The count is the abuse worth bounding: the menu bar is shared with the
    /// application's own menus, and a plugin that could fill the screen from
    /// it would be covering the client rather than extending it.
    #[test]
    fn a_menu_is_capped_and_malformed_entries_are_dropped() {
        let entry = |id: &str, label: &str| {
            let mut m = Map::new();
            m.insert("id".into(), Dynamic::from(String::from(id)));
            m.insert("label".into(), Dynamic::from(String::from(label)));
            Dynamic::from_map(m)
        };

        let plenty: Array = (0..MENU_ITEMS_MAX + 25)
            .map(|i| entry(&format!("id{i}"), &format!("Item {i}")))
            .collect();
        let kept = menu_items("greedy", plenty);
        assert_eq!(kept.len(), MENU_ITEMS_MAX);
        // The head is kept, so a plugin's first and most important items are
        // the ones that survive.
        assert_eq!(kept[0], (String::from("id0"), String::from("Item 0")));

        // Anything that is not a map is not an item. A row with no label is
        // still an item - a blank one is the plugin's mistake to see.
        let mixed: Array = vec![
            entry("a", "A"),
            Dynamic::from(42_i64),
            Dynamic::from(String::from("nope")),
            entry("b", "B"),
        ];
        assert_eq!(
            menu_items("mixed", mixed),
            vec![
                (String::from("a"), String::from("A")),
                (String::from("b"), String::from("B")),
            ]
        );
    }

    /// The two ceilings that have to agree.
    ///
    /// `http_get` returns the response body as a Rhai string, so an engine that
    /// cannot hold `HTTP_LIMIT` bytes turns every larger fetch into "Length of
    /// string too large" - an error the plugin author cannot act on, did not
    /// cause, and cannot see coming. They were 4 MB and 64 KB apart, which made
    /// every feed over 64 KB fail.
    #[test]
    fn the_string_ceiling_can_hold_a_whole_response() {
        let mut engine = rhai::Engine::new();
        crate::plugins::apply_limits(&mut engine);
        assert!(
            engine.max_string_size() >= HTTP_LIMIT,
            "a {HTTP_LIMIT} byte fetch cannot fit in a {} byte string",
            engine.max_string_size()
        );
    }

    /// A feed bigger than the old string ceiling, end to end through the
    /// shipped plugin.
    ///
    /// The earlier live test served 43 KB and passed by luck: it was under the
    /// 64 KB limit by twenty kilobytes. This one is deliberately over it, so
    /// the ceilings drifting apart again fails here rather than on a real feed.
    #[test]
    fn a_feed_larger_than_the_old_string_ceiling_still_loads() {
        use std::sync::Mutex;

        // ~200 KB: comfortably past 64 KB, and a realistic size for a busy
        // tracker's feed.
        let mut body = String::from(
            "<?xml version=\"1.0\"?>\n<rss version=\"2.0\">\n<channel>\n<title>Big</title>\n",
        );
        let wanted = 200 * 1024;
        let mut n = 0;
        while body.len() < wanted {
            body.push_str(&format!(
                "<item><title>Episode {n} of something with a reasonably long name</title>\
                 <link>magnet:?xt=urn:btih:{n:040x}&amp;dn=Episode+{n}</link>\
                 <description><![CDATA[Filler text to make this item a realistic size.]]></description>\
                 </item>\n"
            ));
            n += 1;
        }
        body.push_str("</channel>\n</rss>\n");
        assert!(body.len() > 64 * 1024, "the feed must exceed the old ceiling");

        let size = body.len();
        let (url, server) = serve_once(body);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        let cfg = Arc::new(test_cfg());
        let rows: Arc<Mutex<Vec<String>>> = Arc::default();
        let status: Arc<Mutex<String>> = Arc::default();

        let mut engine = rhai::Engine::new();
        crate::plugins::apply_limits(&mut engine);
        stub_surfaces(&mut engine);
        engine.register_fn("log", |m: &str| println!("plugin: {m}"));
        engine.register_fn("add_torrent_url", |_: &str| -> bool { true });

        let sink = status.clone();
        engine.register_fn("ui_status", move |text: &str| {
            *sink.lock().unwrap() = text.to_owned();
        });
        let sink = rows.clone();
        engine.register_fn("ui_rows", move |items: Array| {
            *sink.lock().unwrap() = items
                .into_iter()
                .filter_map(|i| i.try_cast::<Map>())
                .map(|m| field(&m, "id"))
                .collect();
        });

        let (c, key) = (cfg.clone(), store_key("rss"));
        engine.register_fn("data_get", move |k: &str| -> Dynamic {
            match read_store(&c, &key).remove(k) {
                Some(v) => Dynamic::from(v),
                None => Dynamic::UNIT,
            }
        });
        let (c, key) = (cfg.clone(), store_key("rss"));
        engine.register_fn("data_set", move |k: &str, v: &str| -> bool {
            store_set(&c, &key, k, v)
        });

        let h = runtime.handle().clone();
        engine.register_fn("http_get", move |u: &str| -> Map {
            http_get(&h, &reqwest::Client::new(), u)
        });
        engine.register_fn("parse_xml", |t: &str| -> Dynamic {
            parse_xml(t).unwrap_or(Dynamic::UNIT)
        });

        let ast = engine
            .compile(include_str!("../../docs/plugins/rss.rhai"))
            .unwrap();
        let mut scope = rhai::Scope::new();
        engine.run_ast_with_scope(&mut scope, &ast).unwrap();
        let _ = engine
            .call_fn::<Dynamic>(&mut scope, &ast, "on_session_start", ())
            .unwrap();

        // Adding the feed is what fetches it - the path the screenshot died on.
        let outcome =
            engine.call_fn::<Dynamic>(&mut scope, &ast, "on_ui_button", (String::from("add"), url));
        server.join().unwrap();
        assert!(
            outcome.is_ok(),
            "a {size} byte feed should load, not fail: {:?}",
            outcome.err()
        );

        let listed = rows.lock().unwrap().clone();
        assert_eq!(listed.len(), n, "every item should reach the list");
        assert!(
            listed[0].starts_with("magnet:?xt=urn:btih:"),
            "rows carry the magnet, got {:?}",
            listed.first()
        );
        assert!(
            status.lock().unwrap().contains("item(s) from"),
            "status was: {}",
            status.lock().unwrap()
        );
    }
}
