//! Settings out to JSON and back.
//!
//! For moving a configured NanoTorrent to another machine, and for keeping a
//! copy of a setup before changing it. JSON rather than a copy of the database
//! because the point is a file a person can read, diff and edit - a database
//! copy would also carry the torrent list, which is not what "my settings" is.
//!
//! # Encryption does not enter into it
//!
//! Both directions go through [`Database::with`], which hands over the open
//! database - already decrypted, because an encrypted one is held in memory in
//! the clear. So an export is ordinary readable JSON whether or not the
//! database it came from was encrypted, and an import lands in an encrypted
//! database as readily as in a plain one: the commit is sealed back to disk on
//! the way out, by the same path every other write takes.
//!
//! That also means an export is *not* protected the way the database is. It is
//! a plain file, which is why the credentials below are kept out of it.
//!
//! # What does not travel
//!
//! [`PRIVATE`] names three keys that are left out in *both* directions, and the
//! reason differs for each. Two are credentials that a plain file on disk would
//! be a worse home for than the database they came from - especially now that
//! the database can be encrypted. The third is `plugins.grants`, and that one
//! is the important one: it records the exact permission set the user approved
//! for each plugin, in a dialog, on this machine. An import that could write it
//! would let a settings file hand a script network and disk access nobody
//! agreed to - which is the precise attack [`super::dbkey`] exists to stop, so
//! opening a second door to it here would be self-defeating.
//!
//! Everything left out is named in the [`Report`], never dropped quietly.
//!
//! # What import will not do
//!
//! Settings are written with an UPDATE, so a key that does not exist in this
//! build cannot be invented - it is reported as unknown instead. Labels and
//! filters are matched by name and updated in place, and anything unmatched is
//! added; nothing is ever deleted, because `torrent.label_id` points at labels
//! and a tidy-up here would strand torrents.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::configuration::{Configuration, Filter, Label};
use super::database::Database;

/// Settings that stay on the machine they were set on.
const PRIVATE: &[&str] = &[
    // Stored in the clear, because SOCKS needs the password itself rather than
    // a hash of it. Writing it into an export would undo that containment.
    "libtorrent.proxy_password",
    // A credential for the web interface. Even as a hash, a plain file is a
    // downgrade from where it currently lives.
    "webui.password_hash",
    // Not a setting at all: the permissions the user granted each plugin. See
    // the module docs - this is the one that matters.
    "plugins.grants",
];

/// The file format. Field names are the JSON keys, so renaming one is a
/// breaking change to files people already have.
#[derive(Serialize, Deserialize)]
pub struct Export {
    /// Which build wrote this, for whoever opens the file in a year.
    pub nanotorrent: String,
    pub exported: String,
    /// Only settings that have actually been changed - see [`export`].
    #[serde(default)]
    pub settings: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub labels: Vec<LabelEntry>,
    #[serde(default)]
    pub filters: Vec<FilterEntry>,
}

/// A label without its row id, which means nothing outside the database it
/// came from.
#[derive(Serialize, Deserialize)]
pub struct LabelEntry {
    pub name: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub color_enabled: bool,
    #[serde(default)]
    pub save_path: String,
    #[serde(default)]
    pub save_path_enabled: bool,
    #[serde(default)]
    pub apply_filter: String,
    #[serde(default)]
    pub apply_filter_enabled: bool,
}

#[derive(Serialize, Deserialize)]
pub struct FilterEntry {
    pub name: String,
    pub filter: String,
}

/// What an import actually did.
#[derive(Default)]
pub struct Report {
    pub settings: usize,
    pub labels: usize,
    pub filters: usize,
    /// Keys the file named that this build does not have, and keys held back
    /// by [`PRIVATE`]. Reported rather than dropped in silence: a file full of
    /// typos that appears to import cleanly is worse than one that complains.
    pub skipped: Vec<String>,
}

impl Report {
    pub fn summary(&self) -> String {
        let mut out = format!(
            "Imported {} settings, {} labels and {} filters.",
            self.settings, self.labels, self.filters
        );
        if !self.skipped.is_empty() {
            out.push_str(&format!(
                "\nNot applied ({}): {}",
                self.skipped.len(),
                self.skipped.join(", ")
            ));
        }
        out
    }
}

/// Read the settings out as pretty-printed JSON.
///
/// Only settings that differ from their default, not every key with a stored
/// value. Two reasons: the file then says what was *changed*, which is what
/// someone reading it wants to know; and a later version that improves a
/// default is free to, instead of finding today's default pinned in every
/// export.
///
/// "Has a value" is not the same question and was the wrong one - the
/// migrations write a value for most keys, equal to the default, so a real
/// profile has 41 of them and only four that anybody chose. It also keeps
/// `default_save_path` out of the file whenever it is still this machine's
/// Downloads folder, which is not a setting worth carrying to another one.
pub fn export(db: &Arc<Database>) -> Result<String> {
    let rows: Vec<(String, String)> = db
        .with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT key, value FROM setting \
                 WHERE value IS NOT NULL \
                   AND (default_value IS NULL OR value <> default_value) \
                 ORDER BY key",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .context("could not read the settings")?;

    let mut settings = BTreeMap::new();
    for (key, raw) in rows {
        if PRIVATE.contains(&key.as_str()) {
            continue;
        }
        // Stored values are JSON already (Configuration::set writes them that
        // way), so they go in as values rather than as strings holding JSON.
        // One that will not parse is left out instead of being wrapped in
        // quotes, which would change it on the way back in.
        match serde_json::from_str(&raw) {
            Ok(value) => {
                settings.insert(key, value);
            }
            Err(err) => tracing::warn!("setting {key} is not valid JSON, leaving it out: {err}"),
        }
    }

    let cfg = Configuration::new(db.clone());
    let out = Export {
        nanotorrent: String::from(crate::buildinfo::version()),
        exported: chrono::Utc::now().to_rfc3339(),
        settings,
        labels: cfg
            .get_labels()
            .into_iter()
            .map(|l| LabelEntry {
                name: l.name,
                color: l.color,
                color_enabled: l.color_enabled,
                save_path: l.save_path,
                save_path_enabled: l.save_path_enabled,
                apply_filter: l.apply_filter,
                apply_filter_enabled: l.apply_filter_enabled,
            })
            .collect(),
        filters: cfg
            .get_filters()
            .into_iter()
            .map(|f| FilterEntry {
                name: f.name,
                filter: f.filter,
            })
            .collect(),
    };

    serde_json::to_string_pretty(&out).context("could not write the settings as JSON")
}

/// Apply a file produced by [`export`].
///
/// Merges rather than replaces: keys the file does not mention are left alone,
/// and labels and filters it does not mention are kept.
pub fn import(db: &Arc<Database>, json: &str) -> Result<Report> {
    let file: Export =
        serde_json::from_str(json).context("this is not a NanoTorrent settings file")?;

    let mut report = Report::default();

    for (key, value) in &file.settings {
        if PRIVATE.contains(&key.as_str()) {
            report.skipped.push(key.clone());
            continue;
        }
        let raw = serde_json::to_string(value)?;
        // UPDATE, so a key this build does not have changes no rows rather than
        // inventing a setting nothing reads.
        let changed = db
            .with(|conn| {
                conn.execute(
                    "UPDATE setting SET value = ?1 WHERE key = ?2",
                    rusqlite::params![raw, key],
                )
            })
            .with_context(|| format!("could not write {key}"))?;
        match changed {
            0 => report.skipped.push(key.clone()),
            _ => report.settings += 1,
        }
    }

    let cfg = Configuration::new(db.clone());

    // Matched by name, which is what the user named them; the row id is a fact
    // about the old database and means nothing here.
    let existing = cfg.get_labels();
    for entry in file.labels {
        let id = existing
            .iter()
            .find(|l| l.name == entry.name)
            .map_or(-1, |l| l.id);
        cfg.upsert_label(&Label {
            id,
            name: entry.name,
            color: entry.color,
            color_enabled: entry.color_enabled,
            save_path: entry.save_path,
            save_path_enabled: entry.save_path_enabled,
            apply_filter: entry.apply_filter,
            apply_filter_enabled: entry.apply_filter_enabled,
        });
        report.labels += 1;
    }

    let existing = cfg.get_filters();
    for entry in file.filters {
        let id = existing
            .iter()
            .find(|f| f.name == entry.name)
            .map_or(-1, |f| f.id);
        cfg.upsert_filter(&Filter {
            id,
            name: entry.name,
            filter: entry.filter,
        });
        report.filters += 1;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Arc<Database> {
        let db = Arc::new(Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        db
    }

    #[test]
    fn settings_labels_and_filters_survive_the_round_trip() {
        let from = db();
        let cfg = Configuration::new(from.clone());
        // Real keys from the migrations. Writing a key that does not exist is
        // silently nothing - see `Configuration::set_value` - so a test using
        // invented names would pass while carrying no settings at all.
        cfg.set("locale_name", &"nl-NL");
        cfg.set("theme_id", &"dark");
        cfg.upsert_label(&Label {
            name: String::from("Films"),
            save_path: String::from("D:\\films"),
            save_path_enabled: true,
            ..Label::default()
        });
        cfg.upsert_filter(&Filter {
            id: -1,
            name: String::from("Big"),
            filter: String::from("size > 1000"),
        });

        let json = export(&from).expect("export");

        let to = db();
        let report = import(&to, &json).expect("import");
        // Not an exact count: the migrations leave a handful of keys differing
        // from their default, and pinning the number here would make this test
        // fail every time one of those changed. What matters is that the two
        // settings actually set above came across.
        assert!(report.settings >= 2, "{:?}", report.skipped);

        let cfg = Configuration::new(to.clone());
        assert_eq!(cfg.get_string("locale_name").as_deref(), Some("nl-NL"));
        assert_eq!(cfg.get_string("theme_id").as_deref(), Some("dark"));

        let labels = cfg.get_labels();
        let film = labels.iter().find(|l| l.name == "Films").expect("the label");
        assert_eq!(film.save_path, "D:\\films");
        assert!(film.save_path_enabled);

        assert!(cfg.get_filters().iter().any(|f| f.filter == "size > 1000"));
    }

    /// Importing twice must not pile up duplicates - the second pass matches by
    /// name and updates in place.
    #[test]
    fn importing_the_same_file_twice_changes_nothing_the_second_time() {
        let from = db();
        let cfg = Configuration::new(from.clone());
        cfg.upsert_label(&Label {
            name: String::from("Films"),
            ..Label::default()
        });
        let json = export(&from).expect("export");

        let to = db();
        import(&to, &json).expect("first");
        import(&to, &json).expect("second");

        let cfg = Configuration::new(to);
        assert_eq!(
            cfg.get_labels().iter().filter(|l| l.name == "Films").count(),
            1,
            "the label was added twice"
        );
    }

    /// The credentials and the plugin grants stay behind, in both directions.
    ///
    /// `plugins.grants` is the one with teeth: a settings file that could write
    /// it would hand a plugin whatever permissions it liked.
    #[test]
    fn private_settings_are_neither_exported_nor_imported() {
        let from = db();
        let cfg = Configuration::new(from.clone());
        cfg.set("libtorrent.proxy_password", &"hunter2");
        cfg.set("plugins.grants", &"{\"evil\":[\"network\"]}");
        cfg.set("language", &"nl-NL");

        let json = export(&from).expect("export");
        assert!(!json.contains("hunter2"), "the proxy password was exported");
        assert!(!json.contains("evil"), "the plugin grants were exported");

        // And even a hand-written file naming them does not get them in.
        let hostile = r#"{
            "nanotorrent": "0.0.0",
            "exported": "now",
            "settings": { "plugins.grants": "{\"evil\":[\"network\"]}" }
        }"#;
        let to = db();
        let report = import(&to, hostile).expect("import");
        assert_eq!(report.settings, 0);
        assert_eq!(report.skipped, vec![String::from("plugins.grants")]);
        assert_ne!(
            Configuration::new(to).get_string("plugins.grants").as_deref(),
            Some("{\"evil\":[\"network\"]}")
        );
    }

    /// A key this build does not have is reported, not silently ignored.
    #[test]
    fn unknown_keys_are_reported() {
        let to = db();
        let report = import(
            &to,
            r#"{"nanotorrent":"x","exported":"x","settings":{"no_such_setting":1}}"#,
        )
        .expect("import");
        assert_eq!(report.settings, 0);
        assert_eq!(report.skipped, vec![String::from("no_such_setting")]);
    }

    #[test]
    fn a_file_that_is_not_ours_is_refused() {
        assert!(import(&db(), "{\"hello\":true}").is_err());
        assert!(import(&db(), "not json at all").is_err());
    }
}
