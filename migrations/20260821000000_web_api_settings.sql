/* Settings for the optional web interface (src/webui).

   Defaults are deliberately the safe end of every choice, because this is the
   first feature that can expose the client to anything outside the machine:

   - disabled, so nothing listens until it is asked to;
   - bound to loopback, so enabling it does not immediately publish to the LAN;
   - self-signed TLS, so the first connection is encrypted without anyone having
     to obtain a certificate first;
   - an EMPTY password hash, which the server treats as "not configured" and
     refuses to start on. There is no default password to forget to change, and
     no unauthenticated window - /api/fs browses the filesystem and
     add_torrent/move_storage write to it, so an open port would be a remote
     file manager.

   port 8443 is the conventional unprivileged HTTPS alternate, and avoids the
   ports the other clients grabbed (8080 qBittorrent, 9091 Transmission). */
INSERT INTO setting (key, value, default_value) VALUES
('webui.enabled',        NULL, 'false'),
('webui.bind_address',   NULL, '"127.0.0.1"'),
('webui.port',           NULL, '8443'),
('webui.username',       NULL, '"nanotorrent"'),
('webui.password_hash',  NULL, '""'),
/* off | self-signed | custom */
('webui.tls_mode',       NULL, '"self-signed"'),
('webui.tls_cert_path',  NULL, '""'),
('webui.tls_key_path',   NULL, '""');
