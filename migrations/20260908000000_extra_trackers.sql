/* Trackers the user added to a torrent by hand.

   Only the ADDED ones. librqbit's custom-tracker option extends a torrent's
   own announce list rather than replacing it, so a tracker that came in the
   .torrent cannot be taken away without patching the engine - and a table that
   pretended otherwise would quietly fail to remove them. What is here is what
   this can honestly manage: the extras, which can be added and removed freely.

   Kept out of the `torrent` table because there are many per torrent, and out
   of librqbit's own session persistence because that is rewritten by the
   engine and is not ours to extend. */

CREATE TABLE IF NOT EXISTS torrent_tracker (
    info_hash TEXT NOT NULL REFERENCES torrent(info_hash) ON DELETE CASCADE,
    url       TEXT NOT NULL,
    PRIMARY KEY (info_hash, url)
);
