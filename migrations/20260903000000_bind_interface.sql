/* Bind every socket to one network interface.

   Empty by default, which is today's behaviour: the OS picks the source
   address by its routing table.

   Set to an interface name (WireGuard's `wg0`, OpenVPN's `tun0`, or the
   adapter name on Windows) and librqbit binds peers, trackers, DHT and uTP to
   it. That is stronger than routing torrent traffic through a SOCKS proxy,
   because it is enforced by the operating system rather than by each component
   remembering to ask: when the tunnel drops the interface disappears and every
   socket bound to it fails at once, instead of quietly falling back to the
   real address.

   Stored as a name rather than an address on purpose. A VPN's address changes
   between sessions; its interface name does not, so a stored address would
   silently stop matching and the binding would be lost exactly when it was
   most needed.

   NOT a kill switch on its own - this binds sockets, it does not police the
   process. See docs for what it does and does not promise. */

INSERT INTO setting (key, value, default_value)
VALUES ('network.bind_interface', NULL, '');
