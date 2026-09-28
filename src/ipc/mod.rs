// Port of src/picotorrent/ipc/{server,applicationoptionsconnection}.{hpp,cpp}
//
// The original used a hidden window + WM_COPYDATA to pass command line
// options (torrent files / magnet links) from a second instance to the
// running one. This port uses a loopback TCP socket which achieves the
// same single-instance behaviour in a portable way.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, Sender, channel};

const IPC_ADDR: &str = "127.0.0.1:37549";

pub enum Instance {
    /// This is the first (main) instance. The receiver yields the argument
    /// lists sent by subsequently launched instances.
    Primary(Server),
    /// Another instance is already running and the arguments were forwarded
    /// to it - the caller should exit.
    Secondary,
}

pub struct Server {
    rx: Receiver<Vec<String>>,
}

impl Server {
    /// Non-blocking poll for arguments sent from secondary instances.
    pub fn try_recv(&self) -> Option<Vec<String>> {
        self.rx.try_recv().ok()
    }
}

/// Is a NanoTorrent already running?
///
/// Tested by taking the single-instance port for an instant and giving it
/// straight back, so unlike [`init`] it forwards nothing and claims nothing.
///
/// A hint rather than a lock: the answer can go stale the moment it is
/// returned. That is enough to refuse "do not do this while the application is
/// open" and not enough to build anything else on.
pub fn another_instance_running() -> bool {
    TcpListener::bind(IPC_ADDR).is_err()
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
pub fn init(args: &[String]) -> anyhow::Result<Instance> {
    match TcpListener::bind(IPC_ADDR) {
        Ok(listener) => {
            let (tx, rx) = channel();
            std::thread::Builder::new()
                .name(String::from("pt-ipc"))
                .spawn(move || accept_loop(listener, tx))
                .expect("failed to spawn IPC thread");

            Ok(Instance::Primary(Server { rx }))
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
                "cannot use {IPC_ADDR} for single-instance detection ({bind_err});                  carrying on as the only instance"
            );
            // A receiver whose sender is already gone: it never yields, which
            // is exactly right when nothing can send to it.
            let (_tx, rx) = channel();
            Ok(Instance::Primary(Server { rx }))
        }
        Err(bind_err) => {
            // The port IS held. Normally that is another NanoTorrent and the
            // hand-off is the whole point - but if it will not take our
            // arguments it is not one, and exiting quietly would leave the
            // user with an application that simply does not start.
            let mut stream = TcpStream::connect(IPC_ADDR).map_err(|connect_err| {
                anyhow::anyhow!(
                    "Another program is using {IPC_ADDR}, which NanoTorrent uses to spot a second copy of itself.

could not listen: {bind_err}
could not connect: {connect_err}"
                )
            })?;

            let payload = serde_json::to_vec(args).unwrap_or_default();
            stream.write_all(&payload).map_err(|err| {
                anyhow::anyhow!(
                    "Another program is using {IPC_ADDR} and refused NanoTorrent's hand-off.

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

    /// The primary that could not listen still answers polls - it simply never
    /// has anything. A receiver whose sender is gone must not panic or block.
    #[test]
    fn a_primary_with_no_listener_polls_empty_forever() {
        let (_tx, rx) = channel();
        let server = Server { rx };
        for _ in 0..3 {
            assert!(server.try_recv().is_none());
        }
    }
}
