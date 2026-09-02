/* The Rhai plugin host.

   Off by default, unlike every other feature toggle added here: a plugin is
   arbitrary code running with the client's own reach over the session. Turning
   that on is a decision the user makes deliberately, not one an upgrade makes
   quietly on their behalf. */

INSERT INTO setting (key, value, default_value)
VALUES ('plugins.enabled', NULL, 'false');
