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
        Err(bind_err) => {
            // Something holds the port. Normally that is another NanoTorrent
            // and the hand-off is the whole point - but if it will not take
            // our arguments it is not one, and exiting quietly would leave the
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
