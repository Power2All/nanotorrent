use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use socket2::SockRef;

use crate::{Error, bind_device::BindDevice};

#[derive(Clone, Copy, Debug, Default)]
pub struct ConnectOpts<'a> {
    pub source_port: Option<u16>,
    pub bind_device: Option<&'a BindDevice>,
}

pub async fn tcp_connect<'a>(
    addr: SocketAddr,
    opts: ConnectOpts<'a>,
) -> crate::Result<tokio::net::TcpStream> {
    let (sock, bind_addr) = if addr.is_ipv6() {
        (
            tokio::net::TcpSocket::new_v6().map_err(Error::SocketNew)?,
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, opts.source_port.unwrap_or(0))),
        )
    } else {
        (
            tokio::net::TcpSocket::new_v4().map_err(Error::SocketNew)?,
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, opts.source_port.unwrap_or(0))),
        )
    };
    let sref = SockRef::from(&sock);

    if let Some(bd) = opts.bind_device {
        bd.bind_sref(&sref, addr.is_ipv6())?;
    }

    // NanoTorrent: on Windows the device is applied by binding its address as
    // the source, so the outgoing connection leaves by that interface. The
    // bind then has to happen whether or not a source port was asked for -
    // upstream only binds when a port is set, which would leave the source
    // address unconfined.
    let mut bind_addr = bind_addr;
    let mut must_bind = bind_addr.port() > 0;
    if let Some(bd) = opts.bind_device {
        match bd.bind_ip(addr.is_ipv6()) {
            Some(ip) => {
                bind_addr = SocketAddr::new(ip, bind_addr.port());
                must_bind = true;
            }
            // Only reachable on Windows, and only for a family this device has
            // no address in. Refuse: connecting unbound is the leak.
            #[cfg(windows)]
            None => return Err(Error::BindDeviceNotSupported),
            #[cfg(not(windows))]
            None => {}
        }
    }

    if must_bind {
        #[cfg(not(windows))]
        sref.set_reuse_port(true).map_err(Error::ReusePort)?;
        sref.set_reuse_address(true).map_err(Error::ReuseAddress)?;
        sref.bind(&bind_addr.into()).map_err(Error::Bind)?;
    }

    sock.connect(addr).await.map_err(Error::Connect)
}
