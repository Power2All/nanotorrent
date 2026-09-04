/* Brute-force lockout for the web interface.

   The limiter already existed with two numbers baked into the source: ten
   failures, five minutes, and the same five minutes served as both the window
   the failures were counted over and the time an address was refused. That
   coupling forced a bad trade - a long lockout meant a long memory for stray
   typos, and a short memory meant a short lockout - so the two are separate
   settings now.

   Defaults: five failures inside sixty seconds, then that address is refused
   for an hour. Strict on purpose. This guards one password on a machine whose
   owner can always reach it another way, so being locked out briefly costs
   them little, while being too permissive costs them the client.

   auth_max_failures = 0 switches the lockout off entirely. */

INSERT INTO setting (key, value, default_value) VALUES
('webui.auth_max_failures', NULL, '5'),
('webui.auth_window',       NULL, '60'),
('webui.auth_block',        NULL, '3600');
