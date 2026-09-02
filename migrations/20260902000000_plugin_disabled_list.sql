/* Per-plugin on/off, as a newline-separated list of the plugins that are OFF.
   Newline, not comma: a comma is legal in a filename.

   A disabled-list rather than an enabled-list, so a script dropped into the
   folder runs without a second deliberate act somewhere else. The deliberate
   act is already `plugins.enabled`, which is off by default and gates the lot;
   an enabled-list would silently ignore every new file and look like a bug.

   Names are the file stem, which is what the host already logs and what the
   Preferences tab shows.

   NOTE: this file is left exactly as it was first applied. `plugins.grants`
   and the default_value correction live in 20260902000001, because editing an
   already-applied migration changes nothing for a database that has run it -
   which is how Approve silently did nothing on a profile created in between. */

INSERT INTO setting (key, value, default_value)
VALUES ('plugins.disabled', NULL, '');
