//! Tests against a real, running [`Session`].
//!
//! Everything else in `session.rs` is tested a function at a time, which
//! cannot catch the failures that matter most to someone using the program: a
//! torrent made here that will not seed, a recheck that does not look at the
//! disk, a move that loses the torrent, a restart that forgets what was
//! running. These drive the same public methods the window and the web API
//! call, against an engine with real files under it.
//!
//! Each test gets its own profile and data folder under the temp directory and
//! never touches the network: DHT and local discovery are off, and strict mode
//! with a proxy on the discard port turns off UPnP and uTP as well. No torrent
//! here has a tracker, so nothing would be announced anyway. The listen port
//! is 0, so tests running side by side never fight over one.
//!
//! The torrents are made with `torrent_create::build`, so every one of these is
//! also a check that what the create dialog produces is accepted and verified
//! by the engine - the end-to-end half of the tests in `torrent_create`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{AddParams, AddTorrentSource, Session, SessionEvent};
use crate::bittorrent::torrent_create::{CreateInput, TorrentVersion, build};
use crate::bittorrent::torrentstatus::{State, TorrentStatus};
use crate::core::configuration::{Configuration, Label};
use crate::core::database::Database;
use crate::core::environment::Environment;

/// How long anything here may take. Generous, because a loaded CI machine
/// hashing in a debug build is slow; a pass takes well under a second.
const PATIENCE: Duration = Duration::from_secs(30);

/// One isolated session and the folders it owns.
struct Live {
    /// `None` only between the two halves of [`Live::restart`].
    session: Option<Session>,
    db: Arc<Database>,
    cfg: Configuration,
    env: Environment,
    root: PathBuf,
}

impl Live {
    fn start(tag: &str) -> Live {
        let root = std::env::temp_dir().join(format!("nt-live-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        // macOS's temp folder is under /var, a symlink to /private/var, and a
        // path compared after the engine has resolved it would not match.
        // Not on Windows, where canonical means a \\?\ path nothing else uses.
        #[cfg(not(windows))]
        let root = root.canonicalize().unwrap();

        let db = Arc::new(Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        let cfg = Configuration::new(db.clone());
        isolate(&cfg, &root.join("data"));

        let env = Environment::at(root.join("profile"));
        let session = Session::new(&env, db.clone(), &cfg).expect("the session did not start");
        Live {
            session: Some(session),
            db,
            cfg,
            env,
            root,
        }
    }

    fn s(&self) -> &Session {
        self.session.as_ref().unwrap()
    }

    /// Where test data lives - also the default save path.
    fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    /// Shut down the way the program does on exit, and start again on the
    /// same profile and database.
    fn restart(&mut self) {
        let old = self.session.take().unwrap();
        old.stop();
        drop(old);
        self.session =
            Some(Session::new(&self.env, self.db.clone(), &self.cfg).expect("no restart"));
    }

    /// The torrent's status, or a panic naming what is in the session instead.
    fn status(&self, hash: &str) -> TorrentStatus {
        self.s().torrent(hash).unwrap_or_else(|| {
            let have: Vec<_> = self.s().torrents(&Default::default()).into_iter().map(|t| t.name).collect();
            panic!("{hash} is not in the session; it holds {have:?}")
        })
    }

    /// Poll until `done` holds, or fail saying what was being waited for and
    /// what the torrent looked like when time ran out.
    fn wait(&self, hash: &str, what: &str, done: impl Fn(&TorrentStatus) -> bool) -> TorrentStatus {
        let start = Instant::now();
        loop {
            if let Some(t) = self.s().torrent(hash)
                && done(&t)
            {
                return t;
            }
            if start.elapsed() > PATIENCE {
                let now = self.s().torrent(hash);
                panic!(
                    "timed out waiting for {what}; the torrent is {:?}",
                    now.map(|t| (t.state, t.progress, t.paused, t.error, t.save_path))
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Remove a torrent and wait for the session to announce it gone.
    ///
    /// The event, not just `exists`: the lifecycle scan diffs once a second,
    /// and the same torrent added back before it has noticed the removal
    /// would look to it as if nothing had happened at all.
    fn remove(&self, hash: &str, delete_files: bool) {
        let events = self.s().subscribe();
        self.s().remove(hash, delete_files);
        loop {
            match events.recv_timeout(PATIENCE).expect("no remove event arrived") {
                SessionEvent::TorrentRemoved { hash: gone, .. } if gone == hash => break,
                _ => {}
            }
        }
        assert!(!self.s().exists(hash), "announced removed, still listed");
    }

    /// Add `bytes` the way the window's Add dialog does, and return the hash
    /// the session announced it under. For a v2-only torrent that is the hash
    /// the engine drives it by, which is what every other call takes.
    fn add(&self, bytes: Vec<u8>, save_path: &Path, start: bool) -> String {
        let events = self.s().subscribe();
        assert!(
            self.s().add_torrent(
                AddTorrentSource::TorrentFileBytes(bytes),
                AddParams {
                    save_path: Some(save_path.to_string_lossy().into_owned()),
                    start_torrent: start,
                    only_files: None,
                    label_id: None,
                },
            ),
            "the add was refused before the engine saw it"
        );
        loop {
            match events.recv_timeout(PATIENCE).expect("no add event arrived") {
                SessionEvent::TorrentAdded { hash, .. } => return hash,
                SessionEvent::Error(err) => panic!("the add failed: {err:?}"),
                _ => {}
            }
        }
    }

    /// Make a torrent of `source` and add it, saved where the data already
    /// is - the create dialog's "add to session" - then wait for the engine
    /// to verify it and start seeding.
    fn seed(&self, source: &Path, version: TorrentVersion) -> String {
        let bytes = make(source, version);
        let hash = self.add(bytes, source.parent().unwrap(), true);
        self.wait(&hash, "the data to verify and seed", |t| {
            t.progress >= 1.0 && t.state == State::Uploading
        });
        hash
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            session.stop();
            // Before the folder goes: Windows will not delete a file the
            // engine still has open.
            drop(session);
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Settings for a session that talks to nobody.
fn isolate(cfg: &Configuration, data: &Path) {
    let save_path = serde_json::to_string(&data.to_string_lossy()).unwrap();
    for (key, value) in [
        ("libtorrent.enable_dht", "false"),
        ("libtorrent.enable_lsd", "false"),
        ("libtorrent.enable_utp", "false"),
        // Strict mode with a proxy and no interface switches off everything a
        // proxy cannot carry - UPnP and LSD included, which have no setting of
        // their own here. Port 9 is discard: nothing will ever answer.
        ("network.strict", "true"),
        ("libtorrent.proxy_type", "2"),
        ("libtorrent.proxy_host", "\"127.0.0.1\""),
        ("libtorrent.proxy_port", "9"),
        ("default_save_path", save_path.as_str()),
    ] {
        assert!(cfg.write_value(key, Some(value)), "no setting named {key}");
    }
    for mut iface in cfg.get_listen_interfaces() {
        iface.port = 0;
        cfg.upsert_listen_interface(&iface);
    }
}

/// Patterned bytes, different per `seed`, so no two files hash alike.
fn put(path: &Path, len: usize, seed: u8) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let data: Vec<u8> = (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect();
    std::fs::write(path, data).unwrap();
}

/// Copy a folder and everything in it.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A three-file folder, nested, with sizes that do not line up with a piece.
fn folder(at: &Path) -> PathBuf {
    put(&at.join("a.bin"), 40_000, 1);
    put(&at.join("sub/b.bin"), 70_001, 2);
    put(&at.join("sub/c.bin"), 5, 3);
    at.to_path_buf()
}

fn make(source: &Path, version: TorrentVersion) -> Vec<u8> {
    build(&CreateInput {
        source,
        version,
        piece_length: Some(16_384),
        trackers: &[],
        comment: "",
        private: false,
        created_by: crate::buildinfo::user_agent(),
    })
    .unwrap()
}

/// What the create dialog makes, the engine accepts and verifies as complete
/// from the folder it was made from - every version, one file and many.
///
/// This is the check that matters for torrent creation: a torrent can parse,
/// carry every right key and still describe data that is not there, and then
/// it simply never seeds.
#[test]
fn a_created_torrent_seeds_from_where_it_was_made() {
    let live = Live::start("seed");
    for (i, version) in [TorrentVersion::V1, TorrentVersion::V2, TorrentVersion::Hybrid]
        .into_iter()
        .enumerate()
    {
        let single = live.data().join(format!("single-{i}.bin"));
        put(&single, 50_000, 10 + i as u8);
        live.seed(&single, version);

        let many = folder(&live.data().join(format!("folder-{i}")));
        let hash = live.seed(&many, version);
        // The folder holding the files directly - the torrent's own, here.
        assert_eq!(
            Path::new(&live.status(&hash).save_path),
            many,
            "{version:?} seeded from somewhere else"
        );
    }
}

/// Pause, resume and remove do what they say, and removing without "delete
/// files" leaves the data alone while removing with it does not.
#[test]
fn pause_resume_and_remove() {
    let live = Live::start("lifecycle");
    let source = folder(&live.data().join("payload"));
    let hash = live.seed(&source, TorrentVersion::V1);

    live.s().pause(&hash);
    live.wait(&hash, "the pause", |t| t.paused && t.state == State::UploadingPaused);
    live.s().resume(&hash);
    live.wait(&hash, "the resume", |t| !t.paused && t.state == State::Uploading);

    live.remove(&hash, false);
    assert!(source.join("a.bin").exists(), "a plain remove deleted the data");

    // Back in, then out again with its files.
    let hash = live.seed(&source, TorrentVersion::V1);
    live.remove(&hash, true);
    let start = Instant::now();
    while source.join("a.bin").exists() {
        assert!(start.elapsed() < PATIENCE, "remove with files left the data behind");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A label set on a torrent comes back by name from every reader - the
/// window's list, the web API, and `torrent()`, which plugins use.
///
/// `torrent()` once looked labels up in an empty table, so a plugin always
/// saw "" whatever the torrent was labelled.
#[test]
fn a_label_reaches_every_reader() {
    let live = Live::start("label");
    let single = live.data().join("labelled.bin");
    put(&single, 1000, 7);
    let hash = live.seed(&single, TorrentVersion::V1);

    live.cfg.upsert_label(&Label {
        id: -1,
        name: "Linux".into(),
        ..Default::default()
    });
    let id = live.s().label_id("linux").expect("label lookup is by name, any case");
    live.s().set_label(&hash, Some(id));

    assert_eq!(live.status(&hash).label_id, Some(id));
    assert_eq!(live.status(&hash).label_name, "Linux");
    let row = live
        .s()
        .torrents(&live.s().label_names())
        .into_iter()
        .find(|t| t.info_hash == hash)
        .unwrap();
    assert_eq!(row.label_name, "Linux");

    live.s().set_label(&hash, None);
    assert_eq!(live.status(&hash).label_name, "");
}

/// What was running comes back running after a restart, what was paused
/// stays paused, and a torrent's label and queue place survive with it.
#[test]
fn a_restart_keeps_torrents_and_their_state() {
    let mut live = Live::start("restart");
    let first = live.data().join("first.bin");
    put(&first, 30_000, 1);
    let running = live.seed(&first, TorrentVersion::V1);
    let paused = live.seed(&folder(&live.data().join("second")), TorrentVersion::Hybrid);

    live.s().pause(&paused);
    live.wait(&paused, "the pause", |t| t.paused);
    live.cfg.upsert_label(&Label {
        id: -1,
        name: "Kept".into(),
        ..Default::default()
    });
    let label = live.s().label_id("Kept").unwrap();
    live.s().set_label(&running, Some(label));
    let positions = (live.status(&running).queue_position, live.status(&paused).queue_position);

    live.restart();

    let r = live.wait(&running, "the running torrent to come back running", |t| {
        !t.paused && t.progress >= 1.0
    });
    assert_eq!(r.label_name, "Kept", "the label was lost");
    let p = live.wait(&paused, "the paused torrent to come back", |t| t.progress >= 1.0);
    assert!(p.paused, "a paused torrent was started by the restart");
    assert_eq!(
        (r.queue_position, p.queue_position),
        positions,
        "queue positions moved"
    );
}

/// A restart announces nothing for what it restored: no "added" for torrents
/// that were already there, and no "completed" for ones that finished long
/// ago - each would be a toast, a plugin hook and possibly an auto-move, on
/// every launch. While something added in the first second after startup IS
/// announced; that half is what [`Live::add`] waits on in every test here.
#[test]
fn a_restart_announces_nothing_for_what_it_restored() {
    let mut live = Live::start("quiet");
    let single = live.data().join("old.bin");
    put(&single, 30_000, 5);
    let hash = live.seed(&single, TorrentVersion::V1);

    live.restart();
    let events = live.s().subscribe();
    live.wait(&hash, "the restored torrent to verify", |t| {
        t.progress >= 1.0 && t.state == State::Uploading
    });
    // Three scan ticks: long enough for a wrong "added" or "completed" to
    // have been raised.
    std::thread::sleep(Duration::from_secs(3));
    let raised: Vec<_> = events.try_iter().collect();
    assert!(
        raised.iter().all(|e| !matches!(
            e,
            SessionEvent::TorrentAdded { .. } | SessionEvent::TorrentCompleted { .. }
        )),
        "the restart announced restored torrents: {raised:?}"
    );
}

/// Force recheck reads the disk rather than trusting what it knew: damage a
/// complete torrent's file and the recheck finds the piece missing; repair it
/// and the next recheck finds it whole again.
#[test]
fn recheck_reads_the_disk() {
    let live = Live::start("recheck");
    let single = live.data().join("checked.bin");
    put(&single, 100_000, 4);
    let original = std::fs::read(&single).unwrap();
    let hash = live.seed(&single, TorrentVersion::V1);

    // Pause first so nothing holds the file open for writing on Windows.
    live.s().pause(&hash);
    live.wait(&hash, "the pause", |t| t.paused);
    let mut damaged = original.clone();
    damaged[20_000] ^= 0xFF;
    std::fs::write(&single, &damaged).unwrap();

    live.s().recheck(&hash);
    // One 16 KiB piece of seven is bad: the rest must still be found. A
    // progress of 0 would mean nothing was read at all - which is what a
    // paused torrent used to show after a recheck, until it was started.
    let t = live.wait(&hash, "the recheck to find the damage", |t| {
        t.progress > 0.5 && t.progress < 1.0 && t.state != State::CheckingFiles
    });
    assert!(t.paused, "a recheck of a paused torrent started it");

    std::fs::write(&single, &original).unwrap();
    live.s().recheck(&hash);
    live.wait(&hash, "the repaired data to verify", |t| {
        t.progress >= 1.0 && t.state != State::CheckingFiles
    });
}

/// Move storage moves the files and the torrent with them, and it keeps
/// seeding from the new place without downloading anything again.
///
/// The folder given is the one that will directly hold the files - the
/// engine's rule for an explicit folder - so a multi-file torrent's own
/// directory is named in it, as `manager::move_finished` does.
#[test]
fn move_storage_moves_files_and_torrent() {
    let live = Live::start("move");
    let source = folder(&live.data().join("moving"));
    let hash = live.seed(&source, TorrentVersion::V1);
    let moved = live.data().join("elsewhere").join("moving");

    live.s().move_storage(&hash, &moved.to_string_lossy());

    live.wait(&hash, "the torrent to seed from its new folder", |t| {
        Path::new(&t.save_path) == moved && t.progress >= 1.0
    });
    assert!(moved.join("sub/b.bin").exists(), "the data did not arrive");
    assert!(!source.join("sub/b.bin").exists(), "the data was copied, not moved");
}

/// Set location finds data that was moved by hand, including when the folder
/// picked is the one ABOVE the torrent's own - the easy mistake, which used
/// to start the whole download again into an empty folder.
///
/// Copied rather than moved: Windows may refuse to rename a folder while the
/// paused torrent still has its files open for reading, and the save path
/// asserted below already proves which copy the torrent is using.
#[test]
fn set_location_finds_data_moved_by_hand() {
    let live = Live::start("locate");
    let source = folder(&live.data().join("wandered"));
    let hash = live.seed(&source, TorrentVersion::V1);
    live.s().pause(&hash);
    live.wait(&hash, "the pause", |t| t.paused);

    let elsewhere = live.data().join("new-home");
    copy_tree(&source, &elsewhere.join("wandered"));

    // The parent, not the torrent's folder. Still paused, and still found:
    // the data is checked where it now is without starting the torrent.
    live.s().set_location(&hash, &elsewhere.to_string_lossy());
    let t = live.wait(&hash, "the moved data to verify", |t| {
        t.progress >= 1.0 && t.state != State::CheckingFiles
    });
    assert!(t.paused, "set location started a paused torrent");
    assert_eq!(Path::new(&t.save_path), elsewhere.join("wandered"));
    assert!(
        !live.data().join("new-home/a.bin").exists(),
        "files were created in the folder above instead of the data being found"
    );
}
