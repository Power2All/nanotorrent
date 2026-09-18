// Plugin host: Rhai scripts that react to torrent lifecycle events and drive
// the session back.
//
// Rhai rather than Lua. The deciding constraint is this crate's own: Lua means
// a C build and a hand-rolled sandbox (strip os/io/package/debug, add a debug
// hook to bound runaway scripts), where Rhai is pure Rust and takes its limits
// as constructor arguments. Plugins are untrusted code running on a machine
// whose whole job is ingesting files from strangers, so "sandbox by default"
// is worth more here than "a language people already know" - especially with no
// installed base of scripts to stay compatible with.
//
// The host owns a dedicated OS thread. rhai::Engine and the compiled ASTs never
// leave it, so nothing here needs Rhai's `sync` feature, and a plugin that
// blocks or spins cannot stall the session, the UI or a web request.

mod api;
pub mod strings;
pub mod ui;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rhai::{AST, Engine, Scope};

use crate::bittorrent::session::{Session, SessionEvent};
use crate::core::configuration::Configuration;
use crate::core::environment::Environment;

/// Master switch. Off by default: a plugin is arbitrary code, so it takes a
/// deliberate act to enable one.
pub const ENABLED_KEY: &str = "plugins.enabled";

/// Plugins the user has switched off, comma-separated. Absent means on: the
/// deliberate act is `ENABLED_KEY` plus putting the file in the folder, so a
/// newly dropped-in script should not need a third one.
pub const DISABLED_KEY: &str = "plugins.disabled";

/// Ceiling on a single handler call, in Rhai operations. A script that loops
/// forever dies here instead of pinning a core.
const MAX_OPERATIONS: u64 = 500_000;

/// How often `on_tick` fires. One fixed interval rather than a per-plugin one:
/// a plugin that wants an hour counts sixty ticks, which is a line of Rhai,
/// where a configurable interval is a header field, a parser and a scheduler.
const TICK: std::time::Duration = std::time::Duration::from_secs(60);

/// One loaded script.
struct Plugin {
    name: String,
    /// This plugin's own engine, holding only the host functions it was
    /// granted. One engine per plugin rather than one for the host: the set of
    /// registered functions is what enforces the permission, so sharing an
    /// engine would share the widest grant with every script.
    engine: Engine,
    ast: AST,
    /// Per-plugin state, so a script can keep values between events without
    /// globals shared with other plugins.
    scope: Scope<'static>,
}

impl Plugin {
    /// True when the script defines a handler with this name and arity.
    fn handles(&self, func: &str, params: usize) -> bool {
        self.ast
            .iter_functions()
            .any(|f| f.name == func && f.params.len() == params)
    }
}

/// Start the plugin host if plugins are enabled and any are present.
///
/// Never fatal: a broken plugin folder must not stop a torrent client from
/// starting. Everything that goes wrong here is logged and skipped.
pub fn spawn(session: Arc<Session>, cfg: Arc<Configuration>, env: Arc<Environment>) {
    if !cfg.get_bool(ENABLED_KEY) {
        return;
    }
    start(session, cfg, env);
}

/// The running host's control channel, so a settings change can reach it.
///
/// A global rather than something threaded through the UI: Preferences already
/// knows the session, the configuration and the environment, and making it
/// carry a plugin-host handle as well would put the host into the signature of
/// everything that opens a dialog.
fn control() -> &'static std::sync::Mutex<Option<std::sync::mpsc::Sender<Wake>>> {
    static CONTROL: std::sync::OnceLock<
        std::sync::Mutex<Option<std::sync::mpsc::Sender<Wake>>>,
    > = std::sync::OnceLock::new();
    CONTROL.get_or_init(Default::default)
}

/// Apply a change to the plugin settings without restarting NanoTorrent.
///
/// Reloading is deliberately "as if it had just started": the old set is
/// stopped, its surfaces are dropped, and the new set is compiled and started
/// fresh. A plugin therefore loses whatever it was keeping in its scope, which
/// is the same thing a restart would have done and is far easier to reason
/// about than trying to keep some plugins alive across the change.
pub fn reload(session: Arc<Session>, cfg: Arc<Configuration>, env: Arc<Environment>) {
    // The lock is released before `start`, which wants it too.
    let running = {
        let Ok(slot) = control().lock() else { return };
        slot.clone()
    };

    match (cfg.get_bool(ENABLED_KEY), running) {
        (true, Some(tx)) => {
            let _ = tx.send(Wake::Reload);
        }
        (true, None) => start(session, cfg, env),
        (false, Some(tx)) => {
            let _ = tx.send(Wake::Stop);
        }
        (false, None) => {}
    }
}

/// Start the host thread, unless one is already running.
///
/// Unlike before, this does NOT give up when there is nothing to load. The
/// host stays alive for as long as plugins are switched on, so that approving
/// or ticking a plugin later has somewhere to be delivered - otherwise the
/// first plugin someone approves would need a restart after all.
fn start(session: Arc<Session>, cfg: Arc<Configuration>, env: Arc<Environment>) {
    let Ok(mut slot) = control().lock() else { return };
    if slot.is_some() {
        return;
    }

    // Subscribe on the calling thread, before the host thread starts: an event
    // raised between spawn and the first recv would otherwise be lost.
    let rx = session.subscribe();

    // Session events, window clicks and settings changes arrive from three
    // places and the standard library cannot wait on three channels. One relay
    // folds the session into a single queue, which is also what makes the tick
    // deadline work: everything the loop can wake for comes from one place.
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<Wake>();
    {
        let tx = wake_tx.clone();
        ui::set_event_sink(move |origin, event| {
            let _ = tx.send(Wake::Ui(origin, event));
        });
    }

    let relay_tx = wake_tx.clone();
    let relay = std::thread::Builder::new()
        .name("plugin-events".into())
        .spawn(move || {
            while let Ok(event) = rx.recv() {
                if relay_tx.send(Wake::Session(event)).is_err() {
                    return;
                }
            }
            // The session dropped its sender. Said explicitly because the UI
            // and the control channel hold clones of `wake_tx`, so the channel
            // never closes on its own and the loop would wait forever.
            let _ = relay_tx.send(Wake::Shutdown);
        });
    if let Err(err) = relay {
        tracing::error!("could not start the plugin event relay: {err}");
        return;
    }

    let dir = plugin_dir(&env);
    let env = Arc::clone(&env);
    match std::thread::Builder::new()
        .name("plugins".into())
        .spawn(move || run(session, cfg, env, dir, wake_rx))
    {
        Ok(_) => *slot = Some(wake_tx),
        Err(err) => tracing::error!("could not start the plugin host: {err}"),
    }
}

/// The `.rhai` files that should be loaded right now: present on disk, and not
/// individually switched off.
///
/// Switched-off plugins never reach an engine at all - not compiled, not run -
/// so a disabled plugin cannot cost anything or fail.
fn enabled_scripts(dir: &Path, cfg: &Configuration) -> Vec<PathBuf> {
    let off = disabled(cfg);
    match discover(dir) {
        Ok(scripts) => scripts.into_iter().filter(|p| !off.contains(&stem(p))).collect(),
        Err(err) => {
            tracing::error!("could not read plugin folder {}: {err}", dir.display());
            Vec::new()
        }
    }
}

/// Where plugins live: `<app data>/plugins/*.rhai`.
pub fn plugin_dir(env: &Environment) -> PathBuf {
    env.get_application_data_path().join("plugins")
}

/// What a plugin may reach.
///
/// Coarse on purpose. A list of sixteen function names is not a decision
/// anyone can meaningfully make; "may delete your downloaded files" is. Each
/// one gates a group of host functions in `api::register`, and a function
/// whose permission was not granted is never registered - a script calling it
/// gets "function not found", so the failure is closed rather than silent.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Permission {
    /// torrents, torrent, exists, session_rates
    Read,
    /// pause, resume, recheck
    Control,
    /// add_magnet
    Add,
    /// set_label, clear_label
    Labels,
    /// move_storage
    Storage,
    /// remove - including deleting the downloaded files
    Remove,
    /// notify
    Notify,
    /// http_get, add_torrent_url
    ///
    /// The one that changes what the others mean: a plugin holding `read` and
    /// `network` together can send everything it can see to anyone. Nothing
    /// here can detect that combination being misused, which is why the
    /// approval prompt shows the whole set at once rather than one line at a
    /// time.
    Network,
    /// data_get, data_set, data_remove, data_keys
    Data,
    /// ui_window, ui_rows, ui_buttons, ui_input, ui_status, ui_show
    ///
    /// A window of its own, listed under View. Not a way to draw over the
    /// client: the shape is fixed - a list, a text field and some buttons -
    /// so a plugin cannot put up something that looks like NanoTorrent asking
    /// for a password.
    Ui,
    /// run
    ///
    /// The one that is not like the others. Every permission above bounds what
    /// a script may do to the session; this one hands over the account. A
    /// program started by a plugin is a program started by the user: it can
    /// read their files, reach their network and outlive NanoTorrent, and no
    /// permission here constrains it once it is running.
    ///
    /// Granting it is a decision about the plugin's author, not about the
    /// plugin - which is why it sits last in `ALL`, reads last in the approval
    /// prompt, and is described as what it means rather than what it calls.
    Execute,
}

impl Permission {
    pub const ALL: [Permission; 11] = [
        Permission::Read,
        Permission::Control,
        Permission::Add,
        Permission::Labels,
        Permission::Storage,
        Permission::Remove,
        Permission::Notify,
        Permission::Network,
        Permission::Data,
        Permission::Ui,
        // Last on purpose: the approval prompt lists these in order, and the
        // one that matters most should be the one still on screen when someone
        // reaches for the button.
        Permission::Execute,
    ];

    /// The word used in a script's header and in the stored grant.
    pub fn tag(self) -> &'static str {
        match self {
            Permission::Read => "read",
            Permission::Control => "control",
            Permission::Add => "add",
            Permission::Labels => "labels",
            Permission::Storage => "storage",
            Permission::Remove => "remove",
            Permission::Notify => "notify",
            Permission::Network => "network",
            Permission::Data => "data",
            Permission::Ui => "ui",
            Permission::Execute => "execute",
        }
    }

    pub fn parse(tag: &str) -> Option<Permission> {
        Permission::ALL
            .into_iter()
            .find(|p| p.tag().eq_ignore_ascii_case(tag.trim()))
    }

    /// i18n key for the one-line description shown next to the checkbox.
    pub fn describe_key(self) -> &'static str {
        match self {
            Permission::Read => "perm_read",
            Permission::Control => "perm_control",
            Permission::Add => "perm_add",
            Permission::Labels => "perm_labels",
            Permission::Storage => "perm_storage",
            Permission::Remove => "perm_remove",
            Permission::Notify => "perm_notify",
            Permission::Network => "perm_network",
            Permission::Data => "perm_data",
            Permission::Ui => "perm_ui",
            Permission::Execute => "perm_execute",
        }
    }
}

/// What a script asks for, read from its header WITHOUT running it.
///
///     //! permissions: read, control, remove
///
/// Parsed from the source text rather than from the compiled AST, because
/// evaluating a Rhai constant means executing the script - and the whole point
/// is to know what it wants before any of it runs. Only the leading comment
/// block is scanned, so a `permissions:` line further down (or inside a
/// string) cannot quietly widen the request.
///
/// No header means no permissions: a plugin that declares nothing gets `log`
/// and nothing else. Fail closed.
pub fn declared(source: &str) -> (BTreeSet<Permission>, Vec<String>) {
    let mut want = BTreeSet::new();
    let mut unknown = Vec::new();

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(comment) = trimmed.strip_prefix("//") else {
            break; // first real code line ends the header
        };
        let comment = comment.trim_start_matches(['/', '!']).trim();
        let Some(rest) = comment
            .strip_prefix("permissions:")
            .or_else(|| comment.strip_prefix("permissions :"))
        else {
            continue;
        };
        for tag in rest.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match Permission::parse(tag) {
                Some(p) => {
                    want.insert(p);
                }
                // Surfaced rather than ignored: a typo silently dropping a
                // permission looks like the host is broken.
                None => unknown.push(tag.to_owned()),
            }
        }
    }
    (want, unknown)
}

/// What the user has approved, per plugin.
pub const GRANTS_KEY: &str = "plugins.grants";

fn grants(cfg: &Configuration) -> std::collections::BTreeMap<String, BTreeSet<Permission>> {
    let raw = cfg.get_string(GRANTS_KEY).unwrap_or_default();
    let parsed: std::collections::BTreeMap<String, Vec<String>> =
        serde_json::from_str(&raw).unwrap_or_default();
    parsed
        .into_iter()
        .map(|(name, tags)| {
            (
                name,
                tags.iter().filter_map(|t| Permission::parse(t)).collect(),
            )
        })
        .collect()
}

/// Record consent for exactly the set the script asks for today.
///
/// Stored as the set, not a flag: if the script is later edited to ask for
/// more, the stored grant no longer matches what it declares and consent is
/// needed again. Editing it to ask for *less* also re-prompts, which is the
/// harmless direction.
pub fn grant(cfg: &Configuration, name: &str, perms: &BTreeSet<Permission>) {
    let mut all: std::collections::BTreeMap<String, Vec<String>> = grants(cfg)
        .into_iter()
        .map(|(k, v)| (k, v.iter().map(|p| p.tag().to_owned()).collect()))
        .collect();
    all.insert(
        name.to_owned(),
        perms.iter().map(|p| p.tag().to_owned()).collect(),
    );
    cfg.set(GRANTS_KEY, &serde_json::to_string(&all).unwrap_or_default());
}

/// Withdraw consent, so the plugin is held until it is approved again.
pub fn revoke(cfg: &Configuration, name: &str) {
    let mut all: std::collections::BTreeMap<String, Vec<String>> = grants(cfg)
        .into_iter()
        .map(|(k, v)| (k, v.iter().map(|p| p.tag().to_owned()).collect()))
        .collect();
    all.remove(name);
    cfg.set(GRANTS_KEY, &serde_json::to_string(&all).unwrap_or_default());
}

/// Forget approvals for plugins that are no longer on disk.
///
/// Not housekeeping for its own sake. A grant is keyed by plugin name, so a
/// script that is approved, deleted, and later replaced by a DIFFERENT file
/// with the same name and the same declared permissions would inherit the old
/// consent and run without asking. Dropping the grant when the file goes
/// closes that.
pub fn prune_grants(env: &Environment, cfg: &Configuration) {
    let present: BTreeSet<String> = discover(&plugin_dir(env))
        .unwrap_or_default()
        .iter()
        .map(|p| stem(p))
        .collect();

    for name in grants(cfg).into_keys() {
        if !present.contains(&name) {
            tracing::info!("plugin {name} is gone; forgetting its approval");
            revoke(cfg, &name);
        }
    }
}

/// Put the documented example in the plugin folder the first time NanoTorrent
/// runs, switched off.
///
/// Only when the folder does not exist at all: deleting the example must not
/// bring it back on the next start, and an existing folder is someone's own
/// and is left alone. Switched off explicitly rather than relying on the
/// master switch, so it shows unticked in Preferences and stays that way if
/// plugins are later turned on.
pub fn seed_examples(dir: &Path, cfg: &Configuration) {
    let mut offered = seeded(cfg);

    // A folder that already exists was seeded by an older version, which only
    // ever wrote `example`. Recording that here is what stops this putting
    // back a file someone deliberately deleted before upgrading.
    if offered.is_empty() && dir.exists() {
        offered.insert(String::from("example"));
    }

    let pending: Vec<(&str, &str, Option<&str>)> = EXAMPLES
        .iter()
        .filter(|(name, _, _)| !offered.contains(*name))
        .copied()
        .collect();
    if pending.is_empty() {
        return;
    }

    if let Err(err) = std::fs::create_dir_all(dir) {
        tracing::warn!("could not create {}: {err}", dir.display());
        return;
    }

    for (name, source, catalogue) in pending {
        let path = dir.join(format!("{name}.rhai"));
        // Recorded as offered either way: a file already there is someone
        // else's, possibly edited, and must not be overwritten.
        offered.insert(name.to_owned());
        if path.exists() {
            continue;
        }
        match std::fs::write(&path, source) {
            Ok(()) => {
                set_enabled(cfg, name, false);
                tracing::info!("wrote the {name} plugin to {} (switched off)", path.display());
            }
            Err(err) => {
                tracing::warn!("could not write {}: {err}", path.display());
                continue;
            }
        }

        // The strings, if this example has any. Written only when the script
        // was: a catalogue beside no script is litter, and `t()` falls back to
        // its keys anyway if this fails.
        if let Some(text) = catalogue {
            let json = strings::catalogue_path(&path);
            if let Err(err) = std::fs::write(&json, text) {
                tracing::warn!("could not write {}: {err}", json.display());
            }
        }
    }

    cfg.set_persistent(
        SEEDED_KEY,
        &offered.into_iter().collect::<Vec<_>>().join("\n"),
    );
}

/// The examples this build ships, embedded so they travel with the binary
/// rather than needing the installer to place files.
///
/// One that only watches, one that does something with every subsystem the
/// host has - the second is the answer to "what can a plugin actually do?",
/// which the first does not really show - and one that is a feature people ask
/// for rather than a demonstration.
///
/// All three arrive switched OFF. `player` asks for `execute`, which nobody
/// should get by having installed NanoTorrent.
/// Name, script, and the translations that go beside it.
///
/// All three carry one. None of them has an English string left inline, which
/// is the example worth setting: a plugin that shows a person any text at all
/// should be translatable, and the way to do that is a file next to the script.
const EXAMPLES: &[(&str, &str, Option<&str>)] = &[
    (
        "example",
        include_str!("../../docs/plugins/example.rhai"),
        Some(include_str!("../../docs/plugins/example_translations.json")),
    ),
    (
        "rss",
        include_str!("../../docs/plugins/rss.rhai"),
        Some(include_str!("../../docs/plugins/rss_translations.json")),
    ),
    (
        "player",
        include_str!("../../docs/plugins/player.rhai"),
        Some(include_str!("../../docs/plugins/player_translations.json")),
    ),
];

/// Which examples have already been offered, so a new one added in a later
/// version reaches people who already have a plugins folder - and so deleting
/// one keeps it deleted.
///
/// `persistent_object`, not a setting: `Configuration::set` is an UPDATE
/// against a row a migration created, and this needs to work on a profile
/// upgrading from a version that had no such row.
const SEEDED_KEY: &str = "plugins.seeded";

fn seeded(cfg: &Configuration) -> BTreeSet<String> {
    cfg.get_persistent(SEEDED_KEY)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Permission tags, comma-joined, for a log line.
fn tags(perms: &BTreeSet<Permission>) -> String {
    if perms.is_empty() {
        return String::from("no permissions");
    }
    perms.iter().map(|p| p.tag()).collect::<Vec<_>>().join(", ")
}

/// A plugin's name: its file stem, which is what the host logs and the
/// Preferences tab lists.
fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The plugins the user has switched off.
///
/// Newline-separated, not comma: a comma is legal in a filename on every
/// platform this runs on, and a plugin called `a,b` would otherwise be
/// impossible to switch off.
pub fn disabled(cfg: &Configuration) -> BTreeSet<String> {
    cfg.get_string(DISABLED_KEY)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Switch one plugin on or off.
///
/// Writes the setting only. Whoever changed it calls `reload` to make it so -
/// the CLI does not, because it runs before the host starts.
pub fn set_enabled(cfg: &Configuration, name: &str, on: bool) {
    let mut off = disabled(cfg);
    if on {
        off.remove(name);
    } else {
        off.insert(name.to_owned());
    }
    // BTreeSet, so the stored order is stable and the setting does not churn.
    cfg.set(DISABLED_KEY, &off.into_iter().collect::<Vec<_>>().join("
"));
}

/// One plugin as the Preferences tab sees it.
pub struct PluginInfo {
    pub name: String,
    /// The user's switch, independent of whether it compiles. A broken plugin
    /// stays enabled and shows its error - switching itself off would hide the
    /// problem and lose the setting.
    pub enabled: bool,
    /// Why it will not run, if it will not. `None` means it compiled.
    pub error: Option<String>,
    /// What its header asks for.
    pub requested: BTreeSet<Permission>,
    /// True when the user has approved exactly this set. A script edited to
    /// ask for something different lands back here as false.
    pub granted: bool,
    /// Permission words in the header that are not permissions.
    pub unknown: Vec<String>,
}

/// Every plugin on disk, with its switch and whether it compiles.
///
/// Compile only - the top-level statements a plugin runs at load are NOT
/// executed here, because opening a settings dialog must not have side
/// effects. That catches the common case (a syntax error) but not a script
/// that compiles and then fails on its first line; the host logs that one.
pub fn scan(dir: &Path, cfg: &Configuration) -> Vec<PluginInfo> {
    let off = disabled(cfg);
    let approved = grants(cfg);
    let scripts = discover(dir).unwrap_or_default();

    // A bare engine: Rhai resolves function names at call time, not compile
    // time, so the host API is not needed to check syntax.
    let engine = Engine::new();

    scripts
        .iter()
        .map(|path| {
            let name = stem(path);
            let source = std::fs::read_to_string(path).unwrap_or_default();
            let (requested, unknown) = declared(&source);
            PluginInfo {
                enabled: !off.contains(&name),
                error: engine.compile_file(path.clone()).err().map(|e| e.to_string()),
                granted: approved.get(&name).is_some_and(|g| *g == requested),
                requested,
                unknown,
                name,
            }
        })
        .collect()
}

/// Every `.rhai` file in the plugin folder, in a stable order.
///
/// A missing folder is not an error - it is the normal state for anyone who
/// has never written a plugin.
fn discover(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("rhai")))
        .collect();
    // Load order decides which plugin sees an event first; alphabetical is at
    // least predictable, where read_dir order is not.
    found.sort();
    Ok(found)
}

/// The host thread: compile everything once, then dispatch events forever.
fn run(
    session: Arc<Session>,
    cfg: Arc<Configuration>,
    env: Arc<Environment>,
    dir: PathBuf,
    wake_rx: std::sync::mpsc::Receiver<Wake>,
) {
    let engines = |name: &str, perms: &BTreeSet<Permission>, strings: strings::Strings| {
        build_engine(session.clone(), cfg.clone(), name, perms, strings)
    };

    let mut plugins = load(engines, &cfg, &enabled_scripts(&dir, &cfg));
    let mut tr = crate::load_translator(&env, &cfg);
    tracing::info!("plugin host running with {} plugin(s)", plugins.len());

    call_all(&mut plugins, "on_session_start", ());

    // A deadline rather than `recv_timeout`'s idle period: a busy session
    // delivers an event every few seconds, and a plain timeout would mean the
    // tick never fires on exactly the machines that have most to poll for.
    let mut next_tick = std::time::Instant::now() + TICK;
    loop {
        let now = std::time::Instant::now();
        if now >= next_tick {
            next_tick = now + TICK;
            call_all(&mut plugins, "on_tick", ());
        }
        match wake_rx.recv_timeout(next_tick.saturating_duration_since(std::time::Instant::now())) {
            Ok(Wake::Session(event)) => dispatch(&mut plugins, &tr, event),
            // The origin is in scope for the whole handler, which is what lets
            // `ui_show()` tell a click on this machine from one in a browser.
            Ok(Wake::Ui(origin, event)) => {
                ui::with_origin(origin, || deliver_ui(&mut plugins, event));
            }
            // The settings changed. Stop what is running, forget what it drew,
            // and load whatever the configuration now says - the same sequence
            // a restart would have performed, without the restart.
            Ok(Wake::Reload) => {
                call_all(&mut plugins, "on_session_stop", ());
                ui::clear_surfaces();
                plugins = load(engines, &cfg, &enabled_scripts(&dir, &cfg));
                // The language is a setting, so a reload is also when it may
                // have changed.
                tr = crate::load_translator(&env, &cfg);
                tracing::info!("plugins reloaded: {} running", plugins.len());
                call_all(&mut plugins, "on_session_start", ());
                // A reload is not a tick. Without this, one landing just
                // before the deadline fires `on_tick` at a plugin that has
                // been alive for a millisecond.
                next_tick = std::time::Instant::now() + TICK;
            }
            Ok(Wake::Stop | Wake::Shutdown) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Best effort only: the process does not join this thread, so a shutdown
    // fast enough can cut the handler off. Documented as such rather than
    // plumbed into main's exit path - a plugin should not be saving anything it
    // cannot lose here.
    call_all(&mut plugins, "on_session_stop", ());

    // After the last handler, not before: a menu for a plugin nobody is
    // listening to is a dead click, and clearing first would let a plugin put
    // one back from `on_session_stop`.
    ui::clear();

    // Let a later "switch plugins back on" start a fresh host, rather than
    // sending to a thread that has gone.
    if let Ok(mut slot) = control().lock() {
        *slot = None;
    }
    tracing::info!("plugin host stopped");
}

/// A Rhai engine with the limits a plugin runs under, plus the host API.
fn build_engine(
    session: Arc<Session>,
    cfg: Arc<Configuration>,
    name: &str,
    perms: &BTreeSet<Permission>,
    strings: strings::Strings,
) -> Engine {
    let mut engine = Engine::new();
    apply_limits(&mut engine);
    api::register(&mut engine, session, cfg, name, perms, strings);
    engine
}

/// Bound the damage a bad script can do. These are why this is Rhai: with Lua
/// every one of them would be a hand-written debug hook.
///
/// Every limit is set explicitly, including the two that only restate Rhai's
/// release defaults. Rhai halves several of them under `debug_assertions`
/// (function expression depth 32 becomes 16, call stack 64 becomes 8), so
/// leaving them alone means a plugin that compiles for a release build failing
/// to parse in a debug one - which is a difference nobody would think to look
/// for and which no test in a debug build could ever catch.
fn apply_limits(engine: &mut Engine) {
    engine.set_max_operations(MAX_OPERATIONS);
    engine.set_max_call_levels(64);
    engine.set_max_expr_depths(64, 32);

    // Twice what one HTTP response may be, NOT an independent number. A
    // plugin holds a fetched body in a string and then builds something out of
    // it, so the ceiling has to leave room for both. It was 64 KB against a
    // 4 MB fetch limit, which meant any feed larger than 64 KB died with
    // "Length of string too large" - a limit the plugin author cannot see,
    // cannot raise, and did nothing to deserve.
    engine.set_max_string_size(2 * api::HTTP_LIMIT);

    // Items in one feed, elements in one JSON array. Deliberately not tied to
    // the byte limits: this bounds how much a script can build, and a feed
    // with ten thousand entries is pathological rather than large.
    engine.set_max_array_size(50_000);
    engine.set_max_map_size(50_000);
}

/// Compile each script and run its top level once, so it can set up state.
///
/// A script that fails to compile is dropped with a log line rather than
/// taking the host down with it.
fn load(
    make_engine: impl Fn(&str, &BTreeSet<Permission>, strings::Strings) -> Engine,
    cfg: &Configuration,
    scripts: &[PathBuf],
) -> Vec<Plugin> {
    let mut loaded = Vec::new();
    let approved = grants(cfg);

    for path in scripts {
        let name = stem(path);

        // Read before anything is compiled or run: what a script asks for has
        // to be known before any of it executes.
        let source = match std::fs::read_to_string(path) {
            Ok(src) => src,
            Err(err) => {
                tracing::error!("plugin {name}: cannot read: {err}");
                continue;
            }
        };
        let (wants, unknown) = declared(&source);
        for tag in &unknown {
            tracing::warn!("plugin {name}: unknown permission {tag:?}, ignored");
        }

        // Consent is to a specific set. A script edited to ask for more (or
        // less) no longer matches what was approved, and waits again.
        match approved.get(&name) {
            // Nothing to consent to: with no permissions the only host
            // function it has is `log`. Prompting for that would train people
            // to click through the prompts that matter.
            _ if wants.is_empty() => {}
            Some(granted) if *granted == wants => {}
            _ => {
                tracing::warn!(
                    "plugin {name} is waiting for approval of: {}",
                    tags(&wants)
                );
                continue;
            }
        }

        // Said once, at load, rather than on every silently-ignored call: a
        // plugin written for the desktop is not broken on a headless server,
        // it just has nowhere to draw.
        //
        // A compile-time check, not "has a window appeared yet": the host
        // starts before the UI does, so asking at this moment would report no
        // UI on every desktop build too.
        if wants.contains(&Permission::Ui) && !cfg!(feature = "ui-slint") {
            tracing::info!("plugin {name} asks for a window, but this build has no UI");
        }

        // Read here rather than in `api::register`, which never sees a path.
        // A plugin with no `.json` beside it gets an empty catalogue and `t()`
        // answers with its keys - see `strings`.
        let catalogue = strings::Strings::load(path, Arc::new(cfg.clone()));
        let engine = make_engine(&name, &wants, catalogue);
        let ast = match engine.compile_file(path.clone()) {
            Ok(ast) => ast,
            Err(err) => {
                tracing::error!("plugin {name}: {err}");
                continue;
            }
        };

        // Top-level statements run once here. Anything a handler needs later
        // has to live in this scope.
        let mut scope = Scope::new();
        if let Err(err) = engine.run_ast_with_scope(&mut scope, &ast) {
            tracing::error!("plugin {name} failed on load: {err}");
            continue;
        }

        tracing::info!("loaded plugin {name} with: {}", tags(&wants));
        loaded.push(Plugin { name, engine, ast, scope });
    }

    loaded
}

/// Everything the host loop can wake up for.
enum Wake {
    Session(SessionEvent),
    Ui(ui::Origin, ui::UiEvent),
    /// The plugin settings changed: load whatever they now say.
    Reload,
    /// Plugins were switched off. Distinct from `Shutdown` only in what it
    /// says in the log - both end the host.
    Stop,
    /// The session is gone. Sent by the relay rather than inferred from a
    /// closed channel - see the comment where it is sent.
    Shutdown,
}

/// Hand a window click to the plugin that drew the window, and only that one.
///
/// Addressed by name rather than broadcast: two plugins with a `on_ui_row`
/// handler must not both see a click on one of them.
fn deliver_ui(plugins: &mut [Plugin], event: ui::UiEvent) {
    let (name, func, args) = match event {
        ui::UiEvent::Row { plugin, id } => (plugin, "on_ui_row", vec![id]),
        ui::UiEvent::Group { plugin, id } => (plugin, "on_ui_group", vec![id]),
        ui::UiEvent::Button { plugin, id, input } => (plugin, "on_ui_button", vec![id, input]),
        ui::UiEvent::Menu { plugin, id } => (plugin, "on_ui_menu", vec![id]),
        // Four arguments, one of them a number, so it does not fit the
        // strings-only path below either.
        ui::UiEvent::FileMenu {
            plugin,
            id,
            hash,
            index,
            name,
        } => {
            let Some(target) = plugins.iter_mut().find(|p| p.name == plugin) else {
                return;
            };
            if !target.handles("on_file_menu", 4) {
                return;
            }
            let result = target.engine.call_fn::<rhai::Dynamic>(
                &mut target.scope,
                &target.ast,
                "on_file_menu",
                (id, hash, index, name),
            );
            if let Err(err) = result {
                tracing::error!("plugin {}: on_file_menu failed: {err}", target.name);
                ui::report_failure(&target.name, "on_file_menu", &err.to_string());
            }
            return;
        }
        ui::UiEvent::FormCancelled { plugin, id } => (plugin, "on_ui_form_cancel", vec![id]),
        ui::UiEvent::Configure { plugin } => (plugin, "on_ui_configure", Vec::new()),
        ui::UiEvent::Opened { plugin } => (plugin, "on_ui_open", Vec::new()),
        // The one event whose payload is not a list of strings, so it is
        // called here rather than falling through to the shared path below.
        ui::UiEvent::Form { plugin, id, values } => {
            let Some(target) = plugins.iter_mut().find(|p| p.name == plugin) else {
                return;
            };
            if !target.handles("on_ui_form", 2) {
                return;
            }
            let map: rhai::Map = values
                .into_iter()
                .map(|(k, v)| (k.into(), rhai::Dynamic::from(v)))
                .collect();
            let result = target.engine.call_fn::<rhai::Dynamic>(
                &mut target.scope,
                &target.ast,
                "on_ui_form",
                (id, map),
            );
            if let Err(err) = result {
                tracing::error!("plugin {}: on_ui_form failed: {err}", target.name);
                ui::report_failure(&target.name, "on_ui_form", &err.to_string());
            }
            return;
        }
    };

    let Some(plugin) = plugins.iter_mut().find(|p| p.name == name) else {
        return;
    };
    if !plugin.handles(func, args.len()) {
        return;
    }
    let result =
        plugin
            .engine
            .call_fn::<rhai::Dynamic>(&mut plugin.scope, &plugin.ast, func, args);
    if let Err(err) = result {
        tracing::error!("plugin {}: {func} failed: {err}", plugin.name);
        ui::report_failure(&plugin.name, func, &err.to_string());
    }
}

/// Map an event onto the handler name and arguments a script would define.
fn dispatch(plugins: &mut [Plugin], tr: &crate::ui::translator::Translator, event: SessionEvent) {
    match event {
        SessionEvent::TorrentAdded { hash, name } => {
            call_all(plugins, "on_torrent_added", (hash, name))
        }
        SessionEvent::TorrentCompleted { hash, name } => {
            call_all(plugins, "on_torrent_completed", (hash, name))
        }
        SessionEvent::TorrentRemoved { hash, name } => {
            call_all(plugins, "on_torrent_removed", (hash, name))
        }
        SessionEvent::Error(err) => call_all(plugins, "on_error", (err.text(tr),)),
        // Re-adding something already held is not an add. No hook for it: no
        // plugin has asked, and a script that wants it can compare against
        // torrents(). Left explicit rather than a catch-all so a new event
        // fails the build here instead of being silently swallowed.
        SessionEvent::TorrentDuplicate { .. } => {}
    }
}

/// Call one handler on every plugin that defines it.
///
/// Errors are logged per plugin and never propagate: one broken script must
/// not stop the next one from seeing the event.
fn call_all<A: rhai::FuncArgs + Clone>(plugins: &mut [Plugin], func: &str, args: A) {
    let arity = {
        let mut probe = Vec::new();
        args.clone().parse(&mut probe);
        probe.len()
    };

    for plugin in plugins.iter_mut() {
        if !plugin.handles(func, arity) {
            continue;
        }
        let result = plugin.engine.call_fn::<rhai::Dynamic>(
            &mut plugin.scope,
            &plugin.ast,
            func,
            args.clone(),
        );
        if let Err(err) = result {
            tracing::error!("plugin {}: {func} failed: {err}", plugin.name);
            // A plugin whose `on_tick` dies every minute would otherwise sit
            // there looking busy - the window is where someone is looking.
            ui::report_failure(&plugin.name, func, &err.to_string());
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A scratch plugin folder holding the given `(name, source)` scripts.
    fn folder(tag: &str, scripts: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nanotorrent-plugins-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, source) in scripts {
            std::fs::write(dir.join(name), source).unwrap();
        }
        dir
    }

    /// An engine that records what a script asked it to, instead of touching a
    /// real session. The dispatch machinery is what is under test here, not
    /// the API surface.
    fn recording_engine_shared(log: Arc<Mutex<Vec<String>>>) -> Engine {
        let mut engine = Engine::new();
        let sink = log.clone();
        engine.register_fn("record", move |what: &str| {
            sink.lock().unwrap().push(what.to_string());
        });
        engine
    }

    /// A Configuration on an in-memory database, for load()'s consent check.
    fn test_cfg() -> Configuration {
        let db = Arc::new(crate::core::database::Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        Configuration::new(db)
    }

    /// The buffer each test's scripts record into.
    fn recorder() -> Arc<Mutex<Vec<String>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    /// The three things dispatch has to get right at once: call the handler
    /// that matches, skip the plugin that does not define it, and keep going
    /// after one that fails.
    use crate::bittorrent::session::SessionError;

    /// A translator for the dispatch tests. They care which hook ran, not what
    /// language it was told in.
    fn en() -> crate::ui::translator::Translator {
        crate::ui::translator::Translator::load(Path::new("no-such-lang-dir"), "en-US")
    }

    #[test]
    fn dispatch_calls_matching_handlers_and_survives_a_failing_one() {
        let dir = folder(
            "dispatch",
            &[
                // Sorted load order puts this first, so it throws before the
                // plugin that records - which is the point.
                ("a_broken.rhai", r#"fn on_torrent_completed(hash, name) { throw "boom"; }"#),
                (
                    "b_good.rhai",
                    r#"fn on_torrent_completed(hash, name) { record("done:" + name); }"#,
                ),
                ("c_other.rhai", r#"fn on_error(message) { record("err"); }"#),
            ],
        );

        let seen = recorder();
        let scripts = discover(&dir).unwrap();
        assert_eq!(scripts.len(), 3, "every .rhai file is discovered");

        let log2 = seen.clone();
        let mut plugins = load(|_, _, _| recording_engine_shared(log2.clone()), &test_cfg(), &scripts);
        assert_eq!(plugins.len(), 3, "every script compiles and loads");

        dispatch(
            &mut plugins,
            &en(),
            SessionEvent::TorrentCompleted {
                hash: "abc".into(),
                name: "Ubuntu".into(),
            },
        );

        assert_eq!(
            *seen.lock().unwrap(),
            vec!["done:Ubuntu"],
            "the matching handler ran; the throwing plugin did not stop it and \
             the on_error-only plugin was not called"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A syntax error is one plugin's problem, not the host's.
    #[test]
    fn a_script_that_does_not_compile_is_skipped() {
        let dir = folder(
            "badsyntax",
            &[
                ("broken.rhai", "fn on_error(message) { this is not rhai"),
                ("fine.rhai", r#"fn on_error(message) { record("ok"); }"#),
            ],
        );

        let seen = recorder();
        let log2 = seen.clone();
        let mut plugins = load(|_, _, _| recording_engine_shared(log2.clone()), &test_cfg(), &discover(&dir).unwrap());
        assert_eq!(plugins.len(), 1, "only the valid script loads");

        dispatch(
            &mut plugins,
            &en(),
            SessionEvent::Error(SessionError::raw("disk full")),
        );
        assert_eq!(*seen.lock().unwrap(), vec!["ok"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Arity is part of the match: a handler taking the wrong number of
    /// arguments is a mistake to report, not a call to make.
    #[test]
    fn a_handler_with_the_wrong_arity_is_not_called() {
        let dir = folder(
            "arity",
            &[("wrong.rhai", r#"fn on_torrent_completed(hash) { record("nope"); }"#)],
        );

        let seen = recorder();
        let log2 = seen.clone();
        let mut plugins = load(|_, _, _| recording_engine_shared(log2.clone()), &test_cfg(), &discover(&dir).unwrap());

        dispatch(
            &mut plugins,
            &en(),
            SessionEvent::TorrentCompleted {
                hash: "abc".into(),
                name: "Ubuntu".into(),
            },
        );

        assert!(seen.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The documented example has to stay valid Rhai and keep defining the
    /// handlers the docs say it does - otherwise the first thing anyone copies
    /// is broken.
    #[test]
    fn the_documented_example_compiles_and_defines_its_handlers() {
        let source = include_str!("../../docs/plugins/example.rhai");
        let engine = Engine::new();
        let ast = engine
            .compile(source)
            .expect("docs/plugins/example.rhai must compile");

        let plugin = Plugin {
            name: "example".into(),
            engine: Engine::new(),
            ast,
            scope: Scope::new(),
        };

        for (handler, arity) in [
            ("on_session_start", 0),
            ("on_torrent_added", 2),
            ("on_torrent_completed", 2),
            ("on_error", 1),
        ] {
            assert!(
                plugin.handles(handler, arity),
                "the example should define {handler}/{arity}"
            );
        }
    }

    /// A click on a plugin's file-menu item reaches that plugin, with the file
    /// it was used on, and reaches nobody else.
    #[test]
    fn a_file_menu_click_carries_the_file_to_one_plugin() {
        let dir = folder(
            "filemenu",
            &[
                (
                    "mine.rhai",
                    r#"fn on_file_menu(id, hash, index, name) {
                           record(id + "|" + hash + "|" + index + "|" + name);
                       }"#,
                ),
                // Same handler, not addressed: a second plugin must not see it.
                (
                    "other.rhai",
                    r#"fn on_file_menu(id, hash, index, name) { record("other"); }"#,
                ),
            ],
        );

        let seen = recorder();
        let scripts = discover(&dir).unwrap();
        let log2 = seen.clone();
        let mut plugins = load(|_, _, _| recording_engine_shared(log2.clone()), &test_cfg(), &scripts);

        deliver_ui(
            &mut plugins,
            ui::UiEvent::FileMenu {
                plugin: "mine".into(),
                id: "play".into(),
                hash: "abc".into(),
                index: 2,
                name: "ep01.mkv".into(),
            },
        );

        assert_eq!(*seen.lock().unwrap(), vec!["play|abc|2|ep01.mkv"]);

        // A plugin that never declared the handler is not an error, and a name
        // nobody has is not a panic.
        deliver_ui(
            &mut plugins,
            ui::UiEvent::FileMenu {
                plugin: "nobody".into(),
                id: "play".into(),
                hash: "abc".into(),
                index: 0,
                name: "x.mkv".into(),
            },
        );
        assert_eq!(seen.lock().unwrap().len(), 1, "nothing else ran");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The player example is the first thing anyone will copy for "run a
    /// program from a plugin", so it has to compile and to declare the
    /// handlers the file menu actually calls.
    #[test]
    fn the_player_example_compiles_and_handles_the_file_menu() {
        let source = include_str!("../../docs/plugins/player.rhai");
        let engine = Engine::new();
        let ast = engine
            .compile(source)
            .expect("docs/plugins/player.rhai must compile");

        let plugin = Plugin {
            name: "player".into(),
            engine: Engine::new(),
            ast,
            scope: Scope::new(),
        };

        for (handler, arity) in [
            ("on_session_start", 0),
            ("on_file_menu", 4),
            ("on_ui_configure", 0),
            ("on_ui_form", 2),
        ] {
            assert!(
                plugin.handles(handler, arity),
                "the player example should define {handler}/{arity}"
            );
        }

        // It cannot work without `execute`, and asking for more than it uses
        // is the thing the permission prompt exists to make visible.
        let (asked, unknown) = declared(source);
        assert!(unknown.is_empty(), "its header has no typo in it");
        assert_eq!(
            asked,
            [
                Permission::Read,
                Permission::Notify,
                Permission::Data,
                Permission::Ui,
                Permission::Execute,
            ]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        );
    }

    /// Every `t("key")` in every shipped plugin has a string, and no language
    /// carries a key nothing asks for.
    ///
    /// Silent otherwise: a missing key renders as the key, which looks like a
    /// typo in the plugin rather than a gap in its translations.
    #[test]
    fn the_shipped_plugins_match_their_catalogues() {
        use std::collections::BTreeSet;

        // (name, script, catalogue, keys the scan cannot see whole)
        let shipped: [(&str, &str, &str, &[&str]); 3] = [
            (
                "example",
                include_str!("../../docs/plugins/example.rhai"),
                include_str!("../../docs/plugins/example_translations.json"),
                &[],
            ),
            (
                "rss",
                include_str!("../../docs/plugins/rss.rhai"),
                include_str!("../../docs/plugins/rss_translations.json"),
                &[],
            ),
            (
                // `t("player_" + id)` is built at run time, so the scan sees
                // only the prefix - the four ids are named here instead.
                "player",
                include_str!("../../docs/plugins/player.rhai"),
                include_str!("../../docs/plugins/player_translations.json"),
                // Derived below from the plugin's own PLAYERS table rather
                // than listed here: the dropdown labels are built as
                // `t("player_" + id)`, which the scan cannot see, and a list
                // written out by hand would let a newly added player ship with
                // no string at all - visible only as a raw key in the dropdown.
                &[],
            ),
        ];

        for (name, source, json, extra) in shipped {
            let catalogue: super::strings::Catalog =
                serde_json::from_str(json).unwrap_or_else(|e| panic!("{name}: {e}"));

            let mut wanted: BTreeSet<String> = extra.iter().map(|k| (*k).to_owned()).collect();

            // Keys built at run time from a table in the script itself. Each
            // `#{ id: "x", os: [...] }` in the player's PLAYERS list becomes a
            // `player_x` label, so the two are tied together here instead of
            // being kept in step by hand.
            for (at, _) in source.match_indices("id: \"") {
                let tail = &source[at + 5..];
                let Some(end) = tail.find('"') else { continue };
                let id = &tail[..end];
                let after = tail[end..]
                    .trim_start_matches('"')
                    .trim_start()
                    .trim_start_matches(',')
                    .trim_start();
                if after.starts_with("os:") {
                    wanted.insert(format!("player_{id}"));
                }
            }
            let bytes = source.as_bytes();
            let mut rest = source;
            while let Some(at) = rest.find("t(\"") {
                let absolute = rest.as_ptr() as usize - source.as_ptr() as usize + at;
                // `split("` also ends in `t("`. The character before has to be
                // one a name cannot end with.
                let part_of_a_name = absolute > 0
                    && (bytes[absolute - 1].is_ascii_alphanumeric() || bytes[absolute - 1] == b'_');
                let tail = &rest[at + 3..];
                let Some(end) = tail.find('"') else { break };
                if !part_of_a_name {
                    wanted.insert(tail[..end].to_owned());
                }
                rest = &tail[end..];
            }
            // Fragments of a built key, and the empty string from `t("")`-shaped
            // noise, are not keys.
            wanted.retain(|k| !k.is_empty() && !k.ends_with('_'));

            let english: BTreeSet<String> = catalogue
                .get("en-US")
                .unwrap_or_else(|| panic!("{name} has no en-US"))
                .keys()
                .cloned()
                .collect();

            let missing: Vec<&String> = wanted.difference(&english).collect();
            assert!(missing.is_empty(), "{name}: en-US has no string for {missing:?}");

            let unused: Vec<&String> = english.difference(&wanted).collect();
            assert!(unused.is_empty(), "{name}: en-US carries {unused:?}, unused");

            // Other languages are checked against en-US, not against the script:
            // a half-translated plugin falls back and is fine, but a key
            // MISSPELLED in one language is a string that silently never shows.
            for (locale, strings) in &catalogue {
                let strays: Vec<&String> =
                    strings.keys().filter(|k| !english.contains(*k)).collect();
                assert!(strays.is_empty(), "{name}/{locale} carries {strays:?}, not in en-US");
            }
        }
    }

    /// A missing plugin folder is the normal state, not an error.
    #[test]
    fn a_missing_folder_yields_no_plugins() {
        let dir = std::env::temp_dir().join("nanotorrent-plugins-does-not-exist");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(discover(&dir).unwrap().is_empty());
    }

    /// The switch survives a round trip, a broken plugin still reports as
    /// enabled, and a switched-off one is not compiled away into looking fine.
    #[test]
    fn per_plugin_switch_and_error_reporting() {
        use crate::core::database::Database;

        let dir = folder(
            "switches",
            &[
                ("good.rhai", "fn on_session_start() { }"),
                ("broken.rhai", "fn on_session_start( {"),
            ],
        );
        let db = Arc::new(Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        let cfg = Configuration::new(db);

        let by_name = |v: Vec<PluginInfo>| -> std::collections::BTreeMap<String, PluginInfo> {
            v.into_iter().map(|p| (p.name.clone(), p)).collect()
        };

        // Nothing switched off yet: both on, only the broken one carries an error.
        let found = by_name(scan(&dir, &cfg));
        assert_eq!(found.len(), 2, "expected both scripts");
        assert!(found["good"].enabled && found["good"].error.is_none());
        assert!(
            found["broken"].enabled && found["broken"].error.is_some(),
            "a plugin that does not compile must still read as enabled, with the reason"
        );

        // Switch one off, and back on again.
        set_enabled(&cfg, "good", false);
        let found = by_name(scan(&dir, &cfg));
        assert!(!found["good"].enabled);
        assert!(found["broken"].enabled, "switching one off must not touch the other");

        set_enabled(&cfg, "good", true);
        assert!(by_name(scan(&dir, &cfg))["good"].enabled);
        assert!(
            disabled(&cfg).is_empty(),
            "re-enabling the last one should leave the list empty, not a stray separator"
        );

        // A name with a comma in it, which is why the list is newline-separated.
        set_enabled(&cfg, "a,b", false);
        assert!(disabled(&cfg).contains("a,b"));
    }

    /// Every permission round-trips through its tag and has a description key.
    ///
    /// A permission missing from `ALL` is invisible to the approval prompt but
    /// still grantable by a stored tag, which would be a silent grant. A
    /// missing description key shows the user a humanised key name where the
    /// sentence explaining the risk should be.
    #[test]
    fn every_permission_is_listed_named_and_described() {
        for p in Permission::ALL {
            assert_eq!(Permission::parse(p.tag()), Some(p), "{p:?} does not round-trip");
            assert!(!p.describe_key().is_empty(), "{p:?} has no description key");
            assert!(
                p.describe_key().starts_with("perm_"),
                "{p:?} description key is not a perm_ key"
            );
        }
        // Tags are unique, or two permissions would grant each other.
        let tags: BTreeSet<&str> = Permission::ALL.iter().map(|p| p.tag()).collect();
        assert_eq!(tags.len(), Permission::ALL.len(), "two permissions share a tag");

        assert_eq!(Permission::parse("EXECUTE"), Some(Permission::Execute), "case");
        assert_eq!(Permission::parse("  execute "), Some(Permission::Execute), "spacing");
        assert_eq!(Permission::parse("exec"), None, "not an abbreviation");

        // `execute` is last so it is the line still on screen when someone
        // reaches for Approve. If that ordering is ever changed, change the
        // reasoning in the enum with it.
        assert_eq!(
            Permission::ALL.last(),
            Some(&Permission::Execute),
            "execute must read last in the approval prompt"
        );
    }

    /// The header is read without running anything, and a plugin that asks for
    /// something is held until the user has approved exactly that.
    #[test]
    fn permissions_are_declared_parsed_and_enforced() {
        // --- parsing ---
        let (want, unknown) = declared("//! permissions: read, remove
fn f() {}");
        assert_eq!(want, BTreeSet::from([Permission::Read, Permission::Remove]));
        assert!(unknown.is_empty());

        let (want, unknown) = declared("// permissions: read, wat
");
        assert_eq!(want, BTreeSet::from([Permission::Read]));
        assert_eq!(unknown, vec!["wat"], "a typo must be reported, not swallowed");

        // Only the leading comment block counts: a line further down, or one
        // inside a string, must not widen the request.
        let (want, _) = declared("fn f() { let s = \"// permissions: remove\"; }");
        assert!(want.is_empty(), "a permissions line after code must be ignored");

        assert!(declared("fn f() {}").0.is_empty(), "no header means no permissions");

        // --- the consent gate ---
        let dir = folder(
            "perms",
            &[
                ("quiet.rhai", "fn on_session_start() { }"),
                ("wants.rhai", "//! permissions: read
fn on_session_start() { }"),
            ],
        );
        let cfg = test_cfg();
        let scripts = discover(&dir).unwrap();
        let load_now = |cfg: &Configuration| {
            load(|_, _, _| Engine::new(), cfg, &scripts)
                .into_iter()
                .map(|p| p.name)
                .collect::<Vec<_>>()
        };

        // Nothing approved yet: the one asking for nothing still runs, because
        // it can only log; the one asking for `read` is held.
        assert_eq!(load_now(&cfg), vec!["quiet"], "an unapproved request must not load");

        grant(&cfg, "wants", &BTreeSet::from([Permission::Read]));
        assert_eq!(load_now(&cfg), vec!["quiet", "wants"], "approving it should let it load");

        // Consent is to a set, not a flag: approving something else is not
        // approval of what it actually asks for.
        grant(&cfg, "wants", &BTreeSet::from([Permission::Read, Permission::Remove]));
        assert_eq!(
            load_now(&cfg),
            vec!["quiet"],
            "a grant that no longer matches the header must be re-asked"
        );

        revoke(&cfg, "wants");
        assert_eq!(load_now(&cfg), vec!["quiet"], "revoking holds it again");
    }

    /// Can a handler see a `let` from the script's top level? The shipped
    /// example depends on the answer, so it is checked rather than assumed.
    #[test]
    fn top_level_state_is_visible_to_handlers() {
        let engine = Engine::new();
        let ast = engine
            .compile("let counter = 41;
fn bump() { counter += 1; counter }")
            .unwrap();
        let mut scope = Scope::new();
        engine.run_ast_with_scope(&mut scope, &ast).unwrap();
        let got = engine.call_fn::<i64>(&mut scope, &ast, "bump", ()).unwrap();
        assert_eq!(
            got, 42,
            "handlers must see and mutate the script's top-level state - the              shipped example keeps a counter that way"
        );
    }

    /// Every settings key this module writes must exist as a row.
    ///
    /// `Configuration::set` is an UPDATE, so a key with no migration row is
    /// not an error - it silently does nothing. That is precisely how the
    /// Approve button came to look dead: `plugins.grants` was added to a
    /// migration that had already run.
    #[test]
    fn every_plugin_setting_has_a_row_to_write_to() {
        let db = Arc::new(crate::core::database::Database::open_in_memory().unwrap());
        db.migrate().unwrap();
        let cfg = Configuration::new(db);

        for key in [ENABLED_KEY, DISABLED_KEY, GRANTS_KEY] {
            cfg.set(key, &"probe");
            assert_eq!(
                cfg.get_string(key).as_deref(),
                Some("probe"),
                "{key} has no row in the migrations, so set() silently does nothing"
            );
        }
    }

    /// A grant must not outlive the file it was given to: same name, same
    /// declared permissions, different code would otherwise inherit consent.
    #[test]
    fn a_deleted_plugins_approval_is_forgotten() {
        let dir = folder("prune", &[("keeper.rhai", "//! permissions: read
fn f() {}")]);
        let cfg = test_cfg();

        grant(&cfg, "keeper", &BTreeSet::from([Permission::Read]));
        grant(&cfg, "gone", &BTreeSet::from([Permission::Remove]));

        // prune_grants takes an Environment; drive the same logic directly.
        let present: BTreeSet<String> = discover(&dir).unwrap().iter().map(|p| stem(p)).collect();
        for name in grants(&cfg).into_keys() {
            if !present.contains(&name) {
                revoke(&cfg, &name);
            }
        }

        let left = grants(&cfg);
        assert!(left.contains_key("keeper"), "a plugin still on disk keeps its approval");
        assert!(
            !left.contains_key("gone"),
            "a deleted plugin's approval must not wait for the next file of that name"
        );
    }

    /// What a reload will pick up: whatever is on disk right now, minus the
    /// plugins that are switched off.
    ///
    /// This is the whole difference between "applies immediately" and "needs a
    /// restart" - the host re-asks this question instead of keeping the list
    /// it was handed at startup.
    #[test]
    fn a_reload_sees_the_settings_as_they_are_now() {
        let dir = folder(
            "reload",
            &[
                ("alpha.rhai", "fn on_session_start() { }"),
                ("beta.rhai", "fn on_session_start() { }"),
                ("notes.txt", "not a plugin"),
            ],
        );
        let cfg = test_cfg();

        let names = |cfg: &Configuration| -> Vec<String> {
            enabled_scripts(&dir, cfg).iter().map(|p| stem(p)).collect()
        };

        // Everything present, nothing switched off. The .txt is not a plugin.
        assert_eq!(names(&cfg), vec!["alpha", "beta"]);

        // Switching one off takes it out of the next load, with no restart in
        // between - the setting is re-read, not remembered.
        set_enabled(&cfg, "beta", false);
        assert_eq!(names(&cfg), vec!["alpha"]);

        set_enabled(&cfg, "beta", true);
        assert_eq!(names(&cfg), vec!["alpha", "beta"]);

        // A plugin deleted while running is simply gone from the next load,
        // rather than an error that stops the others loading.
        std::fs::remove_file(dir.join("alpha.rhai")).unwrap();
        assert_eq!(names(&cfg), vec!["beta"]);
    }

    /// A missing folder is not an error here either: someone can switch
    /// plugins on before they have written one.
    #[test]
    fn a_reload_with_no_plugin_folder_finds_nothing() {
        let dir = std::env::temp_dir().join("nanotorrent-plugins-reload-absent");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(enabled_scripts(&dir, &test_cfg()).is_empty());
    }

    /// A fresh profile gets both examples, switched off.
    #[test]
    fn a_new_profile_is_given_every_example() {
        let dir = std::env::temp_dir().join("nanotorrent-plugins-seed-new");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = test_cfg();

        seed_examples(&dir, &cfg);

        for name in ["example", "rss"] {
            assert!(dir.join(format!("{name}.rhai")).exists(), "{name} should be written");
            assert!(disabled(&cfg).contains(name), "{name} should be switched off");
        }
    }

    /// The case this exists for: upgrading from a version that only shipped
    /// `example`. The folder is already there, so the old "only if the folder
    /// is missing" rule would have withheld the new plugin forever.
    #[test]
    fn an_existing_profile_is_given_only_what_is_new() {
        let dir = std::env::temp_dir().join("nanotorrent-plugins-seed-upgrade");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // As an older version left it, and edited since.
        std::fs::write(dir.join("example.rhai"), "// mine now").unwrap();
        let cfg = test_cfg();

        seed_examples(&dir, &cfg);

        assert!(dir.join("rss.rhai").exists(), "the new example should arrive");
        assert_eq!(
            std::fs::read_to_string(dir.join("example.rhai")).unwrap(),
            "// mine now",
            "an existing file is someone else's and must not be overwritten"
        );
    }

    /// Deleting an example keeps it deleted, which is the property the old
    /// folder-exists check was really protecting.
    #[test]
    fn a_deleted_example_is_not_written_back() {
        let dir = std::env::temp_dir().join("nanotorrent-plugins-seed-deleted");
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = test_cfg();

        seed_examples(&dir, &cfg);
        std::fs::remove_file(dir.join("rss.rhai")).unwrap();

        seed_examples(&dir, &cfg);
        assert!(
            !dir.join("rss.rhai").exists(),
            "an example the user removed must stay removed"
        );
    }
}
