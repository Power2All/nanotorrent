use std::{
    net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    task::Poll,
};

use socket2::{Domain, Socket};
use tracing::{debug, trace};

use crate::{
    Error,
    addr::{ToV6Mapped, TryToV4},
    bind_device::BindDevice,
};

#[derive(Clone, Copy, Debug)]
pub enum SocketAddrKind {
    V4(SocketAddrV4),
    V6 {
        addr: SocketAddrV6,
        is_dualstack: bool,
    },
}

impl SocketAddrKind {
    fn is_v6(&self) -> bool {
        matches!(self, SocketAddrKind::V6 { .. })
    }

    fn as_socketaddr(&self) -> SocketAddr {
        match *self {
            SocketAddrKind::V4(addr) => SocketAddr::V4(addr),
            SocketAddrKind::V6 { addr, .. } => SocketAddr::V6(addr),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BindOpts<'a> {
    pub request_dualstack: bool,
    pub reuseport: bool,
    pub device: Option<&'a BindDevice>,
}

impl Default for BindOpts<'_> {
    fn default() -> Self {
        Self {
            request_dualstack: true,
            reuseport: false,
            device: None,
        }
    }
}

pub struct MaybeDualstackSocket<S> {
    socket: S,
    addr_kind: SocketAddrKind,
}

impl<S> MaybeDualstackSocket<S> {
    pub fn socket(&self) -> &S {
        &self.socket
    }

    pub fn bind_addr(&self) -> SocketAddr {
        self.addr_kind.as_socketaddr()
    }

    pub fn is_dualstack(&self) -> bool {
        matches!(
            self.addr_kind,
            SocketAddrKind::V6 {
                is_dualstack: true,
                ..
            }
        )
    }

    pub(crate) fn convert_addr_for_send(&self, addr: SocketAddr) -> SocketAddr {
        if self.is_dualstack() {
            return SocketAddr::V6(addr.to_ipv6_mapped());
        }
        addr
    }
}

impl MaybeDualstackSocket<Socket> {
    fn bind(addr: SocketAddr, opts: BindOpts, is_udp: bool) -> crate::Result<Self> {
        let socket = Socket::new(
            if addr.is_ipv6() {
                Domain::IPV6
            } else {
                Domain::IPV4
            },
            if is_udp {
                socket2::Type::DGRAM
            } else {
                socket2::Type::STREAM
            },
            Some(if is_udp {
                socket2::Protocol::UDP
            } else {
                socket2::Protocol::TCP
            }),
        )
        .map_err(Error::SocketNew)?;

        let mut set_dualstack = false;

        let addr_kind = match (opts.request_dualstack, addr) {
            (request_dualstack, SocketAddr::V6(addr))
                if *addr.ip() == IpAddr::V6(Ipv6Addr::UNSPECIFIED) =>
            {
                let value = !request_dualstack;
                trace!(?addr, only_v6 = value, "setting only_v6");
                socket
                    .set_only_v6(value)
                    .map_err(|e| Error::OnlyV6 { value, source: e })?;
                #[cfg(not(windows))] // socket.only_v6() panics on windows somehow
                trace!(?addr, only_v6=?socket.only_v6());
                set_dualstack = true;
                SocketAddrKind::V6 {
                    addr,
                    is_dualstack: request_dualstack,
                }
            }
            (_, SocketAddr::V6(addr)) => SocketAddrKind::V6 {
                addr,
                is_dualstack: false,
            },
            (_, SocketAddr::V4(addr)) => SocketAddrKind::V4(addr),
        };

        if !set_dualstack {
            debug!(
                ?addr,
                "ignored dualstack request as it only applies to [::] address"
            );
        }

        #[cfg(not(windows))]
        {
            socket
                .set_reuse_address(true)
                .map_err(Error::ReuseAddress)?;
        }

        #[cfg(windows)]
        if opts.reuseport || !is_udp {
            socket
                .set_reuse_address(true)
                .map_err(Error::ReuseAddress)?;
        }

        #[cfg(not(windows))]
        if opts.reuseport {
            socket.set_reuse_port(true).map_err(Error::ReusePort)?;
            debug!(reuse_port=?socket.reuse_port());
            debug!(reuse_addr=?socket.reuse_address());
        }

        if let Some(bd) = opts.device {
            bd.bind_sref(&socket, addr_kind.is_v6())?;
        }

        socket.bind(&addr.into()).map_err(|e| {
            trace!(?addr, "error binding: {e:#}");
            Error::Bind(e)
        })?;

        let local_addr: SocketAddr = socket
            .local_addr()
            .map_err(Error::LocalAddr)?
            .as_socket()
            .ok_or(Error::AsSocket)?;

        let addr_kind = match (addr_kind, local_addr) {
            (SocketAddrKind::V4(..), SocketAddr::V4(received)) => SocketAddrKind::V4(received),
            (SocketAddrKind::V6 { is_dualstack, .. }, SocketAddr::V6(received)) => {
                SocketAddrKind::V6 {
                    addr: received,
                    is_dualstack,
                }
            }
            _ => {
                tracing::debug!(?local_addr, bind_addr=?addr, "mismatch between local_addr() and requested bind_addr");
                return Err(Error::LocalBindAddrMismatch);
            }
        };

        socket
            .set_nonblocking(true)
            .map_err(Error::SetNonblocking)?;

        Ok(Self { socket, addr_kind })
    }
}

#[cfg(target_os = "linux")]
impl TryFrom<std::os::fd::OwnedFd> for MaybeDualstackSocket<tokio::net::TcpListener> {
    type Error = crate::Error;
    /// Convert an owned file-descriptor to a tokio TCP Listener.
    ///
    /// If the passed file descriptor is not a TCP listener, the file descriptor will be closed and
    /// this function will return an error.
    fn try_from(fd: std::os::fd::OwnedFd) -> Result<Self, Self::Error> {
        use std::io;
        let sock = Socket::from(fd);
        match sock.protocol().map_err(Error::SocketFromFd)? {
            Some(socket2::Protocol::TCP) => {}
            Some(proto) => {
                return Err(Error::SocketFromFd(io::Error::other(format!(
                    "expected a TCP socket, got a {proto:?} socket"
                ))));
            }
            None => {
                return Err(Error::SocketFromFd(io::Error::other(
                    "socket has no protocol",
                )));
            }
        };

        if !sock.is_listener().map_err(Error::SocketFromFd)? {
            return Err(Error::SocketFromFd(io::Error::other(
                "expected a listening TCP socket",
            )));
        }

        let addr_kind = match sock
            .local_addr()
            .map_err(Error::LocalAddr)?
            .as_socket()
            .ok_or(Error::AsSocket)?
        {
            SocketAddr::V4(addr) => SocketAddrKind::V4(addr),
            SocketAddr::V6(addr) => SocketAddrKind::V6 {
                addr,
                is_dualstack: addr.ip().is_unspecified()
                    && !sock.only_v6().map_err(Error::SocketFromFd)?,
            },
        };

        sock.set_nonblocking(true).map_err(Error::SetNonblocking)?;

        Ok(Self {
            addr_kind,
            socket: tokio::net::TcpListener::from_std(std::net::TcpListener::from(sock))
                .map_err(Error::TokioFromStd)?,
        })
    }
}

impl MaybeDualstackSocket<tokio::net::TcpListener> {
    pub fn bind_tcp(addr: SocketAddr, opts: BindOpts) -> crate::Result<Self> {
        let sock = MaybeDualstackSocket::bind(addr, opts, false)?;

        debug!(addr=?sock.bind_addr(), requested_addr=?addr, dualstack = sock.is_dualstack(), "listening on TCP");
        sock.socket().listen(1024).map_err(Error::Listen)?;

        Ok(Self {
            socket: tokio::net::TcpListener::from_std(std::net::TcpListener::from(sock.socket))
                .map_err(Error::TokioFromStd)?,
            addr_kind: sock.addr_kind,
        })
    }

    pub async fn accept(&self) -> std::io::Result<(tokio::net::TcpStream, SocketAddr)> {
        let (s, addr) = self.socket.accept().await?;
        Ok((s, addr.try_to_ipv4()))
    }
}

#[cfg(feature = "axum")]
pub mod axum {
    use std::net::SocketAddr;

    use crate::socket::MaybeDualstackSocket;

    #[derive(Clone, Copy)]
    pub struct WrappedSocketAddr(pub SocketAddr);
    impl core::fmt::Debug for WrappedSocketAddr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:?}", self.0)
        }
    }
    impl From<SocketAddr> for WrappedSocketAddr {
        fn from(value: SocketAddr) -> Self {
            Self(value)
        }
    }
    impl From<WrappedSocketAddr> for SocketAddr {
        fn from(value: WrappedSocketAddr) -> Self {
            value.0
        }
    }

    impl axum::serve::Listener for MaybeDualstackSocket<tokio::net::TcpListener> {
        type Io = tokio::net::TcpStream;

        type Addr = WrappedSocketAddr;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            use backon::{ExponentialBuilder, Retryable};
            let (l, a) = (|| MaybeDualstackSocket::accept(self))
                .retry(
                    ExponentialBuilder::new()
                        .without_max_times()
                        .with_max_delay(std::time::Duration::from_secs(5)),
                )
                .notify(|e, retry_in| tracing::trace!(?retry_in, "error accepting: {e:#}"))
                .await
                .unwrap();
            (l, a.into())
        }

        fn local_addr(&self) -> tokio::io::Result<Self::Addr> {
            Ok(self.bind_addr().into())
        }
    }

    impl
        axum::extract::connect_info::Connected<
            axum::serve::IncomingStream<'_, MaybeDualstackSocket<tokio::net::TcpListener>>,
        > for WrappedSocketAddr
    {
        fn connect_info(
            stream: axum::serve::IncomingStream<'_, MaybeDualstackSocket<tokio::net::TcpListener>>,
        ) -> Self {
            *stream.remote_addr()
        }
    }
}

/// NanoTorrent addition: turn off Windows' habit of reporting an ICMP port
/// unreachable as a fatal error on the *next* read of a UDP socket.
///
/// Without this, one unreachable peer or dead DHT bootstrap node makes
/// `recv_from` return WSAECONNRESET and every caller written against Unix
/// semantics treats that as the end of the socket.
#[cfg(windows)]
fn disable_udp_conn_reset(socket: &socket2::Socket) {
    use std::os::windows::io::AsRawSocket;

    // ICMP port unreachable -> WSAECONNRESET (10054) on the next read.
    const SIO_UDP_CONNRESET: u32 = 0x9800_000C;
    // ICMP TTL expired / net unreachable -> WSAENETRESET (10052). A separate
    // ioctl, and a separate failure: switching the first one off simply moved
    // the error code, which is how this one was found.
    const SIO_UDP_NETRESET: u32 = 0x9800_000F;

    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn WSAIoctl(
            s: usize,
            dwIoControlCode: u32,
            lpvInBuffer: *const core::ffi::c_void,
            cbInBuffer: u32,
            lpvOutBuffer: *mut core::ffi::c_void,
            cbOutBuffer: u32,
            lpcbBytesReturned: *mut u32,
            lpOverlapped: *mut core::ffi::c_void,
            lpCompletionRoutine: *mut core::ffi::c_void,
        ) -> i32;
    }

    let enable: u32 = 0; // FALSE - stop reporting these as socket errors
    for (code, name) in [
        (SIO_UDP_CONNRESET, "SIO_UDP_CONNRESET"),
        // Windows 8 and later. An older system fails it harmlessly.
        (SIO_UDP_NETRESET, "SIO_UDP_NETRESET"),
    ] {
        let mut returned: u32 = 0;
        let rc = unsafe {
            WSAIoctl(
                socket.as_raw_socket() as usize,
                code,
                (&raw const enable).cast(),
                std::mem::size_of::<u32>() as u32,
                std::ptr::null_mut(),
                0,
                &raw mut returned,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 {
            debug!("{name} failed; UDP reads may die on an ICMP message");
        }
    }
}

#[cfg(not(windows))]
fn disable_udp_conn_reset(_socket: &socket2::Socket) {}

impl MaybeDualstackSocket<tokio::net::UdpSocket> {
    pub fn bind_udp(addr: SocketAddr, opts: BindOpts) -> crate::Result<Self> {
        let sock = MaybeDualstackSocket::bind(addr, opts, true)?;

        // NanoTorrent: before the socket is handed to tokio and used.
        disable_udp_conn_reset(&sock.socket);

        debug!(addr=?sock.bind_addr(), requested_addr=?addr, dualstack = sock.is_dualstack(), "listening on UDP");

        Ok(Self {
            socket: tokio::net::UdpSocket::from_std(std::net::UdpSocket::from(sock.socket))
                .map_err(Error::TokioFromStd)?,
            addr_kind: sock.addr_kind,
        })
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        let (size, addr) = self.socket.recv_from(buf).await?;
        Ok((size, addr.try_to_ipv4()))
    }

    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> std::io::Result<usize> {
        let target = self.convert_addr_for_send(target);
        self.socket.send_to(buf, target).await
    }

    pub fn poll_send_to(
        &self,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<std::io::Result<usize>> {
        let target = self.convert_addr_for_send(target);
        self.socket.poll_send_to(cx, buf, target)
    }
}
