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
            add_url(&handle, &c, &s, url, AddOptions::default())
        });

        let (handle, s, c) = (session.handle(), session.clone(), http.clone());
        engine.register_fn("add_torrent_url", move |url: &str, save_path: &str| -> bool {
            add_url(
                &handle,
                &c,
                &s,
                url,
                AddOptions {
                    save_path: Some(save_path.to_string()),
                    ..Default::default()
                },
            )
        });

        // The third form takes a map, so a script can say `paused` and
        // `label` without also having to say `save_path`.
        let (handle, s, c) = (session.handle(), session.clone(), http);
        engine.register_fn("add_torrent_url", move |url: &str, opts: Map| -> bool {
            let text = |key: &str| match field(&opts, key) {
                v if v.is_empty() => None,
                v => Some(v),
            };
            add_url(
                &handle,
                &c,
                &s,
                url,
                AddOptions {
                    save_path: text("save_path"),
                    // A checkbox arrives as "1" or "", so non-empty is the
                    // rule - but "0" and "false" are read as off too, because
                    // a script that sends either plainly means off and the
                    // literal reading would be the opposite.
                    paused: matches!(
                        field(&opts, "paused").as_str(),
                        v if !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
                    ),
                    label: text("label"),
                },
            )
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

        // A form, in place of the lists, for the settings a single text field
        // cannot carry. Passing an empty array closes it - so a plugin needs
        // one call to open one and one to put it away, and the window never
        // has to guess which it meant.
        let plugin = name.to_owned();
        engine.register_fn(
            "ui_form",
            move |form_id: &str, title: &str, fields: Array| {
                let form_id = form_id.to_owned();
                let title = title.to_owned();
                let fields: Vec<super::ui::Field> = fields
                    .into_iter()
                    .filter_map(|f| f.try_cast::<Map>())
                    .map(|f| super::ui::Field {
                        id: field(&f, "id"),
                        label: field(&f, "label"),
                        kind: match field(&f, "kind").as_str() {
                            "" => String::from("text"),
                            other => other.to_owned(),
                        },
                        value: field(&f, "value"),
                        // Newline-separated rather than an array: `field`
                        // already returns a string, and a choice list written
                        // as one string is easier to build in a script than an
                        // array of arrays.
                        options: field(&f, "options")
                            .lines()
                            .map(str::to_owned)
                            .filter(|o| !o.is_empty())
                            .collect(),
                        hint: field(&f, "hint"),
                    })
                    .collect();
                super::ui::update(&plugin, move |ui| {
                    // An empty field list is the close, whatever id came with
                    // it: a form with no controls is not a form.
                    let closing = fields.is_empty();
                    ui.form_id = if closing { String::new() } else { form_id };
                    ui.form_title = if closing { String::new() } else { title };
                    ui.fields = fields;
                });
            },
        );

        let plugin = name.to_owned();
        engine.register_fn("ui_form_close", move || {
            super::ui::update(&plugin, move |ui| {
                ui.form_id = String::new();
                ui.form_title = String::new();
                ui.fields = Vec::new();
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

    // Seconds since the Unix epoch. No permission: a clock reveals nothing a
    // script could not already infer from how often it is ticked, and without
    // one "ignore this rule for a week" cannot be written at all.
    engine.register_fn("now", || -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    });

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

    register_regex(engine);

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
/// The compiled form of a pattern, from a small cache.
///
/// Cached because the obvious way to use these is inside a loop over a feed's
/// items, which would otherwise recompile the same pattern for every row.
///
/// `size_limit` bounds the compiled program: the default is generous, and a
/// plugin has no business building a megabyte of automaton. `dot_matches_new_line`
/// stays off, so `.` means what someone writing a rule against a one-line title
/// expects.
///
/// ponytail: a plain map with a cap and a clear-when-full, not an LRU. The
/// working set here is a handful of patterns from one plugin's rules; anything
/// cleverer would be more code than the thing it manages.
fn compiled(pattern: &str) -> Option<std::sync::Arc<regex::Regex>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    /// Beyond this many distinct patterns the cache is emptied rather than
    /// grown. A plugin cycling through hundreds of one-off patterns gets
    /// correctness and a recompile, which is the right trade for the rare case.
    const CACHE_MAX: usize = 64;
    /// 64 KiB of compiled program is far more than a feed rule needs.
    const SIZE_LIMIT: usize = 64 * 1024;

    static CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<regex::Regex>>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());

    if let Some(hit) = cache.get(pattern) {
        return hit.clone();
    }

    let built = regex::RegexBuilder::new(pattern)
        .size_limit(SIZE_LIMIT)
        .build();
    let entry = match built {
        Ok(re) => Some(Arc::new(re)),
        Err(err) => {
            // Once per distinct pattern, not once per call: this sits in a loop.
            tracing::warn!("plugin regex {pattern:?} did not compile: {err}");
            None
        }
    };

    if cache.len() >= CACHE_MAX {
        cache.clear();
    }
    cache.insert(pattern.to_owned(), entry.clone());
    entry
}

/// The regular-expression functions, on their own.
///
/// Separate from [`register`] because they need nothing it provides - no
/// session, no permissions, no HTTP client - which is also what lets the
/// tests put them on a bare engine.
fn register_regex(engine: &mut Engine) {
    // Regular expressions. No permission either, for the same reason: it is
    // arithmetic on a string, with no way out to anything.
    //
    // Safe to hand a pattern nobody vetted because of which engine this is. The
    // `regex` crate compiles to an automaton with no backtracking, so matching
    // is linear in the length of the subject and a pattern like `(a+)+$` -
    // which hangs a PCRE-style engine for the rest of the afternoon - simply
    // runs. That plus a size limit on the compiled program is what makes this
    // a reasonable thing to expose to a script.
    //
    // A pattern that will not compile logs once and reports "no match", rather
    // than throwing: a plugin author gets the reason in the log, and a running
    // feed rule does not take the plugin down with it.
    engine.register_fn("regex_match", |pattern: &str, text: &str| -> bool {
        compiled(pattern).is_some_and(|re| re.is_match(text))
    });

    // The matched text, or "" when there is none. Distinguishing "no match"
    // from "matched empty" is not worth a second return value here; a pattern
    // that can match empty is a pattern with nothing to extract.
    engine.register_fn("regex_find", |pattern: &str, text: &str| -> String {
        compiled(pattern)
            .and_then(|re| re.find(text).map(|m| m.as_str().to_owned()))
            .unwrap_or_default()
    });

    // Capture groups, in order, with group 0 (the whole match) first. An
    // unmatched optional group comes back as "" so the positions still line up.
    engine.register_fn("regex_captures", |pattern: &str, text: &str| -> Array {
        let Some(re) = compiled(pattern) else {
            return Array::new();
        };
        let Some(found) = re.captures(text) else {
            return Array::new();
        };
        found
            .iter()
            .map(|group| Dynamic::from(group.map_or(String::new(), |m| m.as_str().to_owned())))
            .collect()
    });
}

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
/// What a script may say about a torrent it is adding.
///
/// A struct rather than four more `add_torrent_url` overloads: the next option
/// after these would need eight, and a script naming what it means is easier
/// to read than one counting arguments.
#[derive(Default)]
pub struct AddOptions {
    pub save_path: Option<String>,
    /// Added paused. `false` - starting straight away - is the default,
    /// because that is what "add this torrent" means to everyone who has not
    /// asked for otherwise.
    pub paused: bool,
    /// A label BY NAME. Names rather than ids because a script has no way to
    /// learn an id. Looked up, never created - a plugin should not be able to
    /// fill somebody's label list by getting a rule wrong.
    pub label: Option<String>,
}

fn add_url(
    handle: &tokio::runtime::Handle,
    client: &reqwest::Client,
    session: &Session,
    url: &str,
    opts: AddOptions,
) -> bool {
    let params = crate::bittorrent::session::AddParams {
        save_path: opts.save_path,
        start_torrent: !opts.paused,
        only_files: None,
        // Looked up, never created: a rule naming a label that does not
        // exist gets no label rather than a new one nobody asked for.
        label_id: opts.label.as_deref().and_then(|name| {
            let id = session.label_id(name);
            if id.is_none() {
                tracing::warn!(target: "plugin", "add_torrent_url: no label named {name:?}");
            }
            id
        }),
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
                append_text(&mut stack, &e);
            }
            // `&amp;`, `&#38;`, `&#x26;`. An entity nobody defined is put back
            // as it was written rather than dropped - a title with a stray
            // ampersand should look wrong, not look shorter.
            Ok(Event::GeneralRef(e)) => {
                append_text(&mut stack, &resolve_entity(&e));
            }
            // CDATA is where feeds put the description, so dropping it would
            // make this useless for the exact job it exists for. Its content
            // is literal by definition, so it is decoded but not unescaped.
            Ok(Event::CData(e)) => {
                append_text(&mut stack, &e);
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
        Dynamic::from(e.local_name().as_ref().to_owned()),
    );

    let mut attrs = Map::new();
    for attr in e.attributes().flatten() {
        let key = attr.key.local_name().as_ref().to_owned();
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
        engine.register_fn("ui_form", |_: &str, _: &str, _: Array| {});
        engine.register_fn("ui_form_close", || {});
        // A fixed clock rather than the real one: a test that reads `now()`
        // wants an answer, not a different answer every run.
        engine.register_fn("now", || -> i64 { 1_760_000_000 });
        engine.register_fn("data_remove", |_: &str| {});
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

    /// A feed with two items, so a rule has something to reject as well as
    /// something to accept.
    const RULES_FEED: &str = r#"<?xml version="1.0"?>
<rss version="2.0">
  <channel>
    <item>
      <title>Ubuntu 24.04 Desktop amd64</title>
      <enclosure url="https://example.invalid/wanted.torrent"/>
    </item>
    <item>
      <title>Ubuntu 24.04 Desktop amd64 BETA</title>
      <enclosure url="https://example.invalid/beta.torrent"/>
    </item>
    <item>
      <title>Fedora 41 Workstation</title>
      <enclosure url="https://example.invalid/fedora.torrent"/>
    </item>
  </channel>
</rss>"#;

    /// The auto-downloader: rules pick items out of a feed on their own.
    ///
    /// This is the part of the plugin with somewhere to go wrong. "must
    /// contain" has to require every term, "must not contain" has to reject on
    /// any of them, and an item that has already been taken must not come back
    /// on the next sweep - which is what the `seen` list is for and what a feed
    /// re-read would otherwise undo.
    #[test]
    fn the_rss_plugin_auto_downloads_what_its_rules_match() {
        use std::sync::Mutex;

        let store: Arc<Mutex<std::collections::BTreeMap<String, String>>> = Arc::default();
        let added: Arc<Mutex<Vec<String>>> = Arc::default();

        {
            let mut db = store.lock().unwrap();
            db.insert(
                String::from("feeds"),
                String::from("https://example.invalid/feed.xml"),
            );
            // enabled \t name \t must \t must not \t save path.
            // "ubuntu,debian" is the comma-as-OR form the plugin documents.
            db.insert(
                String::from("rules"),
                String::from("1\tISOs\tubuntu,debian amd64\tbeta\tD:\\isos"),
            );
        }

        let mut engine = rhai::Engine::new();
        crate::plugins::apply_limits(&mut engine);
        engine.register_fn("log", |_: &str| {});
        engine.register_fn("notify", |_: &str, _: &str| {});
        engine.register_fn("ui_status", |_: &str| {});
        engine.register_fn("ui_rows", |_: Array| {});
        register_regex(&mut engine);
        stub_surfaces(&mut engine);

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
            map.insert("body".into(), Dynamic::from(String::from(RULES_FEED)));
            map.insert("error".into(), Dynamic::from(String::new()));
            map
        });
        engine.register_fn("parse_xml", |text: &str| -> Dynamic {
            parse_xml(text).unwrap_or(Dynamic::UNIT)
        });

        // The options form, because the rule names a save path - that is what
        // the auto-downloader calls now. The bare form returns false so that a
        // regression to it shows up as nothing being added rather than as a
        // download that quietly lost the rule's save path.
        let sink = added.clone();
        engine.register_fn("add_torrent_url", move |url: &str, opts: Map| -> bool {
            assert_eq!(
                field(&opts, "save_path"),
                "D:\\isos",
                "the rule's save path should reach the host"
            );
            sink.lock().unwrap().push(url.to_owned());
            true
        });
        engine.register_fn("add_torrent_url", |_url: &str| -> bool { false });
        engine.register_fn("add_torrent_url", |_url: &str, _save_path: &str| -> bool {
            false
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

        let _ = engine
            .call_fn::<Dynamic>(
                &mut scope,
                &ast,
                "on_ui_menu",
                (String::from("runrules"),),
            )
            .expect("the rules sweep should not fail");

        assert_eq!(
            added.lock().unwrap().clone(),
            vec![String::from("https://example.invalid/wanted.torrent")],
            "the rule should take the plain amd64 item, reject the BETA one on \
             `must not contain`, and reject Fedora on `must contain`"
        );

        // Taken once, and not again on the next sweep.
        let _ = engine
            .call_fn::<Dynamic>(
                &mut scope,
                &ast,
                "on_ui_menu",
                (String::from("runrules"),),
            )
            .expect("the second sweep should not fail");
        assert_eq!(
            added.lock().unwrap().len(),
            1,
            "the same item was downloaded twice"
        );
        assert!(
            store.lock().unwrap()["seen"].contains("wanted.torrent"),
            "the link was not remembered"
        );
    }

    /// A rule can use a regular expression instead of words.
    ///
    /// The point of the `re:` prefix: "amd64 but not the beta" is easy with
    /// words, and "S02 only, any episode" is not.
    #[test]
    fn an_rss_rule_can_match_with_a_regular_expression() {
        use std::sync::Mutex;

        let store: Arc<Mutex<std::collections::BTreeMap<String, String>>> = Arc::default();
        let added: Arc<Mutex<Vec<String>>> = Arc::default();
        {
            let mut db = store.lock().unwrap();
            db.insert(
                String::from("feeds"),
                String::from("https://example.invalid/feed.xml"),
            );
            // Anchored, so "Ubuntu 24.04 Desktop amd64 BETA" is excluded by the
            // end-of-string anchor rather than by a must-not-contain term.
            db.insert(
                String::from("rules"),
                String::from(r"1	Exact	re:^Ubuntu \d+\.\d+ Desktop amd64$		"),
            );
        }

        let mut engine = rhai::Engine::new();
        crate::plugins::apply_limits(&mut engine);
        engine.register_fn("log", |_: &str| {});
        engine.register_fn("notify", |_: &str, _: &str| {});
        engine.register_fn("ui_status", |_: &str| {});
        engine.register_fn("ui_rows", |_: Array| {});
        register_regex(&mut engine);
        stub_surfaces(&mut engine);

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
            map.insert("body".into(), Dynamic::from(String::from(RULES_FEED)));
            map.insert("error".into(), Dynamic::from(String::new()));
            map
        });
        engine.register_fn("parse_xml", |text: &str| -> Dynamic {
            parse_xml(text).unwrap_or(Dynamic::UNIT)
        });
        let sink = added.clone();
        engine.register_fn("add_torrent_url", move |url: &str| -> bool {
            sink.lock().unwrap().push(url.to_owned());
            true
        });
        // The form the auto-downloader uses, so a rule's save path, label and
        // paused flag have somewhere to go. Same sink: what matters to these
        // tests is which links were added, not how they were asked for.
        let sink = added.clone();
        engine.register_fn("add_torrent_url", move |url: &str, _opts: Map| -> bool {
            sink.lock().unwrap().push(url.to_owned());
            true
        });

        let source = include_str!("../../docs/plugins/rss.rhai");
        let ast = engine.compile(source).expect("rss.rhai must compile");
        let mut scope = rhai::Scope::new();
        engine.run_ast_with_scope(&mut scope, &ast).unwrap();
        let _ = engine
            .call_fn::<Dynamic>(&mut scope, &ast, "on_session_start", ())
            .unwrap();
        let _ = engine
            .call_fn::<Dynamic>(&mut scope, &ast, "on_ui_menu", (String::from("runrules"),))
            .unwrap();

        assert_eq!(
            added.lock().unwrap().clone(),
            vec![String::from("https://example.invalid/wanted.torrent")],
            "the anchored pattern should take only the exact title"
        );
    }

    /// A rule that is switched off does nothing, which is the whole point of
    /// being able to switch one off.
    #[test]
    fn a_disabled_rss_rule_downloads_nothing() {
        use std::sync::Mutex;

        let store: Arc<Mutex<std::collections::BTreeMap<String, String>>> = Arc::default();
        let added: Arc<Mutex<Vec<String>>> = Arc::default();
        {
            let mut db = store.lock().unwrap();
            db.insert(
                String::from("feeds"),
                String::from("https://example.invalid/feed.xml"),
            );
            db.insert(String::from("rules"), String::from("0\tISOs\tubuntu\t\t"));
        }

        let mut engine = rhai::Engine::new();
        crate::plugins::apply_limits(&mut engine);
        engine.register_fn("log", |_: &str| {});
        engine.register_fn("notify", |_: &str, _: &str| {});
        engine.register_fn("ui_status", |_: &str| {});
        engine.register_fn("ui_rows", |_: Array| {});
        register_regex(&mut engine);
        stub_surfaces(&mut engine);

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
            map.insert("body".into(), Dynamic::from(String::from(RULES_FEED)));
            map.insert("error".into(), Dynamic::from(String::new()));
            map
        });
        engine.register_fn("parse_xml", |text: &str| -> Dynamic {
            parse_xml(text).unwrap_or(Dynamic::UNIT)
        });
        let sink = added.clone();
        engine.register_fn("add_torrent_url", move |url: &str| -> bool {
            sink.lock().unwrap().push(url.to_owned());
            true
        });
        // The form the auto-downloader uses, so a rule's save path, label and
        // paused flag have somewhere to go. Same sink: what matters to these
        // tests is which links were added, not how they were asked for.
        let sink = added.clone();
        engine.register_fn("add_torrent_url", move |url: &str, _opts: Map| -> bool {
            sink.lock().unwrap().push(url.to_owned());
            true
        });

        let source = include_str!("../../docs/plugins/rss.rhai");
        let ast = engine.compile(source).expect("rss.rhai must compile");
        let mut scope = rhai::Scope::new();
        engine.run_ast_with_scope(&mut scope, &ast).unwrap();
        let _ = engine
            .call_fn::<Dynamic>(&mut scope, &ast, "on_session_start", ())
            .unwrap();
        let _ = engine
            .call_fn::<Dynamic>(&mut scope, &ast, "on_ui_menu", (String::from("runrules"),))
            .unwrap();

        assert!(
            added.lock().unwrap().is_empty(),
            "a disabled rule downloaded something"
        );
    }

    /// An engine that can run rss.rhai's pure helpers - the matching, the
    /// episode arithmetic - without a feed, a store or a window behind it.
    fn rss_engine() -> (rhai::Engine, rhai::AST) {
        let mut engine = rhai::Engine::new();
        crate::plugins::apply_limits(&mut engine);
        register_regex(&mut engine);
        stub_surfaces(&mut engine);
        engine.register_fn("log", |_: &str| {});
        engine.register_fn("notify", |_: &str, _: &str| {});
        engine.register_fn("ui_status", |_: &str| {});
        engine.register_fn("ui_rows", |_: Array| {});
        engine.register_fn("data_get", |_: &str| -> Dynamic { Dynamic::UNIT });
        engine.register_fn("data_set", |_: &str, _: &str| -> bool { true });
        engine.register_fn("http_get", |_: &str| -> Map { Map::new() });
        engine.register_fn("parse_xml", |_: &str| -> Dynamic { Dynamic::UNIT });
        engine.register_fn("add_torrent_url", |_: &str| -> bool { true });
        engine.register_fn("add_torrent_url", |_: &str, _: Map| -> bool { true });
        let ast = engine
            .compile(include_str!("../../docs/plugins/rss.rhai"))
            .expect("rss.rhai must compile");
        (engine, ast)
    }

    /// Season and episode, out of the two forms anybody writes them in.
    #[test]
    fn rss_reads_an_episode_number_from_a_title() {
        let (engine, ast) = rss_engine();
        let mut scope = rhai::Scope::new();
        engine.run_ast_with_scope(&mut scope, &ast).unwrap();

        let mut ep = |title: &str| -> Option<(i64, i64)> {
            let out: Dynamic = engine
                .call_fn(&mut scope, &ast, "episode_of", (String::from(title),))
                .unwrap();
            out.try_cast::<Map>().map(|m| {
                (
                    m.get("season").unwrap().clone().cast::<i64>(),
                    m.get("episode").unwrap().clone().cast::<i64>(),
                )
            })
        };

        assert_eq!(ep("Some.Show.S01E02.1080p"), Some((1, 2)));
        assert_eq!(ep("Some Show s3e14 720p"), Some((3, 14)));
        assert_eq!(ep("Some Show 2x07 HDTV"), Some((2, 7)));
        // A resolution is not an episode: "1080p" must not read as 10x80.
        assert_eq!(ep("Ubuntu 24.04 Desktop amd64"), None);
        assert_eq!(ep("Some Show 1080p"), None);
    }

    /// The episode filter syntax, clause by clause.
    #[test]
    fn rss_episode_filters_read_every_clause_shape() {
        let (engine, ast) = rss_engine();
        let mut scope = rhai::Scope::new();
        engine.run_ast_with_scope(&mut scope, &ast).unwrap();

        let mut hit = |filter: &str, season: i64, episode: i64| -> bool {
            let mut ep = Map::new();
            ep.insert("season".into(), Dynamic::from(season));
            ep.insert("episode".into(), Dynamic::from(episode));
            engine
                .call_fn(&mut scope, &ast, "episode_filter_matches",
                         (String::from(filter), ep))
                .unwrap()
        };

        // "1x25-;" - episode 25 onward, and every later season.
        assert!(!hit("1x25-;", 1, 24));
        assert!(hit("1x25-;", 1, 25));
        assert!(hit("1x25-;", 1, 99));
        assert!(hit("1x25-;", 2, 1), "later seasons are included");

        // A closed range stays inside its season.
        assert!(hit("1x1-10;", 1, 1));
        assert!(hit("1x1-10;", 1, 10));
        assert!(!hit("1x1-10;", 1, 11));
        assert!(!hit("1x1-10;", 2, 5));

        // A whole season.
        assert!(hit("2x;", 2, 1));
        assert!(hit("2x;", 2, 300));
        assert!(!hit("2x;", 3, 1));

        // Several clauses, and single episodes.
        assert!(hit("1x2;1x5;", 1, 2));
        assert!(hit("1x2;1x5;", 1, 5));
        assert!(!hit("1x2;1x5;", 1, 3));

        // A filter that parses to nothing cannot reject anything: a typo must
        // not silently switch a rule off.
        assert!(hit("nonsense", 4, 4));
    }

    /// The word form, the regex form, and the guard against a rule that would
    /// match everything.
    #[test]
    fn rss_rule_matching_follows_the_documented_rules() {
        let (engine, ast) = rss_engine();
        let mut scope = rhai::Scope::new();
        engine.run_ast_with_scope(&mut scope, &ast).unwrap();

        let rule = |must: &str, must_not: &str, regex: bool, episodes: &str| -> Map {
            let mut m = Map::new();
            m.insert("enabled".into(), Dynamic::from(true));
            m.insert("name".into(), Dynamic::from(String::from("r")));
            m.insert("must".into(), Dynamic::from(String::from(must)));
            m.insert("must_not".into(), Dynamic::from(String::from(must_not)));
            m.insert("regex".into(), Dynamic::from(regex));
            m.insert("episodes".into(), Dynamic::from(String::from(episodes)));
            m
        };
        let mut check = |r: Map, title: &str| -> bool {
            engine
                .call_fn(&mut scope, &ast, "rule_matches", (r, String::from(title)))
                .unwrap()
        };

        // Every term must appear, in any order, ignoring case.
        assert!(check(rule("ubuntu desktop", "", false, ""), "Ubuntu 24.04 DESKTOP"));
        assert!(!check(rule("ubuntu desktop", "", false, ""), "Ubuntu 24.04 Server"));

        // Alternatives with "|".
        assert!(check(rule("ubuntu|debian amd64", "", false, ""), "Debian 12 amd64"));
        assert!(!check(rule("ubuntu|debian amd64", "", false, ""), "Fedora 40 amd64"));

        // Must-not rejects on any term.
        assert!(!check(rule("ubuntu", "beta rc", false, ""), "Ubuntu 24.04 beta"));
        assert!(check(rule("ubuntu", "beta rc", false, ""), "Ubuntu 24.04"));

        // Regex mode treats both fields as patterns.
        assert!(check(rule(r"^Show S0\dE\d+", "", true, ""), "Show S02E11 1080p"));
        assert!(!check(rule(r"^Show S0\dE\d+", "", true, ""), "Other S02E11"));

        // A disabled rule matches nothing.
        let mut off = rule("ubuntu", "", false, "");
        off.insert("enabled".into(), Dynamic::from(false));
        assert!(!check(off, "Ubuntu 24.04"));

        // A rule with nothing to match on would take every item in every feed.
        assert!(!check(rule("", "", false, ""), "anything at all"));

        // An episode filter narrows an otherwise matching rule, and an item
        // with no episode in its title is not one of a series' episodes.
        assert!(check(rule("show", "", false, "1x2;"), "Show S01E02 1080p"));
        assert!(!check(rule("show", "", false, "1x2;"), "Show S01E03 1080p"));
        assert!(!check(rule("show", "", false, "1x2;"), "Show 1080p"));
    }

    /// A rule written by the five-field version still loads, and comes back
    /// with the new fields at their defaults. Somebody's rules survive the
    /// upgrade or this was not worth shipping.
    #[test]
    fn rss_reads_rules_written_by_the_older_format() {
        let (engine, ast) = rss_engine();
        let mut scope = rhai::Scope::new();
        engine.run_ast_with_scope(&mut scope, &ast).unwrap();

        let old = "1\tISOs\tubuntu desktop\tbeta\tD:\\isos";
        let parsed: Map = engine
            .call_fn(&mut scope, &ast, "parse_rule", (String::from(old),))
            .unwrap();

        let text = |k: &str| parsed.get(k).unwrap().clone().cast::<String>();
        let flag = |k: &str| parsed.get(k).unwrap().clone().cast::<bool>();
        assert!(flag("enabled"));
        assert_eq!(text("name"), "ISOs");
        assert_eq!(text("must"), "ubuntu desktop");
        assert_eq!(text("must_not"), "beta");
        assert_eq!(text("save_path"), "D:\\isos");
        assert!(!flag("regex"), "regex defaults off");
        assert!(!flag("smart"));
        assert_eq!(text("episodes"), "");
        assert_eq!(parsed.get("ignore_days").unwrap().clone().cast::<i64>(), 0);

        // ...and a rule written now round-trips through the line format.
        let line: String = engine
            .call_fn(&mut scope, &ast, "rule_line", (parsed.clone(),))
            .unwrap();
        let again: Map = engine
            .call_fn(&mut scope, &ast, "parse_rule", (line,))
            .unwrap();
        assert_eq!(
            again.get("save_path").unwrap().clone().cast::<String>(),
            "D:\\isos"
        );
    }

    /// A number typed into a form is not necessarily a number. Rhai's own
    /// `parse_int` throws on one that is not, which would abandon the rest of
    /// the handler - so the plugin checks first.
    #[test]
    fn rss_survives_a_number_field_with_letters_in_it() {
        let (engine, ast) = rss_engine();
        let mut scope = rhai::Scope::new();
        engine.run_ast_with_scope(&mut scope, &ast).unwrap();

        let mut to_int = |text: &str, fallback: i64| -> i64 {
            engine
                .call_fn(&mut scope, &ast, "to_int", (String::from(text), fallback))
                .unwrap()
        };
        assert_eq!(to_int("42", 7), 42);
        assert_eq!(to_int("  9 ", 7), 9);
        assert_eq!(to_int("", 7), 7);
        assert_eq!(to_int("soon", 7), 7);
        assert_eq!(to_int("12 days", 7), 7);
    }

    /// Saving a form is what writes a rule, so this is the path every rule
    /// now takes. Checked through the store rather than by inspecting the
    /// script's own variables: what survives a restart is what was written.
    #[test]
    fn rss_forms_write_settings_and_rules() {
        use std::sync::Mutex;

        let store: Arc<Mutex<std::collections::BTreeMap<String, String>>> = Arc::default();

        let mut engine = rhai::Engine::new();
        crate::plugins::apply_limits(&mut engine);
        register_regex(&mut engine);
        stub_surfaces(&mut engine);
        engine.register_fn("log", |_: &str| {});
        engine.register_fn("notify", |_: &str, _: &str| {});
        engine.register_fn("ui_status", |_: &str| {});
        engine.register_fn("ui_rows", |_: Array| {});
        engine.register_fn("http_get", |_: &str| -> Map { Map::new() });
        engine.register_fn("parse_xml", |_: &str| -> Dynamic { Dynamic::UNIT });
        engine.register_fn("add_torrent_url", |_: &str| -> bool { true });
        engine.register_fn("add_torrent_url", |_: &str, _: Map| -> bool { true });

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
        let db = store.clone();
        engine.register_fn("data_remove", move |key: &str| {
            db.lock().unwrap().remove(key);
        });

        let ast = engine
            .compile(include_str!("../../docs/plugins/rss.rhai"))
            .expect("rss.rhai must compile");
        let mut scope = rhai::Scope::new();
        engine.run_ast_with_scope(&mut scope, &ast).unwrap();
        let _: Dynamic = engine
            .call_fn(&mut scope, &ast, "on_session_start", ())
            .unwrap();

        // --- the settings form ---------------------------------------------
        let mut values = Map::new();
        for (k, v) in [
            ("interval", "30"),
            ("articles", "12"),
            ("auto", ""),
            ("repacks", "1"),
        ] {
            values.insert(k.into(), Dynamic::from(String::from(v)));
        }
        let _: Dynamic = engine
            .call_fn(&mut scope, &ast, "on_ui_form", (String::from("settings"), values))
            .expect("saving the settings form should not fail");

        {
            let db = store.lock().unwrap();
            assert_eq!(db.get("set.interval").unwrap(), "30");
            assert_eq!(db.get("set.articles").unwrap(), "12");
            // An unticked box is the empty string, which is how "off" is told
            // apart from "never set" - the latter is absent entirely.
            assert_eq!(db.get("set.auto").unwrap(), "");
            assert_eq!(db.get("set.repacks").unwrap(), "1");
        }

        // --- the rule form -------------------------------------------------
        let mut values = Map::new();
        for (k, v) in [
            ("name", "Shows"),
            ("enabled", "1"),
            ("must", "some show"),
            ("must_not", "cam"),
            ("regex", ""),
            ("episodes", "1x2-;"),
            ("smart", "1"),
            ("ignore_days", "3"),
            ("label", "TV"),
            ("save_path", "D:\\tv"),
            ("paused", ""),
            ("feeds", ""),
        ] {
            values.insert(k.into(), Dynamic::from(String::from(v)));
        }
        let _: Dynamic = engine
            .call_fn(&mut scope, &ast, "on_ui_form", (String::from("rule"), values))
            .expect("saving the rule form should not fail");

        let line = store.lock().unwrap().get("rules").cloned().unwrap_or_default();
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields[0], "1", "enabled");
        assert_eq!(fields[1], "Shows");
        assert_eq!(fields[2], "some show");
        assert_eq!(fields[3], "cam");
        assert_eq!(fields[4], "D:\\tv");
        assert_eq!(fields[5], "0", "regex off");
        assert_eq!(fields[6], "1x2-;");
        assert_eq!(fields[7], "1", "smart on");
        assert_eq!(fields[8], "3", "ignore days");
        assert_eq!(fields[9], "TV");
        assert_eq!(fields[10], "0", "not paused");

        // A rule with nothing to match on is refused rather than written: it
        // would take every item in every feed.
        let mut empty = Map::new();
        for k in ["name", "enabled", "must", "must_not", "regex", "episodes",
                  "smart", "ignore_days", "label", "save_path", "paused", "feeds"] {
            empty.insert(k.into(), Dynamic::from(String::new()));
        }
        empty.insert("name".into(), Dynamic::from(String::from("Everything")));
        let _: Dynamic = engine
            .call_fn(&mut scope, &ast, "on_ui_form", (String::from("rule"), empty))
            .expect("a refused form is not an error");
        let after = store.lock().unwrap().get("rules").cloned().unwrap_or_default();
        assert!(
            !after.contains("Everything"),
            "a rule with no criteria was written anyway"
        );
    }

    /// Build an engine with just the regex functions on it.
    fn regex_engine() -> rhai::Engine {
        let mut engine = rhai::Engine::new();
        crate::plugins::apply_limits(&mut engine);
        register_regex(&mut engine);
        engine
    }

    #[test]
    fn regex_match_answers_yes_and_no() {
        let engine = regex_engine();
        assert!(
            engine
                .eval::<bool>(r#"regex_match("^Ubuntu \\d+\\.\\d+", "Ubuntu 24.04 Desktop")"#)
                .unwrap()
        );
        assert!(
            !engine
                .eval::<bool>(r#"regex_match("^Debian", "Ubuntu 24.04 Desktop")"#)
                .unwrap()
        );
    }

    #[test]
    fn regex_find_returns_the_matched_text() {
        let engine = regex_engine();
        assert_eq!(
            engine
                .eval::<String>(r#"regex_find("\\d+\\.\\d+", "Ubuntu 24.04 Desktop")"#)
                .unwrap(),
            "24.04"
        );
        // No match is the empty string, not an error.
        assert_eq!(
            engine
                .eval::<String>(r#"regex_find("\\d{9}", "Ubuntu")"#)
                .unwrap(),
            ""
        );
    }

    /// Groups come back in order with the whole match first, and an optional
    /// group that did not participate holds "" so the positions still line up.
    #[test]
    fn regex_captures_keeps_group_positions() {
        let engine = regex_engine();
        let groups = engine
            .eval::<Array>(r#"regex_captures("(\\d+)x(\\d+)", "Show 2x07 720p")"#)
            .unwrap();
        let groups: Vec<String> = groups
            .into_iter()
            .map(|g| g.into_string().unwrap())
            .collect();
        assert_eq!(groups, vec!["2x07", "2", "07"]);

        let optional = engine
            .eval::<Array>(r#"regex_captures("(a)(b)?", "a")"#)
            .unwrap();
        let optional: Vec<String> = optional
            .into_iter()
            .map(|g| g.into_string().unwrap())
            .collect();
        assert_eq!(optional, vec!["a", "a", ""]);

        // No match at all is an empty array rather than a row of blanks.
        assert!(
            engine
                .eval::<Array>(r#"regex_captures("(z)", "a")"#)
                .unwrap()
                .is_empty()
        );
    }

    /// A pattern that will not compile must not take the plugin down with it.
    #[test]
    fn a_broken_pattern_reports_no_match_rather_than_throwing() {
        let engine = regex_engine();
        assert!(
            !engine
                .eval::<bool>(r#"regex_match("(unclosed", "anything")"#)
                .unwrap()
        );
        assert_eq!(
            engine
                .eval::<String>(r#"regex_find("(unclosed", "anything")"#)
                .unwrap(),
            ""
        );
        assert!(
            engine
                .eval::<Array>(r#"regex_captures("(unclosed", "anything")"#)
                .unwrap()
                .is_empty()
        );
    }

    /// The reason a plugin can be handed an unvetted pattern at all.
    ///
    /// `(a+)+$` against a run of `a` with no trailing `b` is the textbook
    /// catastrophic-backtracking case: a PCRE-style engine takes exponential
    /// time and hangs the thread. This one compiles to an automaton with no
    /// backtracking, so it answers immediately - and the assertion is on the
    /// clock, because "returns false" alone would also be true of an engine
    /// that took a week to do it.
    #[test]
    fn a_pathological_pattern_does_not_hang() {
        let engine = regex_engine();
        let started = std::time::Instant::now();
        let matched = engine
            .eval::<bool>(r#"regex_match("(a+)+$", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa!")"#)
            .unwrap();
        assert!(!matched);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "took {:?}, which means this is not the linear-time engine",
            started.elapsed()
        );
    }

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
        // The form the auto-downloader uses, so a rule's save path, label and
        // paused flag have somewhere to go. Same sink: what matters to these
        // tests is which links were added, not how they were asked for.
        let sink = added.clone();
        engine.register_fn("add_torrent_url", move |url: &str, _opts: Map| -> bool {
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
        let sink = added.clone();
        engine.register_fn("add_torrent_url", move |u: &str, _opts: Map| -> bool {
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
        let sink = added.clone();
        engine.register_fn("add_torrent_url", move |u: &str, _opts: Map| -> bool {
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
        // `serve_once` consumes the body and answers exactly one request, and
        // the second half of this test needs a second server.
        let body_again = body.clone();
        let (url, server) = serve_once(body);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        let cfg = Arc::new(test_cfg());
        // This test is about the string ceiling, not about the plugin's own
        // "keep N articles per feed" setting - so that is turned off here, or
        // the list would stop at the default of 50 and prove nothing about
        // size. The cap gets its own check at the end.
        store_set(&cfg, &store_key("rss"), "set.articles", "0");
        let rows: Arc<Mutex<Vec<String>>> = Arc::default();
        let status: Arc<Mutex<String>> = Arc::default();

        let mut engine = rhai::Engine::new();
        crate::plugins::apply_limits(&mut engine);
        stub_surfaces(&mut engine);
        engine.register_fn("log", |m: &str| println!("plugin: {m}"));
        engine.register_fn("add_torrent_url", |_: &str| -> bool { true });
        engine.register_fn("add_torrent_url", |_: &str, _opts: Map| -> bool { true });

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

        // ...and the limit does cap it when one is asked for. `read_feed`
        // directly, because that is the function the limit lives in.
        let (url2, server2) = serve_once(body_again);
        let capped = engine
            .call_fn::<rhai::Array>(&mut scope, &ast, "read_feed", (url2, true, 50_i64))
            .expect("reading with a limit should not fail");
        server2.join().unwrap();
        assert_eq!(capped.len(), 50, "the per-feed limit should cap the list");
        assert!(
            status.lock().unwrap().contains("item(s) from"),
            "status was: {}",
            status.lock().unwrap()
        );
    }
}
