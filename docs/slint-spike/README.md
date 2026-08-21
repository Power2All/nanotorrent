# Slint spike screenshots

The Phase 2 evaluation running against 5,000 synthetic torrents. Source and
full findings: `spike/slint-list/`.

| File | Shows |
|---|---|
| `01-main-window.png` | The whole frame - menu bar, 16-column list, splitter, four detail tabs, status bar. Compare with `../ui-baseline/03-main-rows.png`. |
| `02-context-menu.png` | `ContextMenuArea` with the torrentcontextmenu.cpp structure, including the Queue position sub-menu. |
| `03-multi-select.png` | Ctrl/shift multi-select - three rows, which `StandardTableView` cannot do at all. |
| `04-splitter-resized.png` | The hand-rolled splitter after dragging - details panel taller, list correspondingly shorter. |

Synthetic data throughout: names are generated, nothing here came off a real
disk or a real torrent.
