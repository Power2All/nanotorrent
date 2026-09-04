#[cfg(test)]
pub(crate) mod tests;

use crate::Error;
use std::{ffi::CString, net::IpAddr, num::NonZeroU32, str::FromStr};
// NanoTorrent: only the Windows arm keeps addresses, so importing these
// unconditionally is an unused-import warning on every other platform.
#[cfg(windows)]
use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone)]
pub struct BindDevice {
    #[allow(unused)]
    index: NonZeroU32,
    #[allow(unused)]
    name: CString,
    // NanoTorrent: Windows has no SO_BINDTODEVICE, so the device is carried as
    // the addresses it owns and applied by binding them. See `bind_ip`.
    #[cfg(windows)]
    v4: Option<Ipv4Addr>,
    #[cfg(windows)]
    v6: Option<Ipv6Addr>,
}

impl BindDevice {
    #[cfg(not(windows))]
    pub fn new_from_name(name: &str) -> crate::Result<Self> {
        let name = CString::new(name).map_err(|_| Error::BindDeviceInvalid)?;

        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        let index = NonZeroU32::new(index)
            .ok_or_else(|| Error::BindDeviceInvalidError(std::io::Error::last_os_error()))?;
        Ok(Self { index, name })
    }

    // NanoTorrent: was a stub returning BindDeviceNotSupported. Windows cannot
    // bind by device, but it can bind by address, and under the strong host
    // model that confines the socket to the interface owning the address.
    //
    // A routable IPv4 address is required. Link-local (169.254/16) is what an
    // adapter has when it has nothing, and binding it would produce a socket
    // that cannot reach anything while looking like it worked.
    #[cfg(windows)]
    pub fn new_from_name(name: &str) -> crate::Result<Self> {
        use network_interface::{NetworkInterface, NetworkInterfaceConfig};

        let cname = CString::new(name).map_err(|_| Error::BindDeviceInvalid)?;
        let interfaces = NetworkInterface::show().map_err(|_| Error::BindDeviceInvalid)?;

        let mut index = None;
        let mut v4 = None;
        let mut v6 = None;

        // Matched case-insensitively: Windows adapter names are shown to
        // people with capitals nobody retypes exactly.
        for itf in interfaces
            .iter()
            .filter(|i| i.name.eq_ignore_ascii_case(name))
        {
            index = index.or(NonZeroU32::new(itf.index));
            for addr in &itf.addr {
                match addr {
                    network_interface::Addr::V4(a)
                        if v4.is_none()
                            && !a.ip.is_loopback()
                            && !a.ip.is_link_local() =>
                    {
                        v4 = Some(a.ip)
                    }
                    network_interface::Addr::V6(a)
                        if v6.is_none()
                            && !a.ip.is_loopback()
                            && (a.ip.segments()[0] & 0xffc0) != 0xfe80 =>
                    {
                        v6 = Some(a.ip)
                    }
                    _ => {}
                }
            }
        }

        let index = index.ok_or(Error::BindDeviceInvalid)?;
        if v4.is_none() {
            // Refuse rather than fall back to an unbound socket: the caller
            // asked for its traffic to be confined, and a half-applied
            // confinement is the state this whole feature exists to avoid.
            return Err(Error::BindDeviceInvalid);
        }

        Ok(Self {
            index,
            name: cname,
            v4,
            v6,
        })
    }

    /// The address a socket must bind to for its traffic to leave by this
    /// device, on platforms with no bind-to-device call.
    ///
    /// `None` where `bind_sref` already does the work, and `None` for a family
    /// this device has no address in - callers must treat that as a refusal
    /// rather than binding the unspecified address, which would leave the
    /// socket free to route anywhere.
    #[cfg(windows)]
    pub fn bind_ip(&self, is_v6: bool) -> Option<IpAddr> {
        if is_v6 {
            self.v6.map(IpAddr::V6)
        } else {
            self.v4.map(IpAddr::V4)
        }
    }

    #[cfg(not(windows))]
    pub fn bind_ip(&self, _is_v6: bool) -> Option<IpAddr> {
        None
    }

    pub fn index(&self) -> NonZeroU32 {
        self.index
    }

    pub fn name(&self) -> &str {
        // We constructed from a string so this can't fail
        unsafe { std::str::from_utf8_unchecked(self.name.to_bytes()) }
    }

    #[cfg(target_os = "macos")]
    pub fn bind_sref(&self, sref: &socket2::Socket, is_v6: bool) -> crate::Result<()> {
        if is_v6 {
            sref.bind_device_by_index_v6(Some(self.index))
                .map_err(Error::BindDeviceSetDeviceError)
        } else {
            sref.bind_device_by_index_v4(Some(self.index))
                .map_err(Error::BindDeviceSetDeviceError)
        }
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn bind_sref(&self, sref: &socket2::Socket, _is_v6: bool) -> crate::Result<()> {
        let name = self.name.as_bytes_with_nul();
        sref.bind_device(Some(name))
            .map_err(Error::BindDeviceSetDeviceError)
    }

    // NanoTorrent: a no-op on Windows rather than an error. The confinement is
    // applied by `bind_ip` at the point the socket is bound, because both call
    // sites bind the socket themselves immediately after this and a bind here
    // would collide with theirs.
    #[cfg(windows)]
    pub fn bind_sref(&self, _sref: &socket2::Socket, _is_v6: bool) -> crate::Result<()> {
        Ok(())
    }
}

impl FromStr for BindDevice {
    type Err = crate::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new_from_name(s)
    }
}
