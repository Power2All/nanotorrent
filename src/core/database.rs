// Port of src/picotorrent/core/database.{hpp,cpp}
//
// Uses the exact same migration SQL files as the original (copied verbatim
// into the migrations/ folder) and registers the same custom SQLite
// functions the C++ code registered (get_known_folder_path and
// get_user_default_ui_language).

use std::sync::Mutex;

use anyhow::Result;
use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;

use super::environment::Environment;

macro_rules! migration {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!("../../migrations/", $name, ".sql")),
        )
    };
}

/// All migrations, in the same order the C++ build embedded them
/// (lexicographic by timestamped file name).
const MIGRATIONS: &[(&str, &str)] = &[
    migration!("20181208115043_create_setting_table"),
    migration!("20181208115732_insert_default_settings"),
    migration!("20181211212324_create_torrent_table"),
    migration!("20181211212637_create_torrent_resume_data_table"),
    migration!("20181211213708_create_column_state_table"),
    migration!("20181212205212_create_session_state_table"),
    migration!("20181212213425_create_log_table"),
    migration!("20190322145531_create_list_state_table"),
    migration!("20191222212037_create_torrent_magnet_uri_table"),
    migration!("20200214214712_create_path_history_table"),
    migration!("20200403220617_create_persistence_table"),
    migration!("20200512234312_create_listen_interface_table"),
    migration!("20200513225831_create_dht_bootstrap_node_table"),
    migration!("20200823145617_insert_tracker_settings"),
    migration!("20200912230012_enhance_setting_table"),
    migration!("20200916213321_insert_locale_name_setting"),
    migration!("20200919221011_create_label_table"),
    migration!("20200925235912_save_resume_data_interval"),
    migration!("20201015200912_insert_console_settings"),
    migration!("20201027213145_insert_overview_columns"),
    migration!("20201107234213_setup_filters"),
    migration!("20201219222232_insert_connections_limit"),
    migration!("20201227195100_insert_ipfilter_settings"),
    migration!("20220508103321_insert_theme_id"),
    migration!("20230511023104_extend_advanced_settings"),
    migration!("20260714000000_add_torrent_timestamps"),
    migration!("20260716000000_reinstate_geoip_settings"),
    migration!("20260815000000_github_release_update_check"),
    migration!("20260821000000_web_api_settings"),
    migration!("20260822000000_notification_settings"),
    migration!("20260828000000_webui_advanced_settings"),
];

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Open (creating if needed) the settings database in the profile folder.
    pub fn open(env: &Environment) -> Result<Database> {
        Self::open_path(&env.get_database_file_path())
    }

    /// Open a database at an explicit path. Used by the tests and by the
    /// one-shot PicoTorrent import, which reads someone else's file.
    pub fn open_path(path: &std::path::Path) -> Result<Database> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        tracing::info!("Loading settings database from {}", path.display());

        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;

        Self::register_functions(&conn)?;

        Ok(Database {
            conn: Mutex::new(conn),
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Database> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Self::register_functions(&conn)?;
        Ok(Database {
            conn: Mutex::new(conn),
        })
    }

    fn register_functions(conn: &Connection) -> Result<()> {
        // Port of Database::GetKnownFolderPath
        conn.create_scalar_function(
            "get_known_folder_path",
            1,
            FunctionFlags::SQLITE_UTF8,
            |ctx| {
                let folder_id: String = ctx.get(0)?;
                if folder_id == "FOLDERID_Downloads" {
                    Ok(Some(
                        Environment::get_downloads_path()
                            .to_string_lossy()
                            .into_owned(),
                    ))
                } else {
                    Ok(None)
                }
            },
        )?;

        // Port of Database::GetUserDefaultUILanguage. 1033 is en-US which
        // was the default on most systems; the original called the Win32
        // GetUserDefaultUILanguage().
        conn.create_scalar_function(
            "get_user_default_ui_language",
            0,
            FunctionFlags::SQLITE_UTF8,
            |_ctx| Ok(1033i64),
        )?;

        Ok(())
    }

    /// Port of Database::Migrate.
    pub fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();

        conn.execute_batch(
            "create table if not exists migration_history (\
                 id integer primary key,\
                 name text not null unique\
             );",
        )?;

        tracing::info!("Found {} migrations", MIGRATIONS.len());

        conn.execute_batch("BEGIN TRANSACTION;")?;

        for (name, sql) in MIGRATIONS {
            // The C++ version embedded migrations as Win32 resources whose
            // names are stored UPPERCASE in migration_history, so compare
            // case-insensitively to stay compatible with databases created
            // by the original client.
            let exists: i64 = conn.query_row(
                "select count(*) from migration_history where name = ?1 COLLATE NOCASE",
                [name],
                |row| row.get(0),
            )?;

            if exists > 0 {
                continue;
            }

            if let Err(err) = conn.execute_batch(sql) {
                tracing::error!("Failed to execute migration {name}: {err}");
                conn.execute_batch("ROLLBACK;")?;
                return Err(err.into());
            }

            // Record the name uppercase, matching the C++ convention, so the
            // original client can also open a database this port migrated.
            conn.execute(
                "insert into migration_history (name) values (?1);",
                [name.to_uppercase()],
            )?;

            tracing::info!("Migration {name} applied");
        }

        conn.execute_batch("COMMIT;")?;

        Ok(())
    }

    /// Run a closure with the underlying connection.
    pub fn with<R>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> rusqlite::Result<R> {
        let conn = self.conn.lock().unwrap();
        f(&conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::configuration::Configuration;
    use std::sync::Arc;

    /// Column widths survive a restart, and re-saving one replaces it.
    ///
    /// The upsert is the table's own `UNIQUE (list_id, column_id) ON CONFLICT
    /// REPLACE` rather than a read-modify-write, so this checks the constraint
    /// is actually doing that job - without it every drag would append a row
    /// and the oldest width would win on the next start.
    #[test]
    fn column_widths_round_trip_and_replace() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        let cfg = Configuration::new(db.clone());

        assert!(cfg.get_column_widths("torrents").is_empty());

        cfg.set_column_width("torrents", 0, 447.0);
        cfg.set_column_width("torrents", 3, 120.0);
        let saved = cfg.get_column_widths("torrents");
        assert_eq!(saved.get(&0).copied(), Some(447.0));
        assert_eq!(saved.get(&3).copied(), Some(120.0));
        assert_eq!(saved.len(), 2);

        // Dragging the same column again replaces rather than accumulates.
        cfg.set_column_width("torrents", 0, 260.0);
        let saved = cfg.get_column_widths("torrents");
        assert_eq!(saved.get(&0).copied(), Some(260.0), "the old width won");
        assert_eq!(saved.len(), 2, "a duplicate row was inserted");

        // Lists are independent - the details tabs will key by their own name.
        assert!(cfg.get_column_widths("peers").is_empty());
    }

    #[test]
    fn migrations_apply_and_defaults_load() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        // Running twice must be a no-op.
        db.migrate().unwrap();

        let cfg = Configuration::new(db.clone());

        // Defaults produced by the original migration SQL.
        assert_eq!(cfg.get_int("libtorrent.active_downloads"), Some(3));
        assert!(cfg.get_bool("libtorrent.enable_dht"));
        assert_eq!(
            cfg.get_string("theme_id").as_deref(),
            Some("system")
        );

        // default_save_path is produced by the custom
        // get_known_folder_path() SQLite function.
        let save_path = cfg.get_string("default_save_path").unwrap();
        assert!(save_path.contains("Downloads"), "was: {save_path}");

        // Default listen interface migrated from the setting table.
        let ifaces = cfg.get_listen_interfaces();
        assert_eq!(ifaces.len(), 1);
        assert_eq!(ifaces[0].address, "0.0.0.0");
        assert_eq!(ifaces[0].port, 6881);

        // Default DHT bootstrap nodes and filters. librqbit brings its own
        // bootstrap list, so nothing reads this table - the assertion is here
        // to catch a migration that stops seeding it.
        let nodes: i64 = db
            .with(|conn| conn.query_row("select count(*) from dht_bootstrap_node", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(nodes, 4);
        assert_eq!(cfg.get_filters().len(), 2);

        // The torrent table has the Rust-port timestamp columns.
        db.with(|conn| {
            conn.execute(
                "insert into torrent (info_hash, queue_position, added_on) values ('abc', 0, 123)",
                [],
            )
        })
        .unwrap();
    }

    #[test]
    fn migrations_are_recognized_in_cpp_databases() {
        // The C++ client stored migration names UPPERCASE (Win32 resource
        // names). Re-running migrate() against such a database must be a
        // no-op instead of failing with "table setting already exists".
        let db = Database::open_in_memory().unwrap();
        db.migrate().unwrap();

        db.with(|conn| {
            conn.execute("update migration_history set name = upper(name)", [])
        })
        .unwrap();

        db.migrate().unwrap();
    }

    /// Manual check against a copy of a real database:
    /// NANOTORRENT_TEST_DB=<path> cargo test -- --ignored
    #[test]
    #[ignore = "requires NANOTORRENT_TEST_DB pointing at a database copy"]
    fn migrate_external_database() {
        let path = std::env::var("NANOTORRENT_TEST_DB").unwrap();
        let db = Database::open_path(std::path::Path::new(&path)).unwrap();
        db.migrate().unwrap();
    }

    #[test]
    fn default_filters_parse() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        db.migrate().unwrap();

        let cfg = Configuration::new(db);
        for filter in cfg.get_filters() {
            crate::ui::filters::TorrentFilter::parse(&filter.filter)
                .unwrap_or_else(|err| panic!("{}: {err}", filter.name));
        }
    }
}
