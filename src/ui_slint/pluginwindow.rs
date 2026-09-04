// The Slint half of plugin windows.
//
// `crate::plugins::ui` holds the state and knows nothing about Slint; this
// draws it and sends the clicks back. The two are joined by two closures
// installed once at startup, both of which hop to the event loop - the plugin
// host is on its own thread and must never touch a component directly.
//
// Windows live in a thread-local rather than on `Ui`, because the closures
// handed to the host have to be `Send` and `Ui` is an `Rc`. Everything in here
// runs on the UI thread, which is what makes that safe.

use std::cell::RefCell;
use std::collections::HashMap;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use super::{MainWindow, PluginButton, PluginRowItem, PluginWindow};
use crate::plugins::ui;

/// `open-menu` in app.slint: 0 none, 1 File, 2 View, 3 Help, 4 a plugin's own.
const PLUGIN_MENU: i32 = 4;

thread_local! {
    /// Plugin name to its window. Kept alive once opened and hidden rather
    /// than dropped, like every other dialog here.
    static OPEN: RefCell<HashMap<String, PluginWindow>> = RefCell::new(HashMap::new());

    /// The main window, for the menu bar. Held here rather than threaded
    /// through every caller: Preferences reaches plugins too, and it has no
    /// reason to know about MainWindow in order to do that.
    static MAIN: RefCell<Option<slint::Weak<MainWindow>>> = const { RefCell::new(None) };
}

/// Teach the plugin host how to reach the UI. Called once, after the main
/// window exists and before the host is spawned.
pub fn install(main: &MainWindow) {
    MAIN.with(|slot| *slot.borrow_mut() = Some(main.as_weak()));

    ui::set_presenter(
        || {
            // Ignored: a failure here means the event loop is gone, which is
            // shutdown, and a plugin repainting into it is not a problem.
            let _ = slint::invoke_from_event_loop(refresh);
        },
        |name| {
            let _ = slint::invoke_from_event_loop(move || open(&name));
        },
    );

    // Catch up on anything already declared. The plugin host is started before
    // the window exists, so a plugin that calls `ui_menu` from
    // `on_session_start` did so when there was no presenter to notify - and
    // without this its dropdown would not appear until it next touched the
    // UI, which for a plugin that only polls is a minute away.
    refresh();
}

/// Re-read every open window's state, and rebuild the menu bar's plugin
/// dropdowns.
fn refresh() {
    let main = MAIN.with(|slot| slot.borrow().clone());
    if let Some(window) = main.and_then(|weak| weak.upgrade()) {
        let titles: Vec<SharedString> = ui::menus()
            .into_iter()
            .map(|(_, title)| SharedString::from(title))
            .collect();
        // A dropdown that is open while its plugin's menu disappears would be
        // showing items nothing will answer, so close it.
        if window.get_open_menu() == PLUGIN_MENU
            && usize::try_from(window.get_open_plugin()).is_ok_and(|i| i >= titles.len())
        {
            window.set_open_menu(0);
        }
        window.set_plugin_menu_titles(ModelRc::new(VecModel::from(titles)));
    }

    OPEN.with(|open| {
        for (name, window) in open.borrow().iter() {
            if let Some(state) = ui::snapshot(name) {
                apply(window, &state);
            }
        }
    });
}

/// Open a plugin's window, or raise it if it is already up.
pub fn open(name: &str) {
    // A plugin can declare a menu, or ask to be configurable, without ever
    // declaring a window - so having an entry here is not the same as having
    // something to show, and an unnamed window would come up blank.
    let Some(state) = ui::snapshot(name).filter(|ui| ui.has_window()) else {
        tracing::debug!("plugin {name} has no window to open");
        return;
    };

    // Built outside the borrow: `make` wires callbacks, and a callback firing
    // while the map is mutably borrowed would panic.
    let fresh = OPEN.with(|open| !open.borrow().contains_key(name));
    if fresh {
        let Some(window) = make(name) else { return };
        OPEN.with(|open| {
            open.borrow_mut().insert(name.to_owned(), window);
        });
    }

    let shown = OPEN.with(|open| {
        let open = open.borrow();
        let Some(window) = open.get(name) else {
            return false;
        };
        apply(window, &state);
        if let Err(err) = window.show() {
            tracing::error!("cannot show the window for plugin {name}: {err}");
            return false;
        }
        // Raise it: choosing it a second time should bring the window
        // forward, not silently do nothing because it is already up.
        window.window().set_minimized(false);
        // Same treatment every other window gets, and re-done on each open
        // rather than only at creation: these windows are kept alive between
        // opens, so one opened on another monitor would otherwise keep that
        // screen's limit.
        super::clamp_to_screen(window, |w, h| w.set_screen_limit(h));
        true
    });

    if shown {
        // The plugin's chance to fill the window before it is looked at.
        ui::post(ui::UiEvent::Opened {
            plugin: name.to_owned(),
        });
        refresh();
    }
}

/// Fill the dropdown for the plugin title at this position in the bar.
///
/// Called as the menu opens rather than kept in step continuously: only one
/// dropdown is open at a time, so there is no reason to hold every plugin's
/// items in the UI at once.
pub fn fill_menu(index: i32) {
    let menus = ui::menus();
    let items = usize::try_from(index)
        .ok()
        .and_then(|i| menus.get(i))
        .map(|(plugin, _)| ui::menu_items(plugin))
        .unwrap_or_default();

    let main = MAIN.with(|slot| slot.borrow().clone());
    if let Some(window) = main.and_then(|weak| weak.upgrade()) {
        let items: Vec<PluginButton> = items
            .into_iter()
            .map(|(id, label)| PluginButton {
                id: SharedString::from(id),
                label: SharedString::from(label),
            })
            .collect();
        window.set_plugin_menu_items(ModelRc::new(VecModel::from(items)));
    }
}

/// A plugin menu item was chosen. Addressed to whichever plugin's dropdown is
/// open, which is the one the bar last filled.
pub fn activate_menu(id: &str) {
    let main = MAIN.with(|slot| slot.borrow().clone());
    let Some(window) = main.and_then(|weak| weak.upgrade()) else {
        return;
    };
    let menus = ui::menus();
    let Some((plugin, _)) = usize::try_from(window.get_open_plugin())
        .ok()
        .and_then(|i| menus.get(i))
    else {
        return;
    };
    ui::post(ui::UiEvent::Menu {
        plugin: plugin.clone(),
        id: id.to_owned(),
    });
}

/// Whether this plugin asked for a Configure button on its Preferences row.
pub fn configurable(name: &str) -> bool {
    ui::configurable(name)
}

/// Configure was pressed. The plugin decides what that means - typically
/// `ui_show()`, but a plugin with no window might do something else entirely.
pub fn configure(name: &str) {
    ui::post(ui::UiEvent::Configure {
        plugin: name.to_owned(),
    });
}

fn make(name: &str) -> Option<PluginWindow> {
    let window = match PluginWindow::new() {
        Ok(window) => window,
        Err(err) => {
            tracing::error!("cannot create a window for plugin {name}: {err}");
            return None;
        }
    };

    {
        let plugin = name.to_owned();
        window.on_row_activated(move |id| {
            ui::post(ui::UiEvent::Row {
                plugin: plugin.clone(),
                id: id.to_string(),
            });
        });
    }

    {
        let plugin = name.to_owned();
        window.on_group_activated(move |id| {
            ui::post(ui::UiEvent::Group {
                plugin: plugin.clone(),
                id: id.to_string(),
            });
        });
    }

    {
        let plugin = name.to_owned();
        window.on_button_pressed(move |id, input| {
            ui::post(ui::UiEvent::Button {
                plugin: plugin.clone(),
                id: id.to_string(),
                input: input.to_string(),
            });
        });
    }

    {
        // Hidden, not dropped - the same reason every other dialog here is
        // kept: dropping a window from inside its own callback is trouble.
        let weak = window.as_weak();
        window.on_closed(move || {
            if let Some(window) = weak.upgrade() {
                let _ = window.hide();
            }
        });
    }

    Some(window)
}

fn apply(window: &PluginWindow, state: &ui::PluginUi) {
    window.set_window_title(SharedString::from(&state.title));
    window.set_status(SharedString::from(&state.status));
    window.set_placeholder(SharedString::from(&state.placeholder));

    let buttons: Vec<PluginButton> = state
        .buttons
        .iter()
        .map(|(id, label)| PluginButton {
            id: SharedString::from(id),
            label: SharedString::from(label),
        })
        .collect();
    window.set_buttons(ModelRc::new(VecModel::from(buttons)));

    window.set_rows(rows_of(&state.rows));
    window.set_groups(rows_of(&state.groups));
}

fn rows_of(rows: &[ui::Row]) -> ModelRc<PluginRowItem> {
    let rows: Vec<PluginRowItem> = rows
        .iter()
        .map(|row| PluginRowItem {
            id: SharedString::from(&row.id),
            title: SharedString::from(&row.title),
            subtitle: SharedString::from(&row.subtitle),
            selected: row.selected,
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

