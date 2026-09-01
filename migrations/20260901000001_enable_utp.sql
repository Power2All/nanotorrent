/* uTP (BEP 29) peer transport.

   Off by default. librqbit 9 integrates librqbit-utp and can listen and
   connect over it, but upstream still gates it behind an experimental flag of
   its own rather than defaulting it on, and it changes what the app puts on
   the network - a second socket, on UDP, on the same port. Defaulting to off
   keeps an upgrade from silently changing that.

   Worth turning on where TCP is throttled or blocked by a middlebox, which is
   most of what uTP exists for.

   `libtorrent.enable_lsd` needs no migration - it has existed since the
   PicoTorrent settings were imported, defaulting to true, and was stored but
   never applied until now. */

INSERT INTO setting (key, value, default_value)
VALUES ('libtorrent.enable_utp', NULL, 'false');
