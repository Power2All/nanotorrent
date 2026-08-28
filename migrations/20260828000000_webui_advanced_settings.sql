/* Actix/HTTP tuning for the web interface - the "Advanced" group.

   These were literals inside webui::build(). They are settings now because the
   right value depends on the deployment: a client reachable over a slow link
   needs a longer request timeout than the 5s slowloris guard allows, and one
   sitting behind a reverse proxy wants a different keep-alive from the proxy's.

   The defaults are the values that were hardcoded, so an existing install
   behaves exactly as it did before this migration ran. Every one of them is
   also clamped on load (see WebConfig::load), because a nonsense value here
   should fall back rather than take the listener down.

   Seconds for the timeouts, whole megabytes for the body limit - the units the
   Preferences fields are labelled with, so what is stored is what was typed. */
INSERT INTO setting (key, value, default_value) VALUES
/* Slowloris guard: how long a client may take to send its request headers. */
('webui.client_request_timeout',    NULL, '5'),
/* How long a client that has stopped reading may hold its worker slot. */
('webui.client_disconnect_timeout', NULL, '5'),
/* 0 disables keep-alive entirely, which actix reads as "close every time". */
('webui.keep_alive',                NULL, '30'),
('webui.max_connections',           NULL, '256'),
/* TLS handshakes in flight - the expensive half of a connection flood. */
('webui.max_connection_rate',       NULL, '64'),
/* Serving one person, not a load test; not one-per-core. */
('webui.workers',                   NULL, '2'),
('webui.shutdown_timeout',          NULL, '5'),
/* Megabytes. A .torrent with thousands of files is a few MB once base64'd. */
('webui.max_body_size',             NULL, '8');
