//! `--headless`, driven as a process: started, handed a torrent by a second
//! launch, and stopped with a signal.
//!
//! A process test because what is being tested is the process - which
//! arguments it takes, what it prints, what it does on SIGTERM - and none of
//! that is reachable from a unit test inside the binary.
//!
//! Unix only: the stop is a signal. The Windows half (Ctrl-Break, the console
//! closing, the launcher's handler) needs a console to send them on.
#![cfg(unix)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(60);

/// A portable copy of the binary in a folder of its own, so the test has a
/// profile and a single-instance port nobody else is using - including a
/// NanoTorrent the developer has open.
struct Copy {
    dir: PathBuf,
    exe: PathBuf,
    /// One copy at a time. A portable profile's single-instance port is a
    /// hash of its folder into a range of a thousand, so two copies running
    /// side by side can land on the same one now and then - and the second
    /// then hands itself to the first, which is the behaviour under test
    /// going off for the wrong reason.
    _turn: std::sync::MutexGuard<'static, ()>,
}

static TURN: Mutex<()> = Mutex::new(());

impl Copy {
    fn new(tag: &str) -> Copy {
        // A test that failed while holding it poisons it; the next one can
        // still run.
        let turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = std::env::temp_dir().join(format!("nt-headless-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let exe = dir.join("nanotorrent");
        std::fs::copy(env!("CARGO_BIN_EXE_nanotorrent-gui"), &exe).unwrap();
        std::fs::write(dir.join("portable.txt"), "").unwrap();
        let copy = Copy { dir, exe, _turn: turn };

        // Nothing leaves the machine: no DHT, LSD or PeX, and a listening port
        // of its own rather than the 6881 a running client already holds.
        let port = std::net::TcpListener::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port();
        let data = copy.dir.join("data");
        let settings: [(&str, &str); 5] = [
            ("dht", "off"),
            ("lsd", "off"),
            ("pex", "off"),
            ("listen-port", &port.to_string()),
            ("save-path", data.to_str().unwrap()),
        ];
        for (name, value) in settings {
            let out = copy.run(&["--set", name, value]);
            assert!(out.status.success(), "--set {name} {value}: {out:?}");
        }
        copy
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.args(args)
            // And no display, so a window could not open if anything tried.
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn spawn(&self, args: &[&str]) -> Run {
        let mut child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let collect = |mut pipe: Box<dyn Read + Send>| {
            let text = Arc::new(Mutex::new(String::new()));
            let sink = text.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n @ 1..) = pipe.read(&mut buf) {
                    sink.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            });
            text
        };
        let out = collect(Box::new(child.stdout.take().unwrap()));
        let err = collect(Box::new(child.stderr.take().unwrap()));
        Run { child, out, err }
    }
}

impl Drop for Copy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A single-file v1 torrent of `data`, bencoded by hand.
fn torrent_of(data: &Path) -> Vec<u8> {
    use sha1::{Digest, Sha1};
    let bytes = std::fs::read(data).unwrap();
    let piece = 16384;
    let pieces: Vec<u8> = bytes.chunks(piece).flat_map(|c| Sha1::digest(c).to_vec()).collect();
    let name = data.file_name().unwrap().to_str().unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(format!("d4:infod6:lengthi{}e4:name{}:{name}", bytes.len(), name.len()).as_bytes());
    out.extend_from_slice(format!("12:piece lengthi{piece}e6:pieces{}:", pieces.len()).as_bytes());
    out.extend_from_slice(&pieces);
    out.extend_from_slice(b"ee");
    out
}

/// A headless run, with what it prints collected as it prints it.
struct Run {
    child: Child,
    out: Arc<Mutex<String>>,
    err: Arc<Mutex<String>>,
}

impl Run {
    /// Wait until stdout has said `needle`. Watched as it arrives, because
    /// the console is where a headless run reports, a tick after the event.
    fn wait_for(&self, needle: &str) {
        let start = Instant::now();
        while !self.out.lock().unwrap().contains(needle) {
            assert!(
                start.elapsed() < PATIENCE,
                "never printed {needle:?}; printed:\n{}\n{}",
                self.out.lock().unwrap(),
                self.err.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Send a signal, wait for the exit - killing it after [`PATIENCE`] - and
    /// return the status with everything printed.
    fn stop(mut self, signal: &str) -> (std::process::ExitStatus, String, String) {
        let pid = self.child.id().to_string();
        assert!(Command::new("kill").args([signal, &pid]).status().unwrap().success());
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if start.elapsed() > PATIENCE {
                let _ = self.child.kill();
                panic!("the headless run did not stop within {PATIENCE:?}");
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        // The readers end at EOF, which the exit has just caused.
        std::thread::sleep(Duration::from_millis(200));
        let out = self.out.lock().unwrap().clone();
        let err = self.err.lock().unwrap().clone();
        (status, out, err)
    }
}

/// Start, take a torrent from a second launch, refuse a second headless run,
/// and save its state on SIGTERM - then come back with the torrent still there.
#[test]
fn a_headless_run_takes_hand_offs_and_stops_cleanly() {
    let copy = Copy::new("run");
    let data = copy.dir.join("data").join("payload.bin");
    std::fs::write(&data, (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect::<Vec<_>>()).unwrap();
    let torrent = copy.dir.join("payload.torrent");
    std::fs::write(&torrent, torrent_of(&data)).unwrap();

    let run = copy.spawn(&["--headless"]);
    run.wait_for("without a window");

    // One profile, one session: a second headless run says so and fails.
    let second = copy.run(&["--headless"]);
    assert_eq!(second.status.code(), Some(1), "{second:?}");
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));

    // A plain launch hands its torrent to the running one and exits.
    let handoff = copy.run(&[torrent.to_str().unwrap()]);
    assert!(handoff.status.success(), "{handoff:?}");
    run.wait_for("Added: payload.bin");

    let (status, out, err) = run.stop("-TERM");
    assert!(status.success(), "{status:?}\n{out}\n{err}");
    assert!(out.contains("Stopping"), "{out}");

    // The stop saved it: the next run comes back with the torrent - restored,
    // so not announced as added again - and SIGINT stops it the same way.
    let run = copy.spawn(&["--headless"]);
    run.wait_for("without a window");
    let again = copy.run(&[torrent.to_str().unwrap()]);
    assert!(again.status.success(), "{again:?}");
    run.wait_for("payload.bin already in the list");
    let (status, out, err) = run.stop("-INT");
    assert!(status.success(), "{status:?}\n{out}\n{err}");
    assert!(!out.contains("Added:"), "{out}");
}

/// A torrent named on the command line is added at startup, and one that
/// cannot be read is reported on stderr rather than only in the log.
#[test]
fn a_headless_run_adds_what_it_is_started_with() {
    let copy = Copy::new("args");
    let data = copy.dir.join("data").join("named.bin");
    std::fs::write(&data, vec![42u8; 50_000]).unwrap();
    let torrent = copy.dir.join("named.torrent");
    std::fs::write(&torrent, torrent_of(&data)).unwrap();
    let missing = copy.dir.join("missing.torrent");

    let run = copy.spawn(&["--headless", torrent.to_str().unwrap(), missing.to_str().unwrap()]);
    run.wait_for("Added: named.bin");
    let (status, out, err) = run.stop("-TERM");
    assert!(status.success(), "{status:?}\n{out}\n{err}");
    assert!(err.contains("missing.torrent"), "{err}");
    // The log is on the console too, not only in the file.
    assert!(err.contains("INFO") && err.contains("running headless"), "{err}");
}

/// `--log-level` sets what reaches the console and the file alike, and a
/// level that is not one is refused before anything starts.
#[test]
fn log_level_quietens_a_headless_run_and_refuses_nonsense() {
    let copy = Copy::new("level");

    let bad = copy.run(&["--headless", "--log-level", "loud"]);
    assert_eq!(bad.status.code(), Some(2), "{bad:?}");
    assert!(String::from_utf8_lossy(&bad.stderr).contains("--log-level"));

    let run = copy.spawn(&["--headless", "--log-level=warn"]);
    run.wait_for("without a window");
    let (status, out, err) = run.stop("-TERM");
    assert!(status.success(), "{status:?}\n{out}\n{err}");
    assert!(!err.contains("INFO"), "{err}");
    let logged: String = std::fs::read_dir(copy.dir.join("logs"))
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect();
    assert!(!logged.contains("INFO"), "{logged}");
}
