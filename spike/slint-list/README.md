# Slint spike: the main window

Answers one question before Phase 2 commits to a toolkit: **can Slint carry
NanoTorrent's main window?**

It started as the torrent list alone - the hardest and most representative
screen, 16 columns, sortable, multi-select, in-cell progress bar, right-click
context menu. That answered the list question and said nothing about the
frame, so it now covers the whole window: menu bar, the splitter, the four
detail tabs and the status bar. Compare against
`docs/ui-baseline/03-main-rows.png` and `04-tab-overview.png`.

Standalone (`[workspace]` in its own `Cargo.toml`), so it costs the shipping
binary nothing.

    cargo run          # opens a window with 5,000 synthetic torrents

## Verdict: yes, but the list is a custom widget

Everything needed works. Nothing needed was missing. But `StandardTableView` -
the obvious starting point - cannot be used, so the list has to be built from
`ListView` and a custom row.

## What `StandardTableView` cannot do

Read from Slint 1.17.1's own source (`widgets/*/tableview.slint`), not inferred:

| Requirement | Available? |
|---|---|
| 16 columns, resizable, sortable headers | Yes |
| **Multi-select** | **No.** Its only selection property is `current-row: int`. No `selected-rows`, no `selection-mode`. |
| **Progress bar in a cell** | **No.** Cells are `StandardListViewItem`, which renders text and an icon. |

NanoTorrent needs both: multi-select drives the context menu (`selected_hashes`
is a slice today), and the Progress column owner-draws a bar in dark mode.

## What the custom row does do

All verified by running it, not by reading docs:

- **16 columns**, widths declared once in a `global` so header and rows cannot
  drift.
- **A real progress bar** inside the Progress cell - the thing
  `StandardTableView` could never draw.
- **Ctrl/shift multi-select.** `PointerEvent` carries `.modifiers.control` and
  `.modifiers.shift`, so this is ordinary code over a per-row `selected` flag
  the Rust model owns.
- **`ContextMenuArea`** with the full `torrentcontextmenu.cpp` structure,
  including a **sub-menu** (Queue position) and separators.
- **`ListView` virtualises**, so 5,000 rows cost the same as 50. The Win32 list
  does not: `sync_list` rebuilds all 16 cell strings for every torrent every
  second and holds a real `LVITEM` per row.

## Measured, at 5,000 rows

| Operation | Time |
|---|---|
| Build all rows | ~50 ms |
| Sort all rows and swap the model | 21 ms |
| Update selection across the whole model | 8.5 ms |

## Four things that cost time, so they are written down

1. **A `TouchArea` inside a `ContextMenuArea` swallows the right-click**, so the
   menu never appears on its own. It has to be shown by hand, and the position
   is relative to the `ContextMenuArea`:

       menu.show({
           x: ta.absolute-position.x + ta.mouse-x - menu.absolute-position.x,
           y: ta.absolute-position.y + ta.mouse-y - menu.absolute-position.y,
       });

2. **Do not wire both `pointer-event` and `clicked` on the same row.**
   `pointer-event` already fires on press *and* carries the modifiers;
   `clicked` fires afterwards with none, silently overwriting every ctrl-click
   with a plain one. This looked exactly like "Slint does not deliver
   modifiers" until the callback was made to print what it received - it does.

3. **`padding-left` is not a `Text` property.** It compiles to a warning and is
   ignored, so right-aligned values run straight into the next column
   ("80.65 MBUploading"). A cell needs to be a `Rectangle` wrapping a `Text`.

4. **The context menu renders light against a dark window.** Slint's menu does
   not pick up the dark palette the rest of the app follows. Cosmetic, but
   visible, and worth resolving before the port ships.

## The frame

| Piece | Result |
|---|---|
| **Menu bar** (File / View / Help, sub-menus, separators) | `MenuBar` builtin. Native on macOS - it goes to the top of the screen - and Slint-drawn elsewhere. Supports `shortcut: @keys(Control + N)`. |
| **Detail tabs** (Overview / Files / Peers / Trackers) | `TabWidget`. Works, but see the caveat below. |
| **Overview fields** | `GridLayout` of label/value pairs, matching the two-column Win32 layout. |
| **Status bar** | No widget; a `Rectangle` with a `HorizontalLayout`. Trivial. |
| **Splitter** | **No widget. Hand-rolled, and harder than it looks - see below.** |

### The tabs do not look like the Win32 ones

Slint's `TabWidget` stretches its tabs to fill the full width. The Win32 tab
control keeps them compact and left-aligned. This is the most visible
difference between the spike and the baseline, and matching it means either
styling `TabWidget` or building a tab strip out of `TouchArea`s - the same
choice the list forced.

### The splitter is not free

Slint has no splitter widget, so the divider between the list and the details
panel is hand-rolled - which is also what the Win32 frame does
(`MainWindow::splitter_dragging`, `details_height`). Getting the drag right is
fiddlier than its six pixels suggest, and **it is not working in this spike
after three attempts**. Recorded as an open item rather than papered over.

The trap, which is real and worth knowing before Phase 2 hits it: the obvious
handler is

    moved => { details-height = details-height - (self.mouse-y - self.pressed-y); }

and it does nothing, because the `TouchArea` sits on top of the panel it
resizes. Change the height and the TouchArea moves, so `mouse-y` moves with it
and the delta cancels itself out. Anchoring to `absolute-position.y + mouse-y`
captured at press is the shape of the fix - the current code does that and
still does not resize, so something further is wrong. Budget real time for it.

## What this means for Phase 2

The list is a custom widget rather than a drop-in, which is more work than
"use the table widget" - but it is ordinary declarative code, not a fight, and
it buys per-cell control the Win32 version only gets by owner-drawing.

The frame is mostly free - menu bar, tabs, status bar and the details grid all
came together quickly and match the baseline closely. Two pieces are not: the
tab strip does not look like the Win32 one, and the splitter needs real work.

Still unanswered, and smaller: the Files tab needs a tree and Slint has no tree
widget (flatten it yourself), and the desktop furniture - tray icon, native
file dialogs, notifications, `magnet:` registration - is assembled from
separate crates rather than provided.

Licensing is unchanged and still the deciding factor for shipping: Slint is
GPLv3 / commercial / royalty-free-with-conditions against this project's MIT.
