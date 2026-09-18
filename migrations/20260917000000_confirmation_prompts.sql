/* Two switches for the confirmations that can now be answered once.

   Removing a torrent and editing a tracker both ask before they act. Both
   questions have a "do not ask again" box on them, and this is where that box
   writes its answer - so it can be found again, and turned back on, in
   Preferences rather than only by whoever ticked it.

   Default to asking. A confirmation that arrives switched off protects nobody,
   and these guard the two things in the window that are not undoable: a
   removal that can take the files with it, and a tracker edit that loses the
   URL it replaced. */

INSERT INTO setting (key, value, default_value)
SELECT 'ui.confirm_remove_torrent', NULL, 'true'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'ui.confirm_remove_torrent');

INSERT INTO setting (key, value, default_value)
SELECT 'ui.confirm_tracker_change', NULL, 'true'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'ui.confirm_tracker_change');
