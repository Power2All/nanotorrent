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

Drop `.rhai` files into the plugins folder and restart:

| Platform | Folder |
|---|---|
| Windows | `%LOCALAPPDATA%\NanoTorrent\plugins` |
| Linux | `~/.local/share/nanotorrent/plugins` (or `$XDG_DATA_HOME`) |
| macOS | `~/Library/Application Support/NanoTorrent/plugins` |

In portable mode (`NANOTORRENT_PORTABLE`, or a `portable.txt` next to the
executable) it is a `plugins` folder beside the executable instead. Preferences
▸ Plugins ▸ *Open plugins folder* opens the right one.

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

It lives for as long as the session does, and is gone at restart. There is no
storage API; a plugin that must remember something across restarts has nowhere
to put it yet.

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
levels, 64 KB strings, 10,000-element arrays and maps. A script that loops
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

Plugins also have no UI of their own. They cannot add anything to the desktop
window or the web interface. What they change in the session shows up in both,
because both read the same session — a torrent a plugin pauses reads as paused
everywhere — but the surfaces themselves are not extensible.
