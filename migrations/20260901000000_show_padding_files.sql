/* Whether BEP 47 padding files are listed in the UI.

   Off by default, because a padding file is not a file. It is an alignment
   artifact: a run of zeros a torrent inserts so that the next real file starts
   on a piece boundary. Hybrid torrents need them so their v1 and v2 layouts
   agree, and NanoTorrent's own v2-only handling inserts them for the same
   reason - so they turn up in ordinary, well-formed torrents rather than only
   in odd ones.

   Nothing is ever downloaded for them either way: the engine synthesises the
   zeros. Showing them only ever meant a phantom ".pad\49152" row in the file
   list and an inflated total size.

   Kept as a setting rather than removed outright because someone debugging a
   torrent's layout has a real reason to want to see them. */

INSERT INTO setting (key, value, default_value)
VALUES ('ui.show_padding_files', NULL, 'false');
