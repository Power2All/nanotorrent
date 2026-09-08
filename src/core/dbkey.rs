//! Encrypting the settings database: where the key lives, and how the file is
//! wrapped.
//!
//! The point of encrypting the database is not secrecy for its own sake. Three
//! things in it are worth defending:
//!
//! - `plugins.grants` holds the exact permission set the user approved for each
//!   plugin. Editing that one row hands a script network and disk access nobody
//!   agreed to - privilege escalation with a text editor.
//! - `libtorrent.proxy_password` is stored in the clear, because SOCKS needs
//!   the actual password rather than a hash of it.
//! - `strict_network` and `bind_interface` are the kill switch. Turning those
//!   off quietly converts "stop rather than leak" into a leak.
//!
//! The file is sealed whole with an AEAD, so an edit made without the key does
//! not merely stay secret - it is *detected*, which is the property that
//! matters here. Every byte is covered and the tag is checked on every open.
//!
//! # Why the whole file, rather than SQLCipher
//!
//! SQLCipher encrypts page by page and keeps a MAC per page, which is the right
//! design for a database too large to hold in memory. It also means linking
//! OpenSSL - it is the only crypto backend `libsqlite3-sys` offers on Windows -
//! which costs a Perl build prerequisite and a vendored C library, for one
//! feature.
//!
//! This database does not need that design. It holds settings, labels, filters,
//! plugin grants and column layout; resume data lives in librqbit's own session
//! files, not here (see [`crate::core::environment`]). A real profile measures
//! ~140 KB, so the whole thing is sealed and written at once, and the
//! encryption is a pure-Rust dependency with no build tools behind it.
//!
//! Two consequences, both good: the tag covers bytes that are never read, where
//! SQLCipher's per-page MAC is only checked on the pages something touches; and
//! there is no C crypto library in the build. One consequence that is not: if
//! resume data ever moves into SQLite, this design has to be revisited, because
//! the whole file is rewritten on every commit.
//!
//! # What automatic unlocking can and cannot do
//!
//! If NanoTorrent can open the database with no input from anyone, then so can
//! anything else running as that user: it reads the key exactly the way this
//! module does. Automatic unlocking therefore protects against
//!
//! - another account on the same machine,
//! - a stolen disk, a backup, a synced or cloud-copied profile,
//! - other programs and casual editing of the file,
//! - and any change made without the key, which the tag catches,
//!
//! and it does not protect against code already running as the user. That is a
//! real boundary and worth having, but the UI must describe it as protecting a
//! database that leaves this machine rather than as preventing tampering
//! outright. Overstating it would be the actual mistake.
//!
//! Only automatic unlocking is implemented. A password mode would be strictly
//! stronger - the key derived on the spot and never written down - but it needs
//! a prompt before the window exists and a way to pass the password to every
//! command-line invocation, and it is not half-built here in the meantime. What
//! it would change is this module and the two call sites in `Database::open`;
//! nothing in the on-disk format stands in the way.

// `bail!` is referenced through its full path below: it is used only in the
// Windows DPAPI branch, and importing it here would be an unused import on the
// Linux and macOS builds.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::XChaCha20Poly1305;

use super::environment::Environment;

/// What a sealed file starts with, so the format identifies itself.
///
/// The database's own header is `SQLite format 3\0`, and the two are told apart
/// by looking - which is what lets the key file be advisory rather than
/// authoritative. A key left behind by a conversion that did not finish sits
/// beside a database that still says `SQLite format 3`, and that database opens
/// as what it is instead of failing to open at all.
const MAGIC: &[u8; 8] = b"NTSQLDB1";

/// XChaCha20-Poly1305, not AES-GCM: the nonce is 24 bytes, which is wide enough
/// to pick at random for every write without tracking a counter. AES-GCM's 12
/// bytes are not, and a repeated nonce there loses the key.
const NONCE: usize = 24;

/// The key file sits beside the database.
///
/// On Windows its contents are useless anywhere else, so this is only a
/// convenience. Elsewhere it is the key itself at mode 0600, which means
/// copying the profile directory copies the key with it - the honest summary
/// being that the file protects the database from other *accounts*, not from
/// someone who takes the whole folder.
pub fn key_path(env: &Environment) -> PathBuf {
    env.get_application_data_path().join("NanoTorrent.key")
}

/// A fresh 256-bit key.
///
/// Used as the AEAD key directly rather than through a KDF: the bytes are
/// already uniformly random, so stretching them would add cost without adding
/// entropy. Deriving from a typed password is the case that needs a KDF, and
/// that is the mode not implemented here.
fn fresh_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    rand::fill(&mut key[..]);
    key
}

/// Does this look like a sealed database rather than a plain one?
pub fn is_sealed(head: &[u8]) -> bool {
    head.starts_with(MAGIC)
}

/// Wrap a plain SQLite image for storage.
///
/// Layout is `MAGIC | nonce | ciphertext+tag`. The magic is passed as
/// associated data as well as written out, so a file cannot be re-labelled as
/// some later format and still authenticate.
pub fn seal(key: &[u8; 32], plain: &[u8]) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; NONCE];
    rand::fill(&mut nonce[..]);

    let sealed = cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: plain,
                aad: MAGIC,
            },
        )
        .map_err(|_| anyhow::anyhow!("the database could not be encrypted"))?;

    let mut out = Vec::with_capacity(MAGIC.len() + NONCE + sealed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Unwrap a sealed database, failing if anything about it has been altered.
pub fn unseal(key: &[u8; 32], file: &[u8]) -> Result<Vec<u8>> {
    let head = MAGIC.len() + NONCE;
    if !is_sealed(file) || file.len() < head {
        anyhow::bail!("this is not a sealed NanoTorrent database");
    }
    let nonce: [u8; NONCE] = file[MAGIC.len()..head]
        .try_into()
        .expect("the slice is NONCE long");

    XChaCha20Poly1305::new(key.into())
        .decrypt(
            (&nonce).into(),
            Payload {
                msg: &file[head..],
                aad: MAGIC,
            },
        )
        // No detail beyond this on purpose: a wrong key and an edited file are
        // the same failure, and which one it was is not something the tag can
        // say.
        .map_err(|_| anyhow::anyhow!("the database could not be read with this key"))
}

/// Read the stored key, or `None` when there is no key file.
pub fn load(env: &Environment) -> Result<Option<[u8; 32]>> {
    let path = key_path(env);
    if !path.exists() {
        return Ok(None);
    }
    let stored = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let key = unprotect(&stored).with_context(|| {
        format!(
            "the key in {} could not be read. On Windows a key is bound to the \
             account that created it, so a copied profile cannot be opened by \
             another user.",
            path.display()
        )
    })?;
    Ok(Some(key))
}

/// Create a key and store it, replacing any existing one.
pub fn create(env: &Environment) -> Result<[u8; 32]> {
    let key = fresh_key();
    let path = key_path(env);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_private(&path, &protect(&key)?)?;
    Ok(key)
}

/// Remove the stored key. Used when encryption is switched off, after the
/// database itself has been rewritten in the clear - never before, or the
/// database becomes unreadable.
pub fn forget(env: &Environment) -> Result<()> {
    let path = key_path(env);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

// --- platform key storage ---------------------------------------------------

/// Write with an owner-only mode from the moment the file exists.
///
/// Created with the mode rather than chmod'd afterwards: between the two there
/// is a window in which the key is world-readable, and a key that was briefly
/// readable is a key that leaked.
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    // Windows has no mode bits to set here. What stands in for them is DPAPI:
    // the bytes below are already ciphertext that only this account can undo,
    // so the file's ACL is not what is protecting anything.
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

/// Windows: hand the key to DPAPI, which ties it to this user account.
#[cfg(windows)]
fn protect(key: &[u8; 32]) -> Result<Vec<u8>> {
    dpapi(key, true)
}

#[cfg(windows)]
fn unprotect(blob: &[u8]) -> Result<[u8; 32]> {
    let out = dpapi(blob, false)?;
    let key: [u8; 32] = out
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("stored key is {} bytes, expected 32", out.len()))?;
    Ok(key)
}

/// `CryptProtectData` / `CryptUnprotectData` behind one shape, since they take
/// and return the same blob type and differ only in direction.
#[cfg(windows)]
fn dpapi(input: &[u8], protecting: bool) -> Result<Vec<u8>> {
    use winapi::shared::minwindef::DWORD;
    use winapi::um::dpapi::{CryptProtectData, CryptUnprotectData};
    use winapi::um::winbase::LocalFree;
    use winapi::um::wincrypt::DATA_BLOB;

    // The API takes a mutable pointer but does not write through it.
    let mut in_blob = DATA_BLOB {
        cbData: input.len() as DWORD,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut out_blob = DATA_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    let ok = unsafe {
        if protecting {
            CryptProtectData(
                &mut in_blob,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                &mut out_blob,
            )
        } else {
            CryptUnprotectData(
                &mut in_blob,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                &mut out_blob,
            )
        }
    };

    if ok == 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!(
            "DPAPI {} failed: {err}",
            if protecting { "protect" } else { "unprotect" }
        );
    }

    // Copied out before the free: the blob belongs to the API, not to us.
    let out =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec();
    unsafe { LocalFree(out_blob.pbData as *mut _) };
    Ok(out)
}

/// Everywhere else the file mode is the protection, so the key is stored as
/// itself.
#[cfg(not(windows))]
fn protect(key: &[u8; 32]) -> Result<Vec<u8>> {
    Ok(key.to_vec())
}

#[cfg(not(windows))]
fn unprotect(blob: &[u8]) -> Result<[u8; 32]> {
    blob.try_into()
        .map_err(|_| anyhow::anyhow!("stored key is {} bytes, expected 32", blob.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_keys_are_not_the_same_key() {
        assert_ne!(fresh_key(), fresh_key());
    }

    /// Whatever the platform does to a key, it has to come back unchanged.
    #[test]
    fn a_protected_key_survives_the_round_trip() {
        let key = fresh_key();
        let blob = protect(&key).expect("protect");
        assert_eq!(unprotect(&blob).expect("unprotect"), key);
    }

    /// On Windows the stored form must not be the key in the clear - that is
    /// the whole contribution DPAPI makes here.
    #[cfg(windows)]
    #[test]
    fn the_stored_form_is_not_the_key_itself() {
        let key = fresh_key();
        let blob = protect(&key).expect("protect");
        assert!(blob.len() > key.len(), "DPAPI blob should carry a header");
        assert!(
            !blob.windows(32).any(|w| w == key),
            "the key appears verbatim in what was written to disk"
        );
    }

    #[test]
    fn a_sealed_database_comes_back_byte_for_byte() {
        let key = fresh_key();
        let plain = b"SQLite format 3\0and then some pages".to_vec();
        let sealed = seal(&key, &plain).expect("seal");

        assert!(is_sealed(&sealed));
        assert!(
            !sealed.windows(6).any(|w| w == b"format"),
            "the plaintext is still visible in the sealed file"
        );
        assert_eq!(unseal(&key, &sealed).expect("unseal"), plain);
    }

    /// Two seals of the same database must not produce the same file - the
    /// nonce is what makes that true, and a constant one would leak that
    /// nothing had changed between two backups.
    #[test]
    fn sealing_twice_does_not_repeat_itself() {
        let key = fresh_key();
        let plain = b"the same bytes".to_vec();
        assert_ne!(seal(&key, &plain).unwrap(), seal(&key, &plain).unwrap());
    }

    /// The whole justification for the feature: an edit made without the key
    /// is refused rather than quietly accepted.
    ///
    /// Every byte after the magic is covered, so unlike a per-page MAC there is
    /// no part of the file that can be altered unnoticed - including parts the
    /// database would never read back.
    #[test]
    fn editing_a_sealed_database_is_detected_rather_than_accepted() {
        let key = fresh_key();
        let sealed = seal(&key, &vec![7u8; 4096]).expect("seal");

        for at in [MAGIC.len(), MAGIC.len() + NONCE, sealed.len() - 1] {
            let mut edited = sealed.clone();
            edited[at] ^= 0xFF;
            assert!(
                unseal(&key, &edited).is_err(),
                "a byte was changed at {at} and the file opened anyway"
            );
        }

        assert!(
            unseal(&fresh_key(), &sealed).is_err(),
            "a different key opened the database"
        );
    }
}
