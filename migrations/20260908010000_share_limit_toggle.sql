/* An explicit switch for the share limits.

   `libtorrent.share_ratio_limit` is inherited from PicoTorrent with a default
   of 200 - ratio 2.00 - and until now nothing read it. Wiring it up without
   this toggle would mean every upgraded profile quietly started stopping its
   torrents at ratio 2, which nobody asked for and which looks like a bug from
   the outside.

   So the limits are off until someone turns them on, and the inherited 200
   becomes what the box is pre-filled with rather than what is enforced. That
   also matches how the setting reads elsewhere, where each limit has its own
   checkbox. */

INSERT INTO setting (key, value, default_value)
SELECT 'queue.share_limit_enabled', NULL, 'false'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'queue.share_limit_enabled');
