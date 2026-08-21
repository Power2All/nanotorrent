//! Feeds the Slint list spike with a realistic number of rows.
//!
//! 5,000 torrents, because that is where the question lies. The Win32 list is
//! not virtualised - mainwindow.rs rebuilds all 16 cell strings for every
//! torrent every second and holds a real LVITEM per row - so "does the
//! replacement hold up at scale" has to be asked at scale.

use std::cell::RefCell;
use std::rc::Rc;

use slint::{Model, ModelRc, SharedString, VecModel};

slint::include_modules!();

const ROWS: usize = 5_000;

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let (mut v, mut u) = (bytes as f64, 0);
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.2} {}", UNITS[u])
}

fn make_row(i: usize) -> Row {
    let size = 1024u64 * 1024 * (50 + (i as u64 * 37) % 4000);
    let progress = ((i as f32 * 7.3) % 100.0) / 100.0;
    let paused = i % 5 == 2;
    let dash = || SharedString::from("-");

    Row {
        name: format!("Some.Torrent.Name.{i:05}.2026.1080p.WEB-DL.x265-GROUP").into(),
        queue: (i + 1).to_string().into(),
        size: human(size).into(),
        remaining: human((size as f32 * (1.0 - progress)) as u64).into(),
        status: match i % 5 {
            0 => "Downloading",
            1 => "Uploading",
            2 => "Paused",
            3 => "Checking files",
            _ => "Downloading metadata",
        }
        .into(),
        progress,
        eta: if paused { dash() } else { format!("{}m", 3 + i % 90).into() },
        dl: if paused { dash() } else { format!("{}/s", human((i as u64 * 971) % 4_000_000)).into() },
        ul: if paused { dash() } else { format!("{}/s", human((i as u64 * 313) % 900_000)).into() },
        availability: if paused { dash() } else { format!("{:.2}", (i % 700) as f32 / 100.0).into() },
        ratio: format!("{:.2}", (i % 400) as f32 / 100.0).into(),
        seeds: format!("{} ({})", i % 40, 40 + i % 200).into(),
        peers: format!("{} ({})", i % 25, 25 + i % 150).into(),
        added: "2026-08-21 14:05".into(),
        completed: if i % 3 == 0 { dash() } else { "2026-08-21 15:41".into() },
        label: match i % 4 {
            0 => "Movies",
            1 => "TV",
            2 => "Linux",
            _ => "",
        }
        .into(),
        selected: false,
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let window = MainWindow::new()?;

    let t0 = std::time::Instant::now();
    let rows: Vec<Row> = (0..ROWS).map(make_row).collect();
    let build_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let model = Rc::new(VecModel::from(rows));
    window.set_rows(ModelRc::from(model.clone()));

    println!("built {ROWS} rows in {build_ms:.0} ms");

    let anchor = Rc::new(RefCell::new(0usize));

    // --- selection ----------------------------------------------------------
    // StandardTableView has no multi-select, so it lives here: the model owns a
    // `selected` flag per row and the UI just renders it. That is more code
    // than a built-in would be, but it is ordinary code rather than a fight.
    {
        let model = model.clone();
        let anchor = anchor.clone();
        let handle = window.as_weak();
        window.on_row_pressed(move |index, ctrl, shift| {
            // Printed so a failed multi-select can be told apart: modifiers
            // Slint never delivered, versus modifiers the test harness never
            // managed to hold down.
            println!("row_pressed index={index} ctrl={ctrl} shift={shift}");
            let index = index as usize;
            let t0 = std::time::Instant::now();

            if shift {
                let (lo, hi) = {
                    let a = *anchor.borrow();
                    (a.min(index), a.max(index))
                };
                for i in 0..model.row_count() {
                    if let Some(mut r) = model.row_data(i) {
                        r.selected = (lo..=hi).contains(&i);
                        model.set_row_data(i, r);
                    }
                }
            } else if ctrl {
                if let Some(mut r) = model.row_data(index) {
                    r.selected = !r.selected;
                    model.set_row_data(index, r);
                }
                *anchor.borrow_mut() = index;
            } else {
                for i in 0..model.row_count() {
                    if let Some(mut r) = model.row_data(i) {
                        let want = i == index;
                        if r.selected != want {
                            r.selected = want;
                            model.set_row_data(i, r);
                        }
                    }
                }
                *anchor.borrow_mut() = index;
            }

            let count = (0..model.row_count())
                .filter(|&i| model.row_data(i).is_some_and(|r| r.selected))
                .count();
            if let Some(w) = handle.upgrade() {
                w.set_selected_count(count as i32);

                // Details follow the row just pressed. With several selected
                // the Win32 version shows the first; same idea here.
                if let Some(r) = model.row_data(index) {
                    w.set_d_name(r.name.clone());
                    w.set_d_hash(format!("{:040x}", index * 0x9E3779B9usize).into());
                    w.set_d_save_path(r"C:\Users\you\Downloads".into());
                    w.set_d_status(r.status.clone());
                    w.set_d_downloaded(r.size.clone());
                    w.set_d_uploaded(r.ul.clone());
                    w.set_d_ratio(r.ratio.clone());
                    w.set_d_peers(r.peers.clone());
                    w.set_d_added(r.added.clone());
                    w.set_d_completed(r.completed.clone());
                    w.set_d_progress(r.progress);
                }
                w.set_status(
                    format!("selection updated in {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0)
                        .into(),
                );
            }
        });
    }

    // --- sorting ------------------------------------------------------------
    {
        let model = model.clone();
        let handle = window.as_weak();
        let descending = Rc::new(RefCell::new(false));
        window.on_sort(move |column| {
            let t0 = std::time::Instant::now();
            let desc = { let mut d = descending.borrow_mut(); *d = !*d; *d };

            let mut rows: Vec<Row> = model.iter().collect();
            let key = |r: &Row| -> SharedString {
                match column {
                    0 => r.name.clone(),
                    1 => r.queue.clone(),
                    2 => r.size.clone(),
                    4 => r.status.clone(),
                    15 => r.label.clone(),
                    _ => r.name.clone(),
                }
            };
            // Numeric columns want numeric comparators; this spike only proves
            // the mechanism, so string order is enough here.
            rows.sort_by(|a, b| {
                let (x, y) = (key(a), key(b));
                if desc { y.cmp(&x) } else { x.cmp(&y) }
            });
            model.set_vec(rows);

            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            println!("sorted {ROWS} rows by column {column} in {ms:.0} ms");
            if let Some(w) = handle.upgrade() {
                w.set_status(format!("sorted by column {column} in {ms:.0} ms").into());
            }
        });
    }

    window.on_context_menu(|index| println!("context menu for row {index}"));

    // Self-test: prove the splitter's height binding actually reaches the
    // layout, without anyone having to drag anything. Asks for a height the
    // panel did not start with, then reports what the layout gave it.
    //
    // Exists because three manual drag attempts failed and it was not clear
    // whether the drag was wrong or the binding was ignored - a question a
    // click cannot answer but a measurement can.
    let selftest = std::env::var_os("NT_SPIKE_SELFTEST").is_some();
    let timer = slint::Timer::default();
    if selftest {
        let handle = window.as_weak();
        let before = std::rc::Rc::new(std::cell::Cell::new(0.0f32));
        // Two ticks: measure, change the height, measure again. One reading
        // proves nothing - the layout might coincidentally agree.
        let phase = std::rc::Rc::new(std::cell::Cell::new(0u8));
        let before2 = before.clone();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(700),
            move || {
                if let Some(w) = handle.upgrade() {
                    let asked = w.get_details_height();
                    let list = w.get_measured_list_height();
                    println!("SELFTEST asked details-height={asked}  list height={list}");
                    // Baseline is printed on the first tick before the change,
                    // so compare the two runs rather than guessing a constant.
                }
                if phase.get() == 0 {
                    if let Some(w) = handle.upgrade() {
                        before2.set(w.get_measured_list_height());
                        w.set_details_height(420.0);
                    }
                    phase.set(1);
                    return;
                }
                if let Some(w) = handle.upgrade() {
                    let after = w.get_measured_list_height();
                    let shrank = before2.get() - after;
                    println!(
                        "SELFTEST list {} -> {} (shrank {shrank}) after details-height 260 -> 420",
                        before2.get(), after
                    );
                    println!(
                        "SELFTEST binding {}",
                        if (shrank - 160.0).abs() < 2.0 { "REACHES the layout" } else { "IGNORED by the layout" }
                    );
                }
                let _ = slint::quit_event_loop();
            },
        );
    }
    window.set_status(format!("{ROWS} rows built in {build_ms:.0} ms").into());

    window.run()
}
