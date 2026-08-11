//! Outgoing peer stream transforms, injected into the engine through the
//! `StreamTransform` seam (vendor patch 0003 - see vendor/librqbit/PATCHES.md).
//! Feature code that wraps peer connections (protocol encryption, logging)
//! lives here in the app, not in engine patches.

use std::net::SocketAddr;

use librqbit::{BoxAsyncRead, BoxAsyncWrite, Id20, StreamTransform};

/// No-op transform: hands the stream halves back untouched. Proves the seam
/// end-to-end and serves as the template for real transforms (e.g. outgoing
/// MSE protocol encryption, which would run its handshake here before
/// returning wrapped read/write halves).
// Only exercised from tests until the first real transform (MSE) lands.
#[allow(dead_code)]
#[derive(Debug)]
pub struct PassthroughTransform;

impl StreamTransform for PassthroughTransform {
    fn transform(
        &self,
        _addr: SocketAddr,
        _info_hash: Id20,
        read: BoxAsyncRead,
        write: BoxAsyncWrite,
    ) -> futures::future::BoxFuture<'_, anyhow::Result<(BoxAsyncRead, BoxAsyncWrite)>> {
        Box::pin(async move { Ok((read, write)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Bytes written through the transformed halves must arrive intact, and
    /// SessionOptions must accept the transform - the full injection path.
    #[test]
    fn stream_transform_seam_roundtrip() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (client, server) = tokio::io::duplex(64);
            let (cr, cw) = tokio::io::split(client);

            let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
            let (mut read, mut write) = PassthroughTransform
                .transform(addr, Id20::new([0u8; 20]), Box::new(cr), Box::new(cw))
                .await
                .unwrap();

            tokio::spawn(async move {
                let (mut sr, mut sw) = tokio::io::split(server);
                tokio::io::copy(&mut sr, &mut sw).await.ok();
            });

            write.write_all(b"nanotorrent").await.unwrap();
            let mut buf = [0u8; 11];
            read.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"nanotorrent");
        });

        let opts = librqbit::SessionOptions {
            stream_transform: Some(std::sync::Arc::new(PassthroughTransform)),
            ..Default::default()
        };
        assert!(opts.stream_transform.is_some());
    }
}
