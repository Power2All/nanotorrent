/* Transfer management: the things somebody coming from a full-featured client
   reaches for first.

   One migration for nine features, because they are one story - what happens
   to a torrent without anybody watching it - and splitting them would mean
   nine files whose only difference is which row they insert.

   Several keys are NOT here because PicoTorrent already carried them and the
   port inherited them unused: move_completed_downloads,
   move_completed_downloads_path, move_completed_downloads_from_default_only
   and libtorrent.share_ratio_limit all exist from the original schema. They
   are wired up rather than re-added; adding them again would give two rows
   answering the same question. */

/* --- Share limits -------------------------------------------------------

   libtorrent.share_ratio_limit already exists and is what "stop at ratio X"
   reads. What it never had is a *time* limit in minutes (libtorrent's own
   seed_time_ratio_limit is another ratio, not a duration) and, more
   importantly, an action: a limit that only logs is not a limit.

   -1 means "no limit" throughout, matching the convention the inherited
   libtorrent keys already use. */
INSERT INTO setting (key, value, default_value)
SELECT 'queue.seed_time_limit', NULL, '-1'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'queue.seed_time_limit');

/* pause | remove | remove_with_data. Pause is the default deliberately: the
   other two destroy something, and a limit reached by accident should not. */
INSERT INTO setting (key, value, default_value)
SELECT 'queue.share_limit_action', NULL, '"pause"'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'queue.share_limit_action');

/* --- Watched folder ------------------------------------------------------

   A directory that is scanned for .torrent files, which are added and then
   moved aside so the same file is not added twice. Moving aside rather than
   deleting: the file is the user's, and a scanner that eats input is one bad
   path away from deleting a folder of torrents somebody was keeping. */
INSERT INTO setting (key, value, default_value)
SELECT 'watch.enabled', NULL, 'false'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'watch.enabled');

INSERT INTO setting (key, value, default_value)
SELECT 'watch.path', NULL, '""'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'watch.path');

/* Started, or added paused for someone to look at first. */
INSERT INTO setting (key, value, default_value)
SELECT 'watch.start', NULL, 'true'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'watch.start');

/* The label every torrent from the watched folder gets, or -1 for none. */
INSERT INTO setting (key, value, default_value)
SELECT 'watch.label_id', NULL, '-1'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'watch.label_id');

/* --- Incomplete folder ---------------------------------------------------

   Download here, move to the real save path on completion. Distinct from
   move_completed_downloads, which moves *out* of the save path afterwards;
   this one keeps partial files off the destination disk entirely, which is
   what people use it for (a fast scratch SSD, or keeping a NAS free of
   half-written files). */
INSERT INTO setting (key, value, default_value)
SELECT 'downloads.incomplete_enabled', NULL, 'false'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'downloads.incomplete_enabled');

INSERT INTO setting (key, value, default_value)
SELECT 'downloads.incomplete_path', NULL, '""'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'downloads.incomplete_path');

/* --- Alternative speed limits -------------------------------------------

   A second pair of rate limits and a switch, plus a schedule that throws the
   switch. Kept separate from libtorrent.download_rate_limit so that turning
   the alternative limits off restores exactly what was configured before,
   with no need to remember the previous numbers. 0 means unlimited, which is
   what the existing rate limit keys mean. */
INSERT INTO setting (key, value, default_value)
SELECT 'speed.alt_enabled', NULL, 'false'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'speed.alt_enabled');

INSERT INTO setting (key, value, default_value)
SELECT 'speed.alt_download_rate', NULL, '0'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'speed.alt_download_rate');

INSERT INTO setting (key, value, default_value)
SELECT 'speed.alt_upload_rate', NULL, '0'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'speed.alt_upload_rate');

INSERT INTO setting (key, value, default_value)
SELECT 'speed.schedule_enabled', NULL, 'false'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'speed.schedule_enabled');

/* Minutes since midnight, local time. Stored as minutes rather than "HH:MM"
   so comparing them is arithmetic instead of parsing, and so a schedule that
   wraps past midnight (from 1380 to 360) is one comparison either way. */
INSERT INTO setting (key, value, default_value)
SELECT 'speed.schedule_from', NULL, '480'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'speed.schedule_from');

INSERT INTO setting (key, value, default_value)
SELECT 'speed.schedule_to', NULL, '1320'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'speed.schedule_to');

/* Bit 0 = Monday .. bit 6 = Sunday. 127 is every day. */
INSERT INTO setting (key, value, default_value)
SELECT 'speed.schedule_days', NULL, '127'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'speed.schedule_days');

/* --- Per-torrent overrides ----------------------------------------------

   NULL in every one of these columns means "follow the global setting", which
   is why they are nullable rather than defaulted. A torrent that has never
   been touched individually must keep tracking the preference when it
   changes, and a 0 or a -1 cannot say that without also being a value someone
   might legitimately want. */
/* sequential / first_last were added here too and dropped again in
   20260908020000 - librqbit already downloads in that order and has no other
   mode, so there was nothing for them to hold. Left in place rather than
   edited out: a migration that has run somewhere must never change. */
ALTER TABLE torrent ADD COLUMN sequential       INTEGER;
ALTER TABLE torrent ADD COLUMN first_last       INTEGER;
ALTER TABLE torrent ADD COLUMN ratio_limit      REAL;
ALTER TABLE torrent ADD COLUMN seed_time_limit  INTEGER;

/* --- Tags ---------------------------------------------------------------

   Labels are one-per-torrent with a colour and a save path; tags are many per
   torrent and carry nothing else. Both are worth having for the same reason,
   and collapsing them would lose the part that makes tags useful - that a
   torrent can be in several at once. */
CREATE TABLE IF NOT EXISTS tag (
    id   INTEGER PRIMARY KEY,
    name TEXT    NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS torrent_tag (
    info_hash TEXT    NOT NULL REFERENCES torrent(info_hash) ON DELETE CASCADE,
    tag_id    INTEGER NOT NULL REFERENCES tag(id) ON DELETE CASCADE,
    PRIMARY KEY (info_hash, tag_id)
);

/* --- File priorities ----------------------------------------------------

   0 = skip, 1 = normal, 2 = high, 3 = maximum. Only rows that differ from
   normal are stored, so a torrent nobody has fiddled with costs nothing.

   Skip is in the same scale rather than beside it because that is what the
   engine already understands: a file at 0 is left out of the download, which
   is exactly the include toggle the Files tab has always had. */
CREATE TABLE IF NOT EXISTS torrent_file_priority (
    info_hash  TEXT    NOT NULL REFERENCES torrent(info_hash) ON DELETE CASCADE,
    file_index INTEGER NOT NULL,
    priority   INTEGER NOT NULL,
    PRIMARY KEY (info_hash, file_index)
);
