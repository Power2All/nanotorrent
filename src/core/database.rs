// Port of src/picotorrent/core/database.{hpp,cpp}
//
// Uses the exact same migration SQL files as the original (copied verbatim
// into the migrations/ folder) and registers the same custom SQLite
// functions the C++ code registered (get_known_folder_path and
// get_user_default_ui_language).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
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
    migration!("20260825000000_plugin_settings"),
    migration!("20260828000000_webui_advanced_settings"),
    migration!("20260901000000_show_padding_files"),
    migration!("20260901000001_enable_utp"),
    migration!("20260902000000_plugin_disabled_list"),
    migration!("20260902000001_plugin_grants"),
    migration!("20260903000000_bind_interface"),
    migration!("20260904000000_strict_network"),
    migration!("20260904100000_web_bruteforce"),
    migration!("20260907000000_database_encryption_prompt"),
    migration!("20260907010000_transfer_management"),
    migration!("20260908000000_extra_trackers"),
    migration!("20260908010000_share_limit_toggle"),
    migration!("20260908020000_drop_sequential_columns"),
    migration!("20260908030000_tracker_tiers"),
];

/// The settings database, open.
///
/// Two shapes behind one handle: a plain file that SQLite maintains itself, and
/// an encrypted one, which lives in memory and is sealed back to disk after
/// every commit. [`Database::set_encryption`] moves between them. Everything
/// above this line goes through [`Database::with`] either way.
pub struct Database {
    inner: Mutex<Inner>,
}

struct Inner {
    conn: Connection,
    /// `None` when the database is a plain file. `Some` when it is held in
    /// memory and written back sealed.
    sealed: Option<Sealed>,
}

/// What an encrypted database needs in order to save itself.
struct Sealed {
    path: PathBuf,
    key: [u8; 32],
    /// Raised by the commit hook, lowered by the write that follows. Shared
    /// with the hook, which runs while the connection is borrowed and so cannot
    /// take a lock of its own.
    dirty: std::sync::Arc<AtomicBool>,
}

impl Inner {
    /// Write the database back out, if anything has been committed since the
    /// last time. A no-op for a plain file, which SQLite has already written.
    fn flush(&self) -> Result<()> {
        let Some(sealed) = &self.sealed else {
            return Ok(());
        };
        if !sealed.dirty.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        // Raised again on failure so the next commit tries once more, rather
        // than the change being dropped because the flag had been cleared.
        Self::write_sealed(&self.conn, sealed).inspect_err(|_| {
            sealed.dirty.store(true, Ordering::Relaxed);
        })
    }

    fn write_sealed(conn: &Connection, sealed: &Sealed) -> Result<()> {
        let plain = conn
            .serialize(rusqlite::MAIN_DB)
            .context("could not read the database out of memory")?;
        let out = super::dbkey::seal(&sealed.key, &plain)?;
        write_atomic(&sealed.path, &out)
    }
}

/// Replace a file with new contents, or leave the old ones entirely alone.
///
/// Written beside the target and renamed over it: rename is atomic on NTFS and
/// on POSIX, so a machine that loses power mid-write still has one of the two
/// complete files rather than half of each. The sync matters as much as the
/// rename - without it the rename can land while the contents are still in the
/// page cache, which is the one ordering that loses data.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let tmp = path.with_extension("new");
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

impl Database {
    /// Open (creating if needed) the settings database in the profile folder,
    /// working out for itself how it is protected.
    ///
    /// The file says what it is: a sealed database carries a magic header and a
    /// plain one starts with `SQLite format 3`. That is deliberately not a
    /// setting - such a setting would live in the database the answer is needed
    /// to open - and deliberately not the key file either, so a key left behind
    /// by a conversion that did not finish cannot make a perfectly readable
    /// database refuse to open.
    pub fn open(env: &Environment) -> Result<Database> {
        let path = env.get_database_file_path();
        if !file_is_sealed(&path) {
            return Self::open_plain(&path).map_err(|err| {
                // A key file beside a database that does not announce itself as
                // sealed is worth saying out loud: the likely cause is a damaged
                // header rather than a file that was never a database, and the
                // bare SQLite message ("file is not a database") sends people
                // looking in the wrong place.
                match super::dbkey::key_path(env).exists() {
                    true => err.context(
                        "there is a key file beside this database, so it was probably \
                         encrypted and its header has been damaged",
                    ),
                    false => err,
                }
            });
        }
        let key = super::dbkey::load(env)?.context(
            "the settings database is encrypted but its key file is missing. \
             The key is stored beside the database and, on Windows, is \
             readable only by the account that created it - so a profile \
             copied from another machine or another user cannot be opened. \
             Without the key file the database cannot be recovered.",
        )?;
        Self::open_sealed(&path, key)
    }

    /// Open a plain database at an explicit path. Used by the tests and by the
    /// one-shot PicoTorrent import, which reads someone else's file.
    pub fn open_path(path: &Path) -> Result<Database> {
        Self::open_plain(path)
    }

    fn open_plain(path: &Path) -> Result<Database> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        tracing::info!("Loading settings database from {} (plain)", path.display());

        let conn = Connection::open(path)?;
        Ok(Database {
            inner: Mutex::new(Inner {
                conn: Self::prepare(conn, None)?,
                sealed: None,
            }),
        })
    }

    /// Unseal the file at `path` and hold it in memory.
    fn open_sealed(path: &Path, key: [u8; 32]) -> Result<Database> {
        tracing::info!(
            "Loading settings database from {} (encrypted)",
            path.display()
        );

        let file = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let plain = super::dbkey::unseal(&key, &file)?;

        let dirty = std::sync::Arc::new(AtomicBool::new(false));
        let conn = Self::deserialize(&plain)?;
        Ok(Database {
            inner: Mutex::new(Inner {
                conn: Self::prepare(conn, Some(dirty.clone()))?,
                sealed: Some(Sealed {
                    path: path.to_path_buf(),
                    key,
                    dirty,
                }),
            }),
        })
    }

    /// A fresh in-memory connection holding `plain`, which must be a complete
    /// SQLite image.
    fn deserialize(plain: &[u8]) -> Result<Connection> {
        let mut conn = Connection::open_in_memory()?;
        // `false` is not read-only: it asks for a resizable image, without
        // which the first write that needs another page fails with SQLITE_FULL.
        conn.deserialize_read_exact(
            rusqlite::MAIN_DB,
            &mut std::io::Cursor::new(plain),
            plain.len(),
            false,
        )
        .context("the database could not be loaded")?;
        Ok(conn)
    }

    /// The settings every connection here needs, whichever shape it is.
    fn prepare(conn: Connection, dirty: Option<std::sync::Arc<AtomicBool>>) -> Result<Connection> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;

        // SQLite opens lazily: `Connection::open` on a file that is not a
        // database at all still succeeds, and the failure only surfaces at
        // whichever query happens to run first. One forced read makes that an
        // error from `open`, where the caller can still say something useful
        // about it.
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        })
        .context("the database could not be read")?;

        Self::register_functions(&conn)?;
        if let Some(dirty) = dirty {
            // Every statement outside an explicit transaction still commits one,
            // so this fires for ordinary writes and for DDL alike - which
            // counting row changes would not catch.
            conn.commit_hook(Some(move || {
                dirty.store(true, Ordering::Relaxed);
                false // false lets the commit proceed.
            }))?;
        }
        Ok(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Database> {
        let conn = Connection::open_in_memory()?;
        Ok(Database {
            inner: Mutex::new(Inner {
                conn: Self::prepare(conn, None)?,
                sealed: None,
            }),
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

    /// Is this database encrypted?
    ///
    /// Asked of the open database rather than of the key file: those two can
    /// disagree for as long as it takes a conversion to finish, and the
    /// database is the half that is true.
    pub fn is_encrypted(&self) -> bool {
        self.inner.lock().unwrap().sealed.is_some()
    }

    /// Turn encryption on or off. `false` means it was already in that state.
    ///
    /// Only the key file is decided here; [`Self::convert`] does the work. The
    /// order is the part that matters. Encrypting writes the *key* first,
    /// because nothing can be sealed without it, and a crash between the two
    /// leaves a key file beside a database that is still plain - which
    /// [`Self::open`] reads as plain, ignores the stray key, and heals the next
    /// time round. The other order would leave a sealed database and no key,
    /// which nobody could recover.
    pub fn set_encryption(&self, env: &Environment, on: bool) -> Result<bool> {
        if self.is_encrypted() == on {
            return Ok(false);
        }
        let path = env.get_database_file_path();
        if on {
            let key = super::dbkey::create(env).context("could not store the database key")?;
            self.convert(&path, Some(key)).inspect_err(|_| {
                let _ = super::dbkey::forget(env);
            })?;
        } else {
            self.convert(&path, None)?;
            super::dbkey::forget(env)
                .context("the database is decrypted but its key file remains")?;
        }
        Ok(true)
    }

    /// Rewrite the database in the other form and re-open this handle onto it.
    ///
    /// Callers keep working across the switch - nothing is restarted. The mutex
    /// is held throughout, so another thread mid-query waits rather than finding
    /// the database gone.
    ///
    /// The image taken at the top is the safety net: whatever fails below, the
    /// database can be written back in either form from those bytes. That is
    /// why there is no `.bak` file here and nothing to pick up afterwards.
    fn convert(&self, path: &Path, to: Option<[u8; 32]>) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        // Anything committed but not yet written back belongs in the image
        // below, rather than stranded in the file being replaced.
        inner.flush()?;
        let plain: Vec<u8> = inner
            .conn
            .serialize(rusqlite::MAIN_DB)
            .context("could not read the database out")?
            .to_vec();

        // The handle has to let go of the file before anything is renamed over
        // it, which Windows refuses while it is open.
        let previous = inner.sealed.take().map(|s| s.key);
        inner.conn = Connection::open_in_memory()?;

        match Self::install(&mut inner, path, &plain, to) {
            Ok(()) => Ok(()),
            Err(err) => {
                if let Err(e) = Self::install(&mut inner, path, &plain, previous) {
                    // Both directions failed, so the handle is now parked on an
                    // empty in-memory database. Saying so is all that is left.
                    tracing::error!("the database could not be put back: {e:#}");
                }
                Err(err)
            }
        }
    }

    /// Write `plain` out in the form `key` asks for, and point `inner` at it.
    fn install(
        inner: &mut Inner,
        path: &Path,
        plain: &[u8],
        key: Option<[u8; 32]>,
    ) -> Result<()> {
        match key {
            Some(key) => {
                write_atomic(path, &super::dbkey::seal(&key, plain)?)?;
                let dirty = std::sync::Arc::new(AtomicBool::new(false));
                inner.conn = Self::prepare(Self::deserialize(plain)?, Some(dirty.clone()))?;
                inner.sealed = Some(Sealed {
                    path: path.to_path_buf(),
                    key,
                    dirty,
                });
            }
            None => {
                write_atomic(path, plain)?;
                inner.conn = Self::prepare(Connection::open(path)?, None)?;
                inner.sealed = None;
            }
        }
        Ok(())
    }

    /// Port of Database::Migrate.
    pub fn migrate(&self) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        let conn = &inner.conn;

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

        inner.flush()
    }

    /// Run a closure with the underlying connection.
    ///
    /// An encrypted database is written back here, after `f` returns. That is
    /// what makes the in-memory copy durable, so the flush error wins over the
    /// closure's own: a query that failed is recoverable, a change that was
    /// never persisted is not.
    pub fn with<R>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> rusqlite::Result<R> {
        let inner = self.inner.lock().unwrap();
        let out = f(&inner.conn);
        // Not `out?` first: `f` may have committed something and then failed.
        inner.flush().map_err(persist_failed)?;
        out
    }
}

/// Does the file at `path` start with the sealed-database magic?
///
/// Only the header is read: the answer decides how to open a file that may be
/// megabytes, and a missing file is simply "not sealed" - [`Database::open`]
/// goes on to create a plain one.
fn file_is_sealed(path: &Path) -> bool {
    use std::io::Read;

    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 8];
    match f.read_exact(&mut head) {
        Ok(()) => super::dbkey::is_sealed(&head),
        Err(_) => false,
    }
}

/// A failure to write the database back, in the shape `with` has to return.
///
/// `SQLITE_IOERR` because that is what it is - the database could not be put on
/// disk - and because the alternative is discarding the reason.
fn persist_failed(err: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
        Some(format!("{err:#}")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::configuration::Configuration;
    use std::sync::Arc;

    /// A scratch path that is not shared between tests in the same run.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nt-db-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("NanoTorrent.sqlite")
    }

    const KEY: [u8; 32] = [0x11; 32];
    const OTHER_KEY: [u8; 32] = [0x22; 32];

    /// Deleting a torrent must take its trackers, tags and file priorities
    /// with it.
    ///
    /// All three lean on ON DELETE CASCADE, which SQLite ignores unless
    /// `PRAGMA foreign_keys` is on - so this is really a test that `prepare`
    /// still turns it on. Without it the startup cleanup would leave a row per
    /// tracker of every torrent ever removed.
    #[test]
    fn removing_a_torrent_takes_its_rows_with_it() {
        let path = scratch("cascade");
        let db = Arc::new(Database::open_path(&path).unwrap());
        db.migrate().unwrap();

        db.with(|conn| {
            conn.execute_batch(
                "insert into torrent (info_hash, queue_position) values ('abc', 0);
                 insert into tag (id, name) values (1, 'linux');
                 insert into torrent_tracker (info_hash, url, tier)
                     values ('abc', 'udp://t.example:1337/announce', 0);
                 insert into torrent_tag (info_hash, tag_id) values ('abc', 1);
                 insert into torrent_file_priority (info_hash, file_index, priority)
                     values ('abc', 0, 3);",
            )
        })
        .unwrap();

        db.with(|conn| conn.execute("delete from torrent where info_hash = 'abc'", []))
            .unwrap();

        for table in ["torrent_tracker", "torrent_tag", "torrent_file_priority"] {
            let left: i64 = db
                .with(|conn| {
                    conn.query_row(&format!("select count(*) from {table}"), [], |r| r.get(0))
                })
                .unwrap();
            assert_eq!(left, 0, "{table} kept a row for a torrent that is gone");
        }
        // The tag itself is not a child of the torrent and must survive.
        let tags: i64 = db
            .with(|conn| conn.query_row("select count(*) from tag", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(tags, 1, "deleting a torrent deleted the tag");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The whole point: once converted, the file does not open without the key,
    /// and the settings are still there when it does.
    #[test]
    fn an_encrypted_database_needs_its_key_and_keeps_its_contents() {
        let path = scratch("encrypt");
        {
            let db = Arc::new(Database::open_path(&path).unwrap());
            db.migrate().unwrap();
            Configuration::new(db.clone()).set("locale_name", &"nl-NL");
            db.convert(&path, Some(KEY)).expect("encrypting");
        }

        assert!(
            Database::open_path(&path).is_err(),
            "an encrypted database opened as a plain one"
        );
        assert!(
            Database::open_sealed(&path, OTHER_KEY).is_err(),
            "a different key opened the database"
        );

        let db = Database::open_sealed(&path, KEY).expect("opening with the key");
        let cfg = Configuration::new(Arc::new(db));
        assert_eq!(cfg.get_string("locale_name").as_deref(), Some("nl-NL"));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Writes to an encrypted database reach the disk.
    ///
    /// The one piece of machinery with nothing else to catch it: an encrypted
    /// database lives in memory, and only the commit hook and the flush behind
    /// `with` put it back on disk. If either stops working, every setting is
    /// still readable for as long as the process lives and silently gone
    /// afterwards - so this reopens the file rather than re-reading the handle.
    #[test]
    fn a_write_to_an_encrypted_database_survives_reopening_it() {
        let path = scratch("persist");
        {
            let db = Arc::new(Database::open_path(&path).unwrap());
            db.migrate().unwrap();
            db.convert(&path, Some(KEY)).expect("encrypting");
            Configuration::new(db).set("locale_name", &"es-ES");
        }

        let db = Database::open_sealed(&path, KEY).expect("reopening");
        assert_eq!(
            Configuration::new(Arc::new(db))
                .get_string("locale_name")
                .as_deref(),
            Some("es-ES")
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The handle the running program is holding survives the conversion.
    ///
    /// This is what lets Preferences encrypt without restarting: the connection
    /// is replaced underneath, so a `Configuration` built before the switch has
    /// to keep reading and writing afterwards - in both directions.
    #[test]
    fn converting_leaves_the_open_handle_usable() {
        let path = scratch("convert");
        let db = Arc::new(Database::open_path(&path).unwrap());
        db.migrate().unwrap();
        let cfg = Configuration::new(db.clone());
        cfg.set("locale_name", &"nl-NL");

        db.convert(&path, Some(KEY)).expect("encrypting");
        assert!(db.is_encrypted());
        assert_eq!(cfg.get_string("locale_name").as_deref(), Some("nl-NL"));
        cfg.set("locale_name", &"es-ES");

        db.convert(&path, None).expect("decrypting");
        assert!(!db.is_encrypted());
        assert_eq!(cfg.get_string("locale_name").as_deref(), Some("es-ES"));

        drop(cfg);
        drop(db);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Settings export and import work on an encrypted database too.
    ///
    /// Lives here rather than beside the rest of `dbexport` because it needs
    /// `convert`. The property is that encryption is invisible to that module:
    /// what comes out is readable JSON, and what goes in is sealed back to disk
    /// by the ordinary write path.
    #[test]
    fn settings_export_and_import_across_encryption() {
        let path = scratch("exportenc");
        let db = Arc::new(Database::open_path(&path).unwrap());
        db.migrate().unwrap();
        Configuration::new(db.clone()).set("locale_name", &"nl-NL");
        db.convert(&path, Some(KEY)).expect("encrypting");

        // Out of an encrypted database, in the clear.
        let json = crate::core::dbexport::export(&db).expect("export");
        assert!(
            json.contains("nl-NL"),
            "the export did not come out readable: {json}"
        );

        // And back into one, reaching the disk rather than only memory.
        let edited = json.replace("nl-NL", "es-ES");
        crate::core::dbexport::import(&db, &edited).expect("import");
        drop(db);

        let db = Database::open_sealed(&path, KEY).expect("reopening");
        assert_eq!(
            Configuration::new(Arc::new(db))
                .get_string("locale_name")
                .as_deref(),
            Some("es-ES")
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Not a one-way door: switching the setting back off has to give a plain
    /// SQLite file again, not merely one this build happens to be able to read.
    #[test]
    fn decrypting_gives_back_a_plain_database() {
        let path = scratch("decrypt");
        {
            let db = Arc::new(Database::open_path(&path).unwrap());
            db.migrate().unwrap();
            db.convert(&path, Some(KEY)).expect("encrypting");
            db.convert(&path, None).expect("decrypting");
        }

        let head = std::fs::read(&path).unwrap();
        assert!(
            head.starts_with(b"SQLite format 3\0"),
            "the file is not a plain SQLite database"
        );
        Database::open_path(&path).expect("opening the decrypted database");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// `user_version` is schema bookkeeping, and a conversion that dropped it
    /// would make every migration run again on the next start. Serializing the
    /// whole image carries it across for free, which is exactly the kind of
    /// thing that stays true until someone changes how the copy is made.
    #[test]
    fn the_schema_version_survives_the_conversion() {
        let path = scratch("version");
        let before: i64 = {
            let db = Database::open_path(&path).unwrap();
            db.with(|c| c.execute_batch("PRAGMA user_version = 42;"))
                .unwrap();
            db.convert(&path, Some(KEY)).expect("encrypting");
            db.with(|c| c.query_row("PRAGMA user_version", [], |r| r.get(0)))
                .unwrap()
        };
        assert_eq!(before, 42);

        let db = Database::open_sealed(&path, KEY).unwrap();
        let after: i64 = db
            .with(|c| c.query_row("PRAGMA user_version", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(after, 42);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

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
