//! The UI: one window, built from `.slint` markup, on Windows, Linux and
//! macOS. It replaced the original Win32 front end in v0.2.0.
//!
//! The markup declares the widget tree and the bindings between widgets; this
//! module is the other half - it fills the models, answers the callbacks the
//! markup raises, and owns the dialogs. Nothing here draws.
//!
//! # Threading
//!
//! Unlike the web interface, no `spawn_blocking` dance is needed here.
//! `Session`'s methods are synchronous with `Runtime::block_on` inside them,
//! which panics only when called *from* a runtime thread - and Slint's event
//! loop is a plain thread, exactly as the Win32 message loop is. Calling
//! `session.pause()` straight from a callback is correct.
//!
//! # Known limitation: the context menu
//!
//! Right-clicking a second torrent while the menu is open dismisses the menu
//! rather than moving it there, so it takes a second right-click. Windows moves
//! it in one. A Slint `PopupWindow` captures the pointer while open, whatever
//! its close policy, so the click never reaches the other row - `no-auto-close`
//! was tried and only risked a menu that would not go away.
//!
//! Matching Windows means making the popup window-sized with a transparent
//! scrim over everything, catching the click there, and mapping its y back to a
//! row index through the ListView's `viewport-y` and the 26px row height. That
//! is worth doing, and it is not worth doing before the dialogs exist.
//!
//! # Refresh
//!
//! A 1 s `slint::Timer`, mirroring the `AnimationTimer` in mainwindow.rs. The
//! whole model is rebuilt each tick rather than diffed: `ListView` culls to the
//! viewport, so the cost is bounded by what is on screen, and 5,000 rows
//! measured ~50 ms to build in the spike. The Win32 list has to diff because it
//! holds a real `LVITEM` per row and repaints on any write.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use slint::{Model, ModelRc, SharedString, VecModel};

use crate::AppContext;
use crate::bittorrent::session::{AddParams, AddTorrentSource, Session};
use crate::bittorrent::torrentstatus::{State, TorrentStatus};
use crate::core::utils;
use crate::ui::format;
use crate::ui::translator::Translator;

slint::include_modules!();

/// Matches the .desktop file name that packaging installs, which is how
/// Wayland compositors find the window icon. See docs/BUILDING.md.
const APP_ID: &str = "nanotorrent";

mod flags;
mod modal;

/// Everything the callbacks need, kept in one `Rc` so each closure clones a
/// single handle rather than five.
struct Ui {
    session: Arc<Session>,
    cfg: Arc<crate::core::configuration::Configuration>,
    /// Swapped in place when the language changes, so every later lookup
    /// sees the new one. A plain Translator would have to be cloned into each
    /// closure, freezing the language those closures were built with.
    tr: RefCell<Translator>,
    /// Info hashes of the selected rows, in the order the list shows them.
    /// The Win32 list keeps this in the control; here the model owns it.
    selected: RefCell<HashSet<String>>,
    /// Anchor for shift-extend, as an index into the last rendered order.
    anchor: RefCell<usize>,
    /// Sort column and direction; None means the session's own order.
    sort: RefCell<Option<(usize, bool)>>,
    /// The three filters the Win32 list also composes: a named PQL filter, a
    /// live one typed into the console, and a label. All must pass.
    active_filter: RefCell<Option<crate::ui::filters::TorrentFilter>>,
    console_filter: RefCell<Option<crate::ui::filters::TorrentFilter>>,
    active_label: RefCell<Option<i32>>,
    /// The rows behind what is on screen, so a callback can map an index to a
    /// torrent without asking the session again.
    rows: RefCell<Vec<TorrentStatus>>,
    /// Arguments forwarded by a second instance, polled on the refresh tick.
    ipc: Option<crate::ipc::Server>,
    /// Torrent lifecycle events, drained on the refresh tick. Replaces
    /// Session::take_completions: detection now runs on the session's own
    /// timer, so it happens whether or not this window is here to poll.
    events: RefCell<std::sync::mpsc::Receiver<crate::bittorrent::session::SessionEvent>>,
    /// Peer country lookups, loaded in the background at startup - the same
    /// database the Win32 peers list uses.
    geoip: Arc<crate::core::geoip::GeoIp>,
    /// Decoded country flags, filled in as peers from new countries appear.
    flags: flags::Flags,
    /// Kept alive while open. Hidden rather than dropped when dismissed -
    /// dropping a window from inside its own callback is asking for trouble,
    /// and the next open replaces it anyway.
    magnet_dialog: RefCell<Option<AddMagnetDialog>>,
    prefs_dialog: RefCell<Option<PreferencesDialog>>,
    about_dialog: RefCell<Option<AboutDialog>>,
    create_dialog: RefCell<Option<CreateTorrentDialog>>,
    /// Where a background create-torrent run leaves its result.
    create_slot: Arc<std::sync::Mutex<Option<crate::bittorrent::session::CreateTorrentOutcome>>>,
    env: Arc<crate::core::environment::Environment>,
    torrent_dialog: RefCell<Option<AddTorrentDialog>>,
    close_prompt: RefCell<Option<ClosePromptDialog>>,
    /// The .torrent currently in the Add dialog, and any queued behind it.
    /// argv can name several, and only one dialog is shown at a time.
    pending: RefCell<Vec<Vec<u8>>>,
    /// Set once the window exists, so a dialog can push a setting back into it
    /// (the theme) without the caller having to thread the window through.
    main: RefCell<Option<slint::Weak<MainWindow>>>,
    /// The tray icon, kept here so a language change can refresh its menu -
    /// it owns its own copy of the `L` global.
    tray: RefCell<Option<Tray>>,
    /// The running web interface, so Preferences can restart it in place.
    web: RefCell<Option<actix_web::dev::ServerHandle>>,
    /// Whether the main window is currently blocked by a dialog. Held here so
    /// the poll and the dismiss path share one view of it and cannot each
    /// re-apply what the other already did.
    blocked: std::cell::Cell<bool>,
}

impl Ui {
    /// Hashes to act on: the selection, or nothing if there is none.
    fn targets(&self) -> Vec<String> {
        let selected = self.selected.borrow();
        self.rows
            .borrow()
            .iter()
            .filter(|r| selected.contains(&r.info_hash))
            .map(|r| r.info_hash.clone())
            .collect()
    }

    /// Label id -> name, rebuilt from the database on each call.
    ///
    /// Not cached: labels change from Preferences, and a stale map shows a
    /// torrent under a name that no longer exists.
    fn labels(&self) -> HashMap<i32, String> {
        self.cfg
            .get_labels()
            .into_iter()
            .map(|l| (l.id, l.name))
            .collect()
    }
}

/// One torrent as the UI shows it. Mirrors the 16 columns of
/// `mainwindow.rs::sync_list`, including its dash-when-paused rules, so the
/// two UIs cannot drift into disagreeing about the same torrent.
fn to_row(status: &TorrentStatus, tr: &Translator, selected: bool) -> Row {
    let paused = matches!(
        status.state,
        State::DownloadingPaused | State::UploadingPaused
    );
    let dash = || SharedString::from("-");

    Row {
        name: status.name.as_str().into(),
        // 1-based for display; queue_position is stored 0-based.
        queue: (status.queue_position + 1).to_string().into(),
        size: utils::to_human_file_size(status.total_wanted).into(),
        remaining: utils::to_human_file_size(status.total_wanted_remaining).into(),
        status: format::state_text(tr, status).into(),
        progress: status.progress,
        eta: if paused {
            dash()
        } else {
            format::eta_text(status).into()
        },
        dl: if paused {
            dash()
        } else {
            format::speed_text(status.download_payload_rate).into()
        },
        ul: if paused {
            dash()
        } else {
            format::speed_text(status.upload_payload_rate).into()
        },
        availability: if paused || status.availability <= 0.0 {
            dash()
        } else {
            format!("{:.2}", status.availability).into()
        },
        ratio: format!("{:.2}", status.ratio).into(),
        seeds: if paused {
            dash()
        } else {
            format!("{} ({})", status.seeds_current, status.seeds_total).into()
        },
        peers: if paused {
            dash()
        } else {
            format!("{} ({})", status.peers_current, status.peers_total).into()
        },
        added: format::date_text(&status.added_on).into(),
        completed: format::opt_date_text(&status.completed_on).into(),
        label: status.label_name.as_str().into(),
        selected,
    }
}

/// Build the window, wire everything to the session, and run the event loop
/// until the window closes.
///
/// Returns only when the UI is done, so the caller can shut the session down
/// afterwards.
pub fn run(ctx: AppContext) -> anyhow::Result<()> {
    // Wayland has no protocol for a client to set its own window icon, so
    // `icon:` in the markup is a no-op there and the compositor falls back to
    // a generic one. What it does instead is match the app id against an
    // installed .desktop file and take the Icon= from that. Must be set before
    // the window is shown; harmless on Windows and macOS.
    if let Err(err) = slint::set_xdg_app_id(APP_ID) {
        tracing::debug!("could not set the xdg app id: {err}");
    }

    let window = MainWindow::new().map_err(|e| {
        // winit's own words for a missing display server are "neither
        // WAYLAND_DISPLAY nor WAYLAND_SOCKET nor DISPLAY is set", which names
        // what is absent but not why or what to do about it.
        let no_display = cfg!(unix)
            && std::env::var_os("DISPLAY").is_none()
            && std::env::var_os("WAYLAND_DISPLAY").is_none();
        let hint = if no_display {
            concat!(
                "\n\nNo display server was found. This build needs a graphical session:\n",
                "  - over SSH: use `ssh -X`, or run it at the machine itself\n",
                "  - under sudo: use `sudo -E` (it does not need root anyway)\n",
                "  - from a text console: switch back to the desktop session\n",
                "On a machine with no desktop at all, build the headless daemon instead\n",
                "(`cargo build --release --no-default-features`) and use the web interface.",
            )
        } else {
            ""
        };
        anyhow::anyhow!("failed to create the Slint window: {e}{hint}")
    })?;

    window.set_window_title(
        format!(
            "NanoTorrent {} (build {})",
            crate::buildinfo::version(),
            crate::buildinfo::build_stamp()
        )
        .into(),
    );

    let ui = Rc::new(Ui {
        session: ctx.session.clone(),
        cfg: ctx.cfg.clone(),
        tr: RefCell::new(ctx.translator.clone()),
        selected: RefCell::new(HashSet::new()),
        anchor: RefCell::new(0),
        sort: RefCell::new(None),
        active_filter: RefCell::new(None),
        console_filter: RefCell::new(None),
        active_label: RefCell::new(None),
        rows: RefCell::new(Vec::new()),
        ipc: ctx.ipc,
        // Subscribed before the first refresh tick, so a torrent that finishes
        // during startup still raises its notification.
        events: RefCell::new(ctx.session.subscribe()),
        geoip: ctx.geoip.clone(),
        flags: flags::Flags::default(),
        magnet_dialog: RefCell::new(None),
        prefs_dialog: RefCell::new(None),
        about_dialog: RefCell::new(None),
        create_dialog: RefCell::new(None),
        create_slot: Arc::new(std::sync::Mutex::new(None)),
        env: ctx.env.clone(),
        torrent_dialog: RefCell::new(None),
        pending: RefCell::new(Vec::new()),
        main: RefCell::new(None),
        tray: RefCell::new(None),
        web: RefCell::new(ctx.web.clone()),
        blocked: std::cell::Cell::new(false),
        close_prompt: RefCell::new(None),
    });

    let model: Rc<VecModel<Row>> = Rc::new(VecModel::from(Vec::new()));
    window.set_rows(ModelRc::from(model.clone()));

    wire_selection(&window, &ui, &model);
    wire_actions(&window, &ui);
    {
        let (widths, titles, total) = columns(&ui.tr.borrow());
        let cols = window.global::<Cols>();
        cols.set_w(ModelRc::new(VecModel::from(widths)));
        cols.set_titles(ModelRc::new(VecModel::from(titles)));
        cols.set_total(total);
    }

    *ui.main.borrow_mut() = Some(window.as_weak());
    // Every visible string in the markup is `L.s("key")`. Wired before the
    // window is shown so the first paint is already translated.
    wire_translations(&window, &ui);

    // Clicking any value in the detail tabs copies it. Reached through a
    // global so the cells do not each need wiring where they are used.
    {
        let (w, u) = (window.as_weak(), ui.clone());
        window.global::<Clip>().on_copy(move |text| {
            if let Some(window) = w.upgrade() {
                copy_to_clipboard(&window, &u, text.to_string());
            }
        });
    }
    wire_filters(&window, &ui, &model);

    // Torrents named on the command line, before the first paint so they are
    // already in the list when it appears.
    handle_params(&ui, &ctx.args);

    // Populate before the first paint so the window never flashes empty.
    refresh(&window, &ui, &model);

    // Registered here, not inside install_tray: without a close handler the
    // window would hide on X and `run_event_loop_until_quit` would keep an
    // invisible process alive, so a tray that failed to build must not take
    // this with it.
    {
        let (u, w) = (ui.clone(), window.as_weak());
        window
            .window()
            .on_close_requested(move || match w.upgrade() {
                Some(window) => request_close(&window, &u),
                None => slint::CloseRequestResponse::HideWindow,
            });
    }

    // Kept alive by the timer closure: dropping the handle removes the icon.
    let tray = install_tray(&window, &ui);
    let tray_visible = std::cell::Cell::new(ui.cfg.get_bool("show_in_notification_area"));

    // A separate, faster timer for the modal guard. Polling beats touching
    // every show()/hide() site: it cannot get stuck with the window disabled
    // after a dialog closes by some path that forgot to unblock, and 100ms is
    // below noticing.
    let modal_timer = slint::Timer::default();
    {
        let (w, u) = (window.as_weak(), ui.clone());
        modal_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(100),
            move || {
                let Some(window) = w.upgrade() else { return };
                // A safety net now, not the main path: dismiss() does the
                // unblocking in the right order, and this only catches a
                // dialog closed by some route that did not go through it.
                set_modal(&u, any_dialog_open(&u, &window));
            },
        );
    }

    let timer = slint::Timer::default();
    {
        let (w, ui, model) = (window.as_weak(), ui.clone(), model.clone());
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(1),
            move || {
                // A second instance forwards its argv here rather than
                // opening a second window - see ipc::init.
                if let Some(forwarded) = ui.ipc.as_ref().and_then(|s| s.try_recv()) {
                    handle_params(&ui, &forwarded);
                }
                poll_create_torrent(&ui);
                if let Some(window) = w.upgrade() {
                    if let Some(tray) = tray.as_ref() {
                        // Only on a change: show()/hide() re-register with the
                        // shell, so calling them every tick would churn the
                        // icon once a second.
                        let want = ui.cfg.get_bool("show_in_notification_area");
                        if tray_visible.replace(want) != want {
                            show_tray(tray, want);
                        }
                    }
                    poll_minimize_to_tray(&window, &ui);
                    refresh(&window, &ui, &model);
                }
            },
        );
    }

    window
        .show()
        .map_err(|e| anyhow::anyhow!("failed to show the Slint window: {e}"))?;
    let result = slint::run_event_loop_until_quit()
        .map_err(|e| anyhow::anyhow!("Slint event loop failed: {e}"));
    drop(modal_timer);
    result
}

/// The tray icon and what closing the window does while it is there.
///
/// Port of the tray half of mainwindow.rs: the icon follows
/// `show_in_notification_area`, minimising hides the window when
/// `minimize_to_notification_area` is also on, and closing consults
/// `ui.close_action` ("exit", "minimize" or "ask").
fn install_tray(window: &MainWindow, ui: &Rc<Ui>) -> Option<Tray> {
    let tray = match Tray::new() {
        Ok(tray) => tray,
        Err(err) => {
            // A Linux desktop with no StatusNotifierItem host lands here. The
            // app carries on without an icon rather than refusing to start.
            tracing::error!("cannot create the tray icon: {err}");
            return None;
        }
    };
    {
        // Not wire_translations: a SystemTrayIcon-rooted component has a
        // smaller generated API and does not implement ComponentHandle. It
        // still owns its own `L`, which is exactly why this is needed - both
        // menu items came up blank without it.
        let u = ui.clone();
        tray.global::<L>()
            .on_s(move |_revision, key| ui_string(&u.tr.borrow(), key.as_str()));
    }

    {
        let w = window.as_weak();
        tray.on_show_window(move || {
            if let Some(window) = w.upgrade() {
                restore(&window);
            }
        });
    }
    {
        let w = window.as_weak();
        tray.on_exit(move || {
            if let Some(window) = w.upgrade() {
                // Not "exit": choosing Exit from the tray is already an
                // explicit decision, so it skips the close prompt.
                window.invoke_action("exit-now".into());
            }
        });
    }

    // Built either way and merely shown or hidden by the setting, so toggling
    // it in Preferences takes effect without a restart. The refresh tick keeps
    // it in step.
    show_tray(&tray, ui.cfg.get_bool("show_in_notification_area"));
    *ui.tray.borrow_mut() = Some(tray.clone_strong());
    Some(tray)
}

/// Show or hide the tray icon.
///
/// A SystemTrayIcon has no `visible` property - show/hide is the whole API -
/// so the Preferences toggle routes through here rather than rebuilding it.
fn show_tray(tray: &Tray, visible: bool) {
    let result = if visible { tray.show() } else { tray.hide() };
    if let Err(err) = result {
        tracing::error!("cannot change the tray icon visibility: {err}");
    }
}

/// How long a toast stays up.
const TOAST_DURATION: std::time::Duration = std::time::Duration::from_millis(1600);

/// Show a transient confirmation at the bottom of the window.
///
/// The counter is what makes rapid copies behave: without it the timer from an
/// earlier toast would clear a later one early, so each toast only clears the
/// message if it is still the newest.
fn show_toast(window: &MainWindow, text: &str) {
    thread_local! {
        static GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    window.set_toast(text.into());
    let mine = GENERATION.with(|g| {
        g.set(g.get().wrapping_add(1));
        g.get()
    });

    let weak = window.as_weak();
    slint::Timer::single_shot(TOAST_DURATION, move || {
        if GENERATION.with(|g| g.get()) != mine {
            return;
        }
        if let Some(window) = weak.upgrade() {
            window.set_toast(SharedString::new());
        }
    });
}

/// One translated string, ready for Slint.
///
/// The JSON is authored for Win32 controls: CRLF line endings, and `&&`
/// for a literal ampersand. Neither means anything to Slint, and both show
/// up on screen if left in.
fn ui_string(tr: &Translator, key: &str) -> SharedString {
    // Win32 ampersand rules, left to right: `&&` is a literal ampersand and a
    // lone `&` marks the accelerator letter. Slint draws its own menus and has
    // no accelerators, so the marker is dropped - otherwise the bar reads
    // "&File &View &Help". CRLF goes too; the JSON is authored for Win32
    // controls and Slint draws the stray CR as a box.
    let text = tr.i18n(key);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            if chars.peek() == Some(&'&') {
                chars.next();
                out.push('&');
            }
            continue;
        }
        out.push(c);
    }
    out.replace("\r\n", "\n").into()
}

/// Roughly how wide `text` renders in the list header, in logical pixels.
///
/// Slint exposes no text-measurement API, so this estimates from character
/// class: CJK and other full-width glyphs take about a full em, everything
/// else a bit over half. It only ever *widens* a column so a translated
/// caption is not clipped - `overflow: elide` still trims anything this
/// underestimates, so being approximate is safe.
fn caption_width(text: &str) -> f32 {
    const FONT_PX: f32 = 13.0;
    text.chars()
        .map(|c| {
            // The ranges that matter in practice: CJK ideographs, kana, Hangul
            // and full-width forms all advance a full em.
            let wide = matches!(c as u32,
                0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3
                | 0xF900..=0xFAFF | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6);
            if wide { FONT_PX } else { FONT_PX * 0.56 }
        })
        .sum()
}

/// The torrent list's column captions and widths.
///
/// Each column is the wider of its designed width and what its caption needs,
/// so switching language cannot clip a header. Returns the widths, the
/// captions and their total.
fn columns(tr: &Translator) -> (Vec<f32>, Vec<SharedString>, f32) {
    // key, designed width. "#", "DL" and "UL" have no key: they are the same
    // symbols in every locale the original ships.
    const COLUMNS: [(&str, f32); 16] = [
        ("name", 260.0),
        ("", 44.0),
        ("size", 90.0),
        ("size_remaining", 120.0),
        ("status", 110.0),
        ("progress", 80.0),
        ("eta", 70.0),
        ("", 90.0),
        ("", 90.0),
        ("availability", 90.0),
        ("ratio", 60.0),
        ("seeds", 80.0),
        ("peers", 80.0),
        ("added_on", 130.0),
        ("completed_on", 130.0),
        ("label", 100.0),
    ];
    const FIXED: [&str; 16] = [
        "", "#", "", "", "", "", "", "DL", "UL", "", "", "", "", "", "", "",
    ];
    // 12px of cell padding, plus room for the "  ^" sort arrow the caption
    // grows by when the column is the active sort.
    const PADDING: f32 = 12.0 + 18.0;

    let mut widths = Vec::with_capacity(COLUMNS.len());
    let mut titles = Vec::with_capacity(COLUMNS.len());
    for (i, (key, base)) in COLUMNS.iter().enumerate() {
        let caption = if key.is_empty() {
            SharedString::from(FIXED[i])
        } else {
            ui_string(tr, key)
        };
        widths.push(base.max(caption_width(&caption) + PADDING));
        titles.push(caption);
    }
    let total = widths.iter().sum();
    (widths, titles, total)
}

/// Wire the translation lookup on a top-level component.
///
/// Slint globals are per top-level component, not per process: the main
/// window, every dialog and the tray icon each get their own copy of `L`, and
/// an unwired one returns "" for every lookup. That has now bitten twice - a
/// Preferences window with no title, and a tray menu whose two items were
/// blank, which looked like a theming fault rather than empty strings.
/// Everything that owns an `L` goes through here.
fn wire_translations<T>(component: &T, ui: &Rc<Ui>)
where
    T: slint::ComponentHandle,
    for<'a> L<'a>: slint::Global<'a, T>,
{
    let u = ui.clone();
    component
        .global::<L>()
        .on_s(move |_revision, key| ui_string(&u.tr.borrow(), key.as_str()));
}

/// Put text on the system clipboard.
///
/// A fresh context per call rather than one held open: X11 hands the selection
/// to whoever asked last, and keeping a provider alive there means owning it
/// for the process lifetime, which is not what a one-shot copy wants.
fn copy_to_clipboard(window: &MainWindow, ui: &Rc<Ui>, text: String) {
    use copypasta::ClipboardProvider;

    let result = copypasta::ClipboardContext::new().and_then(|mut ctx| ctx.set_contents(text));
    match result {
        // Confirming here rather than at each call site: the clipboard gives
        // no visible feedback of its own, so a copy that says nothing looks
        // like a menu item that did nothing.
        Ok(()) => show_toast(window, &ui.tr.borrow().i18n("copied_to_clipboard")),
        Err(err) => tracing::error!("cannot write to the clipboard: {err}"),
    }
}

/// Block or unblock the main window, if that is not already its state.
fn set_modal(ui: &Rc<Ui>, blocked: bool) {
    if ui.blocked.replace(blocked) == blocked {
        return;
    }
    if let Some(window) = ui.main.borrow().as_ref().and_then(|w| w.upgrade()) {
        modal::set_blocked(&window, blocked);
    }
}

/// Make a dialog's own X button unblock the main window before it hides.
///
/// That path never reaches `dismiss`, so without this the title-bar close
/// still flickered another application to the front while the Cancel button
/// no longer did.
fn wire_dialog_close<T>(dialog: &T, ui: &Rc<Ui>)
where
    T: slint::ComponentHandle,
    for<'a> L<'a>: slint::Global<'a, T>,
{
    // Globals are per top-level component, not per process: every dialog gets
    // its own copy of L, and an unwired one returns "" - which showed up as a
    // Preferences window with no title at all.
    wire_translations(dialog, ui);

    let u = ui.clone();
    dialog.window().on_close_requested(move || {
        set_modal(&u, false);
        slint::CloseRequestResponse::HideWindow
    });
}

/// Dismiss a dialog.
///
/// The unblock MUST happen before the hide. Windows will not activate a
/// disabled window, so a dialog destroyed while its owner is still disabled
/// makes the shell hand the foreground to whatever is next in the Z-order -
/// usually another application. Re-enabling afterwards and grabbing the
/// foreground back worked, but the other window was visible in between.
fn dismiss<T: slint::ComponentHandle>(ui: &Rc<Ui>, dialog: &T) {
    set_modal(ui, false);
    let _ = dialog.hide();
}

/// Is this dialog on screen? `Option::None` covers "never opened".
/// Is this dialog on screen? Also claims ownership of it while it is, which
/// is what makes closing it hand activation back to the main window.
fn dialog_visible<T: slint::ComponentHandle>(
    slot: &RefCell<Option<T>>,
    owner: &MainWindow,
) -> bool {
    let slot = slot.borrow();
    let Some(dialog) = slot.as_ref() else {
        return false;
    };
    if !dialog.window().is_visible() {
        return false;
    }
    modal::own(dialog, owner);
    true
}

/// True while any dialog is up, which is when the main window is blocked.
///
/// Every slot is checked rather than short-circuiting on the first hit: the
/// ownership side effect above has to reach all of them.
fn any_dialog_open(ui: &Rc<Ui>, owner: &MainWindow) -> bool {
    let open = [
        dialog_visible(&ui.magnet_dialog, owner),
        dialog_visible(&ui.prefs_dialog, owner),
        dialog_visible(&ui.about_dialog, owner),
        dialog_visible(&ui.create_dialog, owner),
        dialog_visible(&ui.torrent_dialog, owner),
        dialog_visible(&ui.close_prompt, owner),
    ];
    open.iter().any(|open| *open)
}

/// What closing the main window means: quit, hide into the tray, or ask.
///
/// The window's X, File > Exit and (when there is no tray to hide into)
/// everything else all come through here, so the three cannot drift apart -
/// which they had, with File > Exit quitting outright while X asked.
///
/// Returns what to do with the window, so `on_close_requested` can return it
/// directly and the menu path can act on it.
fn request_close(window: &MainWindow, ui: &Rc<Ui>) -> slint::CloseRequestResponse {
    if !ui.cfg.get_bool("show_in_notification_area") {
        // Nothing to hide into, so closing has to mean exit whatever the
        // saved preference says.
        let _ = slint::quit_event_loop();
        return slint::CloseRequestResponse::HideWindow;
    }
    // Only "exit" quits outright and only "minimize" hides silently; anything
    // else - including no saved preference at all - asks.
    match ui
        .cfg
        .get_persistent("ui.close_action")
        .unwrap_or_default()
        .as_str()
    {
        "exit" => {
            let _ = slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        }
        "minimize" => slint::CloseRequestResponse::HideWindow,
        _ => {
            ask_on_close(window, ui);
            slint::CloseRequestResponse::KeepWindowShown
        }
    }
}

/// Bring the window back from the tray. Port of `restore_from_tray`.
fn restore(window: &MainWindow) {
    if let Err(err) = window.show() {
        tracing::error!("cannot show the window: {err}");
        return;
    }
    window.window().set_minimized(false);
    // slint::Window has no raise or focus call, so this goes to the platform.
    modal::raise(window);
}

/// Defaults for a torrent added without a dialog: the session's own save path,
/// started, no label.
fn default_add_params() -> AddParams {
    AddParams {
        save_path: None,
        start_torrent: true,
        only_files: None,
        label_id: None,
    }
}

/// Handle `.torrent` paths and `magnet:` links from argv, or forwarded by a
/// second instance.
///
/// Magnets are added straight away - there is nothing to choose until their
/// metadata resolves. Files go through the Add dialog, so the save path and
/// file selection can be set before anything is written.
fn handle_params(ui: &Rc<Ui>, args: &[String]) {
    for arg in args {
        if arg.starts_with("magnet:") {
            ui.session.add_torrent(
                AddTorrentSource::MagnetUri(arg.clone()),
                default_add_params(),
            );
        } else if arg.to_lowercase().ends_with(".torrent") {
            match std::fs::read(arg) {
                Ok(bytes) => ui.pending.borrow_mut().push(bytes),
                // Not fatal: one unreadable path should not stop the others.
                Err(err) => tracing::error!("cannot read {arg}: {err}"),
            }
        }
    }
    show_next_pending(ui);
}

/// Pull the session's current state into the model.
fn refresh(window: &MainWindow, ui: &Rc<Ui>, model: &Rc<VecModel<Row>>) {
    drain_notifications(window, ui);

    let mut rows = ui.session.torrents(&ui.labels());

    // Filters first, then the sort. Both happen before anything indexes into
    // rows: selection and the context menu map a row index to a torrent, so
    // they must see exactly what the list shows.
    if let Some(filter) = ui.active_filter.borrow().as_ref() {
        rows.retain(|r| filter.includes(r));
    }
    if let Some(filter) = ui.console_filter.borrow().as_ref() {
        rows.retain(|r| filter.includes(r));
    }
    if let Some(label_id) = *ui.active_label.borrow() {
        rows.retain(|r| r.label_id == Some(label_id));
    }

    if let Some((column, ascending)) = *ui.sort.borrow() {
        sort_rows(&mut rows, column, ascending);
    }

    // Drop selections for torrents that are gone, or the count in the status
    // bar drifts upwards every time one is removed.
    {
        let live: HashSet<&str> = rows.iter().map(|r| r.info_hash.as_str()).collect();
        ui.selected
            .borrow_mut()
            .retain(|h| live.contains(h.as_str()));
    }

    let selected = ui.selected.borrow().clone();
    let mapped: Vec<Row> = rows
        .iter()
        .map(|r| to_row(r, &ui.tr.borrow(), selected.contains(&r.info_hash)))
        .collect();
    model.set_vec(mapped);

    let (down, up) = ui.session.session_rates();
    window.set_session_rates(
        format!(
            "DL: {}, UL: {}",
            utils::to_human_speed(down),
            utils::to_human_speed(up)
        )
        .into(),
    );
    window.set_dht_nodes(
        ui.session
            .dht_nodes()
            .map_or_else(|| String::from("-"), |n| n.to_string())
            .into(),
    );
    window.set_selected_count(selected.len() as i32);

    // Details follow the first selected row, matching the Win32 panel.
    if let Some(first) = rows.iter().find(|r| selected.contains(&r.info_hash)) {
        set_details(window, first, &ui.tr.borrow());
        refresh_detail_tab(window, ui, &first.info_hash);
    } else if selected.is_empty() {
        clear_details(window);
        clear_detail_tabs(window);
    }

    *ui.rows.borrow_mut() = rows;
}

/// Fill in whichever detail tab is on screen.
///
/// Only the visible one: peers() and tracker_rows() are per-torrent queries and
/// this runs every second, so filling all three would pay for two nobody is
/// looking at. Tab order matches the TabWidget - Overview, Files, Peers,
/// Surface what the session has been queuing up: finished downloads and
/// errors. Port of the two loops at the end of mainwindow.rs::refresh.
///
/// Both queues must be drained every tick whether or not anything is shown,
/// or they grow without bound.
fn drain_notifications(window: &MainWindow, ui: &Rc<Ui>) {
    use crate::bittorrent::session::SessionEvent;

    let notify = ui.cfg.get_bool(crate::core::toast::ENABLED_KEY);
    // try_recv, not recv: this runs on the UI thread and must never block.
    // Drained whether or not anything is shown - the channel behind it grows
    // otherwise, so turning notifications off must not turn draining off too.
    //
    // The borrow ends with each `let`, not at the end of the body: a handler
    // below that reached back into the queue would otherwise panic on a second
    // borrow rather than misbehave visibly.
    loop {
        let Ok(event) = ui.events.borrow().try_recv() else {
            break;
        };

        match event {
            SessionEvent::TorrentCompleted { name, .. } if notify => {
                // A real desktop notification, not the in-app toast: a finished
                // download is worth seeing when the window is behind something
                // else. No-op off Windows for now - see core::toast.
                tracing::info!("showing completion notification: {name}");
                crate::core::toast::download_complete(
                    &ui.tr.borrow().i18n("download_complete"),
                    &name,
                );
            }
            // A toast rather than a message box: these arrive from background
            // work, and a modal stealing focus mid-download is worse than a
            // message that fades. Already logged where it was raised.
            SessionEvent::Error(err) => show_toast(window, &err),
            _ => {}
        }
    }
}

/// Group torrent file paths into a directory tree, flattened back out into
/// rows: `(file index or None for a folder, depth, leaf name)`.
///
/// Slint has no tree widget, so the tree is expressed as indentation. Folder
/// rows carry no index because only real files map back to the torrent's file
/// list, which is what the include checkboxes act on.
fn file_tree<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<(Option<usize>, usize, &'a str)> {
    // A BTreeMap so sibling folders come out sorted rather than in whatever
    // order the metainfo happened to use. The empty path sorts first, which
    // puts root-level files above the folders.
    let mut tree: std::collections::BTreeMap<Vec<&str>, Vec<(usize, &str)>> =
        std::collections::BTreeMap::new();
    for (index, path) in paths.enumerate() {
        let mut parts: Vec<&str> = path.split(['/', '\\']).collect();
        let leaf = parts.pop().unwrap_or("");
        tree.entry(parts).or_default().push((index, leaf));
    }

    let mut rows = Vec::new();
    let mut open_dirs: Vec<&str> = Vec::new();
    for (dir, entries) in tree {
        // Emit only the folder components this path does not already share
        // with the previous one, so a deep tree does not repeat its parents.
        let shared = dir
            .iter()
            .zip(open_dirs.iter())
            .take_while(|(a, b)| a == b)
            .count();
        for (depth, name) in dir.iter().enumerate().skip(shared) {
            rows.push((None, depth, *name));
        }
        for (index, leaf) in entries {
            rows.push((Some(index), dir.len(), leaf));
        }
        open_dirs = dir;
    }
    rows
}

/// Trackers.
fn refresh_detail_tab(window: &MainWindow, ui: &Rc<Ui>, hash: &str) {
    match window.get_current_tab() {
        1 => {
            let files = ui.session.files(hash);
            let rows: Vec<FileEntryRow> = file_tree(files.iter().map(|f| f.name.as_str()))
                .into_iter()
                .map(|(index, depth, name)| match index {
                    Some(index) => FileEntryRow {
                        index: index as i32,
                        depth: depth as i32,
                        name: name.into(),
                        size: utils::to_human_file_size(files[index].length as i64).into(),
                        progress: files[index].progress,
                        included: files[index].included,
                    },
                    None => FileEntryRow {
                        index: -1,
                        depth: depth as i32,
                        name: name.into(),
                        size: SharedString::new(),
                        progress: 0.0,
                        included: true,
                    },
                })
                .collect();
            window.set_detail_files(ModelRc::new(VecModel::from(rows)));
        }
        2 => {
            let rows: Vec<PeerEntryRow> = ui
                .session
                .peers(hash)
                .into_iter()
                .map(|p| {
                    // Same GeoIP database the Win32 peers list uses; an
                    // unknown address just leaves the column blank.
                    let (iso, country) = ui.geoip.lookup(&p.addr).unwrap_or_default();
                    PeerEntryRow {
                        addr: p.addr.as_str().into(),
                        flag: iso.and_then(|iso| ui.flags.get(&iso)).unwrap_or_default(),
                        country: country.as_str().into(),
                        status: p.state.as_str().into(),
                        downloaded: utils::to_human_file_size(p.fetched_bytes as i64).into(),
                        pieces: p.pieces.to_string().into(),
                    }
                })
                .collect();
            window.set_detail_peers(ModelRc::new(VecModel::from(rows)));
        }
        3 => {
            let rows: Vec<TrackerEntryRow> = ui
                .session
                .tracker_rows(hash, &ui.tr.borrow())
                .into_iter()
                .map(|t| TrackerEntryRow {
                    url: t.label.as_str().into(),
                    status: t.status.as_str().into(),
                    seeds: t
                        .seeders
                        .map_or_else(|| String::from("-"), |v| v.to_string())
                        .into(),
                    leeches: t
                        .leechers
                        .map_or_else(|| String::from("-"), |v| v.to_string())
                        .into(),
                    fails: t.fails.to_string().into(),
                    next_announce: format_next_announce(t.next_announce).into(),
                    indented: matches!(t.kind, crate::bittorrent::session::TrackerRowKind::Tracker),
                })
                .collect();
            window.set_detail_trackers(ModelRc::new(VecModel::from(rows)));
        }
        // Overview needs nothing beyond what set_details already wrote.
        _ => {}
    }
}

/// Empty the Files, Peers and Trackers models.
///
/// Separate from [`clear_details`] because these are list models rather than
/// scalar properties, and the selection-change path clears them at a different
/// moment from the text fields.
fn clear_detail_tabs(window: &MainWindow) {
    window.set_detail_files(ModelRc::new(VecModel::from(Vec::<FileEntryRow>::new())));
    window.set_detail_peers(ModelRc::new(VecModel::from(Vec::<PeerEntryRow>::new())));
    window.set_detail_trackers(ModelRc::new(VecModel::from(Vec::<TrackerEntryRow>::new())));
}

/// "in 4m", or a dash when the tracker has not scheduled one.
fn format_next_announce(at: Option<std::time::SystemTime>) -> String {
    let Some(at) = at else {
        return String::from("-");
    };
    match at.duration_since(std::time::SystemTime::now()) {
        Ok(left) if left.as_secs() >= 60 => format!("in {}m", left.as_secs() / 60),
        Ok(left) => format!("in {}s", left.as_secs()),
        // Already due; the announce is in flight or about to be.
        Err(_) => String::from("now"),
    }
}

/// Fill the Overview tab from one torrent's status.
///
/// Formatting happens here rather than in the markup: Slint has no number
/// formatting, and these are the same helpers the list columns use.
fn set_details(window: &MainWindow, t: &TorrentStatus, tr: &Translator) {
    window.set_d_name(t.name.as_str().into());
    window.set_d_hash(t.info_hash.as_str().into());
    window.set_d_save_path(t.save_path.as_str().into());
    // The same translated state the list column shows, or the error if there
    // is one. (An earlier version read `t.error.is_empty().then(|| "")`, which
    // yields Some("") for a healthy torrent - so Status rendered blank.)
    window.set_d_status(if t.error.is_empty() {
        format::state_text(tr, t).into()
    } else {
        t.error.as_str().into()
    });
    window.set_d_downloaded(utils::to_human_file_size(t.all_time_download).into());
    window.set_d_uploaded(utils::to_human_file_size(t.all_time_upload).into());
    window.set_d_ratio(format!("{:.2}", t.ratio).into());
    window.set_d_peers(format!("{} ({})", t.peers_current, t.peers_total).into());
    window.set_d_added(format::date_text(&t.added_on).into());
    window.set_d_completed(format::opt_date_text(&t.completed_on).into());
    window.set_d_progress(t.progress);
}

/// Blank the Overview fields, for when nothing is selected.
fn clear_details(window: &MainWindow) {
    for set in [
        MainWindow::set_d_name,
        MainWindow::set_d_hash,
        MainWindow::set_d_save_path,
        MainWindow::set_d_status,
        MainWindow::set_d_downloaded,
        MainWindow::set_d_uploaded,
        MainWindow::set_d_ratio,
        MainWindow::set_d_peers,
        MainWindow::set_d_added,
        MainWindow::set_d_completed,
    ] {
        set(window, SharedString::from("-"));
    }
    window.set_d_progress(0.0);
}

/// Click, ctrl-click and shift-click, over a selection the model owns.
///
/// `StandardTableView` has no multi-select at all, so there is nothing to
/// delegate to - see spike/slint-list/README.md.
fn wire_selection(window: &MainWindow, ui: &Rc<Ui>, model: &Rc<VecModel<Row>>) {
    let (w, u, m) = (window.as_weak(), ui.clone(), model.clone());
    {
        let (u, m, w) = (ui.clone(), model.clone(), window.as_weak());
        window.on_clear_selection(move || {
            if u.selected.borrow().is_empty() {
                return;
            }
            u.selected.borrow_mut().clear();
            if let Some(window) = w.upgrade() {
                // Also clears the detail tabs - it calls clear_details when
                // nothing is selected.
                repaint_selection(&window, &u, &m);
            }
        });
    }

    window.on_row_pressed(move |index, ctrl, shift| {
        let index = index.max(0) as usize;
        let hashes: Vec<String> = u
            .rows
            .borrow()
            .iter()
            .map(|r| r.info_hash.clone())
            .collect();
        let Some(hash) = hashes.get(index).cloned() else {
            return;
        };

        {
            let mut selected = u.selected.borrow_mut();
            if shift {
                let anchor = *u.anchor.borrow();
                let (lo, hi) = (anchor.min(index), anchor.max(index));
                selected.clear();
                selected.extend(hashes[lo..=hi.min(hashes.len() - 1)].iter().cloned());
            } else if ctrl {
                if !selected.remove(&hash) {
                    selected.insert(hash);
                }
                *u.anchor.borrow_mut() = index;
            } else {
                selected.clear();
                selected.insert(hash);
                *u.anchor.borrow_mut() = index;
            }
        }

        if let Some(window) = w.upgrade() {
            repaint_selection(&window, &u, &m);
        }
    });

    // Right-click selects the row under the cursor unless it is already part
    // of the selection - so a context menu on a multi-selection keeps it.
    let (w, u, m) = (window.as_weak(), ui.clone(), model.clone());
    window.on_context_menu(move |index| {
        let index = index.max(0) as usize;
        let hash = u.rows.borrow().get(index).map(|r| r.info_hash.clone());
        let Some(hash) = hash else { return };

        if !u.selected.borrow().contains(&hash) {
            let mut selected = u.selected.borrow_mut();
            selected.clear();
            selected.insert(hash);
            *u.anchor.borrow_mut() = index;
            drop(selected);
            if let Some(window) = w.upgrade() {
                repaint_selection(&window, &u, &m);
            }
        }

        // Labels are read here rather than cached: Preferences can add one
        // while the window is open, and the menu is about to be shown.
        if let Some(window) = w.upgrade() {
            let names = std::iter::once(u.tr.borrow().i18n("none"))
                .chain(u.cfg.get_labels().into_iter().map(|l| l.name))
                .map(SharedString::from)
                .collect::<Vec<_>>();
            window.set_ctx_label_names(ModelRc::new(VecModel::from(names)));
            // Both submenus start closed, or one left open stays open on the
            // next torrent.
            window.set_ctx_labels_open(false);
            window.set_ctx_queue_open(false);
        }

        // Grey out whichever of Pause/Resume does not apply, judged by the
        // first selected torrent. Done here because this fires just before the
        // menu opens, so the state is never stale.
        let paused = u
            .rows
            .borrow()
            .iter()
            .find(|r| u.selected.borrow().contains(&r.info_hash))
            .map(|r| r.paused);
        if let Some(window) = w.upgrade() {
            window.set_ctx_can_pause(paused != Some(true));
            window.set_ctx_can_resume(paused != Some(false));
        }
    });
}

/// Re-flag the rows in place rather than waiting for the next tick, so a click
/// highlights immediately instead of up to a second later.
fn repaint_selection(window: &MainWindow, ui: &Rc<Ui>, model: &Rc<VecModel<Row>>) {
    let selected = ui.selected.borrow();
    for (i, status) in ui.rows.borrow().iter().enumerate() {
        let want = selected.contains(&status.info_hash);
        if let Some(mut row) = model.row_data(i)
            && row.selected != want
        {
            row.selected = want;
            model.set_row_data(i, row);
        }
    }
    window.set_selected_count(selected.len() as i32);

    if let Some(first) = ui
        .rows
        .borrow()
        .iter()
        .find(|r| selected.contains(&r.info_hash))
    {
        set_details(window, first, &ui.tr.borrow());
    } else {
        clear_details(window);
    }

    // Ask for a frame explicitly. Mutating the model marks it dirty, but the
    // context-menu path then hands control to a NATIVE menu (muda ->
    // TrackPopupMenu on Windows), which runs a nested modal message loop - and
    // a dirty model that has not been drawn yet stays undrawn for as long as
    // that loop owns the thread. The result is a selection that is correct in
    // memory and a row that is still highlighted somewhere else on screen.
    window.window().request_redraw();
}

/// Menu and context-menu commands, dispatched by name.
fn wire_actions(window: &MainWindow, ui: &Rc<Ui>) {
    let (u, w) = (ui.clone(), window.as_weak());
    window.on_action(move |what| {
        let targets = u.targets();
        match what.as_str() {
            "pause" => targets.iter().for_each(|h| u.session.pause(h)),
            "resume" => targets.iter().for_each(|h| u.session.resume(h)),
            "recheck" => targets.iter().for_each(|h| u.session.recheck(h)),
            "queue-up" => targets.iter().for_each(|h| u.session.queue_move(h, true)),
            "queue-down" => targets.iter().for_each(|h| u.session.queue_move(h, false)),
            "remove" => targets.iter().for_each(|h| u.session.remove(h, false)),
            "remove-files" => targets.iter().for_each(|h| u.session.remove(h, true)),
            "copy-hash" => {
                if let Some(window) = w.upgrade() {
                    copy_to_clipboard(&window, &u, targets.join("\n"));
                }
            }
            "copy-magnet" => {
                let rows = u.rows.borrow();
                let magnets: Vec<String> = rows
                    .iter()
                    .filter(|r| targets.contains(&r.info_hash))
                    .map(|r| u.session.magnet_uri(&r.info_hash, &r.name))
                    .collect();
                drop(rows);
                if let Some(window) = w.upgrade() {
                    copy_to_clipboard(&window, &u, magnets.join("\n"));
                }
            }
            "move" => {
                let Some(dir) = rfd::FileDialog::new()
                    .set_title(u.tr.borrow().i18n("move"))
                    .pick_folder()
                else {
                    return;
                };
                let dir = dir.to_string_lossy().into_owned();
                for hash in &targets {
                    u.session.move_storage(hash, &dir);
                }
            }
            "open-folder" => {
                let rows = u.rows.borrow();
                if let Some(row) = rows.iter().find(|r| targets.contains(&r.info_hash)) {
                    utils::open_and_select(std::path::Path::new(&row.save_path));
                }
            }
            "add-magnet" => open_add_magnet(&u),
            "add-torrent" => pick_and_queue_torrents(&u),
            "preferences" => open_preferences(&u),
            "about" => open_about(&u),
            "create" => open_create_torrent(&u),
            "docs" => {
                if let Err(err) = open::that(WEBSITE) {
                    tracing::error!("cannot open {WEBSITE}: {err}");
                }
            }
            "exit" => {
                if let Some(window) = w.upgrade()
                    && request_close(&window, &u) == slint::CloseRequestResponse::HideWindow
                {
                    let _ = window.hide();
                }
            }
            // The tray's Exit. Deliberate enough not to be second-guessed.
            "exit-now" => {
                let _ = slint::quit_event_loop();
            }
            // The dialogs are still Win32-only; the port lands them next.
            other => tracing::info!("action '{other}' is not implemented in the Slint UI yet"),
        }
    });

    // Sorting is the model's job - `ListView` has no opinion about order.
    let (w, u) = (window.as_weak(), ui.clone());
    window.on_sort(move |column| {
        let column = column.max(0) as usize;
        let next = match *u.sort.borrow() {
            // Clicking the sorted column flips it; a third click would
            // conventionally clear the sort, but the Win32 list does not do
            // that either, so it just toggles.
            Some((c, ascending)) if c == column => Some((column, !ascending)),
            _ => Some((column, true)),
        };
        *u.sort.borrow_mut() = next;

        if let Some(window) = w.upgrade()
            && let Some((c, ascending)) = next
        {
            window.set_sort_column(c as i32);
            window.set_sort_ascending(ascending);
        }
    });
}

/// Open the Add magnet link(s) dialog.
///
/// The dialog is a separate `Window`, which is what the Win32 build does and
/// what the platform expects. It is stashed on `Ui` so it stays alive while
/// shown; dismissing hides it rather than dropping it, because dropping a
/// window from inside its own callback is asking for trouble.
fn open_add_magnet(ui: &Rc<Ui>) {
    // Reuse the window if it is already up, so a second File > Add magnet does
    // not stack dialogs.
    if let Some(existing) = ui.magnet_dialog.borrow().as_ref() {
        let _ = existing.show();
        return;
    }

    let dialog = match AddMagnetDialog::new() {
        Ok(d) => d,
        Err(err) => {
            tracing::error!("cannot create the add-magnet dialog: {err}");
            return;
        }
    };

    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_accepted(move || {
            let Some(d) = weak.upgrade() else { return };
            // Shared with the Win32 dialog, so both accept bare info hashes.
            let links = crate::ui::torrentfile::parse_magnet_links(&d.get_links());
            if links.is_empty() {
                // Nothing recognisable - leave the dialog up rather than
                // closing it and silently doing nothing.
                tracing::info!("add magnet: no magnet links or info hashes in the input");
                return;
            }
            for magnet in links {
                u.session
                    .add_torrent(AddTorrentSource::MagnetUri(magnet), default_add_params());
            }
            d.set_links(SharedString::new());
            dismiss(&u, &d);
        });
    }

    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_cancelled(move || {
            if let Some(d) = weak.upgrade() {
                dismiss(&u, &d);
            }
        });
    }

    wire_dialog_close(&dialog, ui);
    let _ = dialog.show();
    *ui.magnet_dialog.borrow_mut() = Some(dialog);
}

/// File > Add torrent: pick one or more `.torrent` files, then run them
/// through the Add dialog one at a time.
///
/// `rfd` gives the native picker Slint does not have. It blocks the event loop
/// while open, which is what a modal file dialog is supposed to do.
fn pick_and_queue_torrents(ui: &Rc<Ui>) {
    let Some(paths) = rfd::FileDialog::new()
        .add_filter("Torrent files", &["torrent"])
        .set_title("Add torrent(s)")
        .pick_files()
    else {
        return; // cancelled
    };

    for path in paths {
        match std::fs::read(&path) {
            Ok(bytes) => ui.pending.borrow_mut().push(bytes),
            Err(err) => tracing::error!("cannot read {}: {err}", path.display()),
        }
    }
    show_next_pending(ui);
}

/// Show the Add dialog for the next queued `.torrent`, if any and if one is
/// not already up.
/// Show the Add-torrent dialog for everything sitting in `pending`.
///
/// One dialog for the whole batch, not one per file: selecting eight torrents
/// used to mean eight dialogs in a row, with no way to look at the seventh
/// before committing to the first. The queue column lists them all and the
/// file list follows whichever is selected, so every torrent can be inspected
/// and have files ticked off before a single Add.
///
/// Save path and start-immediately are batch-wide - they are the settings
/// people want applied uniformly. File selection stays per torrent, which is
/// why each gets its own model rather than one shared list.
fn show_next_pending(ui: &Rc<Ui>) {
    if ui.torrent_dialog.borrow().is_some() {
        return; // already showing a batch; these wait in `pending`
    }

    let queued: Vec<Vec<u8>> = std::mem::take(&mut ui.pending.borrow_mut());
    if queued.is_empty() {
        return;
    }

    // Parse first, then build the dialog: a batch of unreadable files should
    // produce one error each and no empty dialog at the end of it.
    let mut parsed = Vec::new();
    for bytes in queued {
        match crate::ui::torrentfile::parse(&bytes) {
            // One bad file does not strand the rest of the batch.
            Err(err) => tracing::error!("{err}"),
            Ok(p) => parsed.push((bytes, p)),
        }
    }
    if parsed.is_empty() {
        return;
    }

    let dialog = match AddTorrentDialog::new() {
        Ok(d) => d,
        Err(err) => {
            tracing::error!("cannot create the add-torrent dialog: {err}");
            return;
        }
    };

    // One file model per torrent, kept alive for the life of the dialog so
    // ticking files in one, looking at another and coming back does not lose
    // the first one's choices.
    let file_models: Vec<Rc<VecModel<FileRow>>> = parsed
        .iter()
        .map(|(_, t)| {
            Rc::new(VecModel::from(
                file_tree(t.files.iter().map(|(name, _)| name.as_str()))
                    .into_iter()
                    .map(|(index, depth, name)| FileRow {
                        index: index.map_or(-1, |i| i as i32),
                        depth: depth as i32,
                        name: name.into(),
                        size: index
                            .map(|i| utils::to_human_file_size(t.files[i].1 as i64).into())
                            .unwrap_or_default(),
                        included: true,
                    })
                    .collect::<Vec<_>>(),
            ))
        })
        .collect();

    dialog.set_queue(ModelRc::new(VecModel::from(
        parsed
            .iter()
            .map(|(_, t)| QueueRow {
                name: t.name.as_str().into(),
                size: utils::to_human_file_size(t.total_size).into(),
            })
            .collect::<Vec<_>>(),
    )));
    dialog.set_save_path(
        ui.cfg
            .get_string("default_save_path")
            .unwrap_or_default()
            .into(),
    );

    let parsed = Rc::new(parsed);
    let file_models = Rc::new(file_models);

    // Selecting a torrent swaps which model the file list is bound to, and
    // repoints the name/size above it. Called once up front for the first.
    let select = {
        let (weak, p, m) = (dialog.as_weak(), parsed.clone(), file_models.clone());
        move |index: i32| {
            let (Some(d), Ok(i)) = (weak.upgrade(), usize::try_from(index)) else {
                return;
            };
            let Some((_, t)) = p.get(i) else { return };
            d.set_selected_torrent(index);
            d.set_torrent_name(t.name.as_str().into());
            d.set_torrent_size(utils::to_human_file_size(t.total_size).into());
            d.set_files(ModelRc::from(m[i].clone()));
        }
    };
    select(0);
    dialog.on_select_torrent(select);

    {
        let (weak, m) = (dialog.as_weak(), file_models.clone());
        dialog.on_toggle_file(move |index| {
            let Some(d) = weak.upgrade() else { return };
            let Ok(sel) = usize::try_from(d.get_selected_torrent()) else {
                return;
            };
            let index = index.max(0) as usize;
            if let Some(f) = m.get(sel)
                && let Some(mut row) = f.row_data(index)
            {
                row.included = !row.included;
                f.set_row_data(index, row);
            }
        });
    }

    {
        let weak = dialog.as_weak();
        dialog.on_browse(move || {
            let Some(dir) = rfd::FileDialog::new()
                .set_title("Save torrent to")
                .pick_folder()
            else {
                return;
            };
            if let Some(d) = weak.upgrade() {
                d.set_save_path(dir.to_string_lossy().as_ref().into());
            }
        });
    }

    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        let (p, m) = (parsed.clone(), file_models.clone());
        dialog.on_accepted(move || {
            let Some(d) = weak.upgrade() else { return };

            let save_path = d.get_save_path().to_string();
            let save_path = (!save_path.trim().is_empty()).then_some(save_path);
            let start = d.get_start_torrent();

            for (i, (bytes, _)) in p.iter().enumerate() {
                // row.index, not the row number: the model has folder rows in
                // it, and only_files indexes the torrent's own file list.
                let mut total = 0;
                let mut included: Vec<usize> = Vec::new();
                for r in 0..m[i].row_count() {
                    let Some(row) = m[i].row_data(r) else { continue };
                    let Ok(index) = usize::try_from(row.index) else {
                        continue; // a folder row
                    };
                    total += 1;
                    if row.included {
                        included.push(index);
                    }
                }
                // None means "everything", which is not the same as an explicit
                // list of all of them - keep the distinction the session expects.
                let only_files = (included.len() != total).then_some(included);

                u.session.add_torrent(
                    AddTorrentSource::TorrentFileBytes(bytes.clone()),
                    AddParams {
                        save_path: save_path.clone(),
                        start_torrent: start,
                        only_files,
                        label_id: None,
                    },
                );
            }

            dismiss(&u, &d);
            *u.torrent_dialog.borrow_mut() = None;
            // Anything dropped in while the dialog was open gets its own batch.
            show_next_pending(&u);
        });
    }

    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_cancelled(move || {
            if let Some(d) = weak.upgrade() {
                dismiss(&u, &d);
            }
            *u.torrent_dialog.borrow_mut() = None;
            show_next_pending(&u);
        });
    }

    wire_dialog_close(&dialog, ui);
    let _ = dialog.show();
    *ui.torrent_dialog.borrow_mut() = Some(dialog);
}

/// Themes and close actions, in the order the combo boxes show them. Kept
/// beside the load/save pair so an index cannot mean one thing going in and
/// another coming out.
const THEMES: [&str; 3] = ["system", "light", "dark"];
const CLOSE_ACTIONS: [&str; 3] = ["ask", "minimize", "exit"];

/// Reload the translator and repaint every caption in the new language.
///
/// The alternative the Win32 build uses is to ask for a restart
/// (`prompt_restart` in lang/*.json). Doing it live is nicer and costs one
/// property: every `L.s(L.revision, ...)` binding reads `revision`, so bumping
/// it re-evaluates all of them.
///
/// Only the main window is refreshed. Dialogs build their captions when they
/// open, so the next open is already in the new language - including the
/// Preferences dialog that triggered this, which is why its own labels stay in
/// the old language until it is closed and reopened.
fn apply_language(window: &MainWindow, ui: &Rc<Ui>) {
    let locale = ui
        .cfg
        .get_string("locale_name")
        .unwrap_or_else(|| String::from(crate::DEFAULT_LOCALE));
    if ui.tr.borrow().get_locale() == locale {
        return;
    }

    *ui.tr.borrow_mut() = Translator::load(&ui.env.get_lang_path(), &locale);

    // Column captions are computed in Rust rather than bound in markup, so
    // they do not come along with the revision bump.
    let (widths, titles, total) = columns(&ui.tr.borrow());
    let cols = window.global::<Cols>();
    cols.set_w(ModelRc::new(VecModel::from(widths)));
    cols.set_titles(ModelRc::new(VecModel::from(titles)));
    cols.set_total(total);

    let bump = |l: L<'_>| l.set_revision(l.get_revision().wrapping_add(1));
    bump(window.global::<L>());
    // The tray menu has its own globals, so it needs its own bump.
    if let Some(tray) = ui.tray.borrow().as_ref() {
        bump(tray.global::<L>());
    }
}

/// TLS modes in the order the combo box lists them, and the values stored in
/// `webui.tls_mode`.
const TLS_MODES: [&str; 3] = ["self-signed", "custom", "off"];

/// Why the current web-interface settings would be refused at startup, or an
/// empty string when they are fine.
///
/// These are the same three rules `webui::spawn` enforces. Checking them here
/// is not duplication for its own sake: without it, Ok looks like it worked
/// and the interface simply never appears.
fn web_warning(d: &PreferencesDialog, tr: &Translator, password_set: bool) -> String {
    if !d.get_web_enabled() {
        return String::new();
    }
    if !password_set && d.get_web_password().is_empty() {
        return tr.i18n("web_needs_password");
    }
    let bind = d.get_web_bind();
    let loopback = bind.is_empty() || bind == "127.0.0.1" || bind == "::1" || bind == "localhost";
    if TLS_MODES[d.get_web_tls_index().max(0) as usize % TLS_MODES.len()] == "off" && !loopback {
        return tr.i18n("web_needs_tls");
    }
    if TLS_MODES[d.get_web_tls_index().max(0) as usize % TLS_MODES.len()] == "custom"
        && (d.get_web_cert_path().is_empty() || d.get_web_key_path().is_empty())
    {
        return tr.i18n("web_needs_cert");
    }
    String::new()
}

/// The Web interface tab: the same settings `--webui-set` exposes, plus the
/// password, which the CLI can only take through a prompt.
fn wire_web(dialog: &PreferencesDialog, ui: &Rc<Ui>) {
    let cfg = &ui.cfg;

    dialog.set_web_enabled(cfg.get_bool("webui.enabled"));
    dialog.set_web_bind(
        cfg.get_string("webui.bind_address")
            .unwrap_or_else(|| String::from("127.0.0.1"))
            .into(),
    );
    dialog.set_web_port(cfg.get_int("webui.port").unwrap_or(8443).to_string().into());
    dialog.set_web_username(
        cfg.get_string("webui.username")
            .unwrap_or_else(|| String::from("nanotorrent"))
            .into(),
    );
    // The stored value is an Argon2 hash and cannot be shown; the field starts
    // empty and only says whether one exists.
    let password_set = !cfg
        .get_string("webui.password_hash")
        .unwrap_or_default()
        .is_empty();
    dialog.set_web_password_set(password_set);
    dialog.set_web_password(SharedString::new());

    dialog.set_web_tls_modes(ModelRc::new(VecModel::from(
        ["tls_self_signed", "tls_custom", "tls_off"]
            .iter()
            .map(|key| ui_string(&ui.tr.borrow(), key))
            .collect::<Vec<_>>(),
    )));
    let mode = cfg
        .get_string("webui.tls_mode")
        .unwrap_or_else(|| String::from("self-signed"));
    dialog.set_web_tls_index(TLS_MODES.iter().position(|m| *m == mode).unwrap_or(0) as i32);
    dialog.set_web_cert_path(cfg.get_string("webui.tls_cert_path").unwrap_or_default().into());
    dialog.set_web_key_path(cfg.get_string("webui.tls_key_path").unwrap_or_default().into());

    let refresh = {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        move || {
            if let Some(d) = weak.upgrade() {
                let warning = web_warning(&d, &u.tr.borrow(), d.get_web_password_set());
                d.set_web_warning(warning.into());
            }
        }
    };
    refresh();
    dialog.on_web_changed(refresh);

    {
        let (status, ok) = web_status(ui);
        dialog.set_web_status(status.into());
        dialog.set_web_status_ok(ok);
    }

    {
        let weak = dialog.as_weak();
        dialog.on_browse_web_cert(move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("PEM certificate", &["pem", "crt", "cer"])
                .pick_file()
                && let Some(d) = weak.upgrade()
            {
                d.set_web_cert_path(path.to_string_lossy().as_ref().into());
                d.invoke_web_changed();
            }
        });
    }
    {
        let weak = dialog.as_weak();
        dialog.on_browse_web_key(move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("PEM private key", &["pem", "key"])
                .pick_file()
                && let Some(d) = weak.upgrade()
            {
                d.set_web_key_path(path.to_string_lossy().as_ref().into());
                d.invoke_web_changed();
            }
        });
    }
}

/// Describe what the web interface is currently doing, for the status line.
fn web_status(ui: &Rc<Ui>) -> (String, bool) {
    if ui.web.borrow().is_none() {
        return (ui.tr.borrow().i18n("web_not_running"), false);
    }
    let scheme = match ui
        .cfg
        .get_string("webui.tls_mode")
        .unwrap_or_default()
        .as_str()
    {
        "off" => "http",
        _ => "https",
    };
    let url = format!(
        "{scheme}://{}:{}",
        ui.cfg
            .get_string("webui.bind_address")
            .unwrap_or_else(|| String::from("127.0.0.1")),
        ui.cfg.get_int("webui.port").unwrap_or(8443)
    );
    (
        ui.tr.borrow().i18n1("web_listening_on", &url),
        true,
    )
}

/// Stop the web interface and start it again from the settings just saved.
///
/// The server is otherwise only spawned at startup, which meant pressing Ok
/// appeared to do nothing at all.
fn apply_web(d: &PreferencesDialog, ui: &Rc<Ui>) {
    let current = ui.web.borrow_mut().take();
    match crate::webui::restart(
        current,
        ui.session.clone(),
        ui.cfg.clone(),
        ui.env.clone(),
    ) {
        Ok(handle) => {
            *ui.web.borrow_mut() = handle;
            let (status, ok) = web_status(ui);
            d.set_web_status(status.into());
            d.set_web_status_ok(ok);
        }
        Err(err) => {
            // The three rules the tab already warns about end up here too if
            // someone saves anyway - show the server's own words.
            tracing::error!("web interface did not restart: {err:#}");
            d.set_web_status(format!("{err:#}").into());
            d.set_web_status_ok(false);
        }
    }
}

/// Write the Web interface tab back. Called from `save_preferences`.
fn save_web(d: &PreferencesDialog, ui: &Rc<Ui>) {
    let cfg = &ui.cfg;
    cfg.set("webui.enabled", &d.get_web_enabled());
    cfg.set("webui.bind_address", &d.get_web_bind().to_string());
    if let Ok(port) = d.get_web_port().trim().parse::<i64>()
        && (1..=65535).contains(&port)
    {
        cfg.set("webui.port", &port);
    }
    cfg.set("webui.username", &d.get_web_username().to_string());
    cfg.set(
        "webui.tls_mode",
        &TLS_MODES[(d.get_web_tls_index().max(0) as usize).min(TLS_MODES.len() - 1)],
    );
    cfg.set("webui.tls_cert_path", &d.get_web_cert_path().to_string());
    cfg.set("webui.tls_key_path", &d.get_web_key_path().to_string());

    // Empty means "keep the stored hash". Anything else is hashed here - the
    // password itself is never written to the database.
    let password = d.get_web_password();
    if !password.is_empty() {
        match crate::webui::Credentials::hash_password(password.as_str()) {
            Ok(hash) => cfg.set("webui.password_hash", &hash),
            Err(err) => tracing::error!("could not hash the web interface password: {err}"),
        }
    }
}

/// Refill the Labels & filters lists from the database.
///
/// Called after every save and delete rather than mutating the models: the
/// database assigns ids on insert, so the in-memory row would otherwise keep
/// id -1 and the next save would insert a duplicate.
fn reload_rules(d: &PreferencesDialog, ui: &Rc<Ui>) {
    let labels: Vec<NamedRule> = ui
        .cfg
        .get_labels()
        .into_iter()
        .map(|l| NamedRule {
            id: l.id,
            name: l.name.into(),
            rule: l.save_path.into(),
        })
        .collect();
    let filters: Vec<NamedRule> = ui
        .cfg
        .get_filters()
        .into_iter()
        .map(|f| NamedRule {
            id: f.id,
            name: f.name.into(),
            rule: f.filter.into(),
        })
        .collect();
    d.set_labels(ModelRc::new(VecModel::from(labels)));
    d.set_filters(ModelRc::new(VecModel::from(filters)));
}

/// Clear the label editor back to "new label".
fn clear_label_editor(d: &PreferencesDialog) {
    d.set_label_index(-1);
    d.set_edit_label_name(SharedString::new());
    d.set_edit_label_path(SharedString::new());
}

/// Clear the filter editor back to "new filter".
fn clear_filter_editor(d: &PreferencesDialog) {
    d.set_filter_index(-1);
    d.set_edit_filter_name(SharedString::new());
    d.set_edit_filter_rule(SharedString::new());
    d.set_filter_valid(true);
}

/// The Labels & filters tab: two list-plus-editor panes over the label and
/// filter tables.
///
/// Unlike the rest of Preferences these write straight through rather than
/// waiting for Ok. A list with its own Save and Delete buttons that then did
/// nothing until a second, different button was pressed would be a trap - and
/// the torrent context menu reads labels live.
fn wire_rules(dialog: &PreferencesDialog, ui: &Rc<Ui>) {
    reload_rules(dialog, ui);

    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_pick_label(move |index| {
            let Some(d) = weak.upgrade() else { return };
            let labels = u.cfg.get_labels();
            let Some(label) = usize::try_from(index).ok().and_then(|i| labels.get(i)) else {
                return;
            };
            d.set_label_index(index);
            d.set_edit_label_name(label.name.as_str().into());
            d.set_edit_label_path(label.save_path.as_str().into());
        });
    }
    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_save_label(move || {
            let Some(d) = weak.upgrade() else { return };
            let labels = u.cfg.get_labels();
            let existing = usize::try_from(d.get_label_index())
                .ok()
                .and_then(|i| labels.get(i));
            let path = d.get_edit_label_path().to_string();
            let label = crate::core::configuration::Label {
                // -1 inserts; anything else updates that row.
                id: existing.map_or(-1, |l| l.id),
                name: d.get_edit_label_name().to_string(),
                // A save path only counts when there is one, which is what the
                // enabled flag means to the rest of the code.
                save_path_enabled: !path.trim().is_empty(),
                save_path: path,
                ..existing.cloned().unwrap_or_default()
            };
            u.cfg.upsert_label(&label);
            reload_rules(&d, &u);
            clear_label_editor(&d);
        });
    }
    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_delete_label(move || {
            let Some(d) = weak.upgrade() else { return };
            let labels = u.cfg.get_labels();
            if let Some(label) = usize::try_from(d.get_label_index())
                .ok()
                .and_then(|i| labels.get(i))
            {
                // delete_label also clears the label off any torrent using it.
                u.cfg.delete_label(label.id);
            }
            reload_rules(&d, &u);
            clear_label_editor(&d);
        });
    }
    {
        let weak = dialog.as_weak();
        dialog.on_browse_label_path(move || {
            if let Some(dir) = rfd::FileDialog::new().pick_folder()
                && let Some(d) = weak.upgrade()
            {
                d.set_edit_label_path(dir.to_string_lossy().as_ref().into());
            }
        });
    }

    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_pick_filter(move |index| {
            let Some(d) = weak.upgrade() else { return };
            let filters = u.cfg.get_filters();
            let Some(filter) = usize::try_from(index).ok().and_then(|i| filters.get(i)) else {
                return;
            };
            d.set_filter_index(index);
            d.set_edit_filter_name(filter.name.as_str().into());
            d.set_edit_filter_rule(filter.filter.as_str().into());
            d.set_filter_valid(crate::ui::filters::TorrentFilter::parse(&filter.filter).is_ok());
        });
    }
    {
        let weak = dialog.as_weak();
        dialog.on_filter_rule_changed(move |text| {
            if let Some(d) = weak.upgrade() {
                // An empty box is "not written yet", not "wrong".
                let text = text.trim();
                d.set_filter_valid(
                    text.is_empty() || crate::ui::filters::TorrentFilter::parse(text).is_ok(),
                );
            }
        });
    }
    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_save_filter(move || {
            let Some(d) = weak.upgrade() else { return };
            let filters = u.cfg.get_filters();
            let existing = usize::try_from(d.get_filter_index())
                .ok()
                .and_then(|i| filters.get(i));
            let rule = d.get_edit_filter_rule().to_string();
            // Refuse rather than store something the list can never apply -
            // the button is disabled for this too, so this is the backstop.
            if crate::ui::filters::TorrentFilter::parse(&rule).is_err() {
                d.set_filter_valid(false);
                return;
            }
            u.cfg.upsert_filter(&crate::core::configuration::Filter {
                id: existing.map_or(-1, |f| f.id),
                name: d.get_edit_filter_name().to_string(),
                filter: rule,
            });
            reload_rules(&d, &u);
            clear_filter_editor(&d);
        });
    }
    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_delete_filter(move || {
            let Some(d) = weak.upgrade() else { return };
            let filters = u.cfg.get_filters();
            if let Some(filter) = usize::try_from(d.get_filter_index())
                .ok()
                .and_then(|i| filters.get(i))
            {
                u.cfg.delete_filter(filter.id);
            }
            reload_rules(&d, &u);
            clear_filter_editor(&d);
        });
    }
}

/// Open the Preferences dialog, building it on first use and re-showing the
/// same window - with every tab reloaded - after that.
fn open_preferences(ui: &Rc<Ui>) {
    if let Some(existing) = ui.prefs_dialog.borrow().as_ref() {
        // Reload rather than just re-showing: the dialog is kept alive between
        // opens, so without this it comes back holding whatever was last typed
        // - including a password still sitting in the field, and settings that
        // may have changed underneath it since.
        load_preferences(existing, ui);
        wire_rules(existing, ui);
        wire_web(existing, ui);
        let _ = existing.show();
        return;
    }

    let dialog = match PreferencesDialog::new() {
        Ok(d) => d,
        Err(err) => {
            tracing::error!("cannot create the preferences dialog: {err}");
            return;
        }
    };

    load_preferences(&dialog, ui);
    wire_rules(&dialog, ui);
    wire_web(&dialog, ui);

    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_accepted(move || {
            let Some(d) = weak.upgrade() else { return };
            save_preferences(&d, &u);
            // After the settings are written, not before: restart reads them
            // back from the database.
            apply_web(&d, &u);
            // Theme and language both have to be pushed into the main
            // window; everything else it re-reads on the next tick.
            if let Some(window) = u.main.borrow().as_ref().and_then(|w| w.upgrade()) {
                apply_language(&window, &u);
                window.set_theme_id(
                    u.cfg
                        .get_string("theme_id")
                        .unwrap_or_else(|| String::from("system"))
                        .into(),
                );
            }
            // Rebuild the librqbit session with the new settings, exactly as
            // mainwindow.rs does after its dialog closes.
            u.session.apply_settings(&u.env, &u.cfg);
            dismiss(&u, &d);
        });
    }
    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_cancelled(move || {
            if let Some(d) = weak.upgrade() {
                dismiss(&u, &d);
            }
        });
    }
    {
        let weak = dialog.as_weak();
        dialog.on_browse_save_path(move || {
            if let Some(dir) = rfd::FileDialog::new().set_title("Save path").pick_folder()
                && let Some(d) = weak.upgrade()
            {
                d.set_save_path(dir.to_string_lossy().as_ref().into());
            }
        });
    }
    {
        let weak = dialog.as_weak();
        dialog.on_browse_ipfilter(move || {
            if let Some(file) = rfd::FileDialog::new().set_title("IP filter").pick_file()
                && let Some(d) = weak.upgrade()
            {
                d.set_ipfilter_path(file.to_string_lossy().as_ref().into());
            }
        });
    }
    dialog.on_set_associations(|| match crate::core::file_assoc::register_torrent() {
        Ok(()) => tracing::info!("registered .torrent and magnet associations"),
        Err(err) => tracing::error!("could not register associations: {err:#}"),
    });

    wire_dialog_close(&dialog, ui);
    let _ = dialog.show();
    *ui.prefs_dialog.borrow_mut() = Some(dialog);
}

/// Fill every Preferences tab from the settings database.
///
/// Called on open and again on re-open: the dialog is kept alive between
/// showings, so without this it would still hold whatever was typed last time.
fn load_preferences(d: &PreferencesDialog, ui: &Rc<Ui>) {
    let cfg = &ui.cfg;

    // Translator::languages(), not EMBEDDED_LANGS: that is raw alphabetical
    // (build.rs sorts the filenames), whereas this puts English first and
    // sorts the rest - the order the list is meant to read in.
    //
    // Names, not locale tags: Translator carries the endonym for each one.
    let langs: Vec<SharedString> = ui.tr.borrow()
        .languages()
        .iter()
        .map(|l| SharedString::from(l.name.as_str()))
        .collect();
    let current = cfg
        .get_string("locale_name")
        .unwrap_or_else(|| String::from(crate::DEFAULT_LOCALE));
    // Matched on the locale, not the label: `langs` holds display names now,
    // so comparing the saved "nl-NL" against them never hit and the picker
    // always came up on English.
    let index = ui.tr.borrow()
        .languages()
        .iter()
        .position(|l| l.locale.eq_ignore_ascii_case(&current))
        .unwrap_or(0);
    d.set_language_index(index as i32);
    d.set_languages(ModelRc::new(VecModel::from(langs)));

    d.set_themes(ModelRc::new(VecModel::from(
        ["theme_system", "theme_light", "theme_dark"]
            .iter()
            .map(|key| ui_string(&ui.tr.borrow(), key))
            .collect::<Vec<_>>(),
    )));
    let theme = cfg
        .get_string("theme_id")
        .unwrap_or_else(|| String::from("system"));
    d.set_theme_index(THEMES.iter().position(|t| *t == theme).unwrap_or(0) as i32);

    d.set_close_actions(ModelRc::new(VecModel::from(
        ["close_ask_every_time", "close_minimize_short", "close_exit"]
            .iter()
            .map(|key| ui_string(&ui.tr.borrow(), key))
            .collect::<Vec<_>>(),
    )));
    // close_action is persistent - it survives restore-defaults, unlike the rest.
    let close = cfg.get_persistent("ui.close_action").unwrap_or_default();
    d.set_close_action_index(CLOSE_ACTIONS.iter().position(|a| *a == close).unwrap_or(0) as i32);

    d.set_skip_add_dialog(cfg.get_bool("skip_add_torrent_dialog"));
    d.set_show_in_tray(cfg.get_bool("show_in_notification_area"));
    d.set_minimize_to_tray(cfg.get_bool("minimize_to_notification_area"));
    d.set_notify_complete(cfg.get_bool(crate::core::toast::ENABLED_KEY));
    d.set_can_associate(cfg!(windows));

    d.set_save_path(
        cfg.get_string("default_save_path")
            .unwrap_or_default()
            .into(),
    );
    d.set_pause_on_low_disk(cfg.get_bool("pause_on_low_disk_space"));
    d.set_low_disk_limit(num(cfg.get_int("pause_on_low_disk_space_limit")));
    d.set_active_limit(num(cfg.get_int("libtorrent.active_limit")));
    d.set_active_downloads(num(cfg.get_int("libtorrent.active_downloads")));
    d.set_active_seeds(num(cfg.get_int("libtorrent.active_seeds")));
    d.set_limit_download(cfg.get_bool("libtorrent.enable_download_rate_limit"));
    d.set_download_limit(num(cfg.get_int("libtorrent.download_rate_limit")));
    d.set_limit_upload(cfg.get_bool("libtorrent.enable_upload_rate_limit"));
    d.set_upload_limit(num(cfg.get_int("libtorrent.upload_rate_limit")));

    // Listen interfaces are rows in their own table; the dialog edits the
    // first, which is what the Win32 one does too.
    let iface = cfg.get_listen_interfaces().into_iter().next();
    d.set_listen_address(
        iface
            .as_ref()
            .map(|i| i.address.clone())
            .unwrap_or_else(|| String::from("0.0.0.0"))
            .into(),
    );
    d.set_listen_port(num(iface.as_ref().map(|i| i.port as i64)));

    d.set_enable_dht(cfg.get_bool("libtorrent.enable_dht"));
    d.set_enable_lsd(cfg.get_bool("libtorrent.enable_lsd"));
    d.set_enable_pex(cfg.get_bool("libtorrent.enable_pex"));
    d.set_enable_geoip(cfg.get_bool("geoip.enabled"));
    d.set_enable_ipfilter(cfg.get_bool("ipfilter.enabled"));
    d.set_ipfilter_path(
        cfg.get_string("ipfilter.file_path")
            .unwrap_or_default()
            .into(),
    );
    d.set_require_outgoing_encryption(cfg.get_bool("libtorrent.require_outgoing_encryption"));
    d.set_require_incoming_encryption(cfg.get_bool("libtorrent.require_incoming_encryption"));
    d.set_anonymous_mode(cfg.get_bool("libtorrent.anonymous_mode"));

    d.set_proxy_types(ModelRc::new(VecModel::from(
        [
            "proxy_type_none",
            "socks4",
            "socks5",
            "socks5_with_credentials",
            "http",
            "http_with_credentials",
        ]
        .iter()
        .map(|key| ui_string(&ui.tr.borrow(), key))
        .collect::<Vec<_>>(),
    )));
    d.set_proxy_type_index(cfg.get_int("libtorrent.proxy_type").unwrap_or(0) as i32);
    d.set_proxy_host(
        cfg.get_string("libtorrent.proxy_host")
            .unwrap_or_default()
            .into(),
    );
    d.set_proxy_port(num(cfg.get_int("libtorrent.proxy_port")));
    d.set_proxy_hostnames(cfg.get_bool("libtorrent.proxy_hostnames"));
    d.set_proxy_peers(cfg.get_bool("libtorrent.proxy_peers"));
    d.set_proxy_trackers(cfg.get_bool("libtorrent.proxy_trackers"));
}

/// Write every Preferences tab back.
///
/// Settings are applied live - the caller rebuilds the session and restarts
/// the web interface afterwards - so nothing here needs a restart to take
/// effect.
fn save_preferences(d: &PreferencesDialog, ui: &Rc<Ui>) {
    let cfg = &ui.cfg;

    save_web(d, ui);

    if let Some(lang) = ui.tr.borrow()
        .languages()
        .get(d.get_language_index().max(0) as usize)
    {
        cfg.set("locale_name", &lang.locale);
    }
    cfg.set(
        "theme_id",
        &THEMES[(d.get_theme_index().max(0) as usize).min(THEMES.len() - 1)],
    );
    cfg.set_persistent(
        "ui.close_action",
        CLOSE_ACTIONS[(d.get_close_action_index().max(0) as usize).min(CLOSE_ACTIONS.len() - 1)],
    );

    cfg.set("skip_add_torrent_dialog", &d.get_skip_add_dialog());
    cfg.set("show_in_notification_area", &d.get_show_in_tray());
    cfg.set("minimize_to_notification_area", &d.get_minimize_to_tray());
    cfg.set(crate::core::toast::ENABLED_KEY, &d.get_notify_complete());

    cfg.set("default_save_path", &d.get_save_path().to_string());
    cfg.set("pause_on_low_disk_space", &d.get_pause_on_low_disk());
    set_num(
        cfg,
        "pause_on_low_disk_space_limit",
        &d.get_low_disk_limit(),
    );
    set_num(cfg, "libtorrent.active_limit", &d.get_active_limit());
    set_num(
        cfg,
        "libtorrent.active_downloads",
        &d.get_active_downloads(),
    );
    set_num(cfg, "libtorrent.active_seeds", &d.get_active_seeds());
    cfg.set(
        "libtorrent.enable_download_rate_limit",
        &d.get_limit_download(),
    );
    set_num(
        cfg,
        "libtorrent.download_rate_limit",
        &d.get_download_limit(),
    );
    cfg.set("libtorrent.enable_upload_rate_limit", &d.get_limit_upload());
    set_num(cfg, "libtorrent.upload_rate_limit", &d.get_upload_limit());

    if let Some(mut iface) = cfg.get_listen_interfaces().into_iter().next() {
        iface.address = d.get_listen_address().to_string();
        if let Ok(port) = d.get_listen_port().trim().parse::<i32>() {
            iface.port = port;
        }
        cfg.upsert_listen_interface(&iface);
    }

    cfg.set("libtorrent.enable_dht", &d.get_enable_dht());
    // Written back unchanged: the checkbox is disabled because librqbit 8
    // cannot apply it, but dropping the stored value would lose the preference.
    cfg.set("libtorrent.enable_lsd", &d.get_enable_lsd());
    cfg.set("libtorrent.enable_pex", &d.get_enable_pex());
    cfg.set("geoip.enabled", &d.get_enable_geoip());
    cfg.set("ipfilter.enabled", &d.get_enable_ipfilter());
    cfg.set("ipfilter.file_path", &d.get_ipfilter_path().to_string());
    cfg.set(
        "libtorrent.require_outgoing_encryption",
        &d.get_require_outgoing_encryption(),
    );
    cfg.set(
        "libtorrent.require_incoming_encryption",
        &d.get_require_incoming_encryption(),
    );
    cfg.set("libtorrent.anonymous_mode", &d.get_anonymous_mode());

    cfg.set("libtorrent.proxy_type", &(d.get_proxy_type_index() as i64));
    cfg.set("libtorrent.proxy_host", &d.get_proxy_host().to_string());
    set_num(cfg, "libtorrent.proxy_port", &d.get_proxy_port());
    cfg.set("libtorrent.proxy_hostnames", &d.get_proxy_hostnames());
    cfg.set("libtorrent.proxy_peers", &d.get_proxy_peers());
    cfg.set("libtorrent.proxy_trackers", &d.get_proxy_trackers());
}

/// A numeric setting as text for a LineEdit, empty when unset - so a field
/// with no value shows its placeholder rather than a misleading "0".
fn num(value: Option<i64>) -> SharedString {
    value.map(|v| v.to_string()).unwrap_or_default().into()
}

/// Write a numeric setting, ignoring input that is not a number.
///
/// Ignoring rather than zeroing: a field someone cleared or mistyped should
/// leave the stored value alone, not silently set a rate limit to 0.
fn set_num(cfg: &crate::core::configuration::Configuration, key: &str, text: &str) {
    if let Ok(value) = text.trim().parse::<i64>() {
        cfg.set(key, &value);
    }
}

/// Order the list by a column.
///
/// Compares the underlying `TorrentStatus` values, never the rendered cell
/// text. Sorting the strings is what the spike did and it is wrong the moment a
/// unit changes: "99.00 MB" sorts above "500.00 MB", and "9m" above "10m".
///
/// Column indices match `Cols.titles` in app.slint.
fn sort_rows(rows: &mut [TorrentStatus], column: usize, ascending: bool) {
    use std::cmp::Ordering;

    // Total order over f32 without unwrapping a partial_cmp that NaN can fail:
    // availability and ratio are computed, and a NaN would panic a sort.
    fn f(a: f32, b: f32) -> Ordering {
        a.partial_cmp(&b).unwrap_or(Ordering::Equal)
    }
    fn s(a: &str, b: &str) -> Ordering {
        a.to_lowercase().cmp(&b.to_lowercase())
    }

    rows.sort_by(|a, b| {
        let ord = match column {
            0 => s(&a.name, &b.name),
            1 => a.queue_position.cmp(&b.queue_position),
            2 => a.total_wanted.cmp(&b.total_wanted),
            3 => a.total_wanted_remaining.cmp(&b.total_wanted_remaining),
            4 => s(&format!("{:?}", a.state), &format!("{:?}", b.state)),
            5 => f(a.progress, b.progress),
            // No ETA sorts last ascending, which is where "unknown" belongs.
            6 => match (a.eta, b.eta) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
            7 => a.download_payload_rate.cmp(&b.download_payload_rate),
            8 => a.upload_payload_rate.cmp(&b.upload_payload_rate),
            9 => f(a.availability, b.availability),
            10 => f(a.ratio, b.ratio),
            11 => a.seeds_current.cmp(&b.seeds_current),
            12 => a.peers_current.cmp(&b.peers_current),
            13 => a.added_on.cmp(&b.added_on),
            14 => a.completed_on.cmp(&b.completed_on),
            15 => s(&a.label_name, &b.label_name),
            _ => Ordering::Equal,
        };
        // Name breaks ties, so equal values do not shuffle between ticks -
        // the whole model is rebuilt every second and an unstable order would
        // make rows jump under the cursor.
        let ord = ord.then_with(|| s(&a.name, &b.name));
        if ascending { ord } else { ord.reverse() }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bittorrent::torrentstatus::State;

    fn status(name: &str, size: i64, ratio: f32) -> TorrentStatus {
        TorrentStatus {
            added_on: chrono::Local::now(),
            all_time_download: 0,
            all_time_upload: 0,
            availability: 0.0,
            completed_on: None,
            download_payload_rate: 0,
            error: String::new(),
            eta: None,
            info_hash: name.to_string(),
            label_id: None,
            label_name: String::new(),
            name: name.to_string(),
            paused: false,
            peers_current: 0,
            peers_total: 0,
            progress: 0.0,
            queue_position: 0,
            ratio,
            save_path: String::new(),
            seeds_current: 0,
            seeds_total: 0,
            state: State::Downloading,
            total_wanted: size,
            total_wanted_remaining: 0,
            upload_payload_rate: 0,
        }
    }

    /// The bug this exists to prevent: sorting the rendered text puts
    /// "99.00 MB" above "500.00 MB" because it compares "9" with "5".
    #[test]
    fn size_sorts_numerically_not_as_text() {
        let mut rows = vec![
            status("small", 99 * 1024 * 1024, 0.0),
            status("big", 500 * 1024 * 1024, 0.0),
        ];
        sort_rows(&mut rows, 2, true);
        assert_eq!(rows[0].name, "small");
        assert_eq!(rows[1].name, "big");
    }

    #[test]
    fn direction_reverses() {
        let mut rows = vec![status("b", 2, 0.0), status("a", 1, 0.0)];
        sort_rows(&mut rows, 0, true);
        assert_eq!(rows[0].name, "a");
        sort_rows(&mut rows, 0, false);
        assert_eq!(rows[0].name, "b");
    }

    #[test]
    fn equal_values_keep_a_stable_order() {
        // Every tick rebuilds the model; without a tiebreak, rows with equal
        // values could swap places under the pointer.
        let mut rows = vec![
            status("charlie", 10, 1.0),
            status("alpha", 10, 1.0),
            status("bravo", 10, 1.0),
        ];
        sort_rows(&mut rows, 2, true);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn a_nan_does_not_panic() {
        // availability is computed and can be NaN; sort_by with a comparator
        // that unwrapped partial_cmp would abort here.
        let mut rows = vec![status("a", 1, f32::NAN), status("b", 1, 1.0)];
        sort_rows(&mut rows, 10, true);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn missing_eta_sorts_last_when_ascending() {
        let mut with = status("has-eta", 1, 0.0);
        with.eta = Some(std::time::Duration::from_secs(60));
        let mut rows = vec![status("no-eta", 1, 0.0), with];
        sort_rows(&mut rows, 6, true);
        assert_eq!(rows[0].name, "has-eta");
    }
}

/// The project's own site, shown in About and opened when it is clicked.
const WEBSITE: &str = "https://www.nanotorrent.org";

/// Who wrote it, and where to find them - the second link in About.
const DEVELOPER: &str = "Power2All";
const DEVELOPER_URL: &str = "https://www.power2all.com";

/// Open the About box: version, build stamp and a link to the project site.
fn open_about(ui: &Rc<Ui>) {
    if let Some(existing) = ui.about_dialog.borrow().as_ref() {
        let _ = existing.show();
        return;
    }

    let dialog = match AboutDialog::new() {
        Ok(d) => d,
        Err(err) => {
            tracing::error!("cannot create the about dialog: {err}");
            return;
        }
    };

    dialog.set_version(crate::buildinfo::version().into());
    dialog.set_build_stamp(crate::buildinfo::build_stamp().into());
    dialog.set_website(WEBSITE.into());
    dialog.set_developer(DEVELOPER.into());

    dialog.on_open_developer(|| {
        if let Err(err) = open::that(DEVELOPER_URL) {
            tracing::error!("cannot open {DEVELOPER_URL}: {err}");
        }
    });

    dialog.on_open_website(|| {
        // `open` picks the platform's handler, so this is the one place the
        // three OSes need no branching.
        if let Err(err) = open::that(WEBSITE) {
            tracing::error!("cannot open {WEBSITE}: {err}");
        }
    });
    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_closed(move || {
            if let Some(d) = weak.upgrade() {
                dismiss(&u, &d);
            }
        });
    }

    wire_dialog_close(&dialog, ui);
    let _ = dialog.show();
    *ui.about_dialog.borrow_mut() = Some(dialog);
}

/// Piece sizes offered by the Create dialog, and the values behind them.
/// `None` is "let the builder choose from the total size".
const PIECE_LENGTHS: [(&str, Option<u32>); 7] = [
    ("piece_size_auto", None),
    ("64 KB", Some(64 * 1024)),
    ("128 KB", Some(128 * 1024)),
    ("256 KB", Some(256 * 1024)),
    ("512 KB", Some(512 * 1024)),
    ("1 MB", Some(1024 * 1024)),
    ("2 MB", Some(2 * 1024 * 1024)),
];

/// Open the torrent-creation dialog (v1, v2 or hybrid, with tracker, comment
/// and private options).
fn open_create_torrent(ui: &Rc<Ui>) {
    if let Some(existing) = ui.create_dialog.borrow().as_ref() {
        let _ = existing.show();
        return;
    }

    let dialog = match CreateTorrentDialog::new() {
        Ok(d) => d,
        Err(err) => {
            tracing::error!("cannot create the create-torrent dialog: {err}");
            return;
        }
    };

    dialog.set_piece_lengths(ModelRc::new(VecModel::from(
        PIECE_LENGTHS
            .iter()
            .map(|(label, _)| {
                // Only the first entry is a word; "64 KB" and friends are the
                // same everywhere and are left alone.
                SharedString::from(if *label == "piece_size_auto" {
                    ui.tr.borrow().i18n(label)
                } else {
                    label.to_string()
                })
            })
            .collect::<Vec<_>>(),
    )));
    dialog.set_versions(ModelRc::new(VecModel::from(
        ["v1", "v2", &ui.tr.borrow().i18n("torrent_version_hybrid")]
            .iter()
            .map(|v| SharedString::from(*v))
            .collect::<Vec<_>>(),
    )));

    {
        let weak = dialog.as_weak();
        dialog.on_pick_file(move || {
            if let Some(path) = rfd::FileDialog::new().set_title("Source file").pick_file()
                && let Some(d) = weak.upgrade()
            {
                d.set_source(path.to_string_lossy().as_ref().into());
            }
        });
    }
    {
        let weak = dialog.as_weak();
        dialog.on_pick_folder(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Source folder")
                .pick_folder()
                && let Some(d) = weak.upgrade()
            {
                d.set_source(path.to_string_lossy().as_ref().into());
            }
        });
    }
    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_accepted(move || {
            let Some(d) = weak.upgrade() else { return };

            let source = std::path::PathBuf::from(d.get_source().to_string());
            if !source.exists() {
                d.set_status(format!("{} does not exist", source.display()).into());
                return;
            }

            // Where to write it. Asking now rather than after hashing: a
            // multi-minute run that then pops a Save dialog is a good way to
            // lose the result to an idle timeout.
            let default_name = source
                .file_name()
                .map(|n| format!("{}.torrent", n.to_string_lossy()))
                .unwrap_or_else(|| String::from("torrent.torrent"));
            let Some(output) = rfd::FileDialog::new()
                .set_title("Save torrent as")
                .set_file_name(&default_name)
                .add_filter("Torrent files", &["torrent"])
                .save_file()
            else {
                return; // cancelled at the save prompt
            };

            let params = crate::bittorrent::session::CreateTorrentParams {
                source,
                trackers: d
                    .get_trackers()
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect(),
                comment: d.get_comment().to_string(),
                private: d.get_private(),
                piece_length: PIECE_LENGTHS
                    .get(d.get_piece_length_index().max(0) as usize)
                    .and_then(|(_, v)| *v),
                version: crate::bittorrent::torrent_create::TorrentVersion::from_index(
                    d.get_version_index().max(0) as usize,
                ),
                output,
                add_to_session: d.get_add_to_session(),
            };

            d.set_busy(true);
            d.set_status("hashing...".into());
            // Runs on the session runtime and reports back through the slot,
            // which the refresh tick drains - hashing a large folder takes far
            // too long to block the UI thread on.
            u.session.create_torrent(params, u.create_slot.clone());
        });
    }
    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_cancelled(move || {
            if let Some(d) = weak.upgrade() {
                dismiss(&u, &d);
            }
        });
    }

    wire_dialog_close(&dialog, ui);
    let _ = dialog.show();
    *ui.create_dialog.borrow_mut() = Some(dialog);
}

/// Drain a finished create-torrent run. Called from the refresh tick.
fn poll_create_torrent(ui: &Rc<Ui>) {
    let outcome = ui.create_slot.lock().ok().and_then(|mut slot| slot.take());
    let Some(outcome) = outcome else { return };

    match outcome {
        crate::bittorrent::session::CreateTorrentOutcome::Created {
            name,
            bytes,
            save_path,
            add_to_session,
        } => {
            if add_to_session {
                // save_path is the folder the source data already lives in, so
                // the new torrent seeds in place instead of re-downloading it.
                ui.session.add_torrent(
                    AddTorrentSource::TorrentFileBytes(bytes),
                    AddParams {
                        save_path,
                        start_torrent: true,
                        only_files: None,
                        label_id: None,
                    },
                );
            }
            tracing::info!("created torrent: {name}");
            if let Some(d) = ui.create_dialog.borrow().as_ref() {
                d.set_busy(false);
                d.set_status(SharedString::new());
                dismiss(ui, d);
            }
        }
        crate::bittorrent::session::CreateTorrentOutcome::Failed(err) => {
            tracing::error!("create torrent failed: {err}");
            // Left open with the error showing, so the settings that produced
            // it are still there to correct.
            if let Some(d) = ui.create_dialog.borrow().as_ref() {
                d.set_busy(false);
                d.set_status(err.as_str().into());
            }
        }
    }
}

/// Resolve a Filters/Labels menu index to an entry.
///
/// Both menus lead with "None", so index 0 clears the filter and index n+1 is
/// entry n. Out-of-range indices clear too rather than panicking: the menu is
/// rebuilt from the database, which Preferences can change underneath it.
fn menu_pick<T>(index: i32, entries: &[T]) -> Option<&T> {
    usize::try_from(index)
        .ok()?
        .checked_sub(1)
        .and_then(|i| entries.get(i))
}

/// Build the model behind a Filters/Labels menu: "None" first, then the
/// entries, with the active one ticked.
///
/// Slint's MenuItem has no checked state, so the tick is part of the title -
/// which is why this runs again after every pick.
fn menu_model(
    names: impl Iterator<Item = String>,
    active: i32,
    none: &str,
) -> ModelRc<SharedString> {
    let titles = std::iter::once(none.to_string())
        .chain(names)
        .enumerate()
        .map(|(i, name)| {
            SharedString::from(if i as i32 == active {
                format!("\u{2713} {name}")
            } else {
                format!("   {name}")
            })
        })
        .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(titles))
}

/// Populate the Filters and Labels menus from the database and wire the PQL
/// console.
///
/// Both menus lead with "None", so index 0 clears and index n+1 is entry n -
/// the same shape the Win32 menus use.
fn wire_filters(window: &MainWindow, ui: &Rc<Ui>, model: &Rc<VecModel<Row>>) {
    let none = ui.tr.borrow().i18n("none");
    window.set_filter_names(menu_model(
        ui.cfg.get_filters().into_iter().map(|f| f.name),
        0,
        &none,
    ));
    window.set_label_names(menu_model(
        ui.cfg.get_labels().into_iter().map(|l| l.name),
        0,
        &none,
    ));

    // The three View toggles are restored here and written back through one
    // callback, so a restart comes up the way it was left.
    window.set_show_console(ui.cfg.get_bool("ui.show_console_input"));
    window.set_show_details(ui.cfg.get_bool("ui.show_details_panel"));
    window.set_show_status(ui.cfg.get_bool("ui.show_status_bar"));
    {
        let u = ui.clone();
        window.on_view_toggled(move |key, shown| u.cfg.set(key.as_str(), &shown));
    }

    window.set_theme_id(
        ui.cfg
            .get_string("theme_id")
            .unwrap_or_else(|| String::from("system"))
            .into(),
    );
    {
        let (w, u) = (window.as_weak(), ui.clone());
        window.on_pick_theme(move |theme_id| {
            u.cfg.set("theme_id", &theme_id.to_string());
            if let Some(window) = w.upgrade() {
                window.set_theme_id(theme_id);
            }
        });
    }

    {
        let (w, u, m) = (window.as_weak(), ui.clone(), model.clone());
        window.on_pick_filter(move |index| {
            // Re-read rather than capturing the list: Preferences can change
            // the filters while the window is open.
            let filters = u.cfg.get_filters();
            *u.active_filter.borrow_mut() = menu_pick(index, &filters).and_then(|f| {
                match crate::ui::filters::TorrentFilter::parse(&f.filter) {
                    Ok(parsed) => Some(parsed),
                    // A saved filter that no longer parses should not
                    // silently hide every torrent.
                    Err(err) => {
                        tracing::error!("filter '{}' does not parse: {err}", f.name);
                        None
                    }
                }
            });
            // Refresh now rather than waiting for the tick: a filter that only
            // takes effect a second after the click reads as a dead menu.
            if let Some(window) = w.upgrade() {
                window.set_filter_names(menu_model(
                    filters.into_iter().map(|f| f.name),
                    index,
                    &u.tr.borrow().i18n("none"),
                ));
                refresh(&window, &u, &m);
            }
        });
    }

    // Assigning a label from the context menu. Index 0 is "None", which
    // clears it - the same shape the filter and label menus use.
    {
        let (u, m3, w3) = (ui.clone(), model.clone(), window.as_weak());
        window.on_assign_label(move |index| {
            let labels = u.cfg.get_labels();
            let label_id = menu_pick(index, &labels).map(|l| l.id);
            for hash in u.targets() {
                u.session.set_label(&hash, label_id);
            }
            if let Some(window) = w3.upgrade() {
                refresh(&window, &u, &m3);
            }
        });
    }

    {
        let (w, u, m) = (window.as_weak(), ui.clone(), model.clone());
        window.on_pick_label(move |index| {
            let labels = u.cfg.get_labels();
            *u.active_label.borrow_mut() = menu_pick(index, &labels).map(|l| l.id);
            if let Some(window) = w.upgrade() {
                window.set_label_names(menu_model(
                    labels.into_iter().map(|l| l.name),
                    index,
                    &u.tr.borrow().i18n("none"),
                ));
                refresh(&window, &u, &m);
            }
        });
    }

    {
        let (w, u, m) = (window.as_weak(), ui.clone(), model.clone());
        window.on_console_changed(move |text| {
            let text = text.trim().to_string();
            let (filter, valid) = if text.is_empty() {
                (None, true)
            } else {
                match crate::ui::filters::TorrentFilter::parse(&text) {
                    Ok(parsed) => (Some(parsed), true),
                    // Half-typed expressions are the normal state while
                    // someone types, so the previous filter stays in force and
                    // only the prompt colour says it is not valid yet.
                    // Keep the last good filter in force; only the prompt colour changes.
                    Err(_) => ((*u.console_filter.borrow()).clone(), false),
                }
            };
            *u.console_filter.borrow_mut() = filter;
            if let Some(window) = w.upgrade() {
                window.set_console_valid(valid);
                refresh(&window, &u, &m);
            }
        });
    }
}

#[cfg(test)]
mod filter_tests {
    use super::menu_pick;

    /// The leading "None" entry makes every index off by one, which is exactly
    /// the kind of thing that silently picks the wrong filter.
    #[test]
    fn menu_index_skips_the_none_entry() {
        let entries = ["Downloading", "Uploading", "Big"];
        assert_eq!(
            menu_pick(0, &entries),
            None,
            "0 is None, not the first entry"
        );
        assert_eq!(menu_pick(1, &entries), Some(&"Downloading"));
        assert_eq!(menu_pick(3, &entries), Some(&"Big"));
        // Past the end and negative both clear rather than panic - the menu is
        // rebuilt from the database and can shrink between build and click.
        assert_eq!(menu_pick(4, &entries), None);
        assert_eq!(menu_pick(-1, &entries), None);
        assert_eq!(menu_pick(1, &[] as &[&str]), None);
    }
}

/// The "ask" branch of the close preference: exit, or minimise to the tray?
///
/// Non-modal, like every other dialog here: the choice arrives through the
/// callbacks rather than a blocking return, so the window stays alive until
/// one of them fires.
fn ask_on_close(window: &MainWindow, ui: &Rc<Ui>) {
    if let Some(existing) = ui.close_prompt.borrow().as_ref() {
        let _ = existing.show();
        return;
    }

    let dialog = match ClosePromptDialog::new() {
        Ok(d) => d,
        Err(err) => {
            // Without a prompt the safe answer is to keep running: the tray
            // icon is there, and nothing is lost by not exiting.
            tracing::error!("cannot create the close prompt: {err}");
            return;
        }
    };

    {
        let (weak, u, w) = (dialog.as_weak(), ui.clone(), window.as_weak());
        dialog.on_chosen(move |exit| {
            let Some(d) = weak.upgrade() else { return };
            if d.get_remember() {
                u.cfg
                    .set_persistent("ui.close_action", if exit { "exit" } else { "minimize" });
            }
            dismiss(&u, &d);
            if exit {
                let _ = slint::quit_event_loop();
            } else if let Some(window) = w.upgrade() {
                let _ = window.hide();
            }
        });
    }
    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_cancelled(move || {
            if let Some(d) = weak.upgrade() {
                dismiss(&u, &d);
            }
        });
    }

    wire_dialog_close(&dialog, ui);
    let _ = dialog.show();
    *ui.close_prompt.borrow_mut() = Some(dialog);
}

/// Hide a minimised window into the tray, when both settings ask for it.
///
/// Slint raises no minimise event, so the refresh tick polls for it instead.
/// The delay is under a second and the window is already off the screen by
/// then, so nothing visible happens late.
fn poll_minimize_to_tray(window: &MainWindow, ui: &Rc<Ui>) {
    let handle = window.window();
    if handle.is_minimized()
        && handle.is_visible()
        && ui.cfg.get_bool("minimize_to_notification_area")
        && ui.cfg.get_bool("show_in_notification_area")
    {
        // Un-minimize first: a window hidden while minimised comes back
        // minimised, so restoring from the tray would appear to do nothing.
        handle.set_minimized(false);
        let _ = window.hide();
    }
}

#[cfg(test)]
mod file_tree_tests {
    use super::file_tree;

    /// Folder rows carry no index; files carry their real index into the
    /// torrent's file list. That mapping drives the include checkboxes, so
    /// getting it wrong toggles the wrong file.
    #[test]
    fn nested_paths_become_indented_rows() {
        let paths = [
            "Season 1/ep01.mkv",
            "Season 1/subs/en.srt",
            "Season 2/ep01.mkv",
            "readme.txt",
        ];
        assert_eq!(
            file_tree(paths.iter().copied()),
            vec![
                // Root-level files first: the empty path sorts before any
                // named folder.
                (Some(3), 0, "readme.txt"),
                (None, 0, "Season 1"),
                (Some(0), 1, "ep01.mkv"),
                (None, 1, "subs"),
                (Some(1), 2, "en.srt"),
                (None, 0, "Season 2"),
                (Some(2), 1, "ep01.mkv"),
            ]
        );
    }

    /// A shared parent is emitted once, not repeated for every child folder.
    #[test]
    fn a_shared_parent_folder_is_not_repeated() {
        let paths = ["a/b/one.bin", "a/c/two.bin"];
        let folders: Vec<&str> = file_tree(paths.iter().copied())
            .into_iter()
            .filter(|(index, _, _)| index.is_none())
            .map(|(_, _, name)| name)
            .collect();
        assert_eq!(folders, vec!["a", "b", "c"]);
    }

    /// Windows-style separators appear in torrents made on Windows.
    #[test]
    fn backslash_separators_split_too() {
        assert_eq!(
            file_tree(["dir\\file.bin"].iter().copied()),
            vec![(None, 0, "dir"), (Some(0), 1, "file.bin")]
        );
    }

    /// A flat single-file torrent gets no folder rows at all.
    #[test]
    fn a_flat_torrent_has_no_folders() {
        assert_eq!(
            file_tree(["only.bin"].iter().copied()),
            vec![(Some(0), 0, "only.bin")]
        );
    }
}

#[cfg(test)]
mod ui_string_tests {
    use super::ui_string;
    use crate::ui::translator::Translator;

    fn tr() -> Translator {
        Translator::load(std::path::Path::new("does-not-exist"), "en-US")
    }

    /// Win32 puts `&` before the accelerator letter and `&&` where it wants a
    /// real ampersand. The drawn menus have no accelerators, so the first must
    /// vanish and the second must survive - getting this backwards showed a
    /// menu bar reading "&File &View &Help".
    #[test]
    fn ampersands_follow_the_win32_rules() {
        // "&Exit" -> "Exit"
        assert_eq!(ui_string(&tr(), "amp_exit"), "Exit");
        // "...files && magnet links" keeps one ampersand.
        let assoc = ui_string(&tr(), "set_default_associations");
        assert!(
            assoc.contains(" & "),
            "expected a literal ampersand: {assoc}"
        );
        assert!(!assoc.contains("&&"), "doubled ampersand survived: {assoc}");
    }

    /// The JSON is authored for Win32 controls and uses CRLF; Slint draws the
    /// stray CR as a box.
    #[test]
    fn crlf_becomes_lf() {
        let body = ui_string(&tr(), "close_prompt_body");
        assert!(body.contains('\n'));
        assert!(!body.contains('\r'), "CR survived: {body:?}");
    }
}

#[cfg(test)]
mod column_tests {
    use super::{caption_width, columns};
    use crate::ui::translator::Translator;

    fn tr(locale: &str) -> Translator {
        Translator::load(std::path::Path::new("does-not-exist"), locale)
    }

    /// The total must be the sum of the widths. It used to be a hand-written
    /// constant and had drifted 100px short, which left the Label column
    /// outside the painted row background - it showed as a black gap.
    #[test]
    fn total_is_the_sum_of_the_widths() {
        for locale in ["en-US", "nl-NL", "ja-JP", "ru-RU"] {
            let (widths, titles, total) = columns(&tr(locale));
            assert_eq!(widths.len(), 16, "{locale}");
            assert_eq!(titles.len(), 16, "{locale}");
            let sum: f32 = widths.iter().sum();
            assert!((total - sum).abs() < 0.01, "{locale}: {total} != {sum}");
        }
    }

    /// A column never shrinks below its designed width, however short the
    /// translated caption is.
    #[test]
    fn columns_never_go_below_their_designed_width() {
        let (widths, _, _) = columns(&tr("en-US"));
        assert!(widths[0] >= 260.0, "Name column: {}", widths[0]);
        assert!(widths[1] >= 44.0, "# column: {}", widths[1]);
    }

    /// A caption longer than its designed width widens the column, which is
    /// the whole point: switching language must not clip a header.
    #[test]
    fn a_long_caption_widens_its_column() {
        // "Availability" is 90px by design and needs more than that at 13px.
        let (widths, titles, _) = columns(&tr("en-US"));
        let i = titles.iter().position(|t| t == "Availability").unwrap();
        assert!(
            widths[i] > 90.0,
            "expected {} to widen past its 90px design, got {}",
            titles[i],
            widths[i]
        );
    }

    /// Full-width scripts advance about twice as far as Latin, so a CJK
    /// caption must not be measured as if it were the same length in Latin.
    #[test]
    fn wide_scripts_measure_wider() {
        assert!(
            caption_width("日本語") > caption_width("abc"),
            "CJK should measure wider than Latin of the same char count"
        );
        assert_eq!(caption_width(""), 0.0);
    }
}

#[cfg(test)]
mod web_settings_tests {
    use super::TLS_MODES;

    /// The stored values must match what webui::spawn parses, or a setting
    /// saved here reads back as something else entirely.
    #[test]
    fn tls_modes_match_the_server() {
        assert_eq!(TLS_MODES, ["self-signed", "custom", "off"]);
    }

    /// A password typed into the dialog must never reach the database. Only
    /// its Argon2 hash does, which is what the server compares against.
    #[test]
    fn passwords_are_stored_hashed() {
        let hash = crate::webui::Credentials::hash_password("hunter2").expect("hashes");
        assert!(hash.starts_with("$argon2"), "not an argon2 hash: {hash}");
        assert!(!hash.contains("hunter2"), "the password survived into the hash");

        // Salted, so the same password hashes differently every time - a
        // stored hash can never be matched back to the typed password.
        let again = crate::webui::Credentials::hash_password("hunter2").expect("hashes");
        assert_ne!(hash, again, "hashes are not salted");
    }
}
