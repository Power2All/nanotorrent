/* Which tier each stored tracker belongs to.

   A torrent's announce list is a list of TIERS, not a flat list: every tracker
   in a tier is tried before the next tier is reached (BEP 12). Storing only the
   URLs threw that away - an edited torrent came back with everything in one
   group, which changes the fallback order the torrent asked for.

   0 is the first tier, matching how the tab already numbers them. Existing
   rows were all additions, which the engine placed after the file's own tiers,
   so a default of 0 would put them first and quietly reorder them. They get a
   tier of their own instead, which is where they already were. */

ALTER TABLE torrent_tracker ADD COLUMN tier INTEGER NOT NULL DEFAULT 0;

UPDATE torrent_tracker SET tier = 1;
