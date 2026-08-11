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

pub fn init(args: &[String]) -> Instance {
    match TcpListener::bind(IPC_ADDR) {
        Ok(listener) => {
            let (tx, rx) = channel();
            std::thread::Builder::new()
                .name(String::from("pt-ipc"))
                .spawn(move || accept_loop(listener, tx))
                .expect("failed to spawn IPC thread");

            Instance::Primary(Server { rx })
        }
        Err(_) => {
            // Assume another instance holds the port; forward our args.
            if let Ok(mut stream) = TcpStream::connect(IPC_ADDR) {
                let payload = serde_json::to_vec(args).unwrap_or_default();
                let _ = stream.write_all(&payload);
                let _ = stream.shutdown(std::net::Shutdown::Write);
            }

            Instance::Secondary
        }
    }
}

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
