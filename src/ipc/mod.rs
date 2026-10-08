// Port of src/picotorrent/ipc/{server,applicationoptionsconnection}.{hpp,cpp}
//
// The original used a hidden window + WM_COPYDATA to pass command line
// options (torrent files / magnet links) from a second instance to the
// running one. This port uses a loopback TCP socket which achieves the
// same single-instance behaviour in a portable way.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::core::environment::Environment;

/// The single-instance port of the ordinary, per-user profile. Unchanged since
/// the first release, so an upgrade still finds the copy that is already open.
const DEFAULT_PORT: u16 = 37549;

/// Where a portable profile's port is picked from - just above the default.
const PORTABLE_PORTS: std::ops::Range<u16> = 37550..38550;

/// The single-instance address for this profile.
///
/// One per PROFILE, not one per machine. With a single fixed port, starting a
/// portable copy while the installed one was open handed its arguments to the
/// installed one and quit, so the portable window never appeared - and a
/// magnet clicked in the portable copy's name landed in the other profile.
/// A portable profile therefore gets a port of its own, chosen from where it
/// lives: the same folder always gets the same port, so a second launch of
/// that copy still finds the first. The ordinary profile keeps the old one.
pub fn address(env: &Environment) -> SocketAddr {
    let port = if env.is_portable() {
        portable_port(&env.get_application_data_path())
    } else {
        DEFAULT_PORT
    };
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// FNV-1a of the folder, folded into [`PORTABLE_PORTS`]. Case-folded on
/// Windows, where `D:\NanoTorrent` and `d:\nanotorrent` are the same folder
/// and must not come out as two instances.
fn portable_port(dir: &std::path::Path) -> u16 {
    let text = dir.to_string_lossy();
    let text = if cfg!(windows) { text.to_lowercase() } else { text.into_owned() };
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let span = u32::from(PORTABLE_PORTS.end - PORTABLE_PORTS.start);
    PORTABLE_PORTS.start + (hash % span) as u16
}

pub enum Instance {
    /// This is the first (main) instance. The receiver yields the argument
    /// lists sent by subsequently launched instances.
    Primary(Server),
    /// Another instance is already running and the arguments were forwarded
    /// to it - the caller should exit.
    Secondary,
}

/// The argument lists sent by later instances. Polled with `try_recv`.
pub type Server = Receiver<Vec<String>>;

/// Is a NanoTorrent already running?
///
/// Tested by taking the single-instance port for an instant and giving it
/// straight back, so unlike [`init`] it forwards nothing and claims nothing.
///
/// A hint rather than a lock: the answer can go stale the moment it is
/// returned. That is enough to refuse "do not do this while the application is
/// open" and not enough to build anything else on.
pub fn another_instance_running(env: &Environment) -> bool {
    TcpListener::bind(address(env)).is_err()
}

/// Claim the single-instance role, or hand `args` to whoever already has it.
///
/// Binding the port IS the lock: whichever process gets it is primary, and the
/// rest connect, write their argv as JSON and exit. That avoids a named mutex
/// (Windows-only) and a pid file (which outlives a crash and has to be
/// validated); a port cannot be left stale, because it is released when the
/// process dies.
///
/// Loopback only, so nothing off this machine can reach it.
///
/// Returns `Err` when the port is taken by something that will not accept the
/// hand-off. That case used to exit silently with a success code - no window,
/// no message, nothing in a log, because this runs before logging exists. The
/// caller turns an `Err` into a console line and a message box.
pub fn init(args: &[String], env: &Environment) -> anyhow::Result<Instance> {
    let addr = address(env);
    match TcpListener::bind(addr) {
        Ok(listener) => {
            let (tx, rx) = channel();
            std::thread::Builder::new()
                .name(String::from("pt-ipc"))
                .spawn(move || accept_loop(listener, tx))
                .expect("failed to spawn IPC thread");

            Ok(Instance::Primary(rx))
        }
        // Nothing holds the port - the port itself is unusable. A sandbox
        // with no usable loopback looks exactly like this, and so does a
        // machine where a policy blocks the bind.
        //
        // Carrying on is the only sensible answer. Single-instance detection
        // is a convenience; refusing to start over it means the application
        // does not run at all, which is the very thing the error below was
        // written to avoid. The hand-off is lost, so a second copy opens its
        // own window instead of passing its arguments over.
        Err(bind_err) if bind_err.kind() != std::io::ErrorKind::AddrInUse => {
            tracing::warn!(
                "cannot use {addr} for single-instance detection ({bind_err});                  carrying on as the only instance"
            );
            // A receiver whose sender is already gone: it never yields, which
            // is exactly right when nothing can send to it.
            let (_tx, rx) = channel();
            Ok(Instance::Primary(rx))
        }
        Err(bind_err) => {
            // The port IS held. Normally that is another NanoTorrent and the
            // hand-off is the whole point - but if it will not take our
            // arguments it is not one, and exiting quietly would leave the
            // user with an application that simply does not start.
            let mut stream = TcpStream::connect(addr).map_err(|connect_err| {
                anyhow::anyhow!(
                    "Another program is using {addr}, which NanoTorrent uses to spot a second copy of itself.

could not listen: {bind_err}
could not connect: {connect_err}"
                )
            })?;

            let payload = serde_json::to_vec(args).unwrap_or_default();
            stream.write_all(&payload).map_err(|err| {
                anyhow::anyhow!(
                    "Another program is using {addr} and refused NanoTorrent's hand-off.

NanoTorrent uses that port to spot a second copy of itself.

{err}"
                )
            })?;
            let _ = stream.shutdown(std::net::Shutdown::Write);

            Ok(Instance::Secondary)
        }
    }
}

/// Read forwarded argv from secondary instances until the process ends.
///
/// Runs on its own thread and never fails a connection loudly: a malformed
/// payload is dropped rather than being allowed to take the listener down and
/// silently end single-instance handling for the rest of the session.
fn accept_loop(listener: TcpListener, tx: Sender<Vec<String>>) {
    for stream in listener.incoming().flatten() {
        let mut buf = Vec::new();
        let mut stream = stream;
        if stream.read_to_end(&mut buf).is_ok()
            && let Ok(args) = serde_json::from_slice::<Vec<String>>(&buf)
        {
            let _ = tx.send(args);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The distinction the AppImage catalog's test harness turned up: it runs
    /// each AppImage under `firejail --net=none`, and a startup that refuses
    /// to continue when it cannot take a loopback port never reaches a window.
    ///
    /// A port that is merely BUSY still means "another copy is running", and
    /// must keep the existing hand-off. A port that cannot be taken at all
    /// means nothing of the sort.
    #[test]
    fn only_a_busy_port_means_another_instance() {
        use std::io::ErrorKind;
        // Busy: hand off (or report a program that will not take it).
        assert!(matches!(ErrorKind::AddrInUse, ErrorKind::AddrInUse));
        // Unusable: carry on alone. These are what a sandbox or a policy
        // gives back, and none of them is AddrInUse.
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::AddrNotAvailable,
            ErrorKind::Unsupported,
        ] {
            assert_ne!(kind, ErrorKind::AddrInUse, "{kind:?} must not be read as a busy port");
        }
    }

    /// A portable profile's port has to be the SAME every time for the same
    /// folder - a second launch of that copy must find the first - different
    /// for different folders, never the ordinary profile's, and inside the
    /// range it is documented to come from.
    #[test]
    fn a_portable_port_is_stable_distinct_and_in_range() {
        let a = portable_port(std::path::Path::new("/media/usb/NanoTorrent"));
        assert_eq!(a, portable_port(std::path::Path::new("/media/usb/NanoTorrent")));
        assert!(PORTABLE_PORTS.contains(&a), "{a} is outside {PORTABLE_PORTS:?}");
        assert_ne!(a, DEFAULT_PORT);

        let others: std::collections::HashSet<u16> = ["/a", "/b", "/media/other", "/opt/nt"]
            .iter()
            .map(|p| portable_port(std::path::Path::new(p)))
            .collect();
        assert!(others.len() > 1, "every folder hashed to one port");
    }

    /// Windows paths are case-insensitive; one folder spelled two ways is
    /// still one instance.
    #[test]
    #[cfg(windows)]
    fn a_portable_port_ignores_case_on_windows() {
        assert_eq!(
            portable_port(std::path::Path::new(r"D:\NanoTorrent")),
            portable_port(std::path::Path::new(r"d:\nanotorrent"))
        );
    }

    /// The primary that could not listen still answers polls - it simply never
    /// has anything. A receiver whose sender is gone must not panic or block.
    #[test]
    fn a_primary_with_no_listener_polls_empty_forever() {
        let (_tx, rx) = channel();
        let server: Server = rx;
        for _ in 0..3 {
            assert!(server.try_recv().is_err());
        }
    }
}
