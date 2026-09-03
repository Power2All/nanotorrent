# Plugins

NanoTorrent runs plugins written in [Rhai](https://rhai.rs) — a small, pure-Rust
scripting language. A plugin is one `.rhai` file that defines handler functions;
NanoTorrent calls them when things happen to your torrents.

A worked example ships with NanoTorrent: the first time it runs it writes
[`example.rhai`](plugins/example.rhai) into your plugins folder, **switched
off**, so there is something to read before there is something running.

## Enabling

Two switches, both off by default.

1. **The host.** `nanotorrent --plugins on`, or Preferences ▸ Plugins ▸
   *Enable plugins*. Nothing runs until this is on.
2. **Each plugin.** Preferences ▸ Plugins lists every `.rhai` file found and
   turns them on and off individually. A plugin switched off is never compiled
   or run at all.

Both take effect immediately. Pressing Ok, or approving a plugin, stops what
was running and loads whatever the settings now say - there is no restart, in
keeping with the rest of Preferences. A reloaded plugin starts fresh: it loses
whatever it was keeping in its top-level scope, exactly as a restart would have
made it. Anything it must keep belongs in `data_set`.

NanoTorrent places its own examples in that folder — `example.rhai` and
`rss.rhai` — switched off and unapproved. Each is offered once, by name, so a
new example added in a later version reaches a profile that already has the
folder, and one you delete stays deleted.

Drop your own `.rhai` files in beside them:

| Platform | Folder |
|---|---|
| Windows | `%LOCALAPPDATA%\NanoTorrent\plugins` |
| Linux | `~/.local/share/nanotorrent/plugins` (or `$XDG_DATA_HOME`) |
| macOS | `~/Library/Application Support/NanoTorrent/plugins` |

In portable mode (`NANOTORRENT_PORTABLE`, or a `portable.txt` next to the
executable) it is a `plugins` folder beside the executable instead. Preferences
▸ Plugins ▸ *Open plugins folder* opens the right one.

A file added, edited or deleted while NanoTorrent is running is picked up the
next time the plugin settings are applied - press Ok in Preferences ▸ Plugins
and the folder is read again.

Plugins work in headless builds too — that is arguably where they are most
useful, since there is no window to watch. There is no Preferences dialog
there, so use `--plugins on` and, for now, approve permissions from a desktop
build sharing the same profile.

## Permissions

Every plugin declares what it may reach, on one line, before any of it runs:

```rhai
//! permissions: read, control, notify
```

NanoTorrent reads that line **from the source text, without executing the
script** — the whole point is knowing what a script wants before any of it
runs. Only the leading comment block is scanned, so a `permissions:` line
further down, or inside a string, cannot quietly widen the request.

| Permission | Grants |
|---|---|
| `read` | `torrents`, `torrent`, `exists`, `session_rates` |
| `control` | `pause`, `resume`, `recheck` |
| `add` | `add_magnet` |
| `labels` | `set_label`, `clear_label` |
| `storage` | `move_storage` |
| `remove` | `remove` — **including deleting the downloaded files** |
| `notify` | `notify` |
| `network` | `http_get`, and `add_torrent_url` together with `add` |
| `data` | `data_get`, `data_set`, `data_remove`, `data_keys` |
| `ui` | `ui_window`, `ui_rows`, `ui_buttons`, `ui_input`, `ui_status`, `ui_show` |

`network` is the one that changes what the others mean. A plugin holding
`read` and `network` together can send everything it can see to anyone, and
nothing in the host can tell a feed request from an upload of your torrent
list. That is why the approval prompt shows the whole set at once rather than
asking one line at a time — the combination is the decision, not the parts.

`log` needs no permission. A script that declares nothing gets `log` and
nothing else, and needs no approval — there is nothing to consent to.

### How it is enforced

Each plugin gets **its own Rhai engine**, holding only the functions it was
granted. A function you did not ask for is not registered, so calling it fails
with *"function not found"* rather than being refused at runtime. There is no
way to probe for what sits behind a permission you do not hold, and no shared
engine that could leak one plugin's grant to another.

Ask for the least you need. Over-asking is not free: it is shown to the user in
plain language before they approve, next to your plugin's name.

### Approval, and what invalidates it

A plugin asking for anything is **held** — discovered, listed, but not loaded —
until the user approves it in Preferences ▸ Plugins. The log says so:

```
WARN plugin tidy-up is waiting for approval of: read, storage, remove
```

Consent is stored as **the set you asked for**, not a yes/no flag. Edit the
header to want more and the stored grant no longer matches, so the plugin is
held again until the new set is approved. This is deliberate: an updated plugin
cannot quietly widen its own reach. Editing it to ask for *less* also
re-prompts, which is the harmless direction.

An unknown word in the header is reported rather than ignored, because a typo
silently dropping a permission looks like the host is broken.

## Handlers

Define any of these. All are optional; a plugin that defines none does nothing.

| Handler | Called when |
|---|---|
| `on_session_start()` | The plugin host has finished loading |
| `on_session_stop()` | Shutting down — best effort, see below |
| `on_torrent_added(hash, name)` | A torrent appears in the session |
| `on_torrent_completed(hash, name)` | A torrent finishes downloading |
| `on_torrent_removed(hash, name)` | A torrent leaves the session |
| `on_error(message)` | Background work failed |
| `on_tick()` | Once a minute, for as long as the session runs |
| `on_ui_open()` | Your window was opened |
| `on_ui_row(id)` | A row in your window's main list was clicked |
| `on_ui_group(id)` | A row in the upper list was clicked |
| `on_ui_button(id, input)` | A button was pressed; `input` is the text field |
| `on_ui_menu(id)` | An item in your menu-bar dropdown was chosen |
| `on_ui_configure()` | Configure was pressed on your Preferences row |

`on_tick` fires on a wall-clock deadline, so a busy session does not starve it.
One minute is fixed: a plugin that wants an hour counts sixty ticks, which is
cheaper than a scheduler nobody asked for. Nothing else fires it, so a plugin
that wants to poll something should do it here rather than in a loop.

Lifecycle events are detected by the session itself, not by the UI, so they
fire the same way whether you added the torrent from the window, the web
interface, a magnet link handed to a second instance, or another plugin.

`on_session_stop` is best effort: NanoTorrent does not wait for the plugin
thread before exiting, so a slow handler may be cut off mid-run. Do not use it
to save anything you cannot lose — write state out as you go instead.

`on_torrent_completed` fires on a genuine transition. Torrents that were already
complete when NanoTorrent started do not fire it, and a recheck that un-finishes
a torrent lets it fire again when it re-completes.

## Keeping state between events

Top-level statements run once, when the plugin loads, and the variables they
create are visible to your handlers — and mutable from them:

```rhai
let seen = 0;

fn on_torrent_completed(hash, name) {
    seen += 1;                       // persists across calls
    log("that is " + seen + " this run");
}
```

The scope belongs to that one plugin. Another plugin's top-level `seen` is a
different variable, and neither can see the other's.

**Only handlers can see it.** Rhai functions are pure: the scope is given to
the function NanoTorrent calls, and not to anything that function calls in
turn. This is the single thing most likely to catch you out when a plugin grows
past one handler:

```rhai
let items = [];

fn draw() { ui_rows(items); }        // WRONG - "variable not found: items"
fn draw(list) { ui_rows(list); }     // right - pass it down

fn on_ui_open() {
    items = fetch();                 // fine, a handler sees the scope
    draw(items);
}
```

Keep state in the handlers and pass it to helpers as arguments.

### Across restarts — needs `data`

The scope is gone at restart. For anything that has to outlive the session:

| Function | Effect |
|---|---|
| `data_get(key)` | The stored string, or `()` if there is none |
| `data_set(key, value)` | `false` if the store is full — check it |
| `data_remove(key)` | Forget one key |
| `data_keys()` | Every key this plugin has stored |

Strings only, and namespaced by plugin, so one plugin cannot read or overwrite
another's. There is a 64 KB ceiling per plugin: it lives in the settings
database, which is not the place for a cache of every item a feed ever
published. `data_set` returning `false` means you are over it — prune and
retry rather than ignoring the result.

## What a plugin can call

**Reading** — needs `read`

| Function | Returns |
|---|---|
| `torrents()` | Array of maps, one per torrent |
| `torrent(hash)` | One map, or `()` if it is gone |
| `exists(hash)` | `bool` |
| `session_rates()` | `#{ download: int, upload: int }`, bytes/sec |

Each torrent map has: `hash`, `name`, `save_path`, `label`, `progress` (0.0–1.0),
`ratio`, `paused`, `error`, `size`, `remaining`, `downloaded`, `uploaded`,
`download_rate`, `upload_rate`, `peers`, `seeds`, `queue_position`, `state`.

Field names match the web API's JSON, so a plugin and a web client describe the
same torrent the same way.

**Acting**

| Function | Effect | Needs |
|---|---|---|
| `pause(hash)` / `resume(hash)` | Pause or resume | `control` |
| `recheck(hash)` | Force a recheck | `control` |
| `remove(hash)` | Remove, keeping files | `remove` |
| `remove(hash, delete_files)` | Remove, optionally deleting files | `remove` |
| `move_storage(hash, folder)` | Move the download | `storage` |
| `set_label(hash, id)` / `clear_label(hash)` | Labels | `labels` |
| `add_magnet(uri)` / `add_magnet(uri, save_path)` | Add a magnet link | `add` |
| `notify(title, body)` | Desktop notification | `notify` |
| `log(message)` | Write to the NanoTorrent log | — |

**Reaching the network** — needs `network`

| Function | Returns | Needs |
|---|---|---|
| `http_get(url)` | `#{ ok, status, body, error }` | `network` |
| `add_torrent_url(url)` | `bool` — fetches and adds it | `network` + `add` |
| `add_torrent_url(url, save_path)` | The same, into a folder | `network` + `add` |

Only `http` and `https`; `file://` is refused before the request is made, so
the network permission cannot be turned into a filesystem read. A response is
capped at 4 MB and 30 seconds. An unreachable server is not an error that kills
your handler — it comes back as `ok: false` with `error` set, because a plugin
polling the internet on a timer will meet one sooner or later.

`add_torrent_url` takes a magnet link as-is and fetches anything else as a
`.torrent`, which is what feeds actually contain.

**Making sense of what came back** — no permission

| Function | Returns |
|---|---|
| `parse_json(text)` | Maps, arrays and scalars, or `()` if it will not parse |
| `parse_xml(text)` | `#{ tag, attrs, text, children }`, or `()` |

Neither needs a permission: they are arithmetic on a string you already hold.
`parse_xml` is not a full document model — no namespaces, no comments — but it
is enough to walk an RSS or Atom feed, which is the job it exists for. Entities
and CDATA are decoded, and each element's text is trimmed.

**A window of your own** — needs `ui`

| Function | Effect |
|---|---|
| `ui_window(title)` | Name the window. A plugin with no name has no window |
| `ui_input(placeholder)` | Show a text field; empty means no field |
| `ui_buttons([#{ id, label }])` | A row of buttons |
| `ui_rows([#{ id, title, subtitle, selected }])` | The main list |
| `ui_groups([#{ id, title, subtitle, selected }])` | An optional list above it |
| `ui_status(text)` | One line under the list |
| `ui_show()` | Put the window on screen now |
| `ui_menu(title, [#{ id, label }])` | Your own dropdown in the menu bar |
| `ui_configurable(true)` | Ask for a Configure button in Preferences |

Declaring a window is what lists it in the menu; `ui_show` is separate so a
plugin can prepare one at load without a window appearing unasked. Clicks come
back as `on_ui_row` and `on_ui_button`.

### Reaching your plugin

Nothing appears anywhere unless the script asks for it. There are two ways to
give the user a way in, and they mean different things:

**`ui_menu(title, items)`** puts a dropdown of your own in the main window's
menu bar, after File, View and Help. An empty `title` falls back to the
plugin's name. Choosing an item calls `on_ui_menu(id)`. This is where a plugin
puts the things a person does with it — "Feeds…", "Check now".

**`ui_configurable(true)`** puts a cog on the plugin's row in
Preferences ▸ Plugins, which calls `on_ui_configure()`. That is for a plugin
that will not work until it is set up — the RSS reader has no feeds until you
give it one. It is not a second way to open your window: declare it only if
there is genuinely something to configure, or the cog becomes noise on every
row.

Both appear only once the plugin is **loaded and has actually declared them**,
so a plugin that is ticked but still waiting for approval offers neither.

**One dropdown per plugin**, and that is structural rather than a rule the host
checks: a plugin holds a single menu title and a single item list, so calling
`ui_menu` twice replaces the menu instead of adding another. There is no
arrangement of calls that puts two of your titles in the bar. A menu is capped
at 20 items for the same reason — the bar is shared with the application's own
menus, and a dropdown taller than the screen covers the client rather than
extending it. Items past the cap are dropped with a line in the log.

A plugin can have a menu and no window, a window and no menu, or neither. They
are separate declarations.

### Two lists

`ui_groups` adds a second list *above* the main one, for when the main list is
showing the contents of something the user picks: feeds, categories, accounts.
Clicking one calls `on_ui_group(id)`. Leave it empty and the window is the
single-list one it was before.

Which row is current is the plugin's to decide, not the window's — set
`selected: true` on it when you redraw. The window does not remember a
selection of its own, so the two can never end up disagreeing after a reload.

The shape is fixed — a list, a text field, some buttons — and there is no
layout language. That is deliberate: a plugin says what goes in the window and
NanoTorrent decides how it looks, so a script cannot draw something that passes
for part of the client asking for a password. In a headless build every `ui_*`
call does nothing and the host says so once in the log, rather than failing.

`remove` takes two arities rather than a default argument: Rhai has no optional
parameters, and `remove(hash)` deleting files by accident is a mistake a plugin
author only makes once.

These are the same verbs the web API exposes, deliberately: a plugin cannot
reach anything an authenticated web client could not.

## Distributing a plugin

**As source. There is no compiled form.** Rhai has no serialised-AST or
bytecode format to ship — `compile_file` takes source text and nothing else —
so a plugin is exactly one `.rhai` file that someone copies into their plugins
folder.

For this design that is the right way round rather than a limitation. The
permission header is only trustworthy because it is read from the same text
that will run; a pre-compiled blob would make the declaration unverifiable and
put the user's decision on something they cannot read. Distribute the script,
and let people see what they are approving.

Practical notes:

- One file, one plugin. There is no import or module system, so keep it
  self-contained.
- The **file name is the plugin's identity** — it is what is shown in
  Preferences, what the log lines say, and the key the approval is stored
  under. Renaming a plugin re-asks for approval. Pick something specific:
  `example.rhai` is already taken by the shipped one.
- Put the permission line where a reader will see it, and say in a comment why
  you need each one. It is the first thing anyone installing your plugin reads.

## Limits

Every handler call runs under a ceiling: 500,000 Rhai operations, 64 call
levels, 64 expression depth (32 inside a function), 50,000-element arrays and
maps, and strings of twice the 4 MB HTTP ceiling.

That last pair is deliberately one number and not two. `http_get` hands the
response body back as a string, so the string ceiling has to be able to hold a
whole response with room to build something from it — otherwise a fetch inside
the documented limit produces a value the engine refuses to hold, and every
feed above the smaller number fails with an error its author cannot act on. A script that loops
forever is killed at the limit and the error is logged — it cannot hang the
client.

Plugins run on their own thread, so a slow one delays other plugins but never
the UI, the session or a web request. Handlers are called in alphabetical order
by filename.

## When something goes wrong

A plugin that fails to compile, or throws from a handler, is logged and
skipped. It does not stop other plugins from running, and it does not stop
NanoTorrent from starting.

Preferences ▸ Plugins shows a compile error next to the plugin, with the line
and column. A plugin that fails stays **ticked** — that it is broken is a fact
about the script, not a setting to be undone on your behalf.

Everything else goes to the log: `log()` output is tagged `plugin`, and load
decisions are recorded as they are made.

```
INFO  loaded plugin ratio-keeper with: read, control
WARN  plugin tidy-up is waiting for approval of: read, storage, remove
ERROR plugin broken-thing: Syntax error: ... (line 2, position 32)
INFO  plugin: hello from my script
```

Note that the Preferences tab checks whether a plugin **compiles**; it does not
run its top-level statements, because opening a settings dialog must not have
side effects. A script that compiles and then fails on its first line shows up
in the log, not in the dialog.

## What plugins deliberately cannot do

There is no `run()`, no file reading and no file writing — not behind a
permission, not at all. Those are the difference between a script that manages
torrents and one that owns the machine, and no permission prompt makes
"execute arbitrary programs" a decision a user can sensibly consent to.

Post-download processing that has to launch something needs to go out through
another channel for now.

A plugin's window is the one described above and nothing else. It cannot add
to the main window, the details panel or the web interface, and it cannot draw
its own controls. What a plugin changes in the session still shows up in both
surfaces, because both read the same session — a torrent a plugin pauses reads
as paused everywhere.

Plugin windows are desktop-only. A headless build has nowhere to put one, so
the `ui_*` calls do nothing there; everything else in a plugin works the same.
