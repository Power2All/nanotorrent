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
];

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn open(env: &Environment) -> Result<Database> {
        Self::open_path(&env.get_database_file_path())
    }

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

    #[allow(dead_code)]
    pub fn execute(&self, sql: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(sql)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::configuration::Configuration;
    use std::sync::Arc;

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

        // Default DHT bootstrap nodes and filters.
        assert_eq!(cfg.get_dht_bootstrap_nodes().len(), 4);
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
