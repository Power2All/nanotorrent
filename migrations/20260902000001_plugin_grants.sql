/* What the user has approved each plugin to reach: a JSON object of plugin
   name -> permission tags. Consent is stored as the SET the script asked for,
   not a yes/no flag, so a script later edited to want more no longer matches
   what was approved and is held until it is approved again.

   Separate from 20260902000000 on purpose. That migration had already run on
   profiles created while this feature was being built, and a settings key is
   written with UPDATE - so a missing row makes `set` silently do nothing
   rather than fail. The symptom was the Approve button appearing dead.

   Guarded rather than a plain INSERT: a profile created after 20260902000000
   was (briefly) edited may already hold the row, and `key` is UNIQUE. */

INSERT INTO setting (key, value, default_value)
SELECT 'plugins.grants', NULL, '"{}"'
WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'plugins.grants');

/* '' is not valid JSON, so reading it yielded None and then an empty set - it
   worked, but by accident. Every other text setting stores '""'. */
UPDATE setting SET default_value = '""'
WHERE key = 'plugins.disabled' AND default_value = '';
