use std::{
    fs::{File, OpenOptions},
    io::IoSlice,
    ops::{Deref, DerefMut},
    path::PathBuf,
};

use anyhow::Context;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::Error;

pub trait OurFileExt {
    fn pwrite_all_vectored(&self, offset: u64, bufs: [IoSlice<'_>; 2]) -> anyhow::Result<usize>;
    fn pread_exact(&self, offset: u64, buf: &mut [u8]) -> anyhow::Result<()>;
    fn pwrite_all(&self, offset: u64, buf: &[u8]) -> anyhow::Result<()>;
}

impl OurFileExt for File {
    #[cfg(unix)]
    fn pwrite_all_vectored(&self, offset: u64, bufs: [IoSlice<'_>; 2]) -> anyhow::Result<usize> {
        nix::sys::uio::pwritev(self, &bufs, offset.try_into()?).context("error calling pwritev")
    }

    #[cfg(not(unix))]
    fn pwrite_all_vectored(&self, offset: u64, bufs: [IoSlice<'_>; 2]) -> anyhow::Result<usize> {
        match (bufs[0].len(), bufs[1].len()) {
            (len, 0) if len > 0 => {
                self.pwrite_all(offset, &bufs[0])?;
                Ok(len)
            }
            (0, len) if len > 0 => {
                self.pwrite_all(offset, &bufs[1])?;
                Ok(len)
            }
            (0, 0) => Ok(0),
            (l0, l1) => {
                // concatenate the buffers in memory so that we issue one write call instead of 2
                // assumes the message is <= CHUNK_SIZE
                use librqbit_core::constants::CHUNK_SIZE;
                let mut buf = [0u8; CHUNK_SIZE as usize];

                buf.get_mut(..l0)
                    .context("buf too small")?
                    .copy_from_slice(&bufs[0]);
                buf.get_mut(l0..l0 + l1)
                    .context("buf too small")?
                    .copy_from_slice(&bufs[1]);
                self.pwrite_all(offset, &buf[..l0 + l1])?;
                Ok(l0 + l1)
            }
        }
    }

    #[cfg(unix)]
    fn pread_exact(&self, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        use std::os::unix::fs::FileExt;

        Ok(self.read_exact_at(buf, offset)?)
    }

    #[cfg(windows)]
    fn pread_exact(&self, mut offset: u64, mut buf: &mut [u8]) -> anyhow::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            let n = self.seek_read(buf, offset)?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof").into());
            }
            offset += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }

    #[cfg(not(any(windows, unix)))]
    fn pread_exact(&self, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        anyhow::bail!("pread_exact not implemented for your platform")
    }

    #[cfg(unix)]
    fn pwrite_all(&self, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        use std::os::unix::fs::FileExt;
        Ok(self.write_all_at(buf, offset)?)
    }

    #[cfg(windows)]
    fn pwrite_all(&self, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        use std::os::windows::fs::FileExt;

        let mut remaining = buf.len();
        let mut buf = buf;
        let mut offset = offset;
        while remaining > 0 {
            let written = self.seek_write(&buf[..remaining], offset)?;
            remaining -= written;
            offset += written as u64;
            buf = &buf[written..];
        }
        Ok(())
    }

    #[cfg(not(any(windows, unix)))]
    fn pwrite_all(&self, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        anyhow::bail!("pwrite_all not implemented for your platform")
    }
}

#[derive(Default, Debug)]
struct OpenedFileLocked {
    path: PathBuf,
    fd: Option<File>,
    /// NanoTorrent addition: whether `fd` was opened with write access. Cleared
    /// by `OpenedFile::release_write_access`, restored by `lock_for_write`.
    writable: bool,
    #[cfg(windows)]
    tried_marking_sparse: bool,
}

impl OpenedFileLocked {
    /// NanoTorrent addition: whether the write path has to take the *write*
    /// lock before it can hand out the file - either to reopen it with write
    /// access, or (windows) to mark it sparse the first time.
    fn needs_write_lock(&self) -> bool {
        if self.fd.is_none() {
            // A padding file. Let the caller's mapping fail as it always has.
            return false;
        }
        #[cfg(windows)]
        return !self.writable || !self.tried_marking_sparse;
        #[cfg(not(windows))]
        return !self.writable;
    }
}

impl Deref for OpenedFileLocked {
    type Target = Option<File>;

    fn deref(&self) -> &Self::Target {
        &self.fd
    }
}

impl DerefMut for OpenedFileLocked {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.fd
    }
}

#[derive(Debug)]
pub(crate) struct OpenedFile {
    file: RwLock<OpenedFileLocked>,
}

impl OpenedFile {
    pub fn new(path: PathBuf, f: File) -> Self {
        Self {
            file: RwLock::new(OpenedFileLocked {
                path,
                fd: Some(f),
                writable: true,
                #[cfg(windows)]
                tried_marking_sparse: false,
            }),
        }
    }

    pub fn new_dummy() -> Self {
        Self {
            file: RwLock::new(Default::default()),
        }
    }

    pub fn take_clone(&self) -> anyhow::Result<Self> {
        let f = std::mem::take(&mut *self.file.write());
        Ok(Self {
            file: RwLock::new(f),
        })
    }

    pub fn lock_read(&self) -> crate::Result<impl Deref<Target = File>> {
        RwLockReadGuard::try_map(self.file.read(), |f| f.as_ref())
            .ok()
            .ok_or(Error::FsFileIsNone)
    }

    #[allow(dead_code)]
    pub fn lock_write(&self) -> crate::Result<impl DerefMut<Target = File>> {
        RwLockWriteGuard::try_map(self.file.write(), |f| f.as_mut())
            .ok()
            .ok_or(Error::FsFileIsNone)
    }

    /// NanoTorrent addition: the one accessor for the write path. Replaces
    /// upstream's `try_mark_sparse`, which did the windows sparse marking here;
    /// that is still done, and on top of it a file whose write access was
    /// released is reopened read-write.
    ///
    /// Every write goes through this, so nothing has to track whether a
    /// released file might be written to again: it just reopens.
    pub fn lock_for_write(&self) -> anyhow::Result<impl Deref<Target = File>> {
        {
            let g = self.file.read();
            if !g.needs_write_lock() {
                return Ok(RwLockReadGuard::try_map(g, |f| f.fd.as_ref())
                    .ok()
                    .ok_or(Error::FsFileIsNone)?);
            }
        }
        let mut g = self.file.write();
        if g.fd.is_some() && !g.writable {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&g.path)
                .with_context(|| format!("error reopening {:?} read-write", g.path))?;
            // Assigning drops the read-only handle.
            g.fd = Some(f);
            g.writable = true;
        }
        #[cfg(windows)]
        if !g.tried_marking_sparse {
            g.tried_marking_sparse = true;
            let f = g.fd.as_ref().ok_or(Error::FsFileIsNone)?;
            tracing::debug!(path=?g.path, marked=super::sparse::mark_file_sparse(f), "marking sparse");
        }
        let g = parking_lot::RwLockWriteGuard::downgrade(g);
        Ok(RwLockReadGuard::try_map(g, |f| f.fd.as_ref())
            .ok()
            .ok_or(Error::FsFileIsNone)?)
    }

    /// NanoTorrent addition: reopen the file without write access.
    ///
    /// A torrent that has finished downloading, or that has been paused, is not
    /// going to write - but the handle from `init()` carries `GENERIC_WRITE`,
    /// and on windows that alone fails any other program that opens the file
    /// with `dwShareMode = FILE_SHARE_READ`, which is what a great many of them
    /// use. The symptom is an archive or a video in the download folder that
    /// cannot be opened until NanoTorrent exits.
    ///
    /// Seeding only reads, so the write handle is simply dropped. If a write
    /// does turn out to be needed later - a re-check finding corruption, a file
    /// newly selected, a stream of an unselected file - `lock_for_write`
    /// reopens read-write on demand.
    pub fn release_write_access(&self) -> anyhow::Result<()> {
        {
            let g = self.file.read();
            if g.fd.is_none() || !g.writable {
                return Ok(());
            }
        }
        let mut g = self.file.write();
        if g.fd.is_none() || !g.writable {
            return Ok(());
        }
        let f = OpenOptions::new()
            .read(true)
            .open(&g.path)
            .with_context(|| format!("error reopening {:?} read-only", g.path))?;
        // Assigning drops the read-write handle, which is the point.
        g.fd = Some(f);
        g.writable = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use librqbit_core::constants::CHUNK_SIZE;
    use peer_binary_protocol::DoubleBufHelper;
    use tempfile::TempDir;

    use crate::storage::filesystem::opened_file::OurFileExt;

    #[test]
    fn test_pwrite_all_vectored() {
        let td = TempDir::with_prefix("test_pwrite_all_vectored").unwrap();
        let mut tmp_buf = [0u8; CHUNK_SIZE as usize];
        for bufsize in [10000usize, CHUNK_SIZE as usize] {
            let mut buf = vec![0u8; bufsize];
            rand::fill(&mut buf[..]);
            for split_point in [0, bufsize / 2, bufsize] {
                let path = td.path().join(format!("file_{bufsize}_{split_point}"));
                let file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)
                    .unwrap();
                let (first, second) = buf.split_at(split_point);
                let bufs = DoubleBufHelper::new(first, second).as_ioslices(bufsize);
                file.pwrite_all_vectored(0, bufs).unwrap();

                let mut file = std::fs::File::open(&path).unwrap();
                assert_eq!(file.metadata().unwrap().len(), bufsize as u64, "{path:?}");
                file.read_exact(&mut tmp_buf[..bufsize]).unwrap();
                assert_eq!(&tmp_buf[..bufsize], buf);
            }
        }
    }

    /// NanoTorrent patch 0021. The assertion that matters is the middle one:
    /// before releasing write access, an open with `dwShareMode =
    /// FILE_SHARE_READ` - what archivers and media players use - fails with a
    /// sharing violation, and that is the reported bug.
    #[cfg(windows)]
    #[test]
    fn nanotorrent_release_write_access_unblocks_other_programs() {
        use super::OpenedFile;
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 0x0000_0001;

        let td = TempDir::with_prefix("release_write_access").unwrap();
        let path = td.path().join("payload.bin");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let of = OpenedFile::new(path.clone(), file);
        of.lock_for_write().unwrap().pwrite_all(0, b"hello").unwrap();

        // "others may read, nobody may write" - conflicts with our GENERIC_WRITE.
        let third_party_open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(&path)
        };
        assert!(
            third_party_open().is_err(),
            "the write handle should still have been blocking this"
        );

        of.release_write_access().unwrap();
        third_party_open().expect("releasing write access must unblock the file");

        // Reads keep working, and a write reopens read-write on demand.
        let mut buf = [0u8; 5];
        of.lock_read().unwrap().pread_exact(0, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        of.lock_for_write().unwrap().pwrite_all(0, b"world").unwrap();
        of.lock_read().unwrap().pread_exact(0, &mut buf).unwrap();
        assert_eq!(&buf, b"world");
        assert!(
            third_party_open().is_err(),
            "writing again must have taken write access back"
        );
    }
}
