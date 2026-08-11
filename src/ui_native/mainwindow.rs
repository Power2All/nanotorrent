// Native Win32 main window - port of src/picotorrent/ui/mainframe.cpp,
// torrentlistview.cpp, torrentdetailsview.cpp, statusbar.cpp and
// taskbaricon.cpp using native-windows-gui.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use native_windows_gui as nwg;

use crate::AppContext;
use crate::bittorrent::session::{AddTorrentSource, CreateTorrentOutcome, Session};
use crate::bittorrent::torrentstatus::{State, TorrentStatus};
use crate::core::configuration::{Configuration, Filter, Label};
use crate::core::utils;
use crate::ui::filters::TorrentFilter;
use crate::ui::format;
use crate::ui::translator::Translator;

use super::darkmode;
use super::dialogs::{self, DialogHandle, DialogResult};

pub(crate) const APP_ICON: &[u8] = include_bytes!("../../res/app.ico");

/// Columns - port of TorrentListModel::Columns.
const COLUMNS: [(&str, i32); 16] = [
    ("name", 220),
    ("queue_position", 40),
    ("size", 80),
    ("size_remaining", 100),
    ("status", 110),
    ("progress", 80),
    ("eta", 80),
    ("dl", 90),
    ("ul", 90),
    ("availability", 80),
    ("ratio", 60),
    ("seeds", 70),
    ("peers", 70),
    ("added_on", 110),
    ("completed_on", 110),
    ("label", 80),
];

const DETAILS_HEIGHT: i32 = 240;
const STATUS_HEIGHT: i32 = 24;
const SPLITTER_HEIGHT: i32 = 5;
const DETAILS_MIN: i32 = 100;
const LIST_MIN: i32 = 120;
const CONSOLE_HEIGHT: i32 = 34;
const CONSOLE_INPUT_HEIGHT: i32 = 20;
// x where the input starts - the terminal icon sits left of it.
const CONSOLE_INPUT_X: i32 = 36;

// Many fields exist only to keep the native controls alive for the
// window's lifetime - NWG controls are destroyed when dropped.
#[allow(dead_code)]
pub struct MainWindow {
    // context
    env: Arc<crate::core::environment::Environment>,
    session: Arc<Session>,
    cfg: Arc<Configuration>,
    tr: Translator,
    ipc: Option<crate::ipc::Server>,
    update_slot: Arc<std::sync::Mutex<Option<crate::updatechecker::UpdateInfo>>>,
    geoip: Arc<crate::core::geoip::GeoIp>,
    create_slot: Arc<std::sync::Mutex<Option<CreateTorrentOutcome>>>,
    // Magnet metadata resolutions in flight, and their results (bytes/failure).
    magnet_slot: Arc<std::sync::Mutex<Vec<crate::bittorrent::session::MagnetOutcome>>>,
    resolving_magnets: std::cell::Cell<usize>,
    // Queue limits, cached from the configuration (re-read on prefs save).
    active_limit: Cell<i64>,
    active_downloads: Cell<i64>,
    active_seeds: Cell<i64>,

    // window + resources
    pub window: nwg::Window,
    icon: nwg::Icon,
    bold_font: nwg::Font,
    dark_mode: Cell<bool>,

    // menu bar
    menu_file: nwg::Menu,
    mi_add_torrent: nwg::MenuItem,
    mi_add_magnet: nwg::MenuItem,
    mi_create_torrent: nwg::MenuItem,
    mi_import_pico: nwg::MenuItem,
    mi_exit: nwg::MenuItem,
    menu_view: nwg::Menu,
    menu_filters: nwg::Menu,
    filter_items: RefCell<Vec<(nwg::MenuItem, Option<i32>)>>,
    menu_labels: nwg::Menu,
    label_filter_items: RefCell<Vec<(nwg::MenuItem, Option<i32>)>>,
    mi_details_panel: nwg::MenuItem,
    mi_status_bar: nwg::MenuItem,
    mi_console: nwg::MenuItem,
    menu_theme: nwg::Menu,
    mi_theme_auto: nwg::MenuItem,
    mi_theme_light: nwg::MenuItem,
    mi_theme_dark: nwg::MenuItem,
    mi_preferences: nwg::MenuItem,
    menu_help: nwg::Menu,
    mi_docs: nwg::MenuItem,
    mi_about: nwg::MenuItem,

    // main controls
    console_icon: nwg::Label,
    console_input: nwg::TextInput,
    list: nwg::ListView,
    tabs: nwg::TabsContainer,
    tab_overview: nwg::Tab,
    tab_files: nwg::Tab,
    tab_peers: nwg::Tab,
    tab_trackers: nwg::Tab,
    overview_fields: Vec<(nwg::Label, nwg::Label)>,
    piece_caption: nwg::Label,
    piece_bar: nwg::Label,
    files_list: nwg::ListView,
    peers_list: nwg::ListView,
    trackers_list: nwg::ListView,
    status: nwg::StatusBar,
    timer: nwg::AnimationTimer,

    // tray - port of taskbaricon.cpp
    tray: nwg::TrayNotification,
    tray_menu: nwg::Menu,
    tray_show: nwg::MenuItem,
    tray_exit: nwg::MenuItem,

    // context menu - port of torrentcontextmenu.cpp
    ctx_menu: nwg::Menu,
    ctx_pause: nwg::MenuItem,
    ctx_resume: nwg::MenuItem,
    ctx_recheck: nwg::MenuItem,
    ctx_move: nwg::MenuItem,
    ctx_queue_menu: nwg::Menu,
    ctx_queue_up: nwg::MenuItem,
    ctx_queue_down: nwg::MenuItem,
    ctx_remove: nwg::MenuItem,
    ctx_remove_files: nwg::MenuItem,
    ctx_label_menu: nwg::Menu,
    ctx_label_items: RefCell<Vec<(nwg::MenuItem, Option<i32>)>>,
    ctx_copy_hash: nwg::MenuItem,
    ctx_copy_magnet: nwg::MenuItem,
    ctx_open_explorer: nwg::MenuItem,

    // dialog plumbing
    dialog_notice: nwg::Notice,
    dialog_tx: std::sync::mpsc::Sender<DialogResult>,
    dialog_rx: std::sync::mpsc::Receiver<DialogResult>,
    dialogs_pending: RefCell<Vec<DialogHandle>>,

    // state
    rows: RefCell<Vec<TorrentStatus>>,
    list_cells: RefCell<Vec<[String; 16]>>,
    labels: RefCell<Vec<Label>>,
    filters: RefCell<Vec<Filter>>,
    active_filter: RefCell<Option<TorrentFilter>>,
    active_filter_id: Cell<Option<i32>>,
    active_label_filter: Cell<Option<i32>>,
    console_filter: RefCell<Option<TorrentFilter>>,
    label_checked: RefCell<std::collections::HashSet<String>>,
    sort: Cell<(usize, bool)>,
    show_details: Cell<bool>,
    show_status: Cell<bool>,
    show_console: Cell<bool>,
    add_torrent_queue: RefCell<Vec<Vec<u8>>>,
    exiting: Cell<bool>,

    // Splitter between the torrent list and the details panel - port of
    // the wxSplitterWindow in the original main frame.
    details_height: Cell<i32>,
    splitter_dragging: Cell<bool>,
}

impl MainWindow {
    pub fn build(ctx: AppContext) -> anyhow::Result<Rc<MainWindow>> {
        let AppContext {
            env,
            db: _db,
            cfg,
            session,
            translator: tr,
            ipc,
            args,
            update_slot,
            geoip,
        } = ctx;

        let labels = cfg.get_labels();
        let filters = cfg.get_filters();

        // Port of Configuration::IsDarkMode + wxApp::MSWEnableDarkMode.
        let dark_mode = darkmode::is_dark_mode(&cfg);
        darkmode::set_enabled(dark_mode);

        let mut icon = nwg::Icon::default();
        nwg::Icon::builder()
            .source_bin(Some(APP_ICON))
            .build(&mut icon)?;

        let mut bold_font = nwg::Font::default();
        nwg::Font::builder()
            .family("Segoe UI")
            .size(16)
            .weight(700)
            .build(&mut bold_font)?;

        let mut window = nwg::Window::default();
        let title = format!(
            "NanoTorrent {} (build {})",
            crate::buildinfo::version(),
            crate::buildinfo::build_stamp()
        );
        nwg::Window::builder()
            .title(&title)
            .size((980, 640))
            .center(true)
            .icon(Some(&icon))
            .flags(nwg::WindowFlags::MAIN_WINDOW | nwg::WindowFlags::VISIBLE)
            .build(&mut window)?;

        // NWG's `.icon()` only sets ICON_SMALL; set ICON_BIG too so Alt-Tab and
        // the large taskbar/window views use the app icon as well.
        if let Some(hwnd) = window.handle.hwnd() {
            use winapi::um::winuser::{ICON_BIG, SendMessageW, WM_SETICON};
            unsafe {
                SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, icon.handle as isize);
            }
        }

        // --- menu bar (port of MainFrame::CreateMainMenu) -------------------
        let strip = |s: String| s.replace('&', "");

        let mut menu_file = nwg::Menu::default();
        nwg::Menu::builder()
            .parent(&window)
            .text(&tr.i18n("amp_file"))
            .build(&mut menu_file)?;

        let mut mi_add_torrent = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_file)
            .text(&tr.i18n("amp_add_torrent"))
            .build(&mut mi_add_torrent)?;

        let mut mi_add_magnet = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_file)
            .text(&tr.i18n("amp_add_magnet_link_s"))
            .build(&mut mi_add_magnet)?;

        let mut mi_create_torrent = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_file)
            .text(&tr.i18n("amp_create_torrent"))
            .build(&mut mi_create_torrent)?;

        let mut sep_import = nwg::MenuSeparator::default();
        nwg::MenuSeparator::builder()
            .parent(&menu_file)
            .build(&mut sep_import)?;

        let mut mi_import_pico = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_file)
            // Literal, not i18n: the Translator rebrands every "PicoTorrent" to
            // "NanoTorrent", but here it's a deliberate reference to the other app.
            .text(&tr.i18n1("import_from_app", "PicoTorrent"))
            .build(&mut mi_import_pico)?;

        let mut sep1 = nwg::MenuSeparator::default();
        nwg::MenuSeparator::builder()
            .parent(&menu_file)
            .build(&mut sep1)?;

        let mut mi_exit = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_file)
            .text(&tr.i18n("amp_exit"))
            .build(&mut mi_exit)?;

        let mut menu_view = nwg::Menu::default();
        nwg::Menu::builder()
            .parent(&window)
            .text(&tr.i18n("amp_view"))
            .build(&mut menu_view)?;

        let mut menu_filters = nwg::Menu::default();
        nwg::Menu::builder()
            .parent(&menu_view)
            .text(&strip(tr.i18n("filters")))
            .build(&mut menu_filters)?;

        let mut menu_labels = nwg::Menu::default();
        nwg::Menu::builder()
            .parent(&menu_view)
            .text(&strip(tr.i18n("labels")))
            .build(&mut menu_labels)?;

        let mut sep2 = nwg::MenuSeparator::default();
        nwg::MenuSeparator::builder()
            .parent(&menu_view)
            .build(&mut sep2)?;

        let mut mi_details_panel = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_view)
            .text(&tr.i18n("amp_details_panel"))
            .check(cfg.get::<bool>("ui.show_details_panel").unwrap_or(true))
            .build(&mut mi_details_panel)?;

        let mut mi_status_bar = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_view)
            .text(&tr.i18n("amp_status_bar"))
            .check(cfg.get::<bool>("ui.show_status_bar").unwrap_or(true))
            .build(&mut mi_status_bar)?;

        let show_console = cfg.get_bool("ui.show_console_input");
        let mut mi_console = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_view)
            .text(&tr.i18n("amp_console"))
            .check(show_console)
            .build(&mut mi_console)?;

        // Theme switcher: Auto / Light / Dark
        let mut menu_theme = nwg::Menu::default();
        nwg::Menu::builder()
            .parent(&menu_view)
            .text(&tr.i18n("theme"))
            .build(&mut menu_theme)?;

        let mut mi_theme_auto = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_theme)
            .text(&tr.i18n("auto"))
            .build(&mut mi_theme_auto)?;
        let mut mi_theme_light = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_theme)
            .text(&tr.i18n("light"))
            .build(&mut mi_theme_light)?;
        let mut mi_theme_dark = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_theme)
            .text(&tr.i18n("dark"))
            .build(&mut mi_theme_dark)?;

        let mut sep3 = nwg::MenuSeparator::default();
        nwg::MenuSeparator::builder()
            .parent(&menu_view)
            .build(&mut sep3)?;

        let mut mi_preferences = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_view)
            .text(&tr.i18n("amp_preferences"))
            .build(&mut mi_preferences)?;

        let mut menu_help = nwg::Menu::default();
        nwg::Menu::builder()
            .parent(&window)
            .text(&tr.i18n("amp_help"))
            .build(&mut menu_help)?;

        let mut mi_docs = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_help)
            .text(&tr.i18n("documentation"))
            .build(&mut mi_docs)?;

        let mut mi_about = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&menu_help)
            .text(&tr.i18n("amp_about"))
            .build(&mut mi_about)?;

        // --- console (PQL filter input, port of ui/console.cpp) -------------
        // Query-prompt glyph (">_") left of the input.
        let console_y = (CONSOLE_HEIGHT - CONSOLE_INPUT_HEIGHT) / 2;

        let mut console_icon = nwg::Label::default();
        nwg::Label::builder()
            .parent(&window)
            .text(">_")
            .position((8, console_y))
            .size((24, CONSOLE_INPUT_HEIGHT))
            .font(Some(&bold_font))
            .build(&mut console_icon)?;
        console_icon.set_visible(show_console);

        let mut console_input = nwg::TextInput::default();
        nwg::TextInput::builder()
            .parent(&window)
            .placeholder_text(Some("filter: name contains \"...\" or progress > 0.5"))
            .position((CONSOLE_INPUT_X, console_y))
            .size((980, CONSOLE_INPUT_HEIGHT))
            .build(&mut console_input)?;
        console_input.set_visible(show_console);

        // Breathing room for the text inside the edit (EM_SETMARGINS wants
        // physical pixels).
        if let Some(hwnd) = console_input.handle.hwnd() {
            unsafe {
                let dpi = winapi::um::winuser::GetDpiForWindow(hwnd);
                let margin = (6 * dpi.max(96) / 96) as usize;
                winapi::um::winuser::SendMessageW(
                    hwnd,
                    0x00D3, // EM_SETMARGINS
                    0x1 | 0x2, // EC_LEFTMARGIN | EC_RIGHTMARGIN
                    (margin | (margin << 16)) as isize,
                );
            }
        }

        // --- torrent list ---------------------------------------------------
        let mut list = nwg::ListView::default();
        nwg::ListView::builder()
            .parent(&window)
            .position((0, 0))
            .size((980, 360))
            .list_style(nwg::ListViewStyle::Detailed)
            .ex_flags(
                nwg::ListViewExFlags::FULL_ROW_SELECT | nwg::ListViewExFlags::HEADER_DRAG_DROP,
            )
            .build(&mut list)?;

        for (idx, (key, width)) in COLUMNS.iter().enumerate() {
            list.insert_column(nwg::InsertListViewColumn {
                index: Some(idx as i32),
                fmt: None,
                width: Some(*width),
                text: Some(tr.i18n(key)),
            });
        }
        list.set_headers_enabled(true);

        // --- details tabs ----------------------------------------------------
        let mut tabs = nwg::TabsContainer::default();
        nwg::TabsContainer::builder()
            .parent(&window)
            .build(&mut tabs)?;

        let mut tab_overview = nwg::Tab::default();
        nwg::Tab::builder()
            .parent(&tabs)
            .text(&tr.i18n("overview"))
            .build(&mut tab_overview)?;
        let mut tab_files = nwg::Tab::default();
        nwg::Tab::builder()
            .parent(&tabs)
            .text(&tr.i18n("files"))
            .build(&mut tab_files)?;
        let mut tab_peers = nwg::Tab::default();
        nwg::Tab::builder()
            .parent(&tabs)
            .text(&tr.i18n("peers"))
            .build(&mut tab_peers)?;
        let mut tab_trackers = nwg::Tab::default();
        nwg::Tab::builder()
            .parent(&tabs)
            .text(&tr.i18n("trackers"))
            .build(&mut tab_trackers)?;

        // Overview (port of torrentdetailsoverviewpanel.cpp): like PicoTorrent,
        // the pieces bar sits at the top spanning the full width, with the data
        // grid below it. Exact positions/sizes are set by layout_overview() so
        // it fills the window width; the values here are just initial defaults.
        let mut piece_caption = nwg::Label::default();
        nwg::Label::builder()
            .parent(&tab_overview)
            .text(&tr.i18n("pieces"))
            .position((12, 8))
            .size((110, 22))
            .font(Some(&bold_font))
            .build(&mut piece_caption)?;
        if let Some(hwnd) = piece_caption.handle.hwnd() {
            darkmode::install_field_bg(hwnd);
        }

        let mut piece_bar = nwg::Label::default();
        nwg::Label::builder()
            .parent(&tab_overview)
            .text("")
            .position((12, 32))
            .size((860, 18))
            .build(&mut piece_bar)?;
        if let Some(hwnd) = piece_bar.handle.hwnd() {
            darkmode::install_piece_bar(hwnd);
        }

        let overview_keys = [
            "name",
            "info_hash",
            "save_path",
            "status",
            "downloaded",
            "uploaded",
            "ratio",
            "peers",
            "added_on",
            "completed_on",
        ];
        // Label colors are handled by the tab-page subclass (see darkmode.rs)
        // which answers WM_CTLCOLORSTATIC for both themes. Captions are
        // bold, like the original overview panel.
        let mut overview_fields = Vec::new();
        for (i, key) in overview_keys.iter().enumerate() {
            let col = i % 2;
            let row = i / 2;
            let x = 12 + (col as i32) * 430;
            let y = 64 + (row as i32) * 28;

            let mut caption = nwg::Label::default();
            nwg::Label::builder()
                .parent(&tab_overview)
                .text(&tr.i18n(key))
                .position((x, y))
                .size((110, 22))
                .font(Some(&bold_font))
                .build(&mut caption)?;
            if let Some(hwnd) = caption.handle.hwnd() {
                darkmode::install_field_bg(hwnd);
            }

            let mut value = nwg::Label::default();
            nwg::Label::builder()
                .parent(&tab_overview)
                .text("-")
                .position((x + 116, y))
                .size((300, 22))
                .build(&mut value)?;

            // Keep long values (Name, save path, ...) on a single line: a
            // static defaults to word-wrap, but the label is only one line
            // tall, so a wrapped name spills into hidden space. SS_ENDELLIPSIS
            // truncates with "..." instead; SS_NOPREFIX renders a literal '&'.
            if let Some(hwnd) = value.handle.hwnd() {
                use winapi::um::winuser::{
                    GWL_STYLE, GetWindowLongPtrW, SS_ENDELLIPSIS, SS_NOPREFIX, SetWindowLongPtrW,
                };
                unsafe {
                    let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
                    SetWindowLongPtrW(
                        hwnd,
                        GWL_STYLE,
                        style | (SS_ENDELLIPSIS | SS_NOPREFIX) as isize,
                    );
                }
                darkmode::install_field_bg(hwnd);
            }

            overview_fields.push((caption, value));
        }

        // Files list (port of torrentdetailsfilespanel.cpp)
        let mut files_list = nwg::ListView::default();
        nwg::ListView::builder()
            .parent(&tab_files)
            .list_style(nwg::ListViewStyle::Detailed)
            .ex_flags(nwg::ListViewExFlags::FULL_ROW_SELECT)
            .build(&mut files_list)?;
        for (idx, (key, width)) in [
            ("name", 420),
            ("size", 90),
            ("progress", 80),
            ("include", 70),
        ]
        .iter()
        .enumerate()
        {
            files_list.insert_column(nwg::InsertListViewColumn {
                index: Some(idx as i32),
                fmt: None,
                width: Some(*width),
                text: Some(tr.i18n(key)),
            });
        }
        files_list.set_headers_enabled(true);

        // Peers list (port of torrentdetailspeerspanel.cpp)
        let mut peers_list = nwg::ListView::default();
        nwg::ListView::builder()
            .parent(&tab_peers)
            .list_style(nwg::ListViewStyle::Detailed)
            .ex_flags(nwg::ListViewExFlags::FULL_ROW_SELECT)
            .build(&mut peers_list)?;
        for (idx, (key, width)) in [
            ("address", 180),
            ("country", 130),
            ("status", 120),
            ("downloaded", 110),
            ("pieces", 80),
        ]
        .iter()
        .enumerate()
        {
            peers_list.insert_column(nwg::InsertListViewColumn {
                index: Some(idx as i32),
                fmt: None,
                width: Some(*width),
                text: Some(tr.i18n(key)),
            });
        }
        peers_list.set_headers_enabled(true);

        // Trackers list (port of torrentdetailstrackerspanel.cpp)
        let mut trackers_list = nwg::ListView::default();
        nwg::ListView::builder()
            .parent(&tab_trackers)
            .list_style(nwg::ListViewStyle::Detailed)
            .ex_flags(nwg::ListViewExFlags::FULL_ROW_SELECT)
            .build(&mut trackers_list)?;
        let tracker_cols: Vec<(String, i32)> = vec![
            (tr.i18n("url"), 320),
            (tr.i18n("status"), 130),
            (tr.i18n("seeds"), 70),
            (tr.i18n("leeches"), 70),
            (tr.i18n("fails"), 60),
            (tr.i18n("next_announce"), 120),
        ];
        for (i, (text, width)) in tracker_cols.into_iter().enumerate() {
            trackers_list.insert_column(nwg::InsertListViewColumn {
                index: Some(i as i32),
                fmt: None,
                width: Some(width),
                text: Some(text),
            });
        }
        trackers_list.set_headers_enabled(true);

        // --- status bar -------------------------------------------------------
        let mut status = nwg::StatusBar::default();
        nwg::StatusBar::builder()
            .parent(&window)
            .text("")
            .build(&mut status)?;

        // --- tray (port of taskbaricon.cpp) ------------------------------------
        let mut tray = nwg::TrayNotification::default();
        nwg::TrayNotification::builder()
            .parent(&window)
            .icon(Some(&icon))
            .tip(Some("NanoTorrent"))
            .build(&mut tray)?;

        let mut tray_menu = nwg::Menu::default();
        nwg::Menu::builder()
            .parent(&window)
            .popup(true)
            .build(&mut tray_menu)?;

        let mut tray_show = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&tray_menu)
            .text("NanoTorrent")
            .build(&mut tray_show)?;

        let mut tray_sep = nwg::MenuSeparator::default();
        nwg::MenuSeparator::builder()
            .parent(&tray_menu)
            .build(&mut tray_sep)?;

        let mut tray_exit = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&tray_menu)
            .text(&strip(tr.i18n("amp_exit")))
            .build(&mut tray_exit)?;

        // --- torrent context menu ----------------------------------------------
        let mut ctx_menu = nwg::Menu::default();
        nwg::Menu::builder()
            .parent(&window)
            .popup(true)
            .build(&mut ctx_menu)?;

        let mut ctx_pause = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&ctx_menu)
            .text(&tr.i18n("pause"))
            .build(&mut ctx_pause)?;

        let mut ctx_resume = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&ctx_menu)
            .text(&tr.i18n("resume"))
            .build(&mut ctx_resume)?;

        let mut ctx_recheck = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&ctx_menu)
            .text(&tr.i18n("force_recheck"))
            .build(&mut ctx_recheck)?;

        let mut ctx_move = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&ctx_menu)
            .text(&tr.i18n("move"))
            .build(&mut ctx_move)?;

        // Queue submenu - port of the queue position menu.
        let mut ctx_queue_menu = nwg::Menu::default();
        nwg::Menu::builder()
            .parent(&ctx_menu)
            .text(&strip(tr.i18n("queue_position")))
            .build(&mut ctx_queue_menu)?;
        let mut ctx_queue_up = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&ctx_queue_menu)
            .text(&tr.i18n("up"))
            .build(&mut ctx_queue_up)?;
        let mut ctx_queue_down = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&ctx_queue_menu)
            .text(&tr.i18n("down"))
            .build(&mut ctx_queue_down)?;

        let mut ctx_sep1 = nwg::MenuSeparator::default();
        nwg::MenuSeparator::builder()
            .parent(&ctx_menu)
            .build(&mut ctx_sep1)?;

        let mut ctx_remove = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&ctx_menu)
            .text(&tr.i18n("remove_torrent"))
            .build(&mut ctx_remove)?;

        let mut ctx_remove_files = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&ctx_menu)
            .text(&tr.i18n("remove_torrent_and_files"))
            .build(&mut ctx_remove_files)?;

        let mut ctx_sep2 = nwg::MenuSeparator::default();
        nwg::MenuSeparator::builder()
            .parent(&ctx_menu)
            .build(&mut ctx_sep2)?;

        let mut ctx_label_menu = nwg::Menu::default();
        nwg::Menu::builder()
            .parent(&ctx_menu)
            .text(&tr.i18n("label"))
            .build(&mut ctx_label_menu)?;

        let mut ctx_sep3 = nwg::MenuSeparator::default();
        nwg::MenuSeparator::builder()
            .parent(&ctx_menu)
            .build(&mut ctx_sep3)?;

        let mut ctx_copy_hash = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&ctx_menu)
            .text(&tr.i18n("copy_info_hash"))
            .build(&mut ctx_copy_hash)?;

        let mut ctx_copy_magnet = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&ctx_menu)
            .text(&tr.i18n("magnet_link_s"))
            .build(&mut ctx_copy_magnet)?;

        let mut ctx_open_explorer = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .parent(&ctx_menu)
            .text(&tr.i18n("open_in_explorer"))
            .build(&mut ctx_open_explorer)?;

        // --- timer + dialog notice ----------------------------------------------
        let mut timer = nwg::AnimationTimer::default();
        nwg::AnimationTimer::builder()
            .parent(&window)
            .interval(std::time::Duration::from_millis(1000))
            .build(&mut timer)?;

        let mut dialog_notice = nwg::Notice::default();
        nwg::Notice::builder()
            .parent(&window)
            .build(&mut dialog_notice)?;

        let (dialog_tx, dialog_rx) = std::sync::mpsc::channel();

        // Subclasses drive theme-dependent painting (menu bar, status bar,
        // tab pages + labels) for BOTH themes - installed unconditionally,
        // they branch on the darkmode flag.
        if let Some(hwnd) = window.handle.hwnd() {
            darkmode::install_main_subclass(hwnd);
        }
        for tab in [&tab_overview, &tab_files, &tab_peers, &tab_trackers] {
            if let Some(hwnd) = tab.handle.hwnd() {
                darkmode::install_tab_page_subclass(hwnd);
            }
        }
        for lv in [&list, &files_list, &peers_list, &trackers_list] {
            if let Some(hwnd) = lv.handle.hwnd() {
                darkmode::install_listview_subclass(hwnd);
            }
        }
        // Render the progress column as a bar (main list col 5, files list col 2).
        if let Some(hwnd) = list.handle.hwnd() {
            darkmode::register_progress_column(hwnd, 5);
        }
        if let Some(hwnd) = files_list.handle.hwnd() {
            darkmode::register_progress_column(hwnd, 2);
        }
        if let Some(hwnd) = status.handle.hwnd() {
            darkmode::register_status_bar(hwnd);
        }
        if let Some(hwnd) = tabs.handle.hwnd() {
            darkmode::register_tab_control(hwnd);
        }

        let active_limit = cfg.get_int("libtorrent.active_limit").unwrap_or(15);
        let active_downloads = cfg.get_int("libtorrent.active_downloads").unwrap_or(3);
        let active_seeds = cfg.get_int("libtorrent.active_seeds").unwrap_or(5);

        let this = Rc::new(MainWindow {
            env,
            session,
            cfg,
            tr,
            ipc,
            update_slot,
            geoip,
            create_slot: Arc::new(std::sync::Mutex::new(None)),
            magnet_slot: Arc::new(std::sync::Mutex::new(Vec::new())),
            resolving_magnets: std::cell::Cell::new(0),
            active_limit: Cell::new(active_limit),
            active_downloads: Cell::new(active_downloads),
            active_seeds: Cell::new(active_seeds),

            window,
            icon,
            bold_font,
            dark_mode: Cell::new(dark_mode),

            menu_file,
            mi_add_torrent,
            mi_add_magnet,
            mi_create_torrent,
            mi_import_pico,
            mi_exit,
            menu_view,
            menu_filters,
            filter_items: RefCell::new(Vec::new()),
            menu_labels,
            label_filter_items: RefCell::new(Vec::new()),
            mi_details_panel,
            mi_status_bar,
            mi_console,
            menu_theme,
            mi_theme_auto,
            mi_theme_light,
            mi_theme_dark,
            mi_preferences,
            menu_help,
            mi_docs,
            mi_about,

            console_icon,
            console_input,
            list,
            tabs,
            tab_overview,
            tab_files,
            tab_peers,
            tab_trackers,
            overview_fields,
            piece_caption,
            piece_bar,
            files_list,
            peers_list,
            trackers_list,
            status,
            timer,

            tray,
            tray_menu,
            tray_show,
            tray_exit,

            ctx_menu,
            ctx_pause,
            ctx_resume,
            ctx_recheck,
            ctx_move,
            ctx_queue_menu,
            ctx_queue_up,
            ctx_queue_down,
            ctx_remove,
            ctx_remove_files,
            ctx_label_menu,
            ctx_label_items: RefCell::new(Vec::new()),
            ctx_copy_hash,
            ctx_copy_magnet,
            ctx_open_explorer,

            dialog_notice,
            dialog_tx,
            dialog_rx,
            dialogs_pending: RefCell::new(Vec::new()),

            rows: RefCell::new(Vec::new()),
            list_cells: RefCell::new(Vec::new()),
            labels: RefCell::new(labels),
            filters: RefCell::new(filters),
            active_filter: RefCell::new(None),
            active_filter_id: Cell::new(None),
            active_label_filter: Cell::new(None),
            console_filter: RefCell::new(None),
            label_checked: RefCell::new(std::collections::HashSet::new()),
            sort: Cell::new((1, true)),
            show_details: Cell::new(true),
            show_status: Cell::new(true),
            show_console: Cell::new(show_console),
            add_torrent_queue: RefCell::new(Vec::new()),
            exiting: Cell::new(false),

            details_height: Cell::new(DETAILS_HEIGHT),
            splitter_dragging: Cell::new(false),
        });

        // Restore the saved splitter position (persistent_object table,
        // like the original PersistenceManager).
        if let Some(saved) = this
            .cfg
            .get_persistent("ui.splitter.details_height")
            .and_then(|v| v.parse::<i32>().ok())
        {
            this.details_height.set(saved.max(DETAILS_MIN));
        }

        this.show_details
            .set(this.cfg.get::<bool>("ui.show_details_panel").unwrap_or(true));
        this.show_status
            .set(this.cfg.get::<bool>("ui.show_status_bar").unwrap_or(true));

        this.rebuild_filter_menu();
        this.rebuild_label_menus();

        // Restore saved filter, like the original did at startup.
        if let Some(filter_id) = this.cfg.get::<Option<i32>>("current_filter").flatten() {
            this.set_filter(Some(filter_id));
        }

        // Tray visibility follows the show_in_notification_area setting.
        this.tray
            .set_visibility(this.cfg.get_bool("show_in_notification_area"));

        Self::bind_events(&this);

        this.apply_theme(dark_mode);
        this.restore_ui_state();
        this.layout();
        this.refresh();

        // Test hook: select the first row at startup so selection rendering
        // can be verified without user input.
        if std::env::var_os("NANOTORRENT_TEST_SELECT").is_some() && this.list.len() > 0 {
            this.list.select_item(0, true);
        }

        this.timer.start();

        // Handle initial command line arguments.
        this.handle_params(&args);

        Ok(this)
    }

    // event wiring

    fn bind_events(this: &Rc<MainWindow>) {
        let me = Rc::downgrade(this);

        let handler = move |evt: nwg::Event, data: nwg::EventData, handle: nwg::ControlHandle| {
            let Some(me) = me.upgrade() else {
                return;
            };

            match evt {
                nwg::Event::OnInit => {}
                nwg::Event::OnTimerTick if handle == me.timer.handle => {
                    me.on_tick();
                }
                nwg::Event::OnNotice if handle == me.dialog_notice.handle => {
                    me.on_dialog_finished();
                }
                nwg::Event::OnMenuItemSelected => {
                    me.on_menu(handle);
                }
                nwg::Event::OnListViewColumnClick if handle == me.list.handle => {
                    let (_, col) = data.on_list_view_item_index();
                    me.on_sort_column(col);
                }
                nwg::Event::OnTextInput if handle == me.console_input.handle => {
                    me.on_console_changed();
                }
                nwg::Event::OnListViewRightClick if handle == me.list.handle => {
                    me.show_context_menu();
                }
                nwg::Event::OnListViewItemChanged if handle == me.list.handle => {
                    me.update_details();
                }
                nwg::Event::OnListViewDoubleClick if handle == me.files_list.handle => {
                    let (row, _) = data.on_list_view_item_index();
                    me.toggle_file(row);
                }
                nwg::Event::OnContextMenu if handle == me.tray.handle => {
                    let (x, y) = nwg::GlobalCursor::position();
                    me.tray_menu.popup(x, y);
                }
                nwg::Event::OnMousePress(nwg::MousePressEvent::MousePressLeftUp)
                    if handle == me.tray.handle =>
                {
                    me.restore_from_tray();
                }
                // Splitter between list and details panel.
                nwg::Event::OnMouseMove if handle == me.window.handle => {
                    me.on_window_mouse_move();
                }
                nwg::Event::OnMousePress(nwg::MousePressEvent::MousePressLeftDown)
                    if handle == me.window.handle =>
                {
                    me.on_window_mouse_down();
                }
                nwg::Event::OnMousePress(nwg::MousePressEvent::MousePressLeftUp)
                    if handle == me.window.handle =>
                {
                    me.on_window_mouse_up();
                }
                nwg::Event::OnResize | nwg::Event::OnWindowMaximize
                    if handle == me.window.handle =>
                {
                    me.on_resize();
                }
                nwg::Event::OnWindowMinimize if handle == me.window.handle => {
                    if me.cfg.get_bool("minimize_to_notification_area")
                        && me.cfg.get_bool("show_in_notification_area")
                    {
                        me.window.set_visible(false);
                    }
                }
                nwg::Event::OnWindowClose if handle == me.window.handle => {
                    let tray = me.cfg.get_bool("show_in_notification_area");
                    // Saved close preference: "exit", "minimize", or "ask".
                    let action = me
                        .cfg
                        .get_persistent("ui.close_action")
                        .unwrap_or_default();

                    if me.exiting.get() || !tray || action == "exit" {
                        // Exiting via the tray menu, no tray to hide into, or the
                        // user chose to always exit.
                        me.shutdown();
                    } else if action == "minimize" {
                        if let nwg::EventData::OnWindowClose(close_data) = data {
                            close_data.close(false);
                        }
                        me.window.set_visible(false);
                    } else {
                        // Ask via a themed dialog (which can host the "remember"
                        // checkbox); the result comes back through the dialog
                        // notice, handled in on_dialog_finished.
                        if let nwg::EventData::OnWindowClose(close_data) = data {
                            close_data.close(false);
                        }
                        let dlg = dialogs::spawn_close_prompt(
                            me.tr.clone(),
                            me.hwnd_usize(),
                            me.dialog_tx.clone(),
                            me.dialog_notice.sender(),
                        );
                        me.dialogs_pending.borrow_mut().push(dlg);
                    }
                }
                _ => {}
            }
        };

        nwg::full_bind_event_handler(&this.window.handle, handler);
    }

    fn on_menu(&self, handle: nwg::ControlHandle) {
        if handle == self.mi_add_torrent.handle {
            self.on_add_torrent();
        } else if handle == self.mi_add_magnet.handle {
            let dlg = dialogs::spawn_add_magnet(
                self.tr.clone(),
                self.hwnd_usize(),
                self.dialog_tx.clone(),
                self.dialog_notice.sender(),
            );
            self.dialogs_pending.borrow_mut().push(dlg);
        } else if handle == self.mi_create_torrent.handle {
            let dlg = dialogs::spawn_create_torrent(
                self.tr.clone(),
                self.hwnd_usize(),
                self.dialog_tx.clone(),
                self.dialog_notice.sender(),
            );
            self.dialogs_pending.borrow_mut().push(dlg);
        } else if handle == self.mi_import_pico.handle {
            self.on_import_picotorrent();
        } else if handle == self.mi_console.handle {
            let show = !self.show_console.get();
            self.show_console.set(show);
            self.mi_console.set_checked(show);
            self.cfg.set("ui.show_console_input", &show);
            self.console_input.set_visible(show);
            self.console_icon.set_visible(show);
            if !show {
                self.console_input.set_text("");
                *self.console_filter.borrow_mut() = None;
            }
            self.layout();
            self.refresh();
        } else if handle == self.mi_exit.handle || handle == self.tray_exit.handle {
            self.exiting.set(true);
            self.window.close();
        } else if handle == self.mi_details_panel.handle {
            let show = !self.show_details.get();
            self.show_details.set(show);
            self.mi_details_panel.set_checked(show);
            self.cfg.set("ui.show_details_panel", &show);
            self.layout();
        } else if handle == self.mi_status_bar.handle {
            let show = !self.show_status.get();
            self.show_status.set(show);
            self.mi_status_bar.set_checked(show);
            self.cfg.set("ui.show_status_bar", &show);
            // StatusBar has no set_visible in NWG - use ShowWindow directly.
            if let Some(hwnd) = self.status.handle.hwnd() {
                unsafe {
                    winapi::um::winuser::ShowWindow(
                        hwnd,
                        if show {
                            winapi::um::winuser::SW_SHOW
                        } else {
                            winapi::um::winuser::SW_HIDE
                        },
                    );
                }
            }
            self.layout();
        } else if handle == self.mi_theme_auto.handle {
            self.on_theme_selected("system");
        } else if handle == self.mi_theme_light.handle {
            self.on_theme_selected("light");
        } else if handle == self.mi_theme_dark.handle {
            self.on_theme_selected("dark");
        } else if handle == self.mi_preferences.handle {
            let languages: Vec<(String, String)> = self
                .tr
                .languages()
                .iter()
                .map(|l| (l.locale.clone(), l.name.clone()))
                .collect();
            let dlg = dialogs::spawn_preferences(
                self.tr.clone(),
                self.cfg.clone(),
                languages,
                self.hwnd_usize(),
                self.dialog_tx.clone(),
                self.dialog_notice.sender(),
            );
            self.dialogs_pending.borrow_mut().push(dlg);
        } else if handle == self.mi_docs.handle {
            let _ = open::that("https://www.nanotorrent.org");
        } else if handle == self.mi_about.handle {
            let dlg = dialogs::spawn_about(
                self.tr.clone(),
                self.hwnd_usize(),
                self.dialog_tx.clone(),
                self.dialog_notice.sender(),
            );
            self.dialogs_pending.borrow_mut().push(dlg);
        } else if handle == self.tray_show.handle {
            self.restore_from_tray();
        } else if handle == self.ctx_pause.handle {
            for hash in self.selected_hashes() {
                self.session.pause(&hash);
            }
            self.refresh();
        } else if handle == self.ctx_resume.handle {
            for hash in self.selected_hashes() {
                // A manual resume overrides an earlier scheduler pause.
                self.session.clear_queue_pause(&hash);
                self.session.resume(&hash);
            }
            self.refresh();
        } else if handle == self.ctx_recheck.handle {
            for hash in self.selected_hashes() {
                self.session.recheck(&hash);
            }
            self.refresh();
        } else if handle == self.ctx_move.handle {
            let mut dialog = nwg::FileDialog::default();
            if nwg::FileDialog::builder()
                .title(self.tr.i18n("move"))
                .action(nwg::FileDialogAction::OpenDirectory)
                .build(&mut dialog)
                .is_ok()
                && dialog.run(Some(&self.window))
                && let Ok(dir) = dialog.get_selected_item()
            {
                let dir = dir.to_string_lossy().into_owned();
                for hash in self.selected_hashes() {
                    self.session.move_storage(&hash, &dir);
                }
                self.refresh();
            }
        } else if handle == self.ctx_queue_up.handle {
            for hash in self.selected_hashes() {
                self.session.queue_move(&hash, true);
            }
            self.refresh();
        } else if handle == self.ctx_queue_down.handle {
            for hash in self.selected_hashes() {
                self.session.queue_move(&hash, false);
            }
            self.refresh();
        } else if handle == self.ctx_remove.handle {
            self.remove_selected(false);
        } else if handle == self.ctx_remove_files.handle {
            self.remove_selected(true);
        } else if handle == self.ctx_copy_hash.handle {
            if let Some(hash) = self.selected_hashes().first() {
                nwg::Clipboard::set_data_text(&self.window.handle, hash);
            }
        } else if handle == self.ctx_copy_magnet.handle {
            if let Some(status) = self.selected_statuses().first() {
                let magnet = self.session.magnet_uri(&status.info_hash, &status.name);
                nwg::Clipboard::set_data_text(&self.window.handle, &magnet);
            }
        } else if handle == self.ctx_open_explorer.handle {
            if let Some(status) = self.selected_statuses().first() {
                let base = std::path::Path::new(&status.save_path);
                // librqbit stores files directly under the save path (it does
                // NOT add a {torrent name} subfolder when given an explicit
                // output folder), so open the first file's actual top-level
                // entry instead of assuming save_path/{name}.
                let path = self
                    .session
                    .files(&status.info_hash)
                    .first()
                    .and_then(|f| {
                        std::path::Path::new(&f.name)
                            .components()
                            .next()
                            .map(|c| c.as_os_str().to_owned())
                    })
                    .map(|top| base.join(top))
                    .unwrap_or_else(|| base.to_path_buf());
                utils::open_and_select(&path);
            }
        } else {
            // Dynamic menu items: filters / label filter / label assignment.
            for (item, filter_id) in self.filter_items.borrow().iter() {
                if handle == item.handle {
                    self.set_filter(*filter_id);
                    return;
                }
            }
            for (item, label_id) in self.label_filter_items.borrow().iter() {
                if handle == item.handle {
                    self.active_label_filter.set(*label_id);
                    for (other, other_id) in self.label_filter_items.borrow().iter() {
                        other.set_checked(other_id == label_id);
                    }
                    self.refresh();
                    return;
                }
            }
            for (item, label_id) in self.ctx_label_items.borrow().iter() {
                if handle == item.handle {
                    for hash in self.selected_hashes() {
                        self.session.set_label(&hash, *label_id);
                    }
                    self.refresh();
                    return;
                }
            }
        }
    }

    // dynamic menus

    fn rebuild_filter_menu(&self) {
        let mut items = self.filter_items.borrow_mut();
        items.clear();

        let mut none_item = nwg::MenuItem::default();
        let _ = nwg::MenuItem::builder()
            .parent(&self.menu_filters)
            .text(&self.tr.i18n("none"))
            .check(true)
            .build(&mut none_item);
        items.push((none_item, None));

        for filter in self.filters.borrow().iter() {
            let mut item = nwg::MenuItem::default();
            let _ = nwg::MenuItem::builder()
                .parent(&self.menu_filters)
                .text(&filter.name)
                .build(&mut item);
            items.push((item, Some(filter.id)));
        }
    }

    fn rebuild_label_menus(&self) {
        // View > Labels (filter by label)
        {
            let mut items = self.label_filter_items.borrow_mut();
            items.clear();

            let mut none_item = nwg::MenuItem::default();
            let _ = nwg::MenuItem::builder()
                .parent(&self.menu_labels)
                .text(&self.tr.i18n("none"))
                .check(true)
                .build(&mut none_item);
            items.push((none_item, None));

            for label in self.labels.borrow().iter() {
                let mut item = nwg::MenuItem::default();
                let _ = nwg::MenuItem::builder()
                    .parent(&self.menu_labels)
                    .text(&label.name)
                    .build(&mut item);
                items.push((item, Some(label.id)));
            }
        }

        // Context menu > Label (assign label)
        {
            let mut items = self.ctx_label_items.borrow_mut();
            items.clear();

            let mut none_item = nwg::MenuItem::default();
            let _ = nwg::MenuItem::builder()
                .parent(&self.ctx_label_menu)
                .text(&self.tr.i18n("none"))
                .build(&mut none_item);
            items.push((none_item, None));

            for label in self.labels.borrow().iter() {
                let mut item = nwg::MenuItem::default();
                let _ = nwg::MenuItem::builder()
                    .parent(&self.ctx_label_menu)
                    .text(&label.name)
                    .build(&mut item);
                items.push((item, Some(label.id)));
            }
        }
    }

    /// Apply light/dark theme to every part we control; safe to call at
    /// runtime (the theme switcher uses it).
    fn apply_theme(&self, dark: bool) {
        self.dark_mode.set(dark);
        darkmode::set_enabled(dark);
        darkmode::apply_app_mode(dark);

        if let Some(hwnd) = self.window.handle.hwnd() {
            darkmode::apply_to_window(hwnd, dark);
        }

        for lv in [
            &self.list,
            &self.files_list,
            &self.peers_list,
            &self.trackers_list,
        ] {
            if let Some(hwnd) = lv.handle.hwnd() {
                darkmode::apply_to_listview(hwnd, dark);
            }
        }

        if let Some(hwnd) = self.tabs.handle.hwnd() {
            darkmode::apply_to_tab_control(hwnd, dark);
        }

        if let Some(hwnd) = self.console_input.handle.hwnd() {
            darkmode::apply_to_edit(hwnd, dark);
        }

        if let Some(hwnd) = self.status.handle.hwnd() {
            darkmode::refresh_status(hwnd);
        }

        let theme = self
            .cfg
            .get_string("theme_id")
            .unwrap_or_else(|| String::from("system"));
        self.mi_theme_auto
            .set_checked(theme != "light" && theme != "dark");
        self.mi_theme_light.set_checked(theme == "light");
        self.mi_theme_dark.set_checked(theme == "dark");

        if let Some(hwnd) = self.window.handle.hwnd() {
            darkmode::redraw_all(hwnd);
        }
    }

    fn on_theme_selected(&self, theme_id: &str) {
        self.cfg.set("theme_id", &theme_id.to_string());
        self.apply_theme(darkmode::is_dark_mode(&self.cfg));
    }

    fn set_filter(&self, filter_id: Option<i32>) {
        let filter = filter_id.and_then(|id| {
            let filters = self.filters.borrow();
            let def = filters.iter().find(|f| f.id == id)?;
            match TorrentFilter::parse(&def.filter) {
                Ok(f) => Some(f),
                Err(err) => {
                    nwg::modal_error_message(
                        &self.window.handle,
                        &self.tr.i18n("error"),
                        &format!("Invalid filter '{}': {err}", def.name),
                    );
                    None
                }
            }
        });

        self.active_filter_id.set(filter_id);
        *self.active_filter.borrow_mut() = filter;
        self.cfg.set("current_filter", &filter_id);

        for (item, id) in self.filter_items.borrow().iter() {
            item.set_checked(*id == filter_id);
        }

        self.refresh();
    }

    // actions

    fn on_import_picotorrent(&self) {
        // Literal title, not i18n: the Translator rewrites "PicoTorrent" to
        // "NanoTorrent", but this deliberately names the other app.
        let title = "Import from PicoTorrent";
        let Some(db) = self.env.get_picotorrent_db_path() else {
            nwg::modal_info_message(
                &self.window.handle,
                title,
                "No PicoTorrent installation was found (looked for \
                 %LOCALAPPDATA%\\PicoTorrent\\PicoTorrent.sqlite).",
            );
            return;
        };

        match self.session.import_from_picotorrent(&db) {
            Ok((imported, skipped)) => {
                nwg::modal_info_message(
                    &self.window.handle,
                    title,
                    &format!(
                        "Imported {imported} torrent(s); skipped {skipped} already present.\n\n\
                         Their files are being rechecked to recover progress - this may take \
                         a moment for large torrents.",
                    ),
                );
            }
            Err(e) => {
                nwg::modal_error_message(
                    &self.window.handle,
                    &self.tr.i18n("error"),
                    &format!("Import from PicoTorrent failed: {e:#}"),
                );
            }
        }
    }

    fn on_add_torrent(&self) {
        let mut dialog = nwg::FileDialog::default();
        // NWG filter format: "Name(*.ext;*.ext)|Name(*.*)" - no space before
        // the parenthesis, or the builder fails.
        if let Err(err) = nwg::FileDialog::builder()
            .title(self.tr.i18n("add_torrent_s"))
            .action(nwg::FileDialogAction::Open)
            .multiselect(true)
            .filters("Torrent(*.torrent)|Any(*.*)")
            .build(&mut dialog)
        {
            nwg::modal_error_message(
                &self.window.handle,
                &self.tr.i18n("error"),
                &format!("Failed to open file dialog: {err}"),
            );
            return;
        }

        if dialog.run(Some(&self.window))
            && let Ok(files) = dialog.get_selected_items()
        {
            for file in files {
                match std::fs::read(&file) {
                    Ok(bytes) => self.queue_torrent_file(bytes),
                    Err(err) => {
                        nwg::modal_error_message(
                            &self.window.handle,
                            &self.tr.i18n("error"),
                            &format!("{}: {err}", file.to_string_lossy()),
                        );
                    }
                }
            }
        }
    }

    fn queue_torrent_file(&self, bytes: Vec<u8>) {
        if self.cfg.get_bool("skip_add_torrent_dialog") {
            self.session.add_torrent(
                AddTorrentSource::TorrentFileBytes(bytes),
                self.default_add_params(),
            );
            self.refresh();
        } else {
            self.add_torrent_queue.borrow_mut().push(bytes);
            self.open_next_add_dialog();
        }
    }

    fn open_next_add_dialog(&self) {
        // Only one add-torrent dialog at a time, like the original.
        self.dialogs_pending
            .borrow_mut()
            .retain(|h| !h.is_finished());
        if !self.dialogs_pending.borrow().is_empty() {
            return;
        }

        let Some(bytes) = ({
            let mut queue = self.add_torrent_queue.borrow_mut();
            if queue.is_empty() { None } else { Some(queue.remove(0)) }
        }) else {
            return;
        };

        let save_path = self.cfg.get_string("default_save_path").unwrap_or_default();
        let dlg = dialogs::spawn_add_torrent(
            self.tr.clone(),
            bytes,
            save_path,
            self.labels.borrow().clone(),
            self.hwnd_usize(),
            self.dialog_tx.clone(),
            self.dialog_notice.sender(),
        );
        self.dialogs_pending.borrow_mut().push(dlg);
    }

    /// Main window handle for the dialog threads (usize because HWND is not
    /// Send); the dialogs disable it while they are open (modal behavior).
    fn hwnd_usize(&self) -> usize {
        self.window
            .handle
            .hwnd()
            .map(|hwnd| hwnd as usize)
            .unwrap_or(0)
    }

    fn on_dialog_finished(&self) {
        while let Ok(result) = self.dialog_rx.try_recv() {
            match result {
                DialogResult::Magnets(links) => {
                    // Resolve each magnet's metadata, then show the add dialog.
                    for link in links {
                        self.begin_add_magnet(link);
                    }
                }
                DialogResult::AddTorrent { bytes, params } => {
                    self.session
                        .add_torrent(AddTorrentSource::TorrentFileBytes(bytes), params);
                }
                DialogResult::CreateTorrent(params) => {
                    // Hashing runs on the session runtime; on_tick picks up
                    // the outcome from the slot.
                    self.session.create_torrent(params, self.create_slot.clone());
                }
                DialogResult::PreferencesSaved => {
                    *self.labels.borrow_mut() = self.cfg.get_labels();
                    self.rebuild_label_menus();
                    // Labels (and their auto-apply filters) may have changed.
                    self.label_checked.borrow_mut().clear();
                    self.tray
                        .set_visibility(self.cfg.get_bool("show_in_notification_area"));

                    // Apply the new settings live: refresh the cached queue
                    // limits and rebuild the librqbit session (listen port,
                    // rate limits, proxy, DHT, IP filter).
                    self.active_limit
                        .set(self.cfg.get_int("libtorrent.active_limit").unwrap_or(15));
                    self.active_downloads.set(
                        self.cfg.get_int("libtorrent.active_downloads").unwrap_or(3),
                    );
                    self.active_seeds
                        .set(self.cfg.get_int("libtorrent.active_seeds").unwrap_or(5));
                    self.session.apply_settings(&self.env, &self.cfg);
                }
                DialogResult::CloseChoice { exit, remember } => {
                    if remember {
                        self.cfg.set_persistent(
                            "ui.close_action",
                            if exit { "exit" } else { "minimize" },
                        );
                    }
                    if exit {
                        // Defer to the normal exit path (runs after this drains).
                        self.exiting.set(true);
                        self.window.close();
                    } else {
                        self.window.set_visible(false);
                    }
                }
                DialogResult::Cancelled => {}
            }
        }

        self.refresh();
        self.open_next_add_dialog();
    }

    /// Begin adding a magnet: first resolve its metadata (file list etc.) so
    /// the same add dialog as for a .torrent file can be shown. The dialog is
    /// opened from on_tick once the metadata arrives (or it falls back to a
    /// direct add if resolution fails/times out).
    fn begin_add_magnet(&self, uri: String) {
        self.resolving_magnets
            .set(self.resolving_magnets.get() + 1);
        self.session.resolve_magnet(uri, self.magnet_slot.clone());
        self.update_status_bar();
    }

    fn default_add_params(&self) -> crate::bittorrent::session::AddParams {
        crate::bittorrent::session::AddParams {
            save_path: self.cfg.get_string("default_save_path"),
            start_torrent: true,
            only_files: None,
            label_id: None,
        }
    }

    pub fn handle_params(&self, args: &[String]) {
        for arg in args {
            if arg.starts_with("magnet:") {
                self.begin_add_magnet(arg.clone());
            } else if arg.to_lowercase().ends_with(".torrent") {
                match std::fs::read(arg) {
                    Ok(bytes) => self.queue_torrent_file(bytes),
                    Err(err) => {
                        nwg::modal_error_message(
                            &self.window.handle,
                            &self.tr.i18n("error"),
                            &format!("{arg}: {err}"),
                        );
                    }
                }
            }
        }
        self.refresh();
    }

    fn remove_selected(&self, delete_files: bool) {
        for hash in self.selected_hashes() {
            self.session.remove(&hash, delete_files);
        }
        self.refresh();
    }

    fn toggle_file(&self, file_index: usize) {
        let Some(status) = self.selected_statuses().first().cloned() else {
            return;
        };

        let files = self.session.files(&status.info_hash);
        if file_index >= files.len() {
            return;
        }

        let only: Vec<usize> = files
            .iter()
            .enumerate()
            .filter(|(idx, f)| {
                if *idx == file_index {
                    !f.included
                } else {
                    f.included
                }
            })
            .map(|(idx, _)| idx)
            .collect();

        self.session.update_only_files(&status.info_hash, only);
        self.update_details();
    }

    fn show_context_menu(&self) {
        if self.list.selected_count() == 0 {
            return;
        }

        // Show pause or resume depending on the first selected torrent.
        if let Some(status) = self.selected_statuses().first() {
            self.ctx_pause.set_enabled(!status.paused);
            self.ctx_resume.set_enabled(status.paused);
        }

        let (x, y) = nwg::GlobalCursor::position();
        self.ctx_menu.popup(x, y);
    }

    fn restore_from_tray(&self) {
        self.window.set_visible(true);
        self.window.restore();
        self.window.set_focus();
    }

    fn shutdown(&self) {
        self.save_ui_state();
        self.timer.stop();
        self.tray.set_visibility(false);
        self.session.stop();
        nwg::stop_thread_dispatch();
    }

    // UI state persistence (port of the PersistenceManager usage: window
    // geometry, column widths/order and sort, stored in persistent_object)

    fn window_dpi(&self) -> u32 {
        self.window
            .handle
            .hwnd()
            .map(|hwnd| unsafe { winapi::um::winuser::GetDpiForWindow(hwnd).max(96) })
            .unwrap_or(96)
    }

    fn save_ui_state(&self) {
        use winapi::um::winuser::{GetWindowRect, IsZoomed, SendMessageW};

        let Some(hwnd) = self.window.handle.hwnd() else {
            return;
        };

        let dpi = self.window_dpi();

        // Window placement (physical px + the DPI they were captured at).
        unsafe {
            let mut rc: winapi::shared::windef::RECT = std::mem::zeroed();
            GetWindowRect(hwnd, &mut rc);
            let maximized = IsZoomed(hwnd) != 0;
            self.cfg.set_persistent(
                "ui.window.geometry",
                &format!(
                    "{},{},{},{},{},{}",
                    rc.left,
                    rc.top,
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                    maximized as i32,
                    dpi
                ),
            );
        }

        // Column widths (physical) + display order.
        if let Some(lv) = self.list.handle.hwnd() {
            const LVM_FIRST: u32 = 0x1000;
            const LVM_GETCOLUMNWIDTH: u32 = LVM_FIRST + 29;
            const LVM_GETCOLUMNORDERARRAY: u32 = LVM_FIRST + 59;

            unsafe {
                let widths: Vec<String> = (0..COLUMNS.len())
                    .map(|i| SendMessageW(lv, LVM_GETCOLUMNWIDTH, i, 0).to_string())
                    .collect();
                self.cfg.set_persistent(
                    "ui.list.widths",
                    &format!("{dpi};{}", widths.join(",")),
                );

                let mut order = vec![0i32; COLUMNS.len()];
                if SendMessageW(
                    lv,
                    LVM_GETCOLUMNORDERARRAY,
                    order.len(),
                    order.as_mut_ptr() as isize,
                ) != 0
                {
                    let order: Vec<String> = order.iter().map(|o| o.to_string()).collect();
                    self.cfg.set_persistent("ui.list.order", &order.join(","));
                }
            }
        }

        let (col, asc) = self.sort.get();
        self.cfg
            .set_persistent("ui.list.sort", &format!("{col},{}", asc as i32));
    }

    fn restore_ui_state(&self) {
        use winapi::um::winuser::{
            MONITOR_DEFAULTTONULL, MonitorFromRect, SWP_NOZORDER, SendMessageW, SetWindowPos,
        };

        let dpi = self.window_dpi();

        // Window placement. Only restore when still (partly) on a monitor.
        if let Some(hwnd) = self.window.handle.hwnd()
            && let Some(saved) = self.cfg.get_persistent("ui.window.geometry")
        {
            let parts: Vec<i64> = saved.split(',').filter_map(|p| p.parse().ok()).collect();
            if let [x, y, w, h, maximized, saved_dpi] = parts[..] {
                let scale = dpi as f64 / (saved_dpi.max(96) as f64);
                let (w, h) = ((w as f64 * scale) as i32, (h as f64 * scale) as i32);
                let rc = winapi::shared::windef::RECT {
                    left: x as i32,
                    top: y as i32,
                    right: x as i32 + w,
                    bottom: y as i32 + h,
                };
                unsafe {
                    if !MonitorFromRect(&rc, MONITOR_DEFAULTTONULL).is_null() {
                        SetWindowPos(
                            hwnd,
                            std::ptr::null_mut(),
                            x as i32,
                            y as i32,
                            w,
                            h,
                            SWP_NOZORDER,
                        );
                        if maximized != 0 {
                            self.window.maximize();
                        }
                    }
                }
            }
        }

        if let Some(lv) = self.list.handle.hwnd() {
            const LVM_FIRST: u32 = 0x1000;
            const LVM_SETCOLUMNWIDTH: u32 = LVM_FIRST + 30;
            const LVM_SETCOLUMNORDERARRAY: u32 = LVM_FIRST + 58;

            // Column widths, rescaled for DPI changes between sessions.
            if let Some(saved) = self.cfg.get_persistent("ui.list.widths")
                && let Some((saved_dpi, widths)) = saved.split_once(';')
            {
                let saved_dpi: f64 = saved_dpi.parse().unwrap_or(96.0);
                let scale = dpi as f64 / saved_dpi.max(96.0);
                unsafe {
                    for (i, w) in widths.split(',').enumerate().take(COLUMNS.len()) {
                        if let Ok(w) = w.parse::<f64>() {
                            SendMessageW(
                                lv,
                                LVM_SETCOLUMNWIDTH,
                                i,
                                ((w * scale) as isize).max(20),
                            );
                        }
                    }
                }
            }

            if let Some(saved) = self.cfg.get_persistent("ui.list.order") {
                let order: Vec<i32> =
                    saved.split(',').filter_map(|p| p.parse().ok()).collect();
                if order.len() == COLUMNS.len() {
                    unsafe {
                        SendMessageW(
                            lv,
                            LVM_SETCOLUMNORDERARRAY,
                            order.len(),
                            order.as_ptr() as isize,
                        );
                    }
                }
            }
        }

        if let Some(saved) = self.cfg.get_persistent("ui.list.sort")
            && let Some((col, asc)) = saved.split_once(',')
            && let Ok(col) = col.parse::<usize>()
            && col < COLUMNS.len()
        {
            self.sort.set((col, asc == "1"));
            self.list.set_column_sort_arrow(
                col,
                Some(if asc == "1" {
                    nwg::ListViewColumnSortArrow::Up
                } else {
                    nwg::ListViewColumnSortArrow::Down
                }),
            );
        }
    }

    // selection helpers

    fn selected_hashes(&self) -> Vec<String> {
        let rows = self.rows.borrow();
        self.list
            .selected_items()
            .into_iter()
            .filter_map(|idx| rows.get(idx).map(|r| r.info_hash.clone()))
            .collect()
    }

    fn selected_statuses(&self) -> Vec<TorrentStatus> {
        let rows = self.rows.borrow();
        self.list
            .selected_items()
            .into_iter()
            .filter_map(|idx| rows.get(idx).cloned())
            .collect()
    }

    // periodic refresh - port of the original's 1000ms timer

    fn on_tick(&self) {
        // Args forwarded from a second instance.
        let forwarded = self.ipc.as_ref().and_then(|server| server.try_recv());
        if let Some(args) = forwarded {
            self.handle_params(&args);
            self.restore_from_tray();
        }

        // Finished create-torrent runs.
        let created = self
            .create_slot
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        match created {
            Some(CreateTorrentOutcome::Created {
                name,
                bytes,
                save_path,
                add_to_session,
            }) => {
                if add_to_session {
                    self.session.add_torrent(
                        AddTorrentSource::TorrentFileBytes(bytes),
                        crate::bittorrent::session::AddParams {
                            save_path,
                            start_torrent: true,
                            only_files: None,
                            label_id: None,
                        },
                    );
                }
                self.tray.show(
                    &name,
                    Some(&self.tr.i18n("create_torrent")),
                    Some(nwg::TrayNotificationFlags::USER_ICON),
                    Some(&self.icon),
                );
            }
            Some(CreateTorrentOutcome::Failed(err)) => {
                nwg::modal_error_message(&self.window.handle, &self.tr.i18n("error"), &err);
            }
            None => {}
        }

        // Resolved magnet metadata -> open the add dialog (like a .torrent
        // file); on failure fall back to adding the magnet directly.
        let resolved: Vec<_> = self
            .magnet_slot
            .lock()
            .map(|mut s| std::mem::take(&mut *s))
            .unwrap_or_default();
        for outcome in resolved {
            self.resolving_magnets
                .set(self.resolving_magnets.get().saturating_sub(1));
            match outcome {
                crate::bittorrent::session::MagnetOutcome::Resolved(bytes) => {
                    self.queue_torrent_file(bytes);
                }
                crate::bittorrent::session::MagnetOutcome::Failed(uri) => {
                    // Couldn't fetch metadata (no seeders / timed out) - add it
                    // anyway; librqbit keeps resolving in the background.
                    self.session
                        .add_torrent(AddTorrentSource::MagnetUri(uri), self.default_add_params());
                }
            }
        }

        // Update available notice - shown as a tray balloon.
        let update = self
            .update_slot
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(update) = update {
            self.tray.show(
                &format!("{} v{}", self.tr.i18n("new_version_available"), update.version),
                Some("NanoTorrent"),
                Some(nwg::TrayNotificationFlags::USER_ICON),
                Some(&self.icon),
            );
        }

        self.refresh();
    }

    fn on_sort_column(&self, col: usize) {
        if col >= COLUMNS.len() {
            return;
        }
        let (cur_col, asc) = self.sort.get();
        if cur_col == col {
            self.sort.set((col, !asc));
        } else {
            self.sort.set((col, true));
        }

        for idx in 0..COLUMNS.len() {
            self.list.set_column_sort_arrow(
                idx,
                if idx == col {
                    Some(if self.sort.get().1 {
                        nwg::ListViewColumnSortArrow::Up
                    } else {
                        nwg::ListViewColumnSortArrow::Down
                    })
                } else {
                    None
                },
            );
        }

        self.refresh();
    }

    fn sort_rows(&self, rows: &mut [TorrentStatus]) {
        use std::cmp::Ordering;

        let (col, asc) = self.sort.get();

        rows.sort_by(|a, b| {
            let ord = match col {
                0 => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                1 => a.queue_position.cmp(&b.queue_position),
                2 => a.total_wanted.cmp(&b.total_wanted),
                3 => a.total_wanted_remaining.cmp(&b.total_wanted_remaining),
                4 => (a.state as i32).cmp(&(b.state as i32)),
                5 => a.progress.partial_cmp(&b.progress).unwrap_or(Ordering::Equal),
                6 => a.eta.cmp(&b.eta),
                7 => a.download_payload_rate.cmp(&b.download_payload_rate),
                8 => a.upload_payload_rate.cmp(&b.upload_payload_rate),
                9 => a
                    .availability
                    .partial_cmp(&b.availability)
                    .unwrap_or(Ordering::Equal),
                10 => a.ratio.partial_cmp(&b.ratio).unwrap_or(Ordering::Equal),
                11 => a.seeds_current.cmp(&b.seeds_current),
                12 => a.peers_current.cmp(&b.peers_current),
                13 => a.added_on.cmp(&b.added_on),
                14 => a.completed_on.cmp(&b.completed_on),
                15 => a.label_name.cmp(&b.label_name),
                _ => Ordering::Equal,
            };
            if asc { ord } else { ord.reverse() }
        });
    }

    fn refresh(&self) {
        // Belt and braces for dark mode: the list views' stored colors can
        // be reset by system broadcasts we don't know about - re-assert
        // them on every tick (three cheap messages per list).
        if self.dark_mode.get() {
            for lv in [
                &self.list,
                &self.files_list,
                &self.peers_list,
                &self.trackers_list,
            ] {
                if let Some(hwnd) = lv.handle.hwnd() {
                    darkmode::apply_listview_colors(hwnd, true);
                }
            }
        }

        let label_names: HashMap<i32, String> = self
            .labels
            .borrow()
            .iter()
            .map(|l| (l.id, l.name.clone()))
            .collect();

        let mut rows = self.session.torrents(&label_names);

        // Labels with an auto-apply filter claim matching unlabeled
        // torrents (each torrent is only evaluated once).
        self.auto_apply_labels(&rows);

        // Queue limits (port of libtorrent's active_limit / active_downloads /
        // active_seeds).
        self.session.enforce_queue(
            &rows,
            self.active_limit.get(),
            self.active_downloads.get(),
            self.active_seeds.get(),
        );

        // Pause active torrents whose save path is running low on disk (port of
        // MainFrame::CheckDiskSpace). Off by default, so gate on the flag first.
        if self.cfg.get_bool("pause_on_low_disk_space") {
            self.check_low_disk_space(&rows);
        }

        if let Some(filter) = self.active_filter.borrow().as_ref() {
            rows.retain(|r| filter.includes(r));
        }
        if let Some(filter) = self.console_filter.borrow().as_ref() {
            rows.retain(|r| filter.includes(r));
        }
        if let Some(label_id) = self.active_label_filter.get() {
            rows.retain(|r| r.label_id == Some(label_id));
        }

        self.sort_rows(&mut rows);

        // Preserve selection by info hash across the rebuild.
        let selected_hashes: Vec<String> = self.selected_hashes();

        self.sync_list(&rows, &selected_hashes);
        *self.rows.borrow_mut() = rows;

        self.update_status_bar();
        self.update_details();

        // Real Windows toast (bottom-right popup + Action Center) when a
        // download finishes - a tray balloon only shows if the tray icon is
        // visible and Windows often suppresses it; the toast doesn't need one.
        for name in self.session.take_completions() {
            tracing::info!("showing completion notification: {name}");
            crate::core::toast::download_complete(&self.tr.i18n("download_complete"), &name);
        }

        // Errors surfaced from the session.
        for err in self.session.take_errors() {
            nwg::modal_error_message(&self.window.handle, &self.tr.i18n("error"), &err);
        }
    }

    /// Pause any currently-active torrent whose save path has less free space
    /// than the configured percentage. Disk queries are deduplicated per path,
    /// and only running torrents are considered - once paused a torrent drops
    /// out of the active set, so it is neither re-checked nor re-ballooned.
    fn check_low_disk_space(&self, rows: &[TorrentStatus]) {
        let limit = self.cfg.get_int("pause_on_low_disk_space_limit").unwrap_or(5);
        let mut low_by_path: HashMap<String, bool> = HashMap::new();

        for row in rows {
            let active = matches!(
                row.state,
                State::Downloading | State::DownloadingMetadata | State::Uploading
            );
            if !active || row.save_path.is_empty() {
                continue;
            }

            let low = *low_by_path
                .entry(row.save_path.clone())
                .or_insert_with(|| {
                    disk_free_total(&row.save_path)
                        .map(|(free, total)| low_disk_should_pause(free, total, limit))
                        .unwrap_or(false)
                });

            if low {
                tracing::info!(
                    "pausing {} - low disk space on {}",
                    row.info_hash,
                    row.save_path
                );
                self.session.pause(&row.info_hash);
                self.tray.show(
                    &row.name,
                    Some(&self.tr.i18n("pause_on_low_disk_space_alert")),
                    Some(nwg::TrayNotificationFlags::USER_ICON),
                    Some(&self.icon),
                );
            }
        }
    }

    /// Differential list sync: only cells whose text actually changed are
    /// written. Rewriting everything every tick caused a full repaint
    /// (white flash) of the list once per second.
    fn sync_list(&self, rows: &[TorrentStatus], selected_hashes: &[String]) {
        let lv = &self.list;
        let mut cache = self.list_cells.borrow_mut();

        while lv.len() > rows.len() {
            lv.remove_item(lv.len() - 1);
        }
        cache.truncate(rows.len());
        while lv.len() < rows.len() {
            let idx = lv.len() as i32;
            lv.insert_item(nwg::InsertListViewItem {
                index: Some(idx),
                column_index: 0,
                text: Some(String::new()),
                image: None,
            });
        }
        while cache.len() < rows.len() {
            cache.push(std::array::from_fn(|_| String::new()));
        }

        let currently_selected: std::collections::HashSet<usize> =
            lv.selected_items().into_iter().collect();

        for (i, status) in rows.iter().enumerate() {
            // A paused torrent isn't transferring, so blank the live columns
            // (rate/ETA/peers/seeds/availability) with "-" until it's resumed.
            let paused = matches!(
                status.state,
                State::DownloadingPaused | State::UploadingPaused
            );
            let dash = || String::from("-");
            let cells: [String; 16] = [
                status.name.clone(),
                // 1-based for display (stored queue_position is 0-based).
                (status.queue_position + 1).to_string(),
                utils::to_human_file_size(status.total_wanted),
                utils::to_human_file_size(status.total_wanted_remaining),
                format::state_text(&self.tr, status),
                format!("{:.1} %", status.progress * 100.0),
                if paused {
                    dash()
                } else {
                    format::eta_text(status)
                },
                if paused {
                    dash()
                } else {
                    format::speed_text(status.download_payload_rate)
                },
                if paused {
                    dash()
                } else {
                    format::speed_text(status.upload_payload_rate)
                },
                if paused || status.availability < 0.0 {
                    dash()
                } else {
                    format!("{:.2}", status.availability)
                },
                format!("{:.2}", status.ratio),
                if paused {
                    dash()
                } else {
                    format!("{} ({})", status.seeds_current, status.seeds_total)
                },
                if paused {
                    dash()
                } else {
                    format!("{} ({})", status.peers_current, status.peers_total)
                },
                format::date_text(&status.added_on),
                format::opt_date_text(&status.completed_on),
                status.label_name.clone(),
            ];

            for (col, text) in cells.into_iter().enumerate() {
                if cache[i][col] != text {
                    lv.update_item(
                        i,
                        nwg::InsertListViewItem {
                            index: Some(i as i32),
                            column_index: col as i32,
                            text: Some(text.clone()),
                            image: None,
                        },
                    );
                    cache[i][col] = text;
                }
            }

            let selected = selected_hashes.contains(&status.info_hash);
            if currently_selected.contains(&i) != selected {
                lv.select_item(i, selected);
            }
        }
    }

    fn update_status_bar(&self) {
        if !self.show_status.get() {
            return;
        }

        let Some(hwnd) = self.status.handle.hwnd() else {
            return;
        };

        let rows = self.rows.borrow();
        let (down, up) = self.session.session_rates();
        let dht = self.session.dht_nodes();

        // Owner-drawn in dark mode (status bars have no text color API).
        let part0 = if self.resolving_magnets.get() > 0 {
            self.tr.i18n("retrieving_metadata")
        } else {
            self.tr.i18n1("num_torrents", &rows.len().to_string())
        };
        darkmode::set_status_text(hwnd, 0, &part0);
        darkmode::set_status_text(
            hwnd,
            1,
            &match dht {
                Some(nodes) => self.tr.i18n1("dht_num_nodes", &nodes.to_string()),
                None => self.tr.i18n("dht_disabled"),
            },
        );
        darkmode::set_status_text(
            hwnd,
            2,
            &self.tr.i18n2(
                "dl_s_ul_s",
                &utils::to_human_file_size(down),
                &utils::to_human_file_size(up),
            ),
        );
        darkmode::set_status_text(
            hwnd,
            3,
            &self.tr.i18n(if self.session.ipfilter_active() {
                "ip_filter_enabled"
            } else {
                "ip_filter_disabled"
            }),
        );
    }

    /// Console (PQL) input changed - filter the list live. Invalid or
    /// partial queries simply don't filter.
    fn on_console_changed(&self) {
        let text = self.console_input.text();
        let text = text.trim();
        *self.console_filter.borrow_mut() = if text.is_empty() {
            None
        } else {
            TorrentFilter::parse(text).ok()
        };
        self.refresh();
    }

    /// Port of the label auto-apply behaviour: labels whose filter is
    /// enabled claim unlabeled torrents that match it.
    fn auto_apply_labels(&self, rows: &[TorrentStatus]) {
        let labels = self.labels.borrow();
        let filters: Vec<(i32, TorrentFilter)> = labels
            .iter()
            .filter(|l| l.apply_filter_enabled && !l.apply_filter.is_empty())
            .filter_map(|l| TorrentFilter::parse(&l.apply_filter).ok().map(|f| (l.id, f)))
            .collect();
        if filters.is_empty() {
            return;
        }

        let mut checked = self.label_checked.borrow_mut();
        for row in rows {
            if row.label_id.is_some() || !checked.insert(row.info_hash.clone()) {
                continue;
            }
            if let Some((id, _)) = filters.iter().find(|(_, f)| f.includes(row)) {
                self.session.set_label(&row.info_hash, Some(*id));
            }
        }
    }

    fn update_details(&self) {
        if !self.show_details.get() {
            return;
        }

        let statuses = self.selected_statuses();
        let status = statuses.first();

        // Overview labels
        let values: [String; 10] = match status {
            Some(s) => [
                s.name.clone(),
                s.info_hash.clone(),
                s.save_path.clone(),
                format::state_text(&self.tr, s),
                utils::to_human_file_size(s.all_time_download),
                utils::to_human_file_size(s.all_time_upload),
                format!("{:.2}", s.ratio),
                format!("{} ({})", s.peers_current, s.peers_total),
                format::date_text(&s.added_on),
                format::opt_date_text(&s.completed_on),
            ],
            None => std::array::from_fn(|_| String::from("-")),
        };

        for ((_, value_label), text) in self.overview_fields.iter().zip(values.iter()) {
            if &value_label.text() != text {
                value_label.set_text(text);
            }
        }

        // Piece progress bar for the selected torrent.
        if let Some(hwnd) = self.piece_bar.handle.hwnd() {
            let (bytes, total) = status
                .and_then(|s| self.session.piece_map(&s.info_hash))
                .unwrap_or_default();
            darkmode::set_piece_bar(hwnd, bytes, total);
        }

        let Some(status) = status else {
            self.files_list.clear();
            self.peers_list.clear();
            self.trackers_list.clear();
            return;
        };

        match self.tabs.selected_tab() {
            1 => {
                // Files
                let files = self.session.files(&status.info_hash);
                let lv = &self.files_list;
                lv.set_redraw(false);
                while lv.len() > files.len() {
                    lv.remove_item(lv.len() - 1);
                }
                while lv.len() < files.len() {
                    let idx = lv.len() as i32;
                    lv.insert_item(nwg::InsertListViewItem {
                        index: Some(idx),
                        column_index: 0,
                        text: Some(String::new()),
                        image: None,
                    });
                }
                for (i, file) in files.iter().enumerate() {
                    let cells = [
                        file.name.clone(),
                        utils::to_human_file_size(file.length as i64),
                        format!("{:.1} %", file.progress * 100.0),
                        if file.included {
                            self.tr.i18n("yes")
                        } else {
                            self.tr.i18n("no")
                        },
                    ];
                    for (col, text) in cells.into_iter().enumerate() {
                        lv.update_item(
                            i,
                            nwg::InsertListViewItem {
                                index: Some(i as i32),
                                column_index: col as i32,
                                text: Some(text),
                                image: None,
                            },
                        );
                    }
                }
                lv.set_redraw(true);
            }
            2 => {
                // Peers
                let peers = self.session.peers(&status.info_hash);
                let lv = &self.peers_list;
                lv.set_redraw(false);
                while lv.len() > peers.len() {
                    lv.remove_item(lv.len() - 1);
                }
                while lv.len() < peers.len() {
                    let idx = lv.len() as i32;
                    lv.insert_item(nwg::InsertListViewItem {
                        index: Some(idx),
                        column_index: 0,
                        text: Some(String::new()),
                        image: None,
                    });
                }
                for (i, peer) in peers.iter().enumerate() {
                    let cells = [
                        peer.addr.clone(),
                        self.geoip.country(&peer.addr).unwrap_or_default(),
                        peer.state.clone(),
                        utils::to_human_file_size(peer.fetched_bytes as i64),
                        peer.pieces.to_string(),
                    ];
                    for (col, text) in cells.into_iter().enumerate() {
                        lv.update_item(
                            i,
                            nwg::InsertListViewItem {
                                index: Some(i as i32),
                                column_index: col as i32,
                                text: Some(text),
                                image: None,
                            },
                        );
                    }
                }
                lv.set_redraw(true);
            }
            3 => {
                // Trackers
                use crate::bittorrent::session::TrackerRowKind;
                let rows = self.session.tracker_rows(&status.info_hash, &self.tr);
                let lv = &self.trackers_list;
                lv.set_redraw(false);
                while lv.len() > rows.len() {
                    lv.remove_item(lv.len() - 1);
                }
                while lv.len() < rows.len() {
                    let idx = lv.len() as i32;
                    lv.insert_item(nwg::InsertListViewItem {
                        index: Some(idx),
                        column_index: 0,
                        text: Some(String::new()),
                        image: None,
                    });
                }
                let dash = || "-".to_string();
                // Localized "N/A" for the DHT/LSD/PeX rows, whose per-source
                // counts librqbit genuinely can't provide (not merely unknown).
                let na = self.tr.i18n("not_applicable");
                for (i, t) in rows.iter().enumerate() {
                    // Tracker rows show real (or "-") counts; DHT/LSD/PeX source
                    // rows show "N/A" (not applicable); tier headers leave the
                    // stat columns blank.
                    let (seeds, leeches, fails, next) = match t.kind {
                        TrackerRowKind::Tracker => (
                            t.seeders.map(|n| n.to_string()).unwrap_or_else(dash),
                            t.leechers.map(|n| n.to_string()).unwrap_or_else(dash),
                            if t.fails > 0 {
                                t.fails.to_string()
                            } else {
                                dash()
                            },
                            format_next_announce(t.next_announce, &self.tr),
                        ),
                        TrackerRowKind::Source => {
                            (na.clone(), na.clone(), na.clone(), na.clone())
                        }
                        TrackerRowKind::Tier => (
                            String::new(),
                            String::new(),
                            String::new(),
                            String::new(),
                        ),
                    };
                    let cells = [
                        t.label.clone(),
                        t.status.clone(),
                        seeds,
                        leeches,
                        fails,
                        next,
                    ];
                    for (col, text) in cells.into_iter().enumerate() {
                        lv.update_item(
                            i,
                            nwg::InsertListViewItem {
                                index: Some(i as i32),
                                column_index: col as i32,
                                text: Some(text),
                                image: None,
                            },
                        );
                    }
                }
                lv.set_redraw(true);
            }
            _ => {}
        }
    }

    // layout

    fn on_resize(&self) {
        self.layout();
        self.set_status_parts();
    }

    // splitter between list and details panel

    /// Cursor position in the window's LOGICAL client coordinates (layout
    /// math runs in logical units; the cursor is physical).
    fn cursor_client_y(&self) -> Option<i32> {
        let hwnd = self.window.handle.hwnd()?;

        unsafe {
            let mut pt: winapi::shared::windef::POINT = std::mem::zeroed();
            if winapi::um::winuser::GetCursorPos(&mut pt) == 0 {
                return None;
            }
            if winapi::um::winuser::ScreenToClient(hwnd, &mut pt) == 0 {
                return None;
            }

            // physical -> logical using the ratio of the two client sizes
            let mut rc: winapi::shared::windef::RECT = std::mem::zeroed();
            winapi::um::winuser::GetClientRect(hwnd, &mut rc);
            let (_, logical_h) = self.window.size();
            if rc.bottom <= 0 || logical_h == 0 {
                return Some(pt.y);
            }

            Some((pt.y as i64 * logical_h as i64 / rc.bottom as i64) as i32)
        }
    }

    /// The splitter strip's logical y-range, when visible.
    fn splitter_range(&self) -> Option<(i32, i32)> {
        if !self.show_details.get() {
            return None;
        }

        let (_, h) = self.window.size();
        let h = h as i32;
        let status_h = if self.show_status.get() { STATUS_HEIGHT } else { 0 };
        let console_h = if self.show_console.get() { CONSOLE_HEIGHT } else { 0 };
        let max_details =
            (h - status_h - console_h - SPLITTER_HEIGHT - LIST_MIN).max(DETAILS_MIN);
        let details_h = self.details_height.get().clamp(DETAILS_MIN, max_details);
        let list_h = (h - status_h - console_h - details_h - SPLITTER_HEIGHT).max(60);

        // A little extra grab area on both sides.
        Some((console_h + list_h - 2, console_h + list_h + SPLITTER_HEIGHT + 2))
    }

    fn set_splitter_cursor(&self) {
        unsafe {
            winapi::um::winuser::SetCursor(winapi::um::winuser::LoadCursorW(
                std::ptr::null_mut(),
                winapi::um::winuser::IDC_SIZENS,
            ));
        }
    }

    fn on_window_mouse_move(&self) {
        if self.splitter_dragging.get() {
            self.set_splitter_cursor();

            let Some(y) = self.cursor_client_y() else {
                return;
            };

            let (_, h) = self.window.size();
            let h = h as i32;
            let status_h = if self.show_status.get() { STATUS_HEIGHT } else { 0 };
            let console_h = if self.show_console.get() { CONSOLE_HEIGHT } else { 0 };
            let max_details =
                (h - status_h - console_h - SPLITTER_HEIGHT - LIST_MIN).max(DETAILS_MIN);
            let details_h =
                (h - status_h - SPLITTER_HEIGHT - y).clamp(DETAILS_MIN, max_details);

            if details_h != self.details_height.get() {
                self.details_height.set(details_h);
                self.layout();
            }
        } else if let (Some(y), Some((top, bottom))) =
            (self.cursor_client_y(), self.splitter_range())
            && y >= top
            && y <= bottom
        {
            self.set_splitter_cursor();
        }
    }

    fn on_window_mouse_down(&self) {
        let (Some(y), Some((top, bottom))) = (self.cursor_client_y(), self.splitter_range())
        else {
            return;
        };

        if y >= top && y <= bottom {
            self.splitter_dragging.set(true);
            self.set_splitter_cursor();
            if let Some(hwnd) = self.window.handle.hwnd() {
                unsafe {
                    winapi::um::winuser::SetCapture(hwnd);
                }
            }
        }
    }

    fn on_window_mouse_up(&self) {
        if self.splitter_dragging.get() {
            self.splitter_dragging.set(false);
            unsafe {
                winapi::um::winuser::ReleaseCapture();
            }
            self.cfg.set_persistent(
                "ui.splitter.details_height",
                &self.details_height.get().to_string(),
            );
        }
    }

    /// Lay out the overview tab like PicoTorrent: the pieces bar spans the full
    /// content width at the top, with a two-column data grid below it. Called
    /// from layout() so it tracks the window width.
    fn layout_overview(&self, content_w: i32) {
        let margin = 12;

        // Pieces bar across the top, under its bold caption.
        self.piece_caption.set_position(margin, 8);
        self.piece_bar.set_position(margin, 32);
        self.piece_bar.set_size(content_w as u32, 18);

        // Two-column data grid filling the width, below the bar. The white
        // "up/bottom border" strips NWG's Label paints around v-centered text
        // are overpainted with the theme background by field_bg_subclass
        // (install_field_bg below), so the height here is just normal spacing.
        let cap_w = 110;
        let gap = 6;
        let col_w = content_w / 2;
        let val_w = (col_w - cap_w - gap - 12).max(60);
        for (i, (caption, value)) in self.overview_fields.iter().enumerate() {
            let col = (i % 2) as i32;
            let row = (i / 2) as i32;
            let x = margin + col * col_w;
            let y = 64 + row * 28;
            caption.set_position(x, y);
            value.set_position(x + cap_w + gap, y);
            value.set_size(val_w as u32, 22);
        }
    }

    fn layout(&self) {
        let (w, h) = self.window.size();
        let w = w as i32;
        let h = h as i32;

        tracing::debug!(
            "layout: client={}x{} details={} status={}",
            w,
            h,
            self.show_details.get(),
            self.show_status.get()
        );

        let status_h = if self.show_status.get() { STATUS_HEIGHT } else { 0 };
        let console_h = if self.show_console.get() { CONSOLE_HEIGHT } else { 0 };

        // Clamp the splitter position to the window.
        let (details_h, splitter_h) = if self.show_details.get() {
            let max_details =
                (h - status_h - console_h - SPLITTER_HEIGHT - LIST_MIN).max(DETAILS_MIN);
            (
                self.details_height.get().clamp(DETAILS_MIN, max_details),
                SPLITTER_HEIGHT,
            )
        } else {
            (0, 0)
        };

        let list_h = (h - status_h - console_h - details_h - splitter_h).max(60);

        if self.show_console.get() {
            // Standard input height, centered in the strip - a taller
            // single-line edit paints white bands outside its text rect.
            let y = (CONSOLE_HEIGHT - CONSOLE_INPUT_HEIGHT) / 2;
            self.console_icon.set_position(8, y);
            self.console_icon.set_size(24, CONSOLE_INPUT_HEIGHT as u32);
            self.console_input.set_position(CONSOLE_INPUT_X, y);
            self.console_input.set_size(
                (w - CONSOLE_INPUT_X - 8).max(100) as u32,
                CONSOLE_INPUT_HEIGHT as u32,
            );
        }

        self.list.set_position(0, console_h);
        self.list.set_size(w as u32, list_h as u32);

        // NWG switches the list view to report mode after creating the
        // control, which leaves the column header with a zero height until
        // the list view re-lays itself out - send it an explicit WM_SIZE
        // with its client size.
        if let Some(hwnd) = self.list.handle.hwnd() {
            use winapi::um::winuser::{GetClientRect, SendMessageW, WM_SIZE};
            unsafe {
                let mut rc: winapi::shared::windef::RECT = std::mem::zeroed();
                GetClientRect(hwnd, &mut rc);
                SendMessageW(
                    hwnd,
                    WM_SIZE,
                    0,
                    ((rc.bottom as isize) << 16) | (rc.right as isize & 0xFFFF),
                );
            }
        }

        self.tabs.set_visible(self.show_details.get());
        if self.show_details.get() {
            self.tabs.set_position(0, console_h + list_h + splitter_h);
            self.tabs.set_size(w as u32, details_h as u32);

            let inner_w = (w - 20).max(100) as u32;
            let inner_h = (details_h - 48).max(60) as u32;
            self.files_list.set_position(2, 2);
            self.files_list.set_size(inner_w, inner_h);
            self.peers_list.set_position(2, 2);
            self.peers_list.set_size(inner_w, inner_h);
            self.trackers_list.set_position(2, 2);
            self.trackers_list.set_size(inner_w, inner_h);

            // Overview: pieces bar across the top + data grid, filling the width.
            self.layout_overview((w - 28).max(200));
        }

        self.set_status_parts();
    }

    /// NWG's StatusBar doesn't manage parts, so send SB_SETPARTS directly -
    /// four equal fields, like the original statusbar.cpp.
    fn set_status_parts(&self) {
        use winapi::um::commctrl::SB_SETPARTS;
        use winapi::um::winuser::SendMessageW;

        if !self.show_status.get() {
            return;
        }

        let Some(hwnd) = self.status.handle.hwnd() else {
            return;
        };

        // SB_SETPARTS wants physical pixels; window.size() is logical when
        // the high-dpi feature is active, so read the client rect directly.
        let w = unsafe {
            let mut rc: winapi::shared::windef::RECT = std::mem::zeroed();
            if let Some(win_hwnd) = self.window.handle.hwnd() {
                winapi::um::winuser::GetClientRect(win_hwnd, &mut rc);
            }
            rc.right
        };
        let parts: [i32; 4] = [w / 4, w / 2, w * 3 / 4, -1];

        unsafe {
            SendMessageW(
                hwnd,
                SB_SETPARTS,
                parts.len(),
                parts.as_ptr() as isize,
            );
        }

        self.update_status_bar();
    }
}

/// Format a tracker's next-announce time as a short countdown ("2m 05s"),
/// "now" if already due, or "-" if unknown.
fn format_next_announce(t: Option<std::time::SystemTime>, tr: &Translator) -> String {
    let Some(t) = t else {
        return "-".to_string();
    };
    match t.duration_since(std::time::SystemTime::now()) {
        Ok(d) => {
            let secs = d.as_secs();
            if secs >= 60 {
                format!("{}m {:02}s", secs / 60, secs % 60)
            } else {
                format!("{secs}s")
            }
        }
        Err(_) => tr.i18n("announce_now"),
    }
}

/// Whether a save path is below the free-space threshold. `limit_percent` is
/// the percentage of the volume that must remain free (PicoTorrent semantics):
/// pause when the free fraction drops under it.
fn low_disk_should_pause(free: u64, total: u64, limit_percent: i64) -> bool {
    if total == 0 || limit_percent <= 0 {
        return false;
    }
    (free as f64 / total as f64) < (limit_percent as f64 / 100.0)
}

/// Free and total bytes for the volume containing `path`, via
/// GetDiskFreeSpaceExW. Returns None if the query fails (e.g. path not yet on
/// disk), so the caller simply doesn't pause.
#[cfg(windows)]
fn disk_free_total(path: &str) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use winapi::um::fileapi::GetDiskFreeSpaceExW;
    use winapi::um::winnt::PULARGE_INTEGER;

    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut free_avail: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_avail as *mut u64 as PULARGE_INTEGER,
            &mut total as *mut u64 as PULARGE_INTEGER,
            &mut total_free as *mut u64 as PULARGE_INTEGER,
        )
    };
    if ok != 0 && total > 0 {
        Some((free_avail, total))
    } else {
        None
    }
}

#[cfg(not(windows))]
fn disk_free_total(_path: &str) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::low_disk_should_pause;

    #[test]
    fn low_disk_threshold() {
        let total = 1000u64;
        // 5% limit: pause under 50 bytes free.
        assert!(low_disk_should_pause(49, total, 5));
        assert!(!low_disk_should_pause(50, total, 5)); // exactly at limit is fine
        assert!(!low_disk_should_pause(500, total, 5));
        // Degenerate inputs never pause.
        assert!(!low_disk_should_pause(0, 0, 5));
        assert!(!low_disk_should_pause(0, total, 0));
    }
}
