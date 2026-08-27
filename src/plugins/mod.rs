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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rhai::{AST, Engine, Scope};

use crate::bittorrent::session::{Session, SessionEvent};
use crate::core::configuration::Configuration;
use crate::core::environment::Environment;

/// Master switch. Off by default: a plugin is arbitrary code, so it takes a
/// deliberate act to enable one.
pub const ENABLED_KEY: &str = "plugins.enabled";

/// Ceiling on a single handler call, in Rhai operations. A script that loops
/// forever dies here instead of pinning a core.
const MAX_OPERATIONS: u64 = 500_000;

/// One loaded script.
struct Plugin {
    name: String,
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

    let dir = plugin_dir(&env);
    let scripts = match discover(&dir) {
        Ok(scripts) if scripts.is_empty() => {
            tracing::info!("plugins enabled, none found in {}", dir.display());
            return;
        }
        Ok(scripts) => scripts,
        Err(err) => {
            tracing::error!("could not read plugin folder {}: {err}", dir.display());
            return;
        }
    };

    // Subscribe on the calling thread, before the host thread starts: an event
    // raised between spawn and the first recv would otherwise be lost.
    let rx = session.subscribe();

    let spawned = std::thread::Builder::new()
        .name("plugins".into())
        .spawn(move || run(session, scripts, rx));

    if let Err(err) = spawned {
        tracing::error!("could not start the plugin host: {err}");
    }
}

/// Where plugins live: `<app data>/plugins/*.rhai`.
pub fn plugin_dir(env: &Environment) -> PathBuf {
    env.get_application_data_path().join("plugins")
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
fn run(session: Arc<Session>, scripts: Vec<PathBuf>, rx: std::sync::mpsc::Receiver<SessionEvent>) {
    let engine = build_engine(session.clone());
    let mut plugins = load(&engine, &scripts);

    if plugins.is_empty() {
        tracing::warn!("no plugin loaded successfully - the host is idle");
        return;
    }
    tracing::info!("plugin host running with {} plugin(s)", plugins.len());

    call_all(&engine, &mut plugins, "on_session_start", ());

    // Ends when the session drops the sender, i.e. at shutdown.
    while let Ok(event) = rx.recv() {
        dispatch(&engine, &mut plugins, event);
    }

    // Best effort only: the process does not join this thread, so a shutdown
    // fast enough can cut the handler off. Documented as such rather than
    // plumbed into main's exit path - a plugin should not be saving anything it
    // cannot lose here.
    call_all(&engine, &mut plugins, "on_session_stop", ());
    tracing::info!("plugin host stopped");
}

/// A Rhai engine with the limits a plugin runs under, plus the host API.
fn build_engine(session: Arc<Session>) -> Engine {
    let mut engine = Engine::new();

    // Bound the damage a bad script can do. These are why this is Rhai: with
    // Lua every one of them would be a hand-written debug hook.
    engine.set_max_operations(MAX_OPERATIONS);
    engine.set_max_call_levels(64);
    engine.set_max_string_size(64 * 1024);
    engine.set_max_array_size(10_000);
    engine.set_max_map_size(10_000);

    api::register(&mut engine, session);
    engine
}

/// Compile each script and run its top level once, so it can set up state.
///
/// A script that fails to compile is dropped with a log line rather than
/// taking the host down with it.
fn load(engine: &Engine, scripts: &[PathBuf]) -> Vec<Plugin> {
    let mut loaded = Vec::new();

    for path in scripts {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

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

        tracing::info!("loaded plugin {name}");
        loaded.push(Plugin { name, ast, scope });
    }

    loaded
}

/// Map an event onto the handler name and arguments a script would define.
fn dispatch(engine: &Engine, plugins: &mut [Plugin], event: SessionEvent) {
    match event {
        SessionEvent::TorrentAdded { hash, name } => {
            call_all(engine, plugins, "on_torrent_added", (hash, name))
        }
        SessionEvent::TorrentCompleted { hash, name } => {
            call_all(engine, plugins, "on_torrent_completed", (hash, name))
        }
        SessionEvent::TorrentRemoved { hash, name } => {
            call_all(engine, plugins, "on_torrent_removed", (hash, name))
        }
        SessionEvent::Error(message) => call_all(engine, plugins, "on_error", (message,)),
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
fn call_all<A: rhai::FuncArgs + Clone>(
    engine: &Engine,
    plugins: &mut [Plugin],
    func: &str,
    args: A,
) {
    let arity = {
        let mut probe = Vec::new();
        args.clone().parse(&mut probe);
        probe.len()
    };

    for plugin in plugins.iter_mut() {
        if !plugin.handles(func, arity) {
            continue;
        }
        let result =
            engine.call_fn::<rhai::Dynamic>(&mut plugin.scope, &plugin.ast, func, args.clone());
        if let Err(err) = result {
            tracing::error!("plugin {}: {func} failed: {err}", plugin.name);
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
    fn recording_engine() -> (Engine, Arc<Mutex<Vec<String>>>) {
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut engine = Engine::new();
        engine.set_max_operations(MAX_OPERATIONS);
        let sink = seen.clone();
        engine.register_fn("record", move |what: &str| {
            sink.lock().unwrap().push(what.to_string());
        });
        (engine, seen)
    }

    /// The three things dispatch has to get right at once: call the handler
    /// that matches, skip the plugin that does not define it, and keep going
    /// after one that fails.
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

        let (engine, seen) = recording_engine();
        let scripts = discover(&dir).unwrap();
        assert_eq!(scripts.len(), 3, "every .rhai file is discovered");

        let mut plugins = load(&engine, &scripts);
        assert_eq!(plugins.len(), 3, "every script compiles and loads");

        dispatch(
            &engine,
            &mut plugins,
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

        let (engine, seen) = recording_engine();
        let mut plugins = load(&engine, &discover(&dir).unwrap());
        assert_eq!(plugins.len(), 1, "only the valid script loads");

        dispatch(&engine, &mut plugins, SessionEvent::Error("disk full".into()));
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

        let (engine, seen) = recording_engine();
        let mut plugins = load(&engine, &discover(&dir).unwrap());

        dispatch(
            &engine,
            &mut plugins,
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

    /// A missing plugin folder is the normal state, not an error.
    #[test]
    fn a_missing_folder_yields_no_plugins() {
        let dir = std::env::temp_dir().join("nanotorrent-plugins-does-not-exist");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(discover(&dir).unwrap().is_empty());
    }
}
