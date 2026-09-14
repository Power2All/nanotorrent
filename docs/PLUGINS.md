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

NanoTorrent places its own examples in that folder — `example.rhai`,
`rss.rhai` and `player.rhai` (with its `player_translations.json`) — switched
off and unapproved. Each is offered
once, by name, so a new example added in a later version reaches a profile that
already has the folder, and one you delete stays deleted.

`player.rhai` is the one to read before approving: it asks for `execute`, and
arriving in the folder is not consent to anything. Nothing in it runs until you
tick it and approve that list yourself.

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
| `ui` | `ui_window`, `ui_rows`, `ui_groups`, `ui_buttons`, `ui_input`, `ui_status`, `ui_form`, `ui_form_close`, `ui_menu`, `ui_file_menu`, `ui_configurable`, `ui_show` |
| `execute` | `run`, `open` — **start programs on this computer, as you** |

`network` is the one that changes what the others mean. A plugin holding
`read` and `network` together can send everything it can see to anyone, and
nothing in the host can tell a feed request from an upload of your torrent
list. That is why the approval prompt shows the whole set at once rather than
asking one line at a time — the combination is the decision, not the parts.

`execute` is the largest thing on the list and reads last in the prompt for
that reason. A plugin holding it is limited by what your account can do and by
nothing else: the other ten permissions describe reach over *torrents*, and
this one describes reach over the *machine*. There is no sandbox behind it, no
allow-list of programs, and no way for the host to tell a media player from
anything else. Grant it to scripts you have read, and read what changed when
one updates — which the host makes you do, because a plugin that adds
`execute` to its header is held until you approve the new set.

What the host does offer is a record. Every `run` and every `open` is written
to the log at `INFO` with the plugin's name, the program and its arguments,
before the process starts. Nothing is shelled out: the program and each
argument are passed separately, so a filename out of a torrent cannot become a
second command however it is spelled.

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
| `on_ui_open()` | Your window appeared on screen |
| `on_ui_row(id)` | A row in your window's main list was clicked |
| `on_ui_group(id)` | A row in the upper list was clicked |
| `on_ui_button(id, input)` | A button was pressed; `input` is the text field |
| `on_ui_menu(id)` | An item in your menu-bar dropdown was chosen |
| `on_file_menu(id, hash, index, name)` | One of your items was used on a file |
| `on_ui_form(form_id, values)` | A form was saved; `values` is field id to value |
| `on_ui_form_cancel(form_id)` | A form was dismissed without saving |
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

`on_file_menu` is told which item was used *and* which file it was used on, so
a plugin needs nothing remembered between drawing the menu and the click
arriving. It fires the same way from the desktop details panel and from the web
interface — a plugin cannot tell the two apart, and should not try.

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
| `files(hash)` | Array of `#{ index, name, length, progress, priority }` |
| `peers(hash)` | Array of `#{ addr, state, fetched_bytes, pieces }` |
| `trackers(hash)` | Array of `#{ url, tier }` |
| `share_limits(hash)` | `#{ ratio, seed_minutes }` — `()` where there is no override |
| `magnet_uri(hash)` | A magnet link for a torrent already added, or `""` |
| `dht_nodes()` | Routing table size, `0` when DHT is off |
| `listen_port()` | The port peers are told to reach, `0` if not listening |
| `stream_url(hash, index)` | A URL a media player can open for one file, or `""` |

`trackers()` gives URLs and tiers, not the Trackers tab's status column: that
column is translated, and a plugin branching on it would work in English and
quietly stop working in the other 75 languages.

`stream_url` is how a plugin hands a file to something that is not NanoTorrent.
It points at `127.0.0.1` and carries a **capability token**, not your password:
the token is good for that one file of that one torrent, dies half an hour
after its last use, and is never written to disk. A plugin cannot read the web
interface's password and does not need to. The URL serves byte ranges out of a
file that is still downloading, so a player can start before the torrent
finishes.

It returns `""` when the web interface is switched off, because then there is
nothing listening to stream from — check for that rather than handing a player
a URL into a closed port.

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
| `reannounce(hash)` | Announce to the trackers now | `control` |
| `queue_move(hash, where)` | `"top"`, `"up"`, `"down"`, `"bottom"`; `false` for anything else | `control` |
| `set_file_priority(hash, index, priority)` | 0 skips the file, 1 is normal, higher is sooner | `control` |
| `set_share_limits(hash, ratio, minutes)` | Either may be `()` to clear that half | `control` |
| `set_location(hash, folder)` | Where it will live, without moving what is there | `storage` |
| `add_tracker(hash, url, tier)` | `bool` — the URL is validated as in the UI | `control` **and** `network` |
| `edit_tracker(hash, from, to)` | `bool` | `control` **and** `network` |
| `remove_tracker(hash, url)` | | `control` **and** `network` |
| `add_torrent_file(base64)` / `(base64, save_path)` | `bool` — a `.torrent` the plugin already has | `add` |
| `remove(hash)` | Remove, keeping files | `remove` |
| `remove(hash, delete_files)` | Remove, optionally deleting files | `remove` |
| `move_storage(hash, folder)` | Move the download | `storage` |
| `set_label(hash, id)` / `clear_label(hash)` | Labels | `labels` |
| `add_magnet(uri)` / `add_magnet(uri, save_path)` | Add a magnet link | `add` |
| `notify(title, body)` | Desktop notification | `notify` |
| `log(message)` | Write to the NanoTorrent log | — |

Tracker editing needs **both** `control` and `network`, which no other call
does. Adding a tracker is a control operation on a torrent, but its consequence
is disclosure: the torrent starts announcing to a server the plugin chose, which
is your address and what you are downloading going somewhere new. A plugin
holding only `control` can stop and start a torrent; it cannot redirect where
that torrent tells the world about itself.

`add_torrent_file` is the other half of `http_get`. `add_torrent_url` fetches
for you, which is no use when the file is behind a header, a cookie or a POST —
fetch it yourself, then hand over the bytes.

**Reaching the network** — needs `network`

| Function | Returns | Needs |
|---|---|---|
| `http_get(url)` | `#{ ok, status, body, error }` | `network` |
| `add_torrent_url(url)` | `bool` — fetches and adds it | `network` + `add` |
| `add_torrent_url(url, save_path)` | The same, into a folder | `network` + `add` |
| `add_torrent_url(url, #{ save_path, label, paused })` | The same, with options | `network` + `add` |

Only `http` and `https`; `file://` is refused before the request is made, so
the network permission cannot be turned into a filesystem read. A response is
capped at 4 MB and 30 seconds. An unreachable server is not an error that kills
your handler — it comes back as `ok: false` with `error` set, because a plugin
polling the internet on a timer will meet one sooner or later.

`add_torrent_url` takes a magnet link as-is and fetches anything else as a
`.torrent`, which is what feeds actually contain.

The map form takes any of `save_path`, `label` and `paused`; anything you leave
out keeps its default, so a plugin that only wants to add something paused does
not have to name a folder as well. `label` is a label **name** and is looked up,
never created — a plugin should not be able to fill somebody's label list by
getting a rule wrong — so a name that does not exist means no label, and says so
in the log. `paused` is true for any non-empty value except `"0"` and `"false"`,
which is the same convention a form checkbox follows.

**Starting something else** — needs `execute`

| Function | Effect |
|---|---|
| `run(program, [args])` | Start `program` with those arguments; `true` if it started |
| `run(program)` | The same, with none |
| `open(target)` | Hand a URL, file or folder to whatever the desktop opens it with |

`run` takes the program and its arguments **separately**, and there is no shell
anywhere in the path. A space in a path needs no quoting and nothing a torrent,
a feed or a filename can say becomes a command. The child is not waited on — a
media player runs for hours — so `true` means *it started*, not *it worked*, and
a program that is not installed comes back `false` rather than raising. That is
what makes "try these four paths until one of them is VLC" the obvious way to
find a player without being able to look at the file system.

`open` names no program: the association is one the user already made. It is
how a plugin reaches "the player they chose" without being told which it is.

**Your own strings** — no permission

| Function | Returns |
|---|---|
| `t(key)` | The string for `key` in the user's language |
| `t(key, a)` | The same, with `{0}` replaced by `a` |
| `t(key, a, b)` | The same, with `{0}` and `{1}` replaced |

Put them in **`<your-plugin>_translations.json`**, beside the script — so
`player.rhai` is translated by `player_translations.json`. Locale first, key
second:

```json
{
  "en-US": { "play": "Play", "not_playable": "Nothing here can play {0}" },
  "nl-NL": { "play": "Afspelen", "not_playable": "Hier kan {0} niet mee" }
}
```

Put the **whole sentence** in the catalogue, with `{0}` and `{1}` where the
values go - do not build it with `+`. Word order is exactly what differs between
languages: `"Checking " + n + " feeds against " + m + " rules"` can only ever
come out in English order, while `"Checking {0} feeds against {1} rules"` lets
the translator put the pieces where they belong. Two placeholders is the limit;
a message with three moving parts is usually two messages.

Adding a language is one block. A key with no string in the current language
falls back to `en-US`, and then to **the key itself** — so `t("play")` on a
plugin nobody has translated yet reads `play`, which is obviously a missing
string rather than a blank control.

No permission: it is your file, beside your plugin, saying what your plugin was
going to say anyway. There is no file access here — the host reads that one
path, nothing else, and a plugin with no such file simply gets its keys back.

The language is read **when you call `t`**, not when the plugin loaded, so a
plugin's window follows somebody changing their language in Preferences without
a reload.

One thing worth copying from `player.rhai`: do not store a translated string as
a setting. It saves the option ids (`system`, `vlc`, `mpv`, `custom`) and shows
`t("player_" + id)`, so the saved value means the same thing in every language.
A dropdown whose translated label IS the stored value silently forgets itself
the day somebody switches language.

**Telling the time** — no permission

| Function | Returns |
|---|---|
| `now()` | Seconds since the Unix epoch |

A clock reveals nothing a script could not already infer from how often it is
ticked, and without one "ignore this for a week" cannot be written at all.

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
| `ui_file_menu([#{ id, label }])` | Your items on a file's context menu |
| `ui_form(form_id, title, [#{ id, label, kind, value, options, hint }])` | A form, in place of the lists |
| `ui_form_close()` | Put the lists back |
| `ui_configurable(true)` | Ask for a Configure button in Preferences |

Declaring a window is what lists it in the menu; `ui_show` is separate so a
plugin can prepare one at load without a window appearing unasked. Clicks come
back as `on_ui_row` and `on_ui_button`.

**Do not call `ui_show()` from `on_ui_open()`.** That handler runs *because* the
window appeared, so asking for it again from inside it is a loop. `on_ui_open`
is for filling the window; `ui_show` is for the handler that wanted it on screen
in the first place - `on_ui_configure`, or an item in your menu. `ui_show` on a
window that is already up raises it and does **not** re-run `on_ui_open`, so the
loop terminates now even if a plugin writes it that way, but it is still the
wrong shape.

### Forms

One text field is enough to add a feed and nowhere near enough to edit a rule
with a dozen settings on it. `ui_form` describes the controls you want; the
window draws them and hands the values back in one go.

`kind` is one of:

| `kind` | Control | Value |
|---|---|---|
| `text` | A text field | What was typed |
| `number` | A text field that only takes digits | Still a string — parse it yourself |
| `check` | A checkbox; `label` goes beside it, not above | `"1"` or `""` |
| `choice` | A dropdown over `options` | The entry chosen |

`options` is one entry per line and is ignored by every other kind. `hint` draws
a line of explanation under the control, or nothing when empty.

**Every value arrives as a string**, a checkbox included, so a script reads them
all the same way. Saving calls `on_ui_form(form_id, values)` with a map of field
id to value; Cancel calls `on_ui_form_cancel(form_id)`. Neither closes the form
for you — call `ui_form_close()` when you are ready, which is what lets a
rejected form stay up with what was typed still in it.

A form REPLACES the lists while it is up, in the window and in the web
interface alike. That is deliberate: it is a different thing to be doing, and
half a list behind a form is neither. It also means the redraw a plugin does on
a timer will not disturb one — the window only re-pushes a form whose fields
have actually changed, so it cannot wipe out what somebody is typing.

An empty field list closes the form, so `ui_form(id, title, [])` and
`ui_form_close()` do the same thing.

### Reaching your plugin

Nothing appears anywhere unless the script asks for it. There are two ways to
give the user a way in, and they mean different things:

**`ui_menu(title, items)`** puts a dropdown of your own in the main window's
menu bar, after File, View and Help. An empty `title` falls back to the
plugin's name. Choosing an item calls `on_ui_menu(id)`. This is where a plugin
puts the things a person does with it — "Feeds…", "Check now".

**`ui_file_menu(items)`** puts your items on the context menu of a *file*, in
a torrent's details panel and on the matching row in the web interface. Using
one calls `on_file_menu(id, hash, index, name)`. This is the only place a
plugin draws outside its own window, and it is deliberately the smallest shape
that works: items, no submenus, no icons, no say in where they go.

They go **below** NanoTorrent's own *Open file*, under a separator, in the
order the plugins sort by name. You cannot get above it or replace it.

Items are labelled with your plugin's name — "Player: Play", not "Play". That
is not decoration. A menu item is text of your choosing appearing inside
NanoTorrent's own window, which is the shape of every convincing phishing
prompt ever written; the prefix means an item reading "Verify your password" is
at least visibly somebody's. The name shown is your file's stem with its first
letter capitalised, and nothing else about it is touched — `my-tool.rhai` reads
as "My-tool", not "My Tool".

The same list is offered on every file. The host draws the menu before it knows
which file it will be used on, so a plugin that only handles videos checks the
name it is given in `on_file_menu` and says why, rather than expecting the item
to hide itself.

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
are separate declarations. A plugin with file-menu items needs no window at
all — `docs/plugins/player.rhai` has one only because its settings form has to
be drawn somewhere.

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
  `example.rhai`, `rss.rhai` and `player.rhai` are already taken by the
  shipped ones.
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

`run` and `open` exist, behind `execute`. They did not, and the reasoning
against them was that no prompt makes "execute arbitrary programs" a decision
anybody can sensibly consent to. What changed is not that argument — it is who
is being asked. A permission shown in plain language, held until approved, held
again when the script edits its own header, and logged on every use is the same
consent the other ten permissions rest on. Refusing it did not make plugins
safer; it made the useful ones impossible and pushed people to run the scripts
outside NanoTorrent, where nothing is declared, approved or logged at all.

File reading and file writing are still not offered. A plugin has `data_*` for
its own state and `stream_url` for handing one file to a player, and neither
turns into a general read of your disk.

Four things the web interface can do that a plugin still cannot, each for a
reason rather than an oversight:

- **Change application settings.** A plugin that could write settings could
  move the save path or switch off the network kill switch, which is privilege
  escalation wearing a convenience hat. Reading them is not offered either,
  because the settings include the proxy host and the web interface's own
  configuration.
- **Create a torrent.** Creating one means hashing a path on disk, which is an
  indirect read of the file system this API deliberately closes.
- **Read file contents.** `stream_url` hands a *player* a URL; the bytes go
  from the session to the player without passing through the script. A plugin
  never sees them.
- **Draw anything of its own shape.** Its window is a list, a text field and
  some buttons, and its items on a file's context menu are text and nothing
  else. There is no layout language and no way to place a control, so a script
  cannot draw something that passes for part of the client asking for a
  password.

Beyond the file-menu items, a plugin's window is the one described above and
nothing else — it cannot add to the main window, the toolbar or the rest of the
details panel. What a plugin changes in the session still shows up in both
surfaces, because both read the same session — a torrent a plugin pauses reads
as paused everywhere.

Plugin windows are desktop-only. A headless build has nowhere to put one, so
the `ui_*` calls do nothing there; everything else in a plugin works the same.

File-menu items are the exception: the web interface draws them, so on a
headless build they are the plugin's only way in — and a click in a browser
runs `execute` on the *server*. That is the same trust as installing the plugin
there in the first place, and the web interface is authenticated, but it is
worth knowing before granting `execute` on a machine you log into remotely.
