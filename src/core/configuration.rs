// Port of src/picotorrent/core/configuration.{hpp,cpp}
//
// Settings are stored JSON-encoded in the `setting` table, exactly like the
// original (SELECT IFNULL(value, default_value) FROM setting WHERE key = ?).

use std::sync::Arc;

use rusqlite::OptionalExtension;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::database::Database;

#[derive(Clone, Debug)]
pub struct Filter {
    pub id: i32,
    pub name: String,
    pub filter: String,
}

#[derive(Clone, Debug)]
pub struct Label {
    pub id: i32,
    pub name: String,
    pub color: String,
    pub color_enabled: bool,
    pub save_path: String,
    pub save_path_enabled: bool,
    pub apply_filter: String,
    pub apply_filter_enabled: bool,
}

impl Default for Label {
    fn default() -> Self {
        Label {
            id: -1,
            name: String::new(),
            color: String::new(),
            color_enabled: false,
            save_path: String::new(),
            save_path_enabled: false,
            apply_filter: String::new(),
            apply_filter_enabled: false,
        }
    }
}

/// A tag: a name a torrent can carry alongside any number of others.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    pub id: i32,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct ListenInterface {
    pub id: i32,
    pub address: String,
    pub port: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnectionProxyType {
    None = 0,
    Socks4 = 1,
    Socks5 = 2,
    Socks5Password = 3,
    Http = 4,
    HttpPassword = 5,
}

impl ConnectionProxyType {
    /// Map the stored integer to a proxy type, defaulting to None for a value
    /// this build does not know - the original stored these as raw ints.
    pub fn from_i64(v: i64) -> ConnectionProxyType {
        match v {
            1 => ConnectionProxyType::Socks4,
            2 => ConnectionProxyType::Socks5,
            3 => ConnectionProxyType::Socks5Password,
            4 => ConnectionProxyType::Http,
            5 => ConnectionProxyType::HttpPassword,
            _ => ConnectionProxyType::None,
        }
    }
}

pub struct Configuration {
    db: Arc<Database>,
}

impl Configuration {
    /// Wrap an open database. Settings are read through on every access
    /// rather than cached, so a change from anywhere is visible everywhere.
    pub fn new(db: Arc<Database>) -> Configuration {
        Configuration { db }
    }

    /// The raw stored string for a key, falling back to the migration's
    /// `default_value` when nothing has been set.
    ///
    /// That fallback is why a fresh database needs no seeding pass: every
    /// setting's default arrives with the schema.
    fn get_value(&self, key: &str) -> Option<String> {
        self.db
            .with(|conn| {
                conn.query_row(
                    "SELECT IFNULL(value, default_value) FROM setting WHERE key = ?1",
                    [key],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
            })
            .ok()
            .flatten()
            .flatten()
    }

    /// Write the raw string for a key.
    ///
    /// An UPDATE, not an upsert: keys come from the migrations, so writing one
    /// that does not exist is a typo and silently doing nothing is the right
    /// outcome - it cannot invent a setting nothing reads.
    fn set_value(&self, key: &str, val: &str) {
        let _ = self.db.with(|conn| {
            conn.execute(
                "UPDATE setting SET value = ?1 WHERE key = ?2",
                rusqlite::params![val, key],
            )
        });
    }

    /// Port of Configuration::Get<T> - the stored value is JSON.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let val = self.get_value(key)?;

        if val.is_empty() {
            return None;
        }

        match serde_json::from_str::<T>(&val) {
            Ok(v) => Some(v),
            Err(err) => {
                tracing::warn!("Failed to parse setting {key}: {val} ({err})");
                None
            }
        }
    }

    /// Port of Configuration::Set<T>.
    pub fn set<T: Serialize>(&self, key: &str, value: &T) {
        self.set_value(key, &serde_json::to_string(value).unwrap_or_default());
    }

    /// A boolean setting, false when missing or unparseable.
    pub fn get_bool(&self, key: &str) -> bool {
        self.get::<bool>(key).unwrap_or(false)
    }

    /// An integer setting, or None when missing or unparseable.
    pub fn get_int(&self, key: &str) -> Option<i64> {
        self.get::<i64>(key)
    }

    /// A string setting, or None when missing.
    pub fn get_string(&self, key: &str) -> Option<String> {
        self.get::<String>(key)
    }

    /// Port of the PersistenceManager - free-form key/value state in the
    /// persistent_object table (window geometry, splitter position, ...).
    pub fn get_persistent(&self, key: &str) -> Option<String> {
        self.db
            .with(|conn| {
                conn.query_row(
                    "SELECT value FROM persistent_object WHERE key = ?1",
                    [key],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            })
            .ok()
            .flatten()
    }

    /// Write free-form UI state (window geometry, splitter position, column
    /// widths) to the `persistent_object` table.
    ///
    /// Separate from settings because these are not configuration: nothing
    /// declares them, nothing defaults them, and losing one costs a window
    /// position rather than a preference.
    pub fn set_persistent(&self, key: &str, value: &str) {
        let _ = self.db.with(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO persistent_object (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, value],
            )
        });
    }

    /// Every saved filter, for the Filters menu and the Preferences tab.
    pub fn get_filters(&self) -> Vec<Filter> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare("select id, name, filter from filter")?;
                let rows = stmt.query_map([], |row| {
                    Ok(Filter {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        filter: row.get(2)?,
                    })
                })?;
                rows.collect()
            })
            .unwrap_or_default()
    }

    /// Every label: name, colours, save path and the auto-apply rule.
    /// Saved widths for one list, as `column_id -> width`.
    ///
    /// Uses PicoTorrent's own `column_state` table, which has carried
    /// `(list_id, column_id, width, is_visible, position)` since 2018 and was
    /// migrated but never read. Only `width` is used today; the rest is what a
    /// hide-column or reorder feature would fill in.
    ///
    /// Columns the caller does not recognise are ignored rather than being an
    /// error: a database written by a build with more columns must not stop
    /// this one from starting.
    pub fn get_column_widths(&self, list_id: &str) -> std::collections::HashMap<i64, f32> {
        self.db
            .with(|conn| {
                let mut stmt = conn
                    .prepare("select column_id, width from column_state where list_id = ?1")?;
                let rows = stmt.query_map([list_id], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? as f32))
                })?;
                rows.collect()
            })
            .unwrap_or_default()
    }

    /// Remember one column's width.
    ///
    /// An upsert by way of the table's own `UNIQUE (list_id, column_id) ON
    /// CONFLICT REPLACE`, so this is one statement rather than a read-modify-
    /// write. `position` is stored as the column index because nothing
    /// reorders columns yet - when something does, it becomes the real order
    /// and this row is already the right shape to hold it.
    pub fn set_column_width(&self, list_id: &str, column_id: i64, width: f32) {
        let _ = self.db.with(|conn| {
            conn.execute(
                "insert into column_state (list_id, column_id, width, is_visible, position) \
                 values (?1, ?2, ?3, 1, ?2)",
                rusqlite::params![list_id, column_id, width.round() as i64],
            )
        });
    }

    pub fn get_labels(&self) -> Vec<Label> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "select id, name, color, color_enabled, save_path, save_path_enabled, \
                     apply_filter, apply_filter_enabled from label",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok(Label {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        color: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        color_enabled: row.get::<_, i64>(3)? > 0,
                        save_path: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        save_path_enabled: row.get::<_, i64>(5)? > 0,
                        apply_filter: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                        apply_filter_enabled: row.get::<_, i64>(7)? > 0,
                    })
                })?;
                rows.collect()
            })
            .unwrap_or_default()
    }

    // --- Extra trackers ---------------------------------------------------

    /// This torrent's stored announce list, grouped into tiers.
    ///
    /// Empty means nobody has edited it and the .torrent's own list stands.
    /// Tiers come back in order with no gaps, however the numbers were stored:
    /// a gap would be invisible in the UI and would change which tier a later
    /// edit landed in.
    pub fn tracker_tiers(&self, info_hash: &str) -> Vec<Vec<String>> {
        let rows: Vec<(i64, String)> = self
            .db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "select tier, url from torrent_tracker where info_hash = ?1                      order by tier, rowid",
                )?;
                let rows = stmt
                    .query_map([info_hash], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap_or_default();

        let mut tiers: Vec<Vec<String>> = Vec::new();
        let mut seen: Option<i64> = None;
        for (tier, url) in rows {
            if seen != Some(tier) {
                tiers.push(Vec::new());
                seen = Some(tier);
            }
            // `rows` is ordered by tier, so the last group is always this one.
            if let Some(last) = tiers.last_mut() {
                last.push(url);
            }
        }
        tiers
    }

    /// The same list, flattened. What the announce machinery wants.
    pub fn extra_trackers(&self, info_hash: &str) -> Vec<String> {
        self.tracker_tiers(info_hash).into_iter().flatten().collect()
    }

    /// Replace the whole stored announce list for one torrent.
    ///
    /// An empty list clears the override entirely, so the torrent goes back to
    /// announcing to whatever its .torrent says - which is the only way back
    /// once someone has edited it.
    pub fn set_trackers(&self, info_hash: &str, tiers: &[Vec<String>]) {
        let _ = self.db.with(|conn| {
            conn.execute("delete from torrent_tracker where info_hash = ?1", [info_hash])?;
            // Renumbered from zero as they are written, so a tier emptied by an
            // edit leaves no gap behind.
            let mut next = 0i64;
            for tier in tiers {
                // Trimmed on the way in, so what is stored is what a later
                // edit will be compared against.
                let usable: Vec<&str> = tier
                    .iter()
                    .map(|u| u.trim())
                    .filter(|u| self.is_tracker_url(u))
                    .collect();
                if usable.is_empty() {
                    continue;
                }
                for url in usable {
                    conn.execute(
                        "insert or ignore into torrent_tracker (info_hash, url, tier)                          values (?1, ?2, ?3)",
                        rusqlite::params![info_hash, url, next],
                    )?;
                }
                next += 1;
            }
            Ok::<_, rusqlite::Error>(())
        });
    }

    /// Is this a usable tracker address?
    ///
    /// Both halves are needed. The SCHEME has to be one a tracker speaks, so
    /// `file:///etc/passwd` is out - it parses perfectly and is not a tracker.
    /// And it has to actually PARSE with a host, so `udp://` on its own, or a
    /// pasted sentence, is out too: the announce machinery would take either
    /// and fail somewhere far away from where it was typed.
    pub fn is_tracker_url(&self, url: &str) -> bool {
        let Ok(parsed) = url::Url::parse(url.trim()) else {
            return false;
        };
        if !matches!(parsed.scheme(), "http" | "https" | "udp" | "ws" | "wss") {
            return false;
        }
        // `Url::host_str` is None for schemes it treats as opaque, and empty
        // for "udp://" with nothing after it.
        parsed.host_str().is_some_and(|host| !host.is_empty())
    }

    /// Remember a tracker someone added. Adding the same one twice is not an
    /// error and does not duplicate it.
    ///
    /// The URL is trimmed and checked for a scheme a tracker actually speaks.
    /// Checked here rather than parsed as a generic URL because `file:///etc`
    /// is a perfectly valid URL and not a tracker - the scheme is the part that
    /// makes this a tracker address, and it is better caught where somebody
    /// typed it than in the announce loop.
    pub fn add_extra_tracker(&self, info_hash: &str, url: &str) -> bool {
        let url = url.trim();
        if !self.is_tracker_url(url) {
            return false;
        }
        self.db
            .with(|conn| {
                conn.execute(
                    "insert or ignore into torrent_tracker (info_hash, url) values (?1, ?2)",
                    rusqlite::params![info_hash, url],
                )
            })
            .is_ok()
    }

    pub fn remove_extra_tracker(&self, info_hash: &str, url: &str) {
        let _ = self.db.with(|conn| {
            conn.execute(
                "delete from torrent_tracker where info_hash = ?1 and url = ?2",
                rusqlite::params![info_hash, url],
            )
        });
    }

    // --- Tags -------------------------------------------------------------
    //
    // A label is one per torrent and carries a colour, a save path and a
    // filter. A tag is none of that: many per torrent, and nothing but a name.
    // The two are worth having side by side for the same reason, and
    // collapsing them would lose exactly the part that makes tags useful.

    /// Every tag, in name order.
    pub fn get_tags(&self) -> Vec<Tag> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare("select id, name from tag order by name")?;
                let rows = stmt.query_map([], |row| {
                    Ok(Tag {
                        id: row.get(0)?,
                        name: row.get(1)?,
                    })
                })?;
                rows.collect()
            })
            .unwrap_or_default()
    }

    /// The tag with this name, creating it if it does not exist yet.
    ///
    /// Tags are made by using them - typing one against a torrent is the whole
    /// creation flow - so this is an upsert on the NAME rather than on an id.
    /// `name` is trimmed, and an empty one is refused: a nameless tag would be
    /// impossible to pick out of a list or type again.
    pub fn ensure_tag(&self, name: &str) -> Option<i32> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        self.db
            .with(|conn| {
                // INSERT then SELECT rather than "insert ... returning": the
                // row may already exist, and the SELECT is needed either way.
                conn.execute("insert or ignore into tag (name) values (?1)", [name])?;
                conn.query_row("select id from tag where name = ?1", [name], |r| r.get(0))
            })
            .ok()
    }

    /// Delete a tag, and take it off every torrent that had it.
    ///
    /// The join rows go first. `torrent_tag` declares ON DELETE CASCADE, but
    /// that only fires when foreign keys are enforced - they are here, and this
    /// does not rely on it, because a leftover join row would attach the tag to
    /// whatever id SQLite hands out next.
    pub fn delete_tag(&self, id: i32) {
        let _ = self.db.with(|conn| {
            conn.execute("delete from torrent_tag where tag_id = ?1", [id])?;
            conn.execute("delete from tag where id = ?1", [id])
        });
    }

    /// Rename a tag. A clash with an existing name is refused rather than
    /// merging the two, which would silently retag every torrent on both.
    pub fn rename_tag(&self, id: i32, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        self.db
            .with(|conn| {
                conn.execute(
                    "update tag set name = ?1 where id = ?2",
                    rusqlite::params![name, id],
                )
            })
            .is_ok()
    }

    /// The tags on one torrent, in name order.
    pub fn tags_for(&self, info_hash: &str) -> Vec<Tag> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "select t.id, t.name from tag t \
                     join torrent_tag tt on tt.tag_id = t.id \
                     where tt.info_hash = ?1 order by t.name",
                )?;
                let rows = stmt.query_map([info_hash], |row| {
                    Ok(Tag {
                        id: row.get(0)?,
                        name: row.get(1)?,
                    })
                })?;
                rows.collect()
            })
            .unwrap_or_default()
    }

    /// Put a tag on a torrent. Doing it twice is not an error.
    pub fn add_tag(&self, info_hash: &str, tag_id: i32) {
        let _ = self.db.with(|conn| {
            conn.execute(
                "insert or ignore into torrent_tag (info_hash, tag_id) values (?1, ?2)",
                rusqlite::params![info_hash, tag_id],
            )
        });
    }

    /// Take a tag off a torrent. The tag itself stays, because other torrents
    /// may still be using it.
    pub fn remove_tag(&self, info_hash: &str, tag_id: i32) {
        let _ = self.db.with(|conn| {
            conn.execute(
                "delete from torrent_tag where info_hash = ?1 and tag_id = ?2",
                rusqlite::params![info_hash, tag_id],
            )
        });
    }

    /// Every torrent's tag names, keyed by info hash.
    ///
    /// One query rather than one per torrent: this feeds the list, which is
    /// redrawn every second, and a per-row lookup would be a query per torrent
    /// per tick.
    pub fn tags_by_torrent(&self) -> std::collections::HashMap<String, Vec<String>> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "select tt.info_hash, t.name from torrent_tag tt \
                     join tag t on t.id = tt.tag_id order by t.name",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;

                let mut out: std::collections::HashMap<String, Vec<String>> =
                    std::collections::HashMap::new();
                for (hash, name) in rows {
                    out.entry(hash).or_default().push(name);
                }
                Ok(out)
            })
            .unwrap_or_default()
    }

    /// Remove a saved PQL filter.
    ///
    /// Nothing references a filter by id the way a torrent references a label,
    /// so unlike `delete_label` this needs no fixup pass.
    pub fn delete_filter(&self, id: i32) {
        let _ = self
            .db
            .with(|conn| conn.execute("delete from filter where id = ?1", [id]));
    }

    /// Insert (id < 0) or update a saved PQL filter.
    pub fn upsert_filter(&self, filter: &Filter) {
        let _ = self.db.with(|conn| {
            if filter.id < 0 {
                conn.execute(
                    "insert into filter (name, filter) values (?1, ?2);",
                    rusqlite::params![filter.name, filter.filter],
                )
            } else {
                conn.execute(
                    "update filter set name = ?1, filter = ?2 where id = ?3",
                    rusqlite::params![filter.name, filter.filter, filter.id],
                )
            }
        });
    }

    /// Delete a label, clearing it from every torrent that carries it first.
    ///
    /// That order matters: `torrent.label_id` left pointing at a row that no
    /// longer exists would show a torrent as labelled with a name nothing can
    /// resolve. Doing it first means the worst outcome is an orphaned label
    /// row, which is invisible, rather than an orphaned reference.
    pub fn delete_label(&self, id: i32) {
        let _ = self.db.with(|conn| {
            conn.execute(
                "update torrent set label_id = NULL where label_id = ?1",
                [id],
            )?;
            conn.execute("delete from label where id = ?1", [id])
        });
    }

    /// Insert a label, or update it when `label.id` names an existing one.
    pub fn upsert_label(&self, label: &Label) {
        let _ = self.db.with(|conn| {
            if label.id < 0 {
                conn.execute(
                    "insert into label (name, color, color_enabled, save_path, save_path_enabled, \
                     apply_filter, apply_filter_enabled) values (?1, ?2, ?3, ?4, ?5, ?6, ?7);",
                    rusqlite::params![
                        label.name,
                        label.color,
                        label.color_enabled,
                        label.save_path,
                        label.save_path_enabled,
                        label.apply_filter,
                        label.apply_filter_enabled
                    ],
                )
            } else {
                conn.execute(
                    "update label set name = ?1, color = ?2, color_enabled = ?3, save_path = ?4, \
                     save_path_enabled = ?5, apply_filter = ?6, apply_filter_enabled = ?7 where id = ?8",
                    rusqlite::params![
                        label.name,
                        label.color,
                        label.color_enabled,
                        label.save_path,
                        label.save_path_enabled,
                        label.apply_filter,
                        label.apply_filter_enabled,
                        label.id
                    ],
                )
            }
        });
    }

    /// The configured listen addresses and ports.
    pub fn get_listen_interfaces(&self) -> Vec<ListenInterface> {
        self.db
            .with(|conn| {
                let mut stmt = conn.prepare("select id, address, port from listen_interface")?;
                let rows = stmt.query_map([], |row| {
                    Ok(ListenInterface {
                        id: row.get(0)?,
                        address: row.get(1)?,
                        port: row
                            .get::<_, Option<String>>(2)?
                            .and_then(|p| p.parse().ok())
                            .unwrap_or(6881),
                    })
                })?;
                rows.collect()
            })
            .unwrap_or_default()
    }

    /// Insert or update one listen interface.
    pub fn upsert_listen_interface(&self, iface: &ListenInterface) {
        let _ = self.db.with(|conn| {
            if iface.id < 0 {
                conn.execute(
                    "insert into listen_interface (address, port) values (?1, ?2);",
                    rusqlite::params![iface.address, iface.port.to_string()],
                )
            } else {
                conn.execute(
                    "update listen_interface set address = ?1, port = ?2 where id = ?3",
                    rusqlite::params![iface.address, iface.port.to_string(), iface.id],
                )
            }
        });
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::database::Database;

    fn cfg() -> Configuration {
        let db = Arc::new(Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        Configuration::new(db)
    }

    /// A torrent row has to exist before it can be tagged: `torrent_tag`
    /// references it.
    fn torrent(cfg: &Configuration, hash: &str) {
        let _ = cfg.db.with(|conn| {
            conn.execute(
                "insert into torrent (info_hash, queue_position) values (?1, 0)",
                [hash],
            )
        });
    }

    /// Everything hung off a torrent goes when the torrent does.
    ///
    /// The startup sweep deletes only the `torrent` row and relies on the
    /// cascade for the rest, so this is what makes that safe - and it only
    /// holds while foreign keys are actually enforced, which is a PRAGMA that
    /// would be easy to lose.
    #[test]
    fn removing_a_torrent_takes_its_settings_with_it() {
        let cfg = cfg();
        torrent(&cfg, "aaaa");
        torrent(&cfg, "bbbb");

        cfg.set_trackers("aaaa", &[vec![String::from("udp://one.example:80")]]);
        let tag = cfg.ensure_tag("linux").unwrap();
        cfg.add_tag("aaaa", tag);
        let _ = cfg.db.with(|conn| {
            conn.execute(
                "insert into torrent_file_priority (info_hash, file_index, priority)                  values ('aaaa', 0, 3)",
                [],
            )
        });

        // The other torrent keeps its own.
        cfg.set_trackers("bbbb", &[vec![String::from("udp://two.example:80")]]);
        cfg.add_tag("bbbb", tag);

        let _ = cfg
            .db
            .with(|conn| conn.execute("delete from torrent where info_hash = 'aaaa'", []));

        assert!(cfg.extra_trackers("aaaa").is_empty(), "trackers survived");
        assert!(cfg.tags_for("aaaa").is_empty(), "tags survived");
        let prios: i64 = cfg
            .db
            .with(|conn| {
                conn.query_row(
                    "select count(*) from torrent_file_priority where info_hash = 'aaaa'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(prios, 0, "file priorities survived");

        assert_eq!(cfg.extra_trackers("bbbb").len(), 1, "the wrong torrent was cleared");
        assert_eq!(cfg.tags_for("bbbb").len(), 1);
        // The tag itself is shared, so it stays.
        assert_eq!(cfg.get_tags().len(), 1);
    }

    /// The stored list is the WHOLE announce list once a torrent is edited, so
    /// replacing it has to be able to shrink as well as grow - that is what
    /// makes "remove tracker" possible for one that came in the .torrent.
    #[test]
    fn setting_trackers_replaces_the_whole_list() {
        let cfg = cfg();
        torrent(&cfg, "aaaa");

        cfg.set_trackers(
            "aaaa",
            &[
                vec![String::from("udp://one.example:80")],
                vec![String::from("udp://two.example:80")],
            ],
        );
        assert_eq!(cfg.extra_trackers("aaaa").len(), 2);
        assert_eq!(cfg.tracker_tiers("aaaa").len(), 2, "the tiers were merged");

        // Fewer than before: the missing one is gone, not merged.
        cfg.set_trackers("aaaa", &[vec![String::from("udp://two.example:80")]]);
        assert_eq!(cfg.extra_trackers("aaaa"), vec!["udp://two.example:80"]);

        // Empty clears the override, so the .torrent's own list stands again.
        cfg.set_trackers("aaaa", &[]);
        assert!(cfg.extra_trackers("aaaa").is_empty());
        assert!(cfg.tracker_tiers("aaaa").is_empty());
    }

    /// A URL with surrounding whitespace is the same URL.
    ///
    /// The Trackers tab used to pad its labels with spaces to fake an indent,
    /// so the URL that came back from a row never matched the one stored -
    /// editing and removing quietly did nothing. The padding is gone, and
    /// trimming here means a pasted URL with a stray space still matches.
    #[test]
    fn a_tracker_url_is_trimmed_before_it_is_matched() {
        let cfg = cfg();
        torrent(&cfg, "aaaa");

        assert!(cfg.is_tracker_url("  udp://tracker.example:80/announce  "));
        cfg.add_extra_tracker("aaaa", "  udp://tracker.example:80/announce  ");
        assert_eq!(
            cfg.extra_trackers("aaaa"),
            vec!["udp://tracker.example:80/announce"]
        );
    }

    /// An emptied tier is dropped rather than leaving a gap. A gap would be
    /// invisible in the list and would change which tier the next edit landed
    /// in, because the picker counts tiers rather than reading their numbers.
    #[test]
    fn an_emptied_tier_leaves_no_gap() {
        let cfg = cfg();
        torrent(&cfg, "aaaa");
        cfg.set_trackers(
            "aaaa",
            &[
                vec![String::from("udp://one.example:80")],
                // Everything here is unusable, so the tier vanishes.
                vec![String::from("nonsense")],
                vec![String::from("udp://three.example:80")],
            ],
        );

        let tiers = cfg.tracker_tiers("aaaa");
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0], vec!["udp://one.example:80"]);
        assert_eq!(tiers[1], vec!["udp://three.example:80"]);
    }

    /// Anything that is not a tracker address is dropped on the way in, so a
    /// pasted mistake cannot reach the announce machinery.
    #[test]
    fn setting_trackers_filters_out_what_is_not_one() {
        let cfg = cfg();
        torrent(&cfg, "aaaa");
        cfg.set_trackers(
            "aaaa",
            &[vec![
                String::from("udp://good.example:80"),
                String::from("file:///etc/passwd"),
                String::from("not a url"),
                String::new(),
            ]],
        );
        assert_eq!(cfg.extra_trackers("aaaa"), vec!["udp://good.example:80"]);
    }

    /// A tracker address needs a scheme a tracker speaks. `file:///etc/passwd`
    /// is a valid URL and is not a tracker, which is why this checks the
    /// scheme rather than merely parsing.
    #[test]
    fn only_tracker_schemes_are_accepted() {
        let cfg = cfg();
        torrent(&cfg, "aaaa");

        for good in [
            "http://tracker.example/announce",
            "https://tracker.example/announce",
            "udp://tracker.example:1337",
            "wss://tracker.example",
        ] {
            assert!(cfg.add_extra_tracker("aaaa", good), "{good} was refused");
        }
        assert_eq!(cfg.extra_trackers("aaaa").len(), 4);

        for bad in [
            "",
            "   ",
            // No scheme at all.
            "tracker.example/announce",
            // Parses, but is not a tracker.
            "file:///etc/passwd",
            "javascript:alert(1)",
            // Right scheme, nothing to announce to.
            "udp://",
            "udp:///announce",
            // A sentence somebody pasted.
            "please add udp://tracker.example:80",
        ] {
            assert!(!cfg.add_extra_tracker("aaaa", bad), "{bad:?} was accepted");
        }
        assert_eq!(cfg.extra_trackers("aaaa").len(), 4, "a bad one got in");
    }

    #[test]
    fn extra_trackers_keep_their_order_and_do_not_duplicate() {
        let cfg = cfg();
        torrent(&cfg, "aaaa");

        cfg.add_extra_tracker("aaaa", "udp://one.example:80");
        cfg.add_extra_tracker("aaaa", "udp://two.example:80");
        // Whitespace is trimmed, so this is the same tracker as the first.
        cfg.add_extra_tracker("aaaa", "  udp://one.example:80  ");

        assert_eq!(
            cfg.extra_trackers("aaaa"),
            vec!["udp://one.example:80", "udp://two.example:80"]
        );

        cfg.remove_extra_tracker("aaaa", "udp://one.example:80");
        assert_eq!(cfg.extra_trackers("aaaa"), vec!["udp://two.example:80"]);
    }

    /// Tags are created by using them, so asking for the same name twice has
    /// to give back the same tag rather than a second one.
    #[test]
    fn ensuring_a_tag_twice_gives_the_same_tag() {
        let cfg = cfg();
        let first = cfg.ensure_tag("linux").expect("a tag");
        let again = cfg.ensure_tag("linux").expect("the same tag");
        assert_eq!(first, again);
        assert_eq!(cfg.get_tags().len(), 1);
    }

    /// Whitespace is trimmed, and a name that is nothing but whitespace is
    /// refused - a tag nobody can type again is not worth creating.
    #[test]
    fn a_tag_name_is_trimmed_and_cannot_be_blank() {
        let cfg = cfg();
        let padded = cfg.ensure_tag("  linux  ").expect("a tag");
        assert_eq!(cfg.get_tags()[0].name, "linux");
        assert_eq!(cfg.ensure_tag("linux"), Some(padded), "the trim did not match");

        assert_eq!(cfg.ensure_tag(""), None);
        assert_eq!(cfg.ensure_tag("   "), None);
        assert_eq!(cfg.get_tags().len(), 1);
    }

    /// The whole point of tags rather than labels: several at once.
    #[test]
    fn a_torrent_carries_several_tags() {
        let cfg = cfg();
        torrent(&cfg, "aaaa");

        for name in ["linux", "iso", "seeding"] {
            let id = cfg.ensure_tag(name).unwrap();
            cfg.add_tag("aaaa", id);
        }

        let names: Vec<String> = cfg.tags_for("aaaa").into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["iso", "linux", "seeding"], "not in name order");

        // Adding the same one again is not an error and does not duplicate.
        let id = cfg.ensure_tag("linux").unwrap();
        cfg.add_tag("aaaa", id);
        assert_eq!(cfg.tags_for("aaaa").len(), 3);

        cfg.remove_tag("aaaa", id);
        let names: Vec<String> = cfg.tags_for("aaaa").into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["iso", "seeding"]);
        // Taking it off one torrent does not delete the tag itself.
        assert_eq!(cfg.get_tags().len(), 3);
    }

    /// Deleting a tag takes it off every torrent. A leftover join row would
    /// otherwise reattach it to whatever id SQLite hands out next.
    #[test]
    fn deleting_a_tag_detaches_it_everywhere() {
        let cfg = cfg();
        torrent(&cfg, "aaaa");
        torrent(&cfg, "bbbb");

        let id = cfg.ensure_tag("linux").unwrap();
        cfg.add_tag("aaaa", id);
        cfg.add_tag("bbbb", id);
        cfg.delete_tag(id);

        assert!(cfg.tags_for("aaaa").is_empty());
        assert!(cfg.tags_for("bbbb").is_empty());
        assert!(cfg.get_tags().is_empty());

        // The next tag must not inherit those join rows.
        let fresh = cfg.ensure_tag("something else").unwrap();
        cfg.add_tag("aaaa", fresh);
        assert!(cfg.tags_for("bbbb").is_empty(), "a stale join row came back");
    }

    /// The list redraws every second, so it reads every torrent's tags in one
    /// query rather than one per row.
    #[test]
    fn tags_by_torrent_groups_them_in_one_pass() {
        let cfg = cfg();
        torrent(&cfg, "aaaa");
        torrent(&cfg, "bbbb");

        let linux = cfg.ensure_tag("linux").unwrap();
        let iso = cfg.ensure_tag("iso").unwrap();
        cfg.add_tag("aaaa", linux);
        cfg.add_tag("aaaa", iso);
        cfg.add_tag("bbbb", linux);

        let all = cfg.tags_by_torrent();
        assert_eq!(all["aaaa"], vec!["iso", "linux"]);
        assert_eq!(all["bbbb"], vec!["linux"]);
        // A torrent with no tags is absent rather than present and empty.
        assert!(!all.contains_key("cccc"));
    }

    #[test]
    fn renaming_a_tag_keeps_its_torrents() {
        let cfg = cfg();
        torrent(&cfg, "aaaa");
        let id = cfg.ensure_tag("linux").unwrap();
        cfg.add_tag("aaaa", id);

        assert!(cfg.rename_tag(id, "Linux ISOs"));
        assert_eq!(cfg.tags_for("aaaa")[0].name, "Linux ISOs");

        // Blank is refused rather than leaving a nameless tag behind.
        assert!(!cfg.rename_tag(id, "   "));
        assert_eq!(cfg.tags_for("aaaa")[0].name, "Linux ISOs");
    }

    /// Renaming onto a name that already exists must not merge the two: both
    /// keep their own torrents, and the rename simply does not happen.
    #[test]
    fn renaming_onto_an_existing_name_is_refused() {
        let cfg = cfg();
        let linux = cfg.ensure_tag("linux").unwrap();
        cfg.ensure_tag("iso").unwrap();

        assert!(!cfg.rename_tag(linux, "iso"), "the unique index did not hold");
        let names: Vec<String> = cfg.get_tags().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["iso", "linux"]);
    }
}
