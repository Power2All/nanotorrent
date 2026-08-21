//! The cross-platform UI.
//!
//! Runs against the same [`Session`] the Win32 UI does, so this is a second
//! front end rather than a fork: `src/ui_native` stays the default on Windows
//! until this one reaches parity, and neither knows about the other.
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

/// Everything the callbacks need, kept in one `Rc` so each closure clones a
/// single handle rather than five.
struct Ui {
    session: Arc<Session>,
    cfg: Arc<crate::core::configuration::Configuration>,
    tr: Translator,
    /// Info hashes of the selected rows, in the order the list shows them.
    /// The Win32 list keeps this in the control; here the model owns it.
    selected: RefCell<HashSet<String>>,
    /// Anchor for shift-extend, as an index into the last rendered order.
    anchor: RefCell<usize>,
    /// The rows behind what is on screen, so a callback can map an index to a
    /// torrent without asking the session again.
    rows: RefCell<Vec<TorrentStatus>>,
    /// Arguments forwarded by a second instance, polled on the refresh tick.
    ipc: Option<crate::ipc::Server>,
    /// Peer country lookups, loaded in the background at startup - the same
    /// database the Win32 peers list uses.
    geoip: Arc<crate::core::geoip::GeoIp>,
    /// Kept alive while open. Hidden rather than dropped when dismissed -
    /// dropping a window from inside its own callback is asking for trouble,
    /// and the next open replaces it anyway.
    magnet_dialog: RefCell<Option<AddMagnetDialog>>,
    prefs_dialog: RefCell<Option<PreferencesDialog>>,
    env: Arc<crate::core::environment::Environment>,
    torrent_dialog: RefCell<Option<AddTorrentDialog>>,
    /// The .torrent currently in the Add dialog, and any queued behind it.
    /// argv can name several, and ui_native shows one dialog at a time too.
    pending: RefCell<Vec<Vec<u8>>>,
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
        eta: if paused { dash() } else { format::eta_text(status).into() },
        dl: if paused { dash() } else { format::speed_text(status.download_payload_rate).into() },
        ul: if paused { dash() } else { format::speed_text(status.upload_payload_rate).into() },
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

pub fn run(ctx: AppContext) -> anyhow::Result<()> {
    let window = MainWindow::new()
        .map_err(|e| anyhow::anyhow!("failed to create the Slint window: {e}"))?;

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
        tr: ctx.translator.clone(),
        selected: RefCell::new(HashSet::new()),
        anchor: RefCell::new(0),
        rows: RefCell::new(Vec::new()),
        ipc: ctx.ipc,
        geoip: ctx.geoip.clone(),
        magnet_dialog: RefCell::new(None),
        prefs_dialog: RefCell::new(None),
        env: ctx.env.clone(),
        torrent_dialog: RefCell::new(None),
        pending: RefCell::new(Vec::new()),
    });

    let model: Rc<VecModel<Row>> = Rc::new(VecModel::from(Vec::new()));
    window.set_rows(ModelRc::from(model.clone()));

    wire_selection(&window, &ui, &model);
    wire_actions(&window, &ui);

    // Torrents named on the command line, before the first paint so they are
    // already in the list when it appears.
    handle_params(&ui, &ctx.args);

    // Populate before the first paint so the window never flashes empty.
    refresh(&window, &ui, &model);

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
                if let Some(window) = w.upgrade() {
                    refresh(&window, &ui, &model);
                }
            },
        );
    }

    window
        .run()
        .map_err(|e| anyhow::anyhow!("Slint event loop failed: {e}"))
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
/// metadata resolves. Files go through the Add dialog, as ui_native does, so
/// the save path and file selection can be set before anything is written.
fn handle_params(ui: &Rc<Ui>, args: &[String]) {
    for arg in args {
        if arg.starts_with("magnet:") {
            ui.session
                .add_torrent(AddTorrentSource::MagnetUri(arg.clone()), default_add_params());
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
    let rows = ui.session.torrents(&ui.labels());

    // Drop selections for torrents that are gone, or the count in the status
    // bar drifts upwards every time one is removed.
    {
        let live: HashSet<&str> = rows.iter().map(|r| r.info_hash.as_str()).collect();
        ui.selected.borrow_mut().retain(|h| live.contains(h.as_str()));
    }

    let selected = ui.selected.borrow().clone();
    let mapped: Vec<Row> = rows
        .iter()
        .map(|r| to_row(r, &ui.tr, selected.contains(&r.info_hash)))
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
        set_details(window, first, &ui.tr);
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
/// Trackers.
fn refresh_detail_tab(window: &MainWindow, ui: &Rc<Ui>, hash: &str) {
    match window.get_current_tab() {
        1 => {
            let rows: Vec<FileEntryRow> = ui
                .session
                .files(hash)
                .into_iter()
                .map(|f| FileEntryRow {
                    name: f.name.as_str().into(),
                    size: utils::to_human_file_size(f.length as i64).into(),
                    progress: f.progress,
                    included: f.included,
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
                    let country = ui
                        .geoip
                        .lookup(&p.addr)
                        .map(|(_iso, name)| name)
                        .unwrap_or_default();
                    PeerEntryRow {
                        addr: p.addr.as_str().into(),
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
                .tracker_rows(hash, &ui.tr)
                .into_iter()
                .map(|t| TrackerEntryRow {
                    url: t.label.as_str().into(),
                    status: t.status.as_str().into(),
                    seeds: t.seeders.map_or_else(|| String::from("-"), |v| v.to_string()).into(),
                    leeches: t.leechers.map_or_else(|| String::from("-"), |v| v.to_string()).into(),
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
    window.on_row_pressed(move |index, ctrl, shift| {
        let index = index.max(0) as usize;
        let hashes: Vec<String> = u.rows.borrow().iter().map(|r| r.info_hash.clone()).collect();
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

    if let Some(first) = ui.rows.borrow().iter().find(|r| selected.contains(&r.info_hash)) {
        set_details(window, first, &ui.tr);
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
    let u = ui.clone();
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
                // ponytail: no clipboard yet - Slint has no clipboard API and
                // the Win32 path goes through nwg. Logged so the action is not
                // silently dead while the dialogs are still being ported.
                tracing::info!("copy info hash: {}", targets.join(", "));
            }
            "copy-magnet" => {
                let rows = u.rows.borrow();
                let magnets: Vec<String> = rows
                    .iter()
                    .filter(|r| targets.contains(&r.info_hash))
                    .map(|r| u.session.magnet_uri(&r.info_hash, &r.name))
                    .collect();
                tracing::info!("magnet link(s): {}", magnets.join("\n"));
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
            "exit" => {
                let _ = slint::quit_event_loop();
            }
            // The dialogs are still Win32-only; the port lands them next.
            other => tracing::info!("action '{other}' is not implemented in the Slint UI yet"),
        }
    });

    // Sorting is the model's job - `ListView` has no opinion about order.
    // ponytail: string-order on the rendered cell, not typed comparators, so
    // "99 MB" sorts above "500 MB". Fine while the list is being brought up;
    // needs per-column keys before this UI replaces ui_native.
    window.on_sort(|column| {
        tracing::info!("sort by column {column} is not implemented yet");
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
            let _ = d.hide();
        });
    }

    {
        let weak = dialog.as_weak();
        dialog.on_cancelled(move || {
            if let Some(d) = weak.upgrade() {
                let _ = d.hide();
            }
        });
    }

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
fn show_next_pending(ui: &Rc<Ui>) {
    if ui.torrent_dialog.borrow().is_some() {
        return; // one at a time, like ui_native's add_torrent_queue
    }
    let Some(bytes) = ui.pending.borrow_mut().pop() else {
        return;
    };

    let parsed = match crate::ui::torrentfile::parse(&bytes) {
        Ok(p) => p,
        Err(err) => {
            // Skip it and carry on - one bad file should not strand the queue.
            tracing::error!("{err}");
            show_next_pending(ui);
            return;
        }
    };

    let dialog = match AddTorrentDialog::new() {
        Ok(d) => d,
        Err(err) => {
            tracing::error!("cannot create the add-torrent dialog: {err}");
            return;
        }
    };

    dialog.set_torrent_name(parsed.name.as_str().into());
    dialog.set_torrent_size(utils::to_human_file_size(parsed.total_size).into());
    dialog.set_save_path(
        ui.cfg
            .get_string("default_save_path")
            .unwrap_or_default()
            .into(),
    );

    let files: Rc<VecModel<FileRow>> = Rc::new(VecModel::from(
        parsed
            .files
            .iter()
            .map(|(name, size)| FileRow {
                name: name.as_str().into(),
                size: utils::to_human_file_size(*size as i64).into(),
                included: true,
            })
            .collect::<Vec<_>>(),
    ));
    dialog.set_files(ModelRc::from(files.clone()));

    {
        let f = files.clone();
        dialog.on_toggle_file(move |index| {
            let index = index.max(0) as usize;
            if let Some(mut row) = f.row_data(index) {
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
        let (weak, u, f) = (dialog.as_weak(), ui.clone(), files.clone());
        let bytes = bytes.clone();
        dialog.on_accepted(move || {
            let Some(d) = weak.upgrade() else { return };

            let included: Vec<usize> = (0..f.row_count())
                .filter(|&i| f.row_data(i).is_some_and(|r| r.included))
                .collect();
            // None means "everything", which is not the same as an explicit
            // list of all of them - keep the distinction the session expects.
            let only_files = (included.len() != f.row_count()).then_some(included);

            let save_path = d.get_save_path().to_string();
            u.session.add_torrent(
                AddTorrentSource::TorrentFileBytes(bytes.clone()),
                AddParams {
                    save_path: (!save_path.trim().is_empty()).then_some(save_path),
                    start_torrent: d.get_start_torrent(),
                    only_files,
                    label_id: None,
                },
            );

            let _ = d.hide();
            *u.torrent_dialog.borrow_mut() = None;
            show_next_pending(&u);
        });
    }

    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_cancelled(move || {
            if let Some(d) = weak.upgrade() {
                let _ = d.hide();
            }
            *u.torrent_dialog.borrow_mut() = None;
            show_next_pending(&u);
        });
    }

    let _ = dialog.show();
    *ui.torrent_dialog.borrow_mut() = Some(dialog);
}

/// Themes and close actions, in the order the combo boxes show them. Kept
/// beside the load/save pair so an index cannot mean one thing going in and
/// another coming out.
const THEMES: [&str; 3] = ["system", "light", "dark"];
const CLOSE_ACTIONS: [&str; 3] = ["ask", "minimize", "exit"];

/// Port of ui_native/dialogs.rs::spawn_preferences.
fn open_preferences(ui: &Rc<Ui>) {
    if let Some(existing) = ui.prefs_dialog.borrow().as_ref() {
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

    {
        let (weak, u) = (dialog.as_weak(), ui.clone());
        dialog.on_accepted(move || {
            let Some(d) = weak.upgrade() else { return };
            save_preferences(&d, &u);
            // Rebuild the librqbit session with the new settings, exactly as
            // mainwindow.rs does after its dialog closes.
            u.session.apply_settings(&u.env, &u.cfg);
            let _ = d.hide();
        });
    }
    {
        let weak = dialog.as_weak();
        dialog.on_cancelled(move || {
            if let Some(d) = weak.upgrade() {
                let _ = d.hide();
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

    let _ = dialog.show();
    *ui.prefs_dialog.borrow_mut() = Some(dialog);
}

fn load_preferences(d: &PreferencesDialog, ui: &Rc<Ui>) {
    let cfg = &ui.cfg;

    // Translator::languages(), not EMBEDDED_LANGS: that is raw alphabetical
    // (build.rs sorts the filenames), whereas this is the order ui_native shows
    // - English first, the rest sorted - so the two dialogs agree.
    //
    // ponytail: locale tags, not native display names. ui_native gets those
    // from GetLocaleInfoEx, which is Win32-only; this needs a cross-platform
    // source or a table before it reads nicely.
    let langs: Vec<SharedString> = ui
        .tr
        .languages()
        .iter()
        .map(|l| SharedString::from(l.locale.as_str()))
        .collect();
    let current = cfg
        .get_string("locale_name")
        .unwrap_or_else(|| String::from(crate::DEFAULT_LOCALE));
    let index = langs.iter().position(|l| *l == current).unwrap_or(0);
    d.set_language_index(index as i32);
    // Scroll it into view, or a non-English selection sits below the fold and
    // the list looks like nothing is selected.
    d.set_language_scroll(-(index as f32) * 24.0);
    d.set_languages(ModelRc::new(VecModel::from(langs)));

    d.set_themes(ModelRc::new(VecModel::from(
        THEMES
            .iter()
            .map(|t| SharedString::from(*t))
            .collect::<Vec<_>>(),
    )));
    let theme = cfg
        .get_string("theme_id")
        .unwrap_or_else(|| String::from("system"));
    d.set_theme_index(THEMES.iter().position(|t| *t == theme).unwrap_or(0) as i32);

    d.set_close_actions(ModelRc::new(VecModel::from(
        ["Ask every time", "Minimize", "Exit"]
            .iter()
            .map(|t| SharedString::from(*t))
            .collect::<Vec<_>>(),
    )));
    // close_action is persistent - it survives restore-defaults, unlike the rest.
    let close = cfg.get_persistent("ui.close_action").unwrap_or_default();
    d.set_close_action_index(CLOSE_ACTIONS.iter().position(|a| *a == close).unwrap_or(0) as i32);

    d.set_skip_add_dialog(cfg.get_bool("skip_add_torrent_dialog"));
    d.set_show_in_tray(cfg.get_bool("show_in_notification_area"));
    d.set_minimize_to_tray(cfg.get_bool("minimize_to_notification_area"));
    d.set_can_associate(cfg!(windows));

    d.set_save_path(cfg.get_string("default_save_path").unwrap_or_default().into());
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
    d.set_ipfilter_path(cfg.get_string("ipfilter.file_path").unwrap_or_default().into());
    d.set_require_outgoing_encryption(cfg.get_bool("libtorrent.require_outgoing_encryption"));
    d.set_require_incoming_encryption(cfg.get_bool("libtorrent.require_incoming_encryption"));
    d.set_anonymous_mode(cfg.get_bool("libtorrent.anonymous_mode"));

    d.set_proxy_types(ModelRc::new(VecModel::from(
        ["None", "SOCKS4", "SOCKS5", "SOCKS5 (auth)", "HTTP", "HTTP (auth)"]
            .iter()
            .map(|t| SharedString::from(*t))
            .collect::<Vec<_>>(),
    )));
    d.set_proxy_type_index(cfg.get_int("libtorrent.proxy_type").unwrap_or(0) as i32);
    d.set_proxy_host(cfg.get_string("libtorrent.proxy_host").unwrap_or_default().into());
    d.set_proxy_port(num(cfg.get_int("libtorrent.proxy_port")));
    d.set_proxy_hostnames(cfg.get_bool("libtorrent.proxy_hostnames"));
    d.set_proxy_peers(cfg.get_bool("libtorrent.proxy_peers"));
    d.set_proxy_trackers(cfg.get_bool("libtorrent.proxy_trackers"));
}

fn save_preferences(d: &PreferencesDialog, ui: &Rc<Ui>) {
    let cfg = &ui.cfg;

    if let Some(lang) = ui.tr.languages().get(d.get_language_index().max(0) as usize) {
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

    cfg.set("default_save_path", &d.get_save_path().to_string());
    cfg.set("pause_on_low_disk_space", &d.get_pause_on_low_disk());
    set_num(cfg, "pause_on_low_disk_space_limit", &d.get_low_disk_limit());
    set_num(cfg, "libtorrent.active_limit", &d.get_active_limit());
    set_num(cfg, "libtorrent.active_downloads", &d.get_active_downloads());
    set_num(cfg, "libtorrent.active_seeds", &d.get_active_seeds());
    cfg.set(
        "libtorrent.enable_download_rate_limit",
        &d.get_limit_download(),
    );
    set_num(cfg, "libtorrent.download_rate_limit", &d.get_download_limit());
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
