/* GeoIP support returns (peer countries in the Peers tab). The original
   20200912230012 migration removed these settings when the C++ client
   dropped GeoIP - reinstate them with JSON-encoded defaults. The original
   picotorrent.org database URL is dead; default to the DB-IP free country
   database ({YYYY-MM} is expanded to the current month at download time). */
INSERT INTO setting (key, value, default_value) VALUES
('geoip.enabled',      NULL, 'true'),
('geoip.database_url', NULL, '"https://download.db-ip.com/free/dbip-country-lite-{YYYY-MM}.mmdb.gz"');
