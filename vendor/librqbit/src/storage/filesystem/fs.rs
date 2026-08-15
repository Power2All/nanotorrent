use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
};

use anyhow::Context;
use tracing::warn;

use crate::{
    storage::StorageFactoryExt,
    torrent_state::{ManagedTorrentShared, TorrentMetadata},
};

use crate::storage::{StorageFactory, TorrentStorage};

use super::opened_file::OpenedFile;

#[derive(Default, Clone, Copy)]
pub struct FilesystemStorageFactory {}

impl StorageFactory for FilesystemStorageFactory {
    type Storage = FilesystemStorage;

    fn create(
        &self,
        shared: &ManagedTorrentShared,
        _metadata: &TorrentMetadata,
    ) -> anyhow::Result<FilesystemStorage> {
        Ok(FilesystemStorage {
            output_folder: shared.options.output_folder.clone(),
            opened_files: Default::default(),
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.boxed()
    }
}

pub struct FilesystemStorage {
    pub(super) output_folder: PathBuf,
    pub(super) opened_files: Vec<OpenedFile>,
}

impl FilesystemStorage {
    pub(super) fn take_fs(&self) -> anyhow::Result<Self> {
        Ok(Self {
            opened_files: self
                .opened_files
                .iter()
                .map(|f| f.take_clone())
                .collect::<anyhow::Result<Vec<_>>>()?,
            output_folder: self.output_folder.clone(),
        })
    }
}

impl TorrentStorage for FilesystemStorage {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let of = self.opened_files.get(file_id).context("no such file")?;
        #[cfg(target_family = "unix")]
        {
            use std::os::unix::fs::FileExt;
            Ok(of
                .file
                .read()
                .as_ref()
                .context("file is None")?
                .read_exact_at(buf, offset)?)
        }
        #[cfg(target_family = "windows")]
        {
            use std::os::windows::fs::FileExt;
            let g = of.file.read();
            let f = g.as_ref().context("file is None")?;
            // NanoTorrent: seek_read is a single ReadFile - at (or past) EOF it
            // returns a SHORT COUNT, it does not fail. Discarding that count made
            // every read of a not-yet-downloaded file "succeed" while leaving the
            // caller's buffer untouched, so nothing here behaved like the unix
            // read_exact_at this is supposed to mirror: the initial check hashed
            // whole torrents that hold no data yet instead of skipping them, and
            // a short read could serve stale buffer bytes to a peer. Loop to fill
            // the buffer, and report EOF as an error like read_exact_at does.
            let mut buf = buf;
            let mut offset = offset;
            while !buf.is_empty() {
                match f.seek_read(buf, offset)? {
                    0 => {
                        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into())
                    }
                    n => {
                        let rest = buf;
                        buf = &mut rest[n..];
                        offset += n as u64;
                    }
                }
            }
            Ok(())
        }
        #[cfg(not(any(target_family = "unix", target_family = "windows")))]
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut g = of.file.write();
            let mut f = g.as_ref().context("file is None")?;
            f.seek(SeekFrom::Start(offset))?;
            Ok(f.read_exact(buf)?)
        }
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        let of = self.opened_files.get(file_id).context("no such file")?;
        #[cfg(target_family = "unix")]
        {
            use std::os::unix::fs::FileExt;
            Ok(of
                .file
                .read()
                .as_ref()
                .context("file is None")?
                .write_all_at(buf, offset)?)
        }
        #[cfg(target_family = "windows")]
        {
            use std::os::windows::fs::FileExt;
            let g = of.file.read();
            let f = g.as_ref().context("file is None")?;
            // NanoTorrent: same short-count problem as pread_exact above, plus
            // this loop re-wrote the WHOLE buf at the SAME offset every pass, so
            // a partial write made `remaining` underflow. Advance both instead.
            let mut buf = buf;
            let mut offset = offset;
            while !buf.is_empty() {
                match f.seek_write(buf, offset)? {
                    0 => return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).into()),
                    n => {
                        buf = &buf[n..];
                        offset += n as u64;
                    }
                }
            }
            Ok(())
        }
        #[cfg(not(any(target_family = "unix", target_family = "windows")))]
        {
            use std::io::{Read, Seek, SeekFrom, Write};
            let mut g = of.file.write();
            let mut f = g.as_ref().context("file is None")?;
            f.seek(SeekFrom::Start(offset))?;
            Ok(f.write_all(buf)?)
        }
    }

    fn remove_file(&self, _file_id: usize, filename: &Path) -> anyhow::Result<()> {
        Ok(std::fs::remove_file(self.output_folder.join(filename))?)
    }

    fn ensure_file_length(&self, file_id: usize, len: u64) -> anyhow::Result<()> {
        Ok(self.opened_files[file_id]
            .file
            .write()
            .as_ref()
            .context("file is None")?
            .set_len(len)?)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(Self {
            opened_files: self
                .opened_files
                .iter()
                .map(|f| f.take_clone())
                .collect::<anyhow::Result<Vec<_>>>()?,
            output_folder: self.output_folder.clone(),
        }))
    }

    fn remove_directory_if_empty(&self, path: &Path) -> anyhow::Result<()> {
        let path = self.output_folder.join(path);
        if !path.is_dir() {
            anyhow::bail!("cannot remove dir: {path:?} is not a directory")
        }
        if std::fs::read_dir(&path)?.count() == 0 {
            std::fs::remove_dir(&path).with_context(|| format!("error removing {path:?}"))
        } else {
            warn!("did not remove {path:?} as it was not empty");
            Ok(())
        }
    }

    fn init(
        &mut self,
        shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        let mut files = Vec::<OpenedFile>::new();
        for file_details in metadata.file_infos.iter() {
            let mut full_path = self.output_folder.clone();
            let relative_path = &file_details.relative_filename;
            full_path.push(relative_path);

            if file_details.attrs.padding {
                files.push(OpenedFile::new_dummy());
                continue;
            };
            std::fs::create_dir_all(full_path.parent().context("bug: no parent")?)?;
            let f = if shared.options.allow_overwrite {
                OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&full_path)
                    .with_context(|| format!("error opening {full_path:?} in read/write mode"))?
            } else {
                // create_new does not seem to work with read(true), so calling this twice.
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&full_path)
                    .with_context(|| {
                        format!(
                            "error creating a new file (because allow_overwrite = false) {:?}",
                            &full_path
                        )
                    })?;
                OpenOptions::new().read(true).write(true).open(&full_path)?
            };
            files.push(OpenedFile::new(f));
        }

        self.opened_files = files;
        Ok(())
    }
}

// NanoTorrent: guards the short-read/short-write fix in pread_exact/pwrite_all.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::TorrentStorage;

    fn storage(name: &str, contents: &[u8]) -> FilesystemStorage {
        let dir = std::env::temp_dir().join("librqbit-fs-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        let f = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        FilesystemStorage {
            output_folder: dir,
            opened_files: vec![OpenedFile::new(f)],
        }
    }

    #[test]
    fn pread_exact_fails_on_empty_file() {
        // The case that made the initial check hash whole torrents that have
        // nothing on disk: a file init() just created, read before any download.
        let s = storage("empty.bin", b"");
        let mut buf = [0xAAu8; 4096];
        assert!(s.pread_exact(0, 0, &mut buf).is_err());
    }

    #[test]
    fn pread_exact_fails_past_eof_and_leaves_no_stale_bytes() {
        let s = storage("short.bin", b"0123456789");
        let mut buf = [0xAAu8; 64];
        assert!(s.pread_exact(0, 0, &mut buf).is_err());

        // A fully-covered read must still work, and read the real bytes.
        let mut buf = [0u8; 10];
        s.pread_exact(0, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"0123456789");
    }

    #[test]
    fn pwrite_all_advances_the_offset() {
        let s = storage("write.bin", b"..........");
        s.pwrite_all(0, 4, b"XY").unwrap();
        let mut buf = [0u8; 10];
        s.pread_exact(0, 0, &mut buf).unwrap();
        assert_eq!(&buf, b"....XY....");
    }
}
