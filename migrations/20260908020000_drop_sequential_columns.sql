/* Remove two columns that describe a feature which does not exist.

   `sequential` and `first_last` were added for a per-torrent "download in
   order" toggle. Reading librqbit's picker afterwards showed there is nothing
   to toggle: it already asks for pieces in file-priority order, taking the
   first and last piece of each file before the rest, and there is no
   rarest-first mode to switch away from (see the README's differences section
   and `file_info.rs::iter_piece_priorities`).

   So the columns never got a writer, and a column nothing writes is worse than
   no column: the next person to read this schema would reasonably conclude the
   setting exists somewhere. */

ALTER TABLE torrent DROP COLUMN sequential;
ALTER TABLE torrent DROP COLUMN first_last;
