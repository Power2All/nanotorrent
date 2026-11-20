/* The switch for the qBittorrent-compatible Web API under /api/v2.

   Off by default. It is a second way into the web interface - a login form
   and a session cookie beside the Basic auth everything else uses - and a
   second API to keep answering, so it is there for whoever wants Sonarr,
   Radarr and the like to drive NanoTorrent, and absent for everyone else. It
   only does anything while the web interface itself is on. */

INSERT INTO setting (key, value, default_value)
SELECT 'webui.qbittorrent_api', NULL, 'false'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'webui.qbittorrent_api');
