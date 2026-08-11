use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use librqbit_core::hash_id::Id20;

pub type BoxAsyncRead = Box<dyn tokio::io::AsyncRead + Send + Unpin>;
pub type BoxAsyncWrite = Box<dyn tokio::io::AsyncWrite + Send + Unpin>;

/// A pluggable transform applied to every outgoing peer stream right after
/// the TCP (or proxy) connection is established, before the BitTorrent
/// handshake. Lets the embedding application wrap connections (e.g. with
/// protocol encryption) without modifying the engine.
pub trait StreamTransform: Send + Sync + std::fmt::Debug {
    fn transform(
        &self,
        addr: SocketAddr,
        info_hash: Id20,
        read: BoxAsyncRead,
        write: BoxAsyncWrite,
    ) -> futures::future::BoxFuture<'_, anyhow::Result<(BoxAsyncRead, BoxAsyncWrite)>>;
}

/// The incoming counterpart of [`StreamTransform`], applied to every accepted
/// peer stream before the BitTorrent handshake is read. Unlike the outgoing
/// side, the info-hash is not yet known, so the candidate info-hashes of all
/// active torrents are passed in for the transform to resolve (e.g. an MSE
/// responder matching the peer's SKEY). Lets the application add inbound
/// protocol encryption without modifying the engine.
pub trait IncomingStreamTransform: Send + Sync + std::fmt::Debug {
    fn transform(
        &self,
        addr: SocketAddr,
        info_hashes: Vec<Id20>,
        read: BoxAsyncRead,
        write: BoxAsyncWrite,
    ) -> futures::future::BoxFuture<'_, anyhow::Result<(BoxAsyncRead, BoxAsyncWrite)>>;
}

#[derive(Debug, Clone)]
pub(crate) struct SocksProxyConfig {
    pub host: String,
    pub port: u16,
    pub username_password: Option<(String, String)>,
}

impl SocksProxyConfig {
    pub fn parse(url: &str) -> anyhow::Result<Self> {
        let url = ::url::Url::parse(url).context("invalid proxy URL")?;
        if url.scheme() != "socks5" {
            anyhow::bail!("proxy URL should have socks5 scheme");
        }
        let host = url.host_str().context("missing host")?;
        let port = url.port().context("missing port")?;
        let up = url
            .password()
            .map(|p| (url.username().to_owned(), p.to_owned()));
        Ok(Self {
            host: host.to_owned(),
            port,
            username_password: up,
        })
    }

    async fn connect(
        &self,
        addr: SocketAddr,
    ) -> anyhow::Result<(
        impl tokio::io::AsyncRead + Unpin,
        impl tokio::io::AsyncWrite + Unpin,
    )> {
        let proxy_addr = (self.host.as_str(), self.port);

        let stream = if let Some((username, password)) = self.username_password.as_ref() {
            tokio_socks::tcp::Socks5Stream::connect_with_password(
                proxy_addr,
                addr,
                username.as_str(),
                password.as_str(),
            )
            .await
            .context("error connecting to proxy")?
        } else {
            tokio_socks::tcp::Socks5Stream::connect(proxy_addr, addr)
                .await
                .context("error connecting to proxy")?
        };

        Ok(tokio::io::split(stream))
    }
}

#[derive(Debug, Default)]
pub(crate) struct StreamConnector {
    proxy_config: Option<SocksProxyConfig>,
    transform: Option<Arc<dyn StreamTransform>>,
}

impl From<Option<SocksProxyConfig>> for StreamConnector {
    fn from(proxy_config: Option<SocksProxyConfig>) -> Self {
        Self {
            proxy_config,
            transform: None,
        }
    }
}

impl StreamConnector {
    pub fn new(
        proxy_config: Option<SocksProxyConfig>,
        transform: Option<Arc<dyn StreamTransform>>,
    ) -> Self {
        Self {
            proxy_config,
            transform,
        }
    }

    pub async fn connect(
        &self,
        addr: SocketAddr,
        info_hash: Id20,
    ) -> anyhow::Result<(BoxAsyncRead, BoxAsyncWrite)> {
        let (read, write): (BoxAsyncRead, BoxAsyncWrite) =
            if let Some(proxy) = self.proxy_config.as_ref() {
                let (r, w) = proxy.connect(addr).await?;
                (Box::new(r), Box::new(w))
            } else {
                let (r, w) = tokio::net::TcpStream::connect(addr)
                    .await
                    .context("error connecting")?
                    .into_split();
                (Box::new(r), Box::new(w))
            };

        match self.transform.as_ref() {
            Some(t) => t.transform(addr, info_hash, read, write).await,
            None => Ok((read, write)),
        }
    }
}
