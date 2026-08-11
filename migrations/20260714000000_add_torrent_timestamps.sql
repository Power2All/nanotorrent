/* Rust port: the C++ version stored added/completed timestamps inside the
   libtorrent resume data blob. librqbit keeps resume state in its own JSON
   session folder, so these live as columns on the torrent table instead. */
ALTER TABLE torrent ADD COLUMN added_on     INTEGER;
ALTER TABLE torrent ADD COLUMN completed_on INTEGER;
