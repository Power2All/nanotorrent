/* The web server's tuning settings go: they have been constants since 0.4.0.

   20260828000000 made eight actix knobs settings for one release; the next
   one turned them back into constants in webui::mod.rs, where the reason is
   written down - nobody running a torrent client has a reason to move a worker
   count or a handshake rate. The rows stayed behind. Nothing has read them
   since, but a settings export still carried them and a PicoTorrent import
   still counted them as settings this build has.

   A build from before 0.4.0 that opens this database finds them missing and
   uses its own defaults, which are the same values. */

DELETE FROM setting WHERE key IN (
    'webui.client_request_timeout',
    'webui.client_disconnect_timeout',
    'webui.keep_alive',
    'webui.max_connections',
    'webui.max_connection_rate',
    'webui.workers',
    'webui.shutdown_timeout',
    'webui.max_body_size'
);
