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
