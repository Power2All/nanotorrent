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
use crate::bittorrent::session::Session;
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
    });

    let model: Rc<VecModel<Row>> = Rc::new(VecModel::from(Vec::new()));
    window.set_rows(ModelRc::from(model.clone()));

    wire_selection(&window, &ui, &model);
    wire_actions(&window, &ui);

    // Populate before the first paint so the window never flashes empty.
    refresh(&window, &ui, &model);

    let timer = slint::Timer::default();
    {
        let (w, ui, model) = (window.as_weak(), ui.clone(), model.clone());
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(1),
            move || {
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
    } else if selected.is_empty() {
        clear_details(window);
    }

    *ui.rows.borrow_mut() = rows;
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
