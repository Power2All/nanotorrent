// What a plugin is allowed to put on screen.
//
// Three surfaces, each opted into from the script and none of them appearing
// unless it asks:
//
//   - a menu of its own in the main window's menu bar, titled by the plugin
//   - a window: a list, a text field and a row of buttons
//   - a Configure button on its row in Preferences, for a plugin that needs
//     setting up before it can do anything useful
//
// Deliberately not a layout language. The vocabulary is fixed so a plugin says
// what goes in its surfaces and NanoTorrent decides how they look - which is
// what stops a script drawing something that passes for part of the client.
// Widening this later is easy; narrowing it once plugins depend on it is not.
//
// Two threads meet here. The plugin host owns the engines and writes the state;
// the Slint event loop reads it and sends clicks back. Neither calls into the
// other directly: the host writes and asks for a repaint, the UI reads and
// posts an event. That is why this module holds plain data and no Slint types.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

/// One line in a plugin's list.
#[derive(Clone, Default)]
pub struct Row {
    /// Handed back to `on_ui_row`. The plugin's own identifier - a feed URL, a
    /// torrent hash, whatever it needs to act on the click.
    pub id: String,
    pub title: String,
    pub subtitle: String,
    /// Drawn as the current one. The plugin decides what is selected, because
    /// the plugin is what the selection means something to - the window does
    /// not track it, so a reload cannot leave the two disagreeing.
    pub selected: bool,
}

/// Everything the UI knows about one plugin.
#[derive(Clone, Default)]
pub struct PluginUi {
    // ---- its window ----------------------------------------------------
    /// Empty until the plugin calls `ui_window`, which is also what decides
    /// whether it has a window at all.
    pub title: String,
    /// One line under the list. Where a plugin says "checked 4 feeds, 2 new".
    pub status: String,
    /// Placeholder for the text field. Empty means no field at all.
    pub placeholder: String,
    /// `(id, label)` pairs, drawn left to right.
    pub buttons: Vec<(String, String)>,
    /// An optional upper list, above the main one: the things the main list is
    /// showing the contents OF. Empty means one list, as before.
    pub groups: Vec<Row>,
    pub rows: Vec<Row>,

    // ---- its menu ------------------------------------------------------
    /// The dropdown's name in the menu bar. Empty falls back to the plugin's
    /// own name, so a plugin need not repeat itself.
    pub menu_title: String,
    /// `(id, label)` pairs. Empty means no dropdown: a menu with no items is
    /// a title that opens onto nothing.
    pub menu_items: Vec<(String, String)>,

    // ---- its settings --------------------------------------------------
    /// The plugin says it needs setting up before it will do anything useful,
    /// so Preferences offers a Configure button on its row.
    pub configurable: bool,
}

impl PluginUi {
    /// A plugin has a window once it has named one.
    pub fn has_window(&self) -> bool {
        !self.title.is_empty()
    }
}

/// A click, on its way back to the plugin that drew it.
pub enum UiEvent {
    Row { plugin: String, id: String },
    /// A row in the upper list.
    Group { plugin: String, id: String },
    Button { plugin: String, id: String, input: String },
    /// An item in the plugin's own menu-bar dropdown.
    Menu { plugin: String, id: String },
    /// Configure, on the plugin's row in Preferences.
    Configure { plugin: String },
    /// Its window was opened. The plugin's chance to fill it before it is
    /// looked at.
    Opened { plugin: String },
}

/// The callbacks out of this module, held as `Arc` so they can be lifted out
/// of the lock and called with nothing held.
///
/// That matters: the repaint callback wakes the UI thread, which immediately
/// wants this same lock to read what changed. Calling it while holding the
/// lock is an invitation to a deadlock the day one of these callbacks starts
/// doing something less trivial than posting to a queue.
#[derive(Default)]
struct Registry {
    plugins: BTreeMap<String, PluginUi>,
    /// Into the plugin host's loop. A closure rather than a Sender so this
    /// module does not have to know the host's own event type. None until the
    /// host is running, which is the normal state when plugins are off.
    events: Option<Arc<dyn Fn(UiEvent) + Send + Sync>>,
    /// Set by the Slint side. None in a headless build, which is why every
    /// `ui_*` host function is a no-op there rather than an error.
    repaint: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Put one plugin's window on screen. Separate from `repaint` because
    /// declaring a window and opening it are different acts.
    open: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Called by the plugin host once its loop is up.
pub fn set_event_sink(sink: impl Fn(UiEvent) + Send + Sync + 'static) {
    if let Ok(mut reg) = registry().lock() {
        reg.events = Some(Arc::new(sink));
    }
}

/// Called by the Slint side to say how to ask for a redraw, and how to put a
/// window on screen.
pub fn set_presenter(
    repaint: impl Fn() + Send + Sync + 'static,
    open: impl Fn(String) + Send + Sync + 'static,
) {
    if let Ok(mut reg) = registry().lock() {
        reg.repaint = Some(Arc::new(repaint));
        reg.open = Some(Arc::new(open));
    }
}

/// Open one plugin's window, or raise it if it is already up.
pub fn show(plugin: &str) {
    let open = registry().lock().ok().and_then(|reg| reg.open.clone());
    if let Some(open) = open {
        open(plugin.to_owned());
    }
}

/// Change one plugin's surfaces, then ask for a repaint.
///
/// The repaint call happens after the lock is released: it hops to the UI
/// thread, which immediately wants the same lock to read what changed.
pub fn update(plugin: &str, edit: impl FnOnce(&mut PluginUi)) {
    let repaint = {
        let Ok(mut reg) = registry().lock() else { return };
        edit(reg.plugins.entry(plugin.to_owned()).or_default());
        reg.repaint.is_some()
    };
    if repaint {
        request_repaint();
    }
}

fn request_repaint() {
    let repaint = registry().lock().ok().and_then(|reg| reg.repaint.clone());
    if let Some(f) = repaint {
        f();
    }
}

/// What one plugin currently declares, for the UI to draw.
pub fn snapshot(plugin: &str) -> Option<PluginUi> {
    registry().lock().ok()?.plugins.get(plugin).cloned()
}

/// Every plugin that has declared a window, in a stable order.
///
/// `(name, title, configurable)`. Used by the web interface, which lists the
/// same plugins the desktop menu bar offers and renders their surfaces itself
/// - the surface state lives here, not in the Slint window, so a second
/// renderer costs nothing but the drawing.
pub fn windows() -> Vec<(String, String, bool)> {
    let Ok(reg) = registry().lock() else {
        return Vec::new();
    };
    reg.plugins
        .iter()
        .filter(|(_, ui)| ui.has_window())
        .map(|(name, ui)| (name.clone(), ui.title.clone(), ui.configurable))
        .collect()
}

/// Every plugin that has declared a menu, in a stable order - the menu bar.
///
/// `(plugin name, dropdown title)`. A plugin with a title but no items is not
/// listed: a dropdown that opens onto nothing is worse than no dropdown.
pub fn menus() -> Vec<(String, String)> {
    let Ok(reg) = registry().lock() else {
        return Vec::new();
    };
    reg.plugins
        .iter()
        .filter(|(_, ui)| !ui.menu_items.is_empty())
        .map(|(name, ui)| {
            let title = if ui.menu_title.is_empty() {
                name.clone()
            } else {
                ui.menu_title.clone()
            };
            (name.clone(), title)
        })
        .collect()
}

/// The items in one plugin's dropdown, as `(id, label)`.
pub fn menu_items(plugin: &str) -> Vec<(String, String)> {
    registry()
        .lock()
        .ok()
        .and_then(|reg| reg.plugins.get(plugin).map(|ui| ui.menu_items.clone()))
        .unwrap_or_default()
}

/// Whether this plugin asked for a Configure button.
pub fn configurable(plugin: &str) -> bool {
    registry()
        .lock()
        .is_ok_and(|reg| reg.plugins.get(plugin).is_some_and(|ui| ui.configurable))
}

/// Post a click back to the plugin that drew the thing clicked.
///
/// Dropped on the floor when the host is not running: a window left on screen
/// after plugins were switched off should be inert, not a panic.
pub fn post(event: UiEvent) {
    let sink = registry().lock().ok().and_then(|reg| reg.events.clone());
    if let Some(sink) = sink {
        sink(event);
    }
}

/// Say why a handler did not finish, in the plugin's own window.
///
/// The log has it too, but a window stuck on "Checking..." with a healthy log
/// file somewhere else is indistinguishable from the plugin having hung. The
/// status line is where someone is already looking.
pub fn report_failure(plugin: &str, func: &str, err: &str) {
    let message = format!("{func} failed: {err}");
    update(plugin, move |ui| {
        if ui.has_window() {
            ui.status = message;
        }
    });
}

/// Forget every plugin's surfaces, keeping the channels open.
///
/// For a reload: the set about to be loaded declares its own, and a menu
/// belonging to a plugin that has just been switched off must not linger in
/// the bar. Repaints, or the removed titles would stay on screen until
/// something else happened to ask for one.
pub fn clear_surfaces() {
    if let Ok(mut reg) = registry().lock() {
        reg.plugins.clear();
    }
    request_repaint();
}

/// Forget everything. Called when the host stops so a stale menu does not
/// outlive the plugin that put it there.
pub fn clear() {
    clear_surfaces();
    if let Ok(mut reg) = registry().lock() {
        reg.events = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The registry is process-wide and the test harness is threaded, so these
    // use names of their own and never call `clear()` - one test wiping the
    // registry out from under another is a flaky failure that says nothing
    // about the code.

    fn item(id: &str, label: &str) -> (String, String) {
        (String::from(id), String::from(label))
    }

    /// The bar shows the declared title, and falls back to the plugin's own
    /// name rather than a blank dropdown when it has not set one.
    #[test]
    fn a_menu_without_a_title_is_listed_by_its_plugin_name() {
        update("menu-titled", |ui| {
            ui.menu_title = String::from("Feeds");
            ui.menu_items = vec![item("go", "Open")];
        });
        update("menu-untitled", |ui| {
            ui.menu_items = vec![item("go", "Open")];
        });

        let listed: Vec<(String, String)> = menus()
            .into_iter()
            .filter(|(name, _)| name.starts_with("menu-"))
            .collect();
        assert_eq!(
            listed,
            vec![
                (String::from("menu-titled"), String::from("Feeds")),
                (String::from("menu-untitled"), String::from("menu-untitled")),
            ]
        );
    }

    /// Declaring a title is not declaring a menu. Without items there is
    /// nothing to open, so the bar must not grow a dead dropdown.
    #[test]
    fn a_menu_with_no_items_is_not_listed() {
        update("empty-menu", |ui| ui.menu_title = String::from("Nothing"));
        assert!(!menus().iter().any(|(name, _)| name == "empty-menu"));

        update("empty-menu", |ui| ui.menu_items = vec![item("a", "A")]);
        assert!(menus().iter().any(|(name, _)| name == "empty-menu"));
    }

    /// One dropdown per plugin, and that is structural: a plugin holds a
    /// single title and a single item list, so declaring a menu twice replaces
    /// it. There is no arrangement of calls that puts two titles in the bar.
    #[test]
    fn declaring_a_menu_twice_replaces_it() {
        update("twice", |ui| {
            ui.menu_title = String::from("First");
            ui.menu_items = vec![item("a", "A")];
        });
        update("twice", |ui| {
            ui.menu_title = String::from("Second");
            ui.menu_items = vec![item("b", "B"), item("c", "C")];
        });

        let mine: Vec<(String, String)> = menus()
            .into_iter()
            .filter(|(name, _)| name == "twice")
            .collect();
        assert_eq!(
            mine,
            vec![(String::from("twice"), String::from("Second"))],
            "a plugin must occupy exactly one place in the menu bar"
        );
        assert_eq!(menu_items("twice").len(), 2);
    }

    /// `windows()` is what the web API lists, so it must show exactly the
    /// plugins that have declared a window - not every plugin that has ever
    /// touched its surface. A plugin with only a menu, or only a status line,
    /// has nothing for the browser to draw.
    #[test]
    fn only_plugins_with_a_window_are_listed() {
        update("web-has-window", |ui| {
            ui.title = String::from("Has one");
            ui.configurable = true;
        });
        update("web-no-window", |ui| ui.status = String::from("no window here"));

        let listed = windows();
        let found = |name: &str| listed.iter().any(|(n, _, _)| n == name);

        assert!(found("web-has-window"), "a declared window was not listed");
        assert!(!found("web-no-window"), "a plugin with no window was listed");

        let (_, title, configurable) = listed
            .iter()
            .find(|(n, _, _)| n == "web-has-window")
            .cloned()
            .expect("just asserted it is there");
        assert_eq!(title, "Has one");
        assert!(configurable);
    }

    /// One plugin's surfaces are not another's, and the snapshot is a copy -
    /// the UI thread must never hold a reference into the registry to draw.
    #[test]
    fn surfaces_are_per_plugin() {
        update("iso-a", |ui| ui.status = String::from("first"));
        update("iso-b", |ui| {
            ui.status = String::from("second");
            ui.configurable = true;
        });

        assert_eq!(snapshot("iso-a").unwrap().status, "first");
        assert_eq!(snapshot("iso-b").unwrap().status, "second");
        assert!(snapshot("iso-never-seen").is_none());

        assert!(configurable("iso-b"));
        assert!(!configurable("iso-a"), "configurable must be opted into");
        assert!(!configurable("iso-never-seen"));
    }

    /// A plugin has a window once it has named one, and not before - which is
    /// what `ui_show` and the menu both key off.
    #[test]
    fn a_window_exists_only_once_it_is_named() {
        update("win-none", |ui| ui.rows.push(Row::default()));
        assert!(!snapshot("win-none").unwrap().has_window());

        update("win-named", |ui| ui.title = String::from("Feeds"));
        assert!(snapshot("win-named").unwrap().has_window());
    }
}
