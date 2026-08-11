//! Register NanoTorrent as the handler for `.torrent` files and `magnet:`
//! links. Everything is written under `HKEY_CURRENT_USER\Software\Classes`, so
//! no administrator rights are needed.
//!
//! Note: on Windows 10/11 an app can register itself but cannot silently steal
//! the *default* when the user has already chosen one (that choice is hash-
//! protected). After registering we notify the shell; if Windows doesn't
//! switch automatically the user confirms it once via "Open with" or Settings.

/// ProgID we register the `.torrent` association under.
#[cfg(windows)]
const PROGID: &str = "NanoTorrent.Torrent";

/// ProgID for the `magnet:` URL protocol.
#[cfg(windows)]
const MAGNET_PROGID: &str = "NanoTorrent.Magnet";

/// Application name for the RegisteredApplications / Capabilities entry.
#[cfg(windows)]
const APP_NAME: &str = "NanoTorrent";

#[cfg(windows)]
#[link(name = "shell32")]
unsafe extern "system" {
    fn SHChangeNotify(
        w_event_id: i32,
        u_flags: u32,
        dw_item1: *const core::ffi::c_void,
        dw_item2: *const core::ffi::c_void,
    );
}

#[cfg(windows)]
pub fn register_torrent() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let exe = exe.to_string_lossy();
    let command = format!("\"{exe}\" \"%1\"");
    let icon = format!("\"{exe}\",0");

    let progid = format!("Software\\Classes\\{PROGID}");
    set_string(&progid, None, "BitTorrent Document")?;
    set_string(&format!("{progid}\\DefaultIcon"), None, &icon)?;
    set_string(&format!("{progid}\\shell\\open\\command"), None, &command)?;

    // .torrent -> our ProgID.
    set_string("Software\\Classes\\.torrent", None, PROGID)?;
    set_string(
        "Software\\Classes\\.torrent",
        Some("Content Type"),
        "application/x-bittorrent",
    )?;
    set_string(
        "Software\\Classes\\.torrent\\OpenWithProgids",
        Some(PROGID),
        "",
    )?;

    // magnet: ProgID (a proper URL-protocol handler, mirroring the .torrent
    // ProgID). A protocol needs its own ProgID + URLAssociations to be OFFERED
    // as a default - the bare `magnet` key below is not enough on Win10/11.
    let magnet = format!("Software\\Classes\\{MAGNET_PROGID}");
    set_string(&magnet, None, "Magnet URI")?;
    set_string(&magnet, Some("URL Protocol"), "")?;
    set_string(&format!("{magnet}\\DefaultIcon"), None, &icon)?;
    set_string(&format!("{magnet}\\shell\\open\\command"), None, &command)?;

    // The `magnet` protocol key itself (used when no UserChoice exists yet).
    set_string("Software\\Classes\\magnet", None, "URL:magnet protocol")?;
    set_string("Software\\Classes\\magnet", Some("URL Protocol"), "")?;
    set_string(
        "Software\\Classes\\magnet\\shell\\open\\command",
        None,
        &command,
    )?;

    // Register NanoTorrent as an application with capabilities, so Windows lists
    // it under Settings > Default apps and OFFERS it for BOTH .torrent (a file
    // type) and magnet (a URL protocol). Without the URLAssociations entry the
    // magnet protocol is never offered - the reason .torrent could be set as
    // default but magnet could not.
    let caps = format!("Software\\{APP_NAME}\\Capabilities");
    set_string(&caps, Some("ApplicationName"), APP_NAME)?;
    set_string(
        &caps,
        Some("ApplicationDescription"),
        "A tiny, hackable BitTorrent client.",
    )?;
    set_string(&format!("{caps}\\FileAssociations"), Some(".torrent"), PROGID)?;
    set_string(
        &format!("{caps}\\URLAssociations"),
        Some("magnet"),
        MAGNET_PROGID,
    )?;
    set_string("Software\\RegisteredApplications", Some(APP_NAME), &caps)?;

    // Tell the shell associations changed so Settings > Default apps reflects
    // the new registration without needing a sign-out.
    const SHCNE_ASSOCCHANGED: i32 = 0x0800_0000;
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, 0, std::ptr::null(), std::ptr::null());
    }

    Ok(())
}

#[cfg(not(windows))]
pub fn register_torrent() -> anyhow::Result<()> {
    anyhow::bail!("file association is only supported on Windows")
}

/// Create `HKCU\<subkey>` if needed and set a REG_SZ value (`None` name = the
/// key's default value).
#[cfg(windows)]
pub(crate) fn set_string(subkey: &str, value_name: Option<&str>, data: &str) -> anyhow::Result<()> {
    use std::ptr::{null, null_mut};
    use winapi::um::winnt::{KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ};
    use winapi::um::winreg::{HKEY_CURRENT_USER, RegCloseKey, RegCreateKeyExW, RegSetValueExW};

    let wsub = wide(subkey);
    let mut hkey = null_mut();
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wsub.as_ptr(),
            0,
            null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            null_mut(),
            &mut hkey,
            null_mut(),
        )
    };
    if rc != 0 {
        anyhow::bail!("RegCreateKeyExW({subkey}) failed: {rc}");
    }

    let wname = value_name.map(wide);
    let wdata = wide(data);
    let name_ptr = wname.as_ref().map_or(null(), |w| w.as_ptr());
    // Byte length includes the trailing NUL that `wide` appends.
    let rc = unsafe {
        RegSetValueExW(
            hkey,
            name_ptr,
            0,
            REG_SZ,
            wdata.as_ptr() as *const u8,
            (wdata.len() * 2) as u32,
        )
    };
    unsafe {
        RegCloseKey(hkey);
    }
    if rc != 0 {
        anyhow::bail!("RegSetValueExW({subkey}) failed: {rc}");
    }
    Ok(())
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
