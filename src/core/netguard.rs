//! Strict mode: stop rather than leak.
//!
//! Binding sockets to a VPN interface, or routing them through a proxy, is a
//! *preference* on its own - the parts that cannot honour it carry on
//! regardless, and a torrent client's loudest components are exactly the ones
//! that cannot. DHT and uTP are UDP and there is no proxy code in either;
//! local service discovery is LAN multicast; UPnP talks to the router. Leave
//! them running behind a SOCKS proxy and the client announces its real address
//! on every torrent while the settings screen says "proxy".
//!
//! Strict mode is the switch that says: if a component cannot be covered,
//! **do not run it**, and if the protection itself is missing, **do not
//! start**. A kill switch that covers five paths out of six is worse than
//! none, because people stop being careful.
//!
//! The decision is a pure function of the settings and a handful of facts
//! about the named interface. Enumerating interfaces is the part that needs a
//! real machine; deciding what to do about them is not, and is tested here.

/// What the user asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Intent {
    /// `network.strict`.
    pub strict: bool,
    /// `network.bind_interface`, already trimmed and emptied to None.
    pub bind_interface: Option<String>,
    /// Whether a SOCKS proxy is configured at all.
    pub proxy: bool,
}

/// What the named interface actually looks like on this machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Facts {
    pub exists: bool,
    /// Whether it carries a routable IPv4 address.
    ///
    /// Only interesting on Windows, which has no bind-to-device call: there
    /// the interface is applied by binding its own address, so an interface
    /// without one cannot be bound to at all.
    pub has_ipv4: bool,
    /// Whether it carries a routable IPv6 address.
    ///
    /// A v4-only tunnel on a machine with working IPv6 is the classic leak:
    /// the binding covers v4, and v6 sockets route around it through the
    /// ordinary interface.
    pub has_ipv6: bool,
}

/// What must be switched off for the traffic to stay where it was told.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Restrictions {
    pub disable_dht: bool,
    pub disable_utp: bool,
    pub disable_lsd: bool,
    pub disable_upnp: bool,
    pub ipv4_only: bool,
}

impl Restrictions {
    /// Whether anything at all is being held back, for the log line.
    pub fn any(&self) -> bool {
        self.disable_dht || self.disable_utp || self.disable_lsd || self.disable_upnp
            || self.ipv4_only
    }

    /// The switched-off components, for telling somebody what happened.
    pub fn describe(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.disable_dht {
            out.push("DHT");
        }
        if self.disable_utp {
            out.push("uTP");
        }
        if self.disable_lsd {
            out.push("local peer discovery");
        }
        if self.disable_upnp {
            out.push("UPnP port forwarding");
        }
        if self.ipv4_only {
            out.push("IPv6");
        }
        out
    }
}

/// The verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Not in strict mode. Run exactly as configured.
    Open,
    /// Run, with these held back.
    Restricted(Restrictions),
    /// Do not start, and say this to the user.
    Refuse(String),
}

/// Decide, from the settings and the facts about the named interface.
///
/// `facts` is `None` when no interface was named; it is `Some` with
/// `exists: false` when one was named and is not there - which is the case
/// worth refusing over, because it is what a dropped tunnel looks like.
pub fn decide(intent: &Intent, facts: Option<Facts>) -> Decision {
    if !intent.strict {
        return Decision::Open;
    }

    match (intent.bind_interface.as_deref(), facts) {
        // Named, present, but with no address to bind on a platform that binds
        // by address. Refused here rather than in the engine, which would
        // report it as an opaque bind failure at session start.
        (Some(name), Some(f)) if f.exists && cfg!(windows) && !f.has_ipv4 => {
            Decision::Refuse(format!(
                "the network interface \"{name}\" has no usable IPv4 address to bind to"
            ))
        }

        // Bound to an interface that is there. DHT and uTP are fine: they are
        // inside the tunnel like everything else. LSD and UPnP are not - both
        // talk to the local network, which the tunnel does not cover.
        (Some(_), Some(f)) if f.exists => Decision::Restricted(Restrictions {
            disable_dht: false,
            disable_utp: false,
            disable_lsd: true,
            disable_upnp: true,
            // Windows binds the interface's address rather than the device,
            // and one socket can only carry one address - a dualstack
            // listener pinned to a v6 address stops accepting v4 silently.
            // Sticking to v4 keeps it single-family and predictable, and
            // no v6 at all is no v6 leak.
            ipv4_only: cfg!(windows) || !f.has_ipv6,
            }),

        // Named, and missing. This is a dropped tunnel, a renamed adapter, or
        // a typo, and all three mean the same thing: the protection asked for
        // is not in place, so nothing should go out.
        (Some(name), _) => Decision::Refuse(format!(
            "strict mode is on and the network interface \"{name}\" is not present"
        )),

        // Proxy only. Everything the proxy cannot carry has to go, which is
        // most of what makes a torrent client noisy.
        (None, _) if intent.proxy => Decision::Restricted(Restrictions {
            disable_dht: true,
            disable_utp: true,
            disable_lsd: true,
            disable_upnp: true,
            // A SOCKS proxy is reached over one address family and this client
            // cannot verify which; refusing v6 keeps the answer simple.
            ipv4_only: true,
        }),

        // Strict, with nothing to be strict about. Starting would be ordinary
        // direct traffic under a setting that promises otherwise, which is the
        // single most misleading state available.
        (None, _) => Decision::Refuse(String::from(
            "strict mode is on but no proxy and no network interface are configured",
        )),
    }
}

/// Look up an interface by name on this machine.
///
/// Matched case-insensitively: Windows adapter names are shown to people with
/// capitals that nobody retypes exactly.
pub fn look_up(name: &str) -> Facts {
    use network_interface::{NetworkInterface, NetworkInterfaceConfig};

    let Ok(interfaces) = NetworkInterface::show() else {
        // Cannot enumerate: report the interface as absent. In strict mode
        // that refuses to start, which is the safe direction - the alternative
        // is assuming a tunnel is there because we failed to look.
        tracing::warn!("cannot enumerate network interfaces");
        return Facts::default();
    };

    let mut facts = Facts::default();
    for interface in interfaces.iter().filter(|i| i.name.eq_ignore_ascii_case(name)) {
        facts.exists = true;
        for addr in &interface.addr {
            // Loopback and link-local are not connectivity; they are what an
            // interface has when it has nothing. This must agree with
            // BindDevice::new_from_name in the engine, which applies the same
            // rule and would otherwise fail a bind we said was fine.
            match addr {
                network_interface::Addr::V4(v4) => {
                    if !v4.ip.is_loopback() && !v4.ip.is_link_local() {
                        facts.has_ipv4 = true;
                    }
                }
                network_interface::Addr::V6(v6) => {
                    if !v6.ip.is_loopback() && (v6.ip.segments()[0] & 0xffc0) != 0xfe80 {
                        facts.has_ipv6 = true;
                    }
                }
            }
        }
    }
    facts
}

/// Whether this is the moment the interface went missing.
///
/// True exactly once per disappearance: the caller pauses everything on that
/// edge, and nothing happens on the ticks either side of it. Recovery clears
/// the flag but is deliberately not an event - torrents paused by the kill
/// switch look exactly like torrents the user paused, and resuming them would
/// start traffic nobody asked to start.
pub fn just_lost(flag: &std::sync::atomic::AtomicBool, present: bool) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    !flag.swap(!present, Relaxed) && !present
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tunnel that is up and carries both families.
    fn live() -> Facts {
        Facts { exists: true, has_ipv4: true, has_ipv6: true }
    }

    fn strict(iface: Option<&str>, proxy: bool) -> Intent {
        Intent {
            strict: true,
            bind_interface: iface.map(str::to_owned),
            proxy,
        }
    }

    /// Off means off. Nothing is forced, nothing is refused.
    #[test]
    fn without_strict_mode_nothing_changes() {
        let intent = Intent { strict: false, bind_interface: Some("wg0".into()), proxy: true };
        assert_eq!(decide(&intent, Some(Facts::default())), Decision::Open);
    }

    /// The case the whole feature exists for: the tunnel is gone, so nothing
    /// goes out. Not "carry on without it".
    #[test]
    fn a_missing_interface_refuses_to_start() {
        let d = decide(&strict(Some("wg0"), false), Some(Facts::default()));
        match d {
            Decision::Refuse(why) => assert!(why.contains("wg0"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// Strict with nothing configured is the most misleading state there is:
    /// ordinary direct traffic under a setting that promises otherwise.
    #[test]
    fn strict_with_nothing_to_enforce_refuses() {
        assert!(matches!(decide(&strict(None, false), None), Decision::Refuse(_)));
    }

    /// Bound to a live tunnel: DHT and uTP ride inside it, so they stay. LSD
    /// and UPnP talk to the local network, which the tunnel does not cover.
    #[test]
    fn a_live_interface_keeps_dht_and_drops_the_lan_protocols() {
        let d = decide(&strict(Some("wg0"), false), Some(live()));
        let Decision::Restricted(r) = d else { panic!("expected restrictions") };

        assert!(!r.disable_dht, "DHT is inside the tunnel");
        assert!(!r.disable_utp, "uTP is inside the tunnel");
        assert!(r.disable_lsd, "LSD is LAN multicast and escapes");
        assert!(r.disable_upnp, "UPnP talks to the router and escapes");
        #[cfg(not(windows))]
        assert!(!r.ipv4_only, "the tunnel carries v6, so v6 is fine");
    }

    /// Windows binds the interface's address, so an interface without a usable
    /// one is refused up front rather than failing as an opaque bind error
    /// deep inside the engine.
    #[test]
    #[cfg(windows)]
    fn an_interface_with_no_v4_address_is_refused_on_windows() {
        let facts = Facts { exists: true, has_ipv4: false, has_ipv6: true };
        match decide(&strict(Some("wg0"), false), Some(facts)) {
            Decision::Refuse(why) => assert!(why.contains("IPv4"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// One socket carries one address, so a v6-pinned dualstack listener would
    /// stop accepting v4 without saying so. Windows stays single-family.
    #[test]
    #[cfg(windows)]
    fn windows_stays_v4_only_even_on_a_v6_capable_tunnel() {
        let Decision::Restricted(r) = decide(&strict(Some("wg0"), false), Some(live())) else {
            panic!("expected restrictions")
        };
        assert!(r.ipv4_only, "binding by address cannot be dualstack");
    }

    /// A v4-only tunnel on a machine with IPv6 is the classic leak: v6 sockets
    /// route around the binding through the ordinary interface.
    #[test]
    fn a_v4_only_tunnel_forces_v4_only() {
        let d = decide(
            &strict(Some("tun0"), false),
            Some(Facts { exists: true, has_ipv4: true, has_ipv6: false }),
        );
        let Decision::Restricted(r) = d else { panic!("expected restrictions") };
        assert!(r.ipv4_only, "a tunnel with no v6 must not leave v6 running");
    }

    /// A SOCKS proxy carries TCP. Everything else has to stop - which is most
    /// of what makes a torrent client noisy, and exactly what used to keep
    /// running while the settings screen said "proxy".
    #[test]
    fn proxy_only_stops_everything_it_cannot_carry() {
        let d = decide(&strict(None, true), None);
        let Decision::Restricted(r) = d else { panic!("expected restrictions") };

        assert!(r.disable_dht, "no proxy code exists in the DHT");
        assert!(r.disable_utp, "uTP is UDP");
        assert!(r.disable_lsd);
        assert!(r.disable_upnp);
        assert!(r.ipv4_only);
        assert_eq!(
            r.describe(),
            vec!["DHT", "uTP", "local peer discovery", "UPnP port forwarding", "IPv6"]
        );
    }

    /// An interface takes precedence over a proxy: it is the stronger of the
    /// two, and its restrictions are the looser ones.
    #[test]
    fn an_interface_wins_over_a_proxy() {
        let d = decide(&strict(Some("wg0"), true), Some(live()));
        let Decision::Restricted(r) = d else { panic!("expected restrictions") };
        assert!(!r.disable_dht, "bound to a tunnel, the DHT is covered");
    }

    /// The pause fires on the edge, once, and recovery is silent.
    #[test]
    fn the_kill_switch_fires_once_per_disappearance() {
        let flag = std::sync::atomic::AtomicBool::new(false);

        assert!(!just_lost(&flag, true), "nothing happens while it is there");
        assert!(just_lost(&flag, false), "gone: pause everything");
        assert!(!just_lost(&flag, false), "still gone: already paused");
        assert!(!just_lost(&flag, true), "back: nothing is resumed");
        assert!(just_lost(&flag, false), "gone again: pause again");
    }

    /// Enumerating a name that cannot exist reports absent rather than
    /// panicking - and absent is the safe answer.
    #[test]
    fn looking_up_a_missing_interface_is_not_an_error() {
        let facts = look_up("nanotorrent-no-such-interface");
        assert!(!facts.exists);
        assert!(!facts.has_ipv6);
    }
}

/// A malicious `.torrent` must not be able to write outside the save folder.
///
/// `TorrentMetaV1Info::validate` rejects `..` and any component containing a
/// separator, which is the traversal everyone tests for. It does not reject a
/// **drive prefix**, and on Windows `PathBuf::push` replaces the buffer when
/// the pushed path carries one - so a file named `C:evil.txt` passes
/// validation and then throws the save folder away, writing to the current
/// directory of drive C instead. Next to a portable install that is a
/// DLL-planting primitive, from nothing but a torrent someone was handed.
///
/// Engine patch 0015 (`safe_join` in librqbit's filesystem storage) refuses
/// any relative path whose components are not all `Component::Normal`. This
/// test drives a real session so the guard is exercised where it actually
/// sits, rather than re-implementing the rule here and testing the copy.
#[cfg(test)]
mod path_escape {
    /// A minimal multi-file info dict whose single file path is `component`.
    fn torrent_with_path(component: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"d4:infod5:filesl");
        b.extend_from_slice(
            format!("d6:lengthi1e4:pathl{}:{}ee", component.len(), component).as_bytes(),
        );
        b.extend_from_slice(b"e4:name4:root12:piece lengthi16384e6:pieces20:");
        b.extend_from_slice(&[0u8; 20]);
        b.extend_from_slice(b"ee");
        b
    }

    #[test]
    fn a_drive_relative_filename_cannot_escape_the_save_folder() {
        let dir = std::env::temp_dir().join(format!("nt-escape-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let added = rt.block_on(async {
            let session = librqbit::Session::new_with_opts(
                dir.clone(),
                librqbit::SessionOptions {
                    dht: None,
                    listen: None,
                    disable_trackers: true,
                    persistence: None,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

            session
                .add_torrent(
                    librqbit::AddTorrent::from_bytes(torrent_with_path("C:evil.txt")),
                    Some(librqbit::AddTorrentOptions {
                        overwrite: true,
                        ..Default::default()
                    }),
                )
                .await
                .map(|_| ())
        });

        let _ = std::fs::remove_dir_all(&dir);

        // Either the engine refuses the torrent outright, or - if some future
        // version accepts it - the file must at least have landed inside the
        // save folder. What must never happen is a write to `C:evil.txt`.
        assert!(
            added.is_err(),
            "a drive-relative torrent path was accepted: {added:?}"
        );
        assert!(
            !std::path::Path::new("C:evil.txt").exists(),
            "the save folder was escaped - C:evil.txt was created"
        );
    }
}
