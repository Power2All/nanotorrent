/* Keep a torrent's uploaded byte count across session rebuilds.

   Ratio was computed straight from librqbit's live counters, which start again
   at zero every time the engine is rebuilt - and `apply_settings` rebuilds it
   for any preference change at all, including Queue and seeding. Changing an
   unrelated setting therefore reset every torrent's ratio to 0.00, and so did
   a restart.

   Only the upload side is stored. The denominator is `progress_bytes`, which
   is how much of the torrent is on disk: state the engine recomputes from the
   files, not a counter that can be lost. Storing that as well would mean
   keeping a running total that a re-check could legitimately contradict.

   Existing rows start at zero. There is nowhere to recover a real figure from
   - the counts that would have told us are exactly the ones that were being
   discarded - so the totals begin accumulating from the upgrade rather than
   pretending to a history they do not have. */

ALTER TABLE torrent ADD COLUMN uploaded_bytes INTEGER NOT NULL DEFAULT 0;
