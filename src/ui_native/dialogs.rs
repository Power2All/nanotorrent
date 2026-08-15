// Native Win32 dialogs - ports of ui/dialogs/*.cpp using native-windows-gui.
//
// Dialogs run on their own thread with their own event loop (the pattern
// from NWG's official multithreaded-dialog example) and report back to the
// main window through a NoticeSender.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

use librqbit::ByteBufOwned;
use librqbit::torrent_from_bytes;

use crate::bittorrent::session::{AddParams, CreateTorrentParams};
use crate::core::configuration::{Configuration, Label};
use crate::core::utils;
use crate::ui::translator::Translator;

extern crate native_windows_gui as nwg;

/// Result of a dialog, handed back to the main window.
pub enum DialogResult {
    Magnets(Vec<String>),
    AddTorrent { bytes: Vec<u8>, params: AddParams },
    CreateTorrent(CreateTorrentParams),
    PreferencesSaved,
    /// Saved, and the UI language changed - the caller offers a restart.
    PreferencesSavedLanguageChanged,
    /// The close prompt: exit the app or just minimize to tray, and whether to
    /// remember the choice.
    CloseChoice { exit: bool, remember: bool },
    Cancelled,
}

/// Dialog threads push their result through an mpsc channel *before*
/// pinging the Notice, so the main thread can never observe the
/// notification without the result being available (a JoinHandle-based
/// handoff races: the notice fires before the thread counts as finished).
pub type DialogHandle = JoinHandle<()>;

/// Disables the owner (main) window for the dialog's lifetime - true modal
/// behavior. Re-enables on drop, so the main window comes back even if the
/// dialog thread panics.
struct ModalGuard {
    parent: usize,
}

impl ModalGuard {
    fn new(parent: usize, dialog: Option<winapi::shared::windef::HWND>) -> ModalGuard {
        use winapi::um::winuser::{EnableWindow, GWLP_HWNDPARENT, SetWindowLongPtrW};

        if parent != 0 {
            unsafe {
                if let Some(dialog) = dialog {
                    // Owner relationship keeps the dialog above the main
                    // window while it is disabled.
                    SetWindowLongPtrW(dialog, GWLP_HWNDPARENT, parent as isize);
                }
                EnableWindow(parent as _, 0);
            }
        }

        ModalGuard { parent }
    }
}

impl Drop for ModalGuard {
    fn drop(&mut self) {
        use winapi::um::winuser::{EnableWindow, SetForegroundWindow};

        if self.parent != 0 {
            unsafe {
                // Re-enable BEFORE the dialog window is destroyed, otherwise
                // Windows deactivates the whole application.
                EnableWindow(self.parent as _, 1);
                SetForegroundWindow(self.parent as _);
            }
        }
    }
}

/// Explicit label background matching the theme. NWG labels center their
/// text by shrinking the client area and paint the leftover non-client
/// strips with COLOR_WINDOW (white) unless given an explicit background.
fn label_bg() -> [u8; 3] {
    if super::darkmode::is_enabled() {
        super::darkmode::BG
    } else {
        [255, 255, 255]
    }
}

/// The app icon, for dialog title bars / taskbar. Built per-thread (NWG
/// resources are thread-local); the caller keeps it alive for the dialog's
/// lifetime so the HICON isn't destroyed while the window is up.
fn app_icon() -> Option<nwg::Icon> {
    let mut icon = nwg::Icon::default();
    nwg::Icon::builder()
        .source_bin(Some(super::mainwindow::APP_ICON))
        .build(&mut icon)
        .ok()?;
    Some(icon)
}

/// Bold font for field captions, matching the main window's overview labels.
fn caption_font() -> nwg::Font {
    let mut font = nwg::Font::default();
    let _ = nwg::Font::builder()
        .family("Segoe UI")
        .size(16)
        .weight(700)
        .build(&mut font);
    font
}

fn run_dialog_window(window: &nwg::Window, parent: usize) {
    let hwnd = window.handle.hwnd();

    // Theme the dialog content (dark title bar, backgrounds, control colors).
    if let Some(hwnd) = hwnd {
        super::darkmode::prepare_dialog(hwnd);
    }

    let _modal = ModalGuard::new(parent, hwnd);

    window.set_visible(true);
    window.set_focus();

    // Give the dialog the app icon, after it's visible so the title-bar icon
    // reliably updates (setting it while hidden can be dropped for owned
    // windows). NWG's `.icon()` builder only sets ICON_SMALL; set ICON_BIG too
    // so Alt-Tab / large views match. The Icon is kept alive until dispatch
    // returns (the dialog closes) so the HICON isn't destroyed while in use.
    let icon = app_icon();
    if let (Some(hwnd), Some(icon)) = (hwnd, icon.as_ref()) {
        use winapi::um::winuser::{ICON_BIG, ICON_SMALL, SendMessageW, WM_SETICON};
        let h = icon.handle as isize;
        unsafe {
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, h);
            SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, h);
        }
    }

    nwg::dispatch_thread_events();
}

// About - replaces the stock MessageBox (which ignores dark mode)

pub fn spawn_about(
    tr: Translator,
    parent: usize,
    tx: Sender<DialogResult>,
    notice: nwg::NoticeSender,
) -> DialogHandle {
    std::thread::spawn(move || {
        let mut heading_font = nwg::Font::default();
        let _ = nwg::Font::builder()
            .family("Segoe UI")
            .size(32)
            .weight(700)
            .build(&mut heading_font);

        let mut window = nwg::Window::default();
        let mut heading = nwg::Label::default();
        let mut version_label = nwg::Label::default();
        let mut tagline = nwg::Label::default();
        let mut link = nwg::Label::default();
        let mut credits = nwg::Label::default();
        let mut developed = nwg::Label::default();
        let mut engine = nwg::Label::default();
        let mut ok_btn = nwg::Button::default();

        nwg::Window::builder()
            .title(&tr.i18n1("about_picotorrent", "NanoTorrent"))
            .size((460, 350))
            .center(true)
            // Fixed-size dialog, built hidden (run_dialog_window themes it
            // before showing).
            .flags(nwg::WindowFlags::WINDOW)
            .build(&mut window)
            .expect("about window");

        let label = |text: &str, y: i32, h: i32, out: &mut nwg::Label| {
            nwg::Label::builder()
                .background_color(Some(label_bg()))
                .parent(&window)
                .text(text)
                .position((24, y))
                .size((412, h))
                .build(out)
                .unwrap();
        };

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            .text("NanoTorrent")
            .position((24, 18))
            .size((412, 40))
            .font(Some(&heading_font))
            .build(&mut heading)
            .unwrap();

        label(
            &format!(
                "Version {} (build {})",
                crate::buildinfo::version(),
                crate::buildinfo::build_stamp()
            ),
            64,
            22,
            &mut version_label,
        );
        label("A tiny, hackable BitTorrent client.", 94, 22, &mut tagline);
        label("https://www.nanotorrent.org", 124, 22, &mut link);
        label(
            "Based on PicoTorrent © Viktor Elofsson and contributors.",
            162,
            22,
            &mut credits,
        );
        label("Developed by Power2All in Rust.", 192, 22, &mut developed);
        label(
            &format!("BitTorrent engine: librqbit {}", librqbit::version()),
            222,
            22,
            &mut engine,
        );

        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("ok"))
            .position((346, 264))
            .size((90, 30))
            .build(&mut ok_btn)
            .unwrap();

        let window_handle = window.handle;
        let ok_handle = ok_btn.handle;
        let link_handle = link.handle;

        let handler = nwg::full_bind_event_handler(&window_handle, move |evt, _data, handle| {
            match evt {
                nwg::Event::OnButtonClick if handle == ok_handle => {
                    nwg::stop_thread_dispatch();
                }
                nwg::Event::OnLabelClick if handle == link_handle => {
                    let _ = open::that("https://www.nanotorrent.org");
                }
                nwg::Event::OnWindowClose if handle == window_handle => {
                    nwg::stop_thread_dispatch();
                }
                _ => {}
            }
        });

        run_dialog_window(&window, parent);
        nwg::unbind_event_handler(&handler);

        let _ = tx.send(DialogResult::Cancelled);
        notice.notice();
    })
}

// Add magnet link(s) - port of addmagnetlinkdialog.cpp

pub fn parse_magnet_links(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            line.starts_with("magnet:")
                || line.len() == 40 && line.chars().all(|c| c.is_ascii_hexdigit())
        })
        .map(|line| {
            if line.starts_with("magnet:") {
                line.to_string()
            } else {
                format!("magnet:?xt=urn:btih:{line}")
            }
        })
        .collect()
}

pub fn spawn_add_magnet(
    tr: Translator,
    parent: usize,
    tx: Sender<DialogResult>,
    notice: nwg::NoticeSender,
) -> DialogHandle {
    std::thread::spawn(move || {
        let result = Rc::new(RefCell::new(DialogResult::Cancelled));

        let mut window = nwg::Window::default();
        let mut text_box = nwg::TextBox::default();
        let mut add_btn = nwg::Button::default();
        let mut cancel_btn = nwg::Button::default();

        nwg::Window::builder()
            .title(&tr.i18n("add_magnet_link_s"))
            .size((500, 320))
            .center(true)
            // Built hidden: run_dialog_window themes the content first, then
            // shows it (the default WS_VISIBLE would flash white in dark mode).
            .flags(nwg::WindowFlags::MAIN_WINDOW)
            .build(&mut window)
            .expect("dialog window");

        nwg::TextBox::builder()
            .parent(&window)
            .position((10, 10))
            .size((480, 240))
            // Word wrap, auto scroll, no always-visible scrollbars (the
            // default adds permanent scrollbars - a legacy look).
            .flags(
                nwg::TextBoxFlags::VISIBLE
                    | nwg::TextBoxFlags::TAB_STOP
                    | nwg::TextBoxFlags::AUTOVSCROLL,
            )
            .build(&mut text_box)
            .expect("text box");

        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("add"))
            .position((300, 255))
            .size((90, 28))
            .build(&mut add_btn)
            .expect("add button");

        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("cancel"))
            .position((398, 255))
            .size((90, 28))
            .build(&mut cancel_btn)
            .expect("cancel button");

        // Pre-fill from the clipboard if it already holds a magnet link (or a
        // bare info-hash), so pasting is one less step.
        if let Some(clip) = nwg::Clipboard::data_text(&window) {
            let magnets = parse_magnet_links(&clip);
            if !magnets.is_empty() {
                text_box.set_text(&magnets.join("\r\n"));
            }
        }

        let window_handle = window.handle;
        let result_ref = result.clone();
        let text_ref = text_box.handle;

        let text_box = Rc::new(text_box);
        let text_box_ref = text_box.clone();
        let add_handle = add_btn.handle;
        let cancel_handle = cancel_btn.handle;

        let _ = text_ref;

        let handler = nwg::full_bind_event_handler(&window_handle, move |evt, _data, handle| {
            match evt {
                nwg::Event::OnButtonClick if handle == add_handle => {
                    let links = parse_magnet_links(&text_box_ref.text());
                    if !links.is_empty() {
                        *result_ref.borrow_mut() = DialogResult::Magnets(links);
                    }
                    nwg::stop_thread_dispatch();
                }
                nwg::Event::OnButtonClick if handle == cancel_handle => {
                    nwg::stop_thread_dispatch();
                }
                nwg::Event::OnWindowClose if handle == window_handle => {
                    nwg::stop_thread_dispatch();
                }
                _ => {}
            }
        });

        run_dialog_window(&window, parent);
        nwg::unbind_event_handler(&handler);

        let result = Rc::try_unwrap(result)
            .map(RefCell::into_inner)
            .unwrap_or(DialogResult::Cancelled);
        let _ = tx.send(result);
        notice.notice();
    })
}

// Close prompt - a themed dialog (so it can host a "remember" checkbox, unlike
// a stock message box) asking whether to exit or keep running in the tray.

pub fn spawn_close_prompt(
    tr: Translator,
    parent: usize,
    tx: Sender<DialogResult>,
    notice: nwg::NoticeSender,
) -> DialogHandle {
    std::thread::spawn(move || {
        let result = Rc::new(RefCell::new(DialogResult::Cancelled));

        let mut window = nwg::Window::default();
        let mut question = nwg::Label::default();
        let mut remember_check = nwg::CheckBox::default();
        let mut exit_btn = nwg::Button::default();
        let mut min_btn = nwg::Button::default();
        let mut cancel_btn = nwg::Button::default();

        nwg::Window::builder()
            .title("NanoTorrent")
            // 240, not 210: the buttons were made taller (48) to let long
            // translations wrap, and at 210 the row ended at y=198 - past the
            // ~177px client area left after the title bar, so the prompt came
            // up with nothing usable in it.
            .size((440, 240))
            .center(true)
            .flags(nwg::WindowFlags::WINDOW)
            .build(&mut window)
            .expect("close prompt window");

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            .text(&tr.i18n("close_prompt_body"))
            .position((16, 16))
            .size((408, 70))
            .build(&mut question)
            .unwrap();

        nwg::CheckBox::builder()
            .parent(&window)
            .text(&tr.i18n("close_remember"))
            .position((16, 96))
            .size((408, 22))
            .build(&mut remember_check)
            .unwrap();

        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("close_exit"))
            .position((16, 150))
            .size((110, 48))
            .build(&mut exit_btn)
            .unwrap();
        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("close_minimize"))
            .position((134, 150))
            .size((150, 48))
            .build(&mut min_btn)
            .unwrap();
        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("cancel"))
            .position((320, 150))
            .size((104, 48))
            .build(&mut cancel_btn)
            .unwrap();
        // Long translations wrap instead of clipping; widths are unchanged so
        // the row keeps its spacing.
        for b in [&exit_btn, &min_btn, &cancel_btn] {
            super::set_button_multiline(b);
        }

        let window_handle = window.handle;
        let result_ref = result.clone();
        let remember_check = Rc::new(remember_check);
        let remember_ref = remember_check.clone();
        let exit_handle = exit_btn.handle;
        let min_handle = min_btn.handle;
        let cancel_handle = cancel_btn.handle;

        let handler = nwg::full_bind_event_handler(&window_handle, move |evt, _data, handle| {
            let remember = remember_ref.check_state() == nwg::CheckBoxState::Checked;
            match evt {
                nwg::Event::OnButtonClick if handle == exit_handle => {
                    *result_ref.borrow_mut() = DialogResult::CloseChoice {
                        exit: true,
                        remember,
                    };
                    nwg::stop_thread_dispatch();
                }
                nwg::Event::OnButtonClick if handle == min_handle => {
                    *result_ref.borrow_mut() = DialogResult::CloseChoice {
                        exit: false,
                        remember,
                    };
                    // Hide the main window here, on this thread, before the modal
                    // guard re-foregrounds it and before this dialog window is
                    // destroyed - both would otherwise re-activate (and re-show)
                    // the main window, racing the main thread's hide.
                    if parent != 0 {
                        unsafe {
                            winapi::um::winuser::ShowWindow(
                                parent as winapi::shared::windef::HWND,
                                winapi::um::winuser::SW_HIDE,
                            );
                        }
                    }
                    nwg::stop_thread_dispatch();
                }
                nwg::Event::OnButtonClick if handle == cancel_handle => {
                    nwg::stop_thread_dispatch();
                }
                nwg::Event::OnWindowClose if handle == window_handle => {
                    nwg::stop_thread_dispatch();
                }
                _ => {}
            }
        });

        run_dialog_window(&window, parent);
        nwg::unbind_event_handler(&handler);

        let result = Rc::try_unwrap(result)
            .map(RefCell::into_inner)
            .unwrap_or(DialogResult::Cancelled);
        let _ = tx.send(result);
        notice.notice();
    })
}

// Create torrent - port of createtorrentdialog.cpp

pub fn spawn_create_torrent(
    tr: Translator,
    parent: usize,
    tx: Sender<DialogResult>,
    notice: nwg::NoticeSender,
) -> DialogHandle {
    std::thread::spawn(move || {
        let result = Rc::new(RefCell::new(DialogResult::Cancelled));

        let bold = caption_font();

        let mut window = nwg::Window::default();
        let mut source_caption = nwg::Label::default();
        let mut source_input = nwg::TextInput::default();
        let mut file_btn = nwg::Button::default();
        let mut dir_btn = nwg::Button::default();
        let mut trackers_caption = nwg::Label::default();
        let mut trackers_box = nwg::TextBox::default();
        let mut comment_caption = nwg::Label::default();
        let mut comment_input = nwg::TextInput::default();
        let mut piece_caption = nwg::Label::default();
        let mut piece_combo: nwg::ComboBox<String> = nwg::ComboBox::default();
        let mut version_caption = nwg::Label::default();
        let mut version_combo: nwg::ComboBox<String> = nwg::ComboBox::default();
        let mut private_check = nwg::CheckBox::default();
        let mut add_check = nwg::CheckBox::default();
        let mut create_btn = nwg::Button::default();
        let mut cancel_btn = nwg::Button::default();

        nwg::Window::builder()
            .title(&tr.i18n("create_torrent"))
            .size((560, 520))
            .center(true)
            .flags(nwg::WindowFlags::WINDOW)
            .build(&mut window)
            .expect("create torrent window");

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            .text(&tr.i18n("status_select_file_or_directory"))
            .position((10, 12))
            .size((300, 20))
            .font(Some(&bold))
            .build(&mut source_caption)
            .unwrap();
        nwg::TextInput::builder()
            .parent(&window)
            .position((10, 36))
            .size((290, 24))
            .build(&mut source_input)
            .unwrap();
        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("select_file"))
            .position((306, 35))
            .size((110, 26))
            .build(&mut file_btn)
            .unwrap();
        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("select_directory"))
            .position((422, 35))
            .size((128, 26))
            .build(&mut dir_btn)
            .unwrap();

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            .text(&tr.i18n("trackers_input_per_line"))
            .position((10, 74))
            .size((300, 20))
            .font(Some(&bold))
            .build(&mut trackers_caption)
            .unwrap();
        nwg::TextBox::builder()
            .parent(&window)
            .position((10, 98))
            .size((540, 120))
            .flags(
                nwg::TextBoxFlags::VISIBLE
                    | nwg::TextBoxFlags::TAB_STOP
                    | nwg::TextBoxFlags::AUTOVSCROLL,
            )
            .build(&mut trackers_box)
            .unwrap();

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            .text(&tr.i18n("comment"))
            .position((10, 232))
            .size((150, 20))
            .font(Some(&bold))
            .build(&mut comment_caption)
            .unwrap();
        nwg::TextInput::builder()
            .parent(&window)
            .position((10, 256))
            .size((540, 24))
            .build(&mut comment_input)
            .unwrap();

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            .text(&tr.i18n("piece_size"))
            .position((10, 294))
            .size((150, 20))
            .font(Some(&bold))
            .build(&mut piece_caption)
            .unwrap();
        let piece_sizes: [(&str, Option<u32>); 6] = [
            ("Auto", None),
            ("256 KB", Some(256 * 1024)),
            ("512 KB", Some(512 * 1024)),
            ("1 MB", Some(1024 * 1024)),
            ("2 MB", Some(2 * 1024 * 1024)),
            ("4 MB", Some(4 * 1024 * 1024)),
        ];
        nwg::ComboBox::builder()
            .parent(&window)
            .collection(piece_sizes.iter().map(|(n, _)| n.to_string()).collect())
            .selected_index(Some(0))
            .position((170, 292))
            .size((160, 24))
            .build(&mut piece_combo)
            .unwrap();

        // Torrent format. v1 = classic (SHA-1); v2 = BEP 52 (SHA-256 merkle,
        // v2-only clients); hybrid = both, widest compatibility.
        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            .text(&tr.i18n("torrent_version"))
            .position((345, 294))
            .size((60, 20))
            .font(Some(&bold))
            .build(&mut version_caption)
            .unwrap();
        nwg::ComboBox::builder()
            .parent(&window)
            .collection(vec![
                String::from("BitTorrent v1"),
                String::from("BitTorrent v2"),
                String::from("Hybrid (v1+v2)"),
            ])
            .selected_index(Some(0))
            .position((410, 292))
            .size((140, 24))
            .build(&mut version_combo)
            .unwrap();

        nwg::CheckBox::builder()
            .parent(&window)
            .text(&tr.i18n("private"))
            .position((10, 330))
            .size((250, 22))
            .build(&mut private_check)
            .unwrap();
        nwg::CheckBox::builder()
            .parent(&window)
            .text(&tr.i18n("add_to_session"))
            .check_state(nwg::CheckBoxState::Checked)
            .position((10, 358))
            .size((250, 22))
            .build(&mut add_check)
            .unwrap();

        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("create_torrent"))
            .position((320, 430))
            .size((130, 30))
            .build(&mut create_btn)
            .unwrap();
        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("cancel"))
            .position((460, 430))
            .size((90, 30))
            .build(&mut cancel_btn)
            .unwrap();

        let window_handle = window.handle;
        let file_handle = file_btn.handle;
        let dir_handle = dir_btn.handle;
        let create_handle = create_btn.handle;
        let cancel_handle = cancel_btn.handle;

        let source_input = Rc::new(source_input);
        let trackers_box = Rc::new(trackers_box);
        let comment_input = Rc::new(comment_input);
        let piece_combo = Rc::new(piece_combo);
        let version_combo = Rc::new(version_combo);
        let private_check = Rc::new(private_check);
        let add_check = Rc::new(add_check);

        let result_ref = result.clone();
        let source_ref = source_input.clone();
        let trackers_ref = trackers_box.clone();
        let comment_ref = comment_input.clone();
        let piece_ref = piece_combo.clone();
        let version_ref = version_combo.clone();
        let private_ref = private_check.clone();
        let add_ref = add_check.clone();
        let tr2 = tr.clone();

        let handler = nwg::full_bind_event_handler(&window_handle, move |evt, _data, handle| {
            match evt {
                nwg::Event::OnButtonClick if handle == file_handle => {
                    let mut dialog = nwg::FileDialog::default();
                    if nwg::FileDialog::builder()
                        .title(tr2.i18n("select_file"))
                        .action(nwg::FileDialogAction::Open)
                        .build(&mut dialog)
                        .is_ok()
                        && dialog.run::<nwg::ControlHandle>(None)
                        && let Ok(path) = dialog.get_selected_item()
                    {
                        source_ref.set_text(&path.to_string_lossy());
                    }
                }
                nwg::Event::OnButtonClick if handle == dir_handle => {
                    let mut dialog = nwg::FileDialog::default();
                    if nwg::FileDialog::builder()
                        .title(tr2.i18n("select_directory"))
                        .action(nwg::FileDialogAction::OpenDirectory)
                        .build(&mut dialog)
                        .is_ok()
                        && dialog.run::<nwg::ControlHandle>(None)
                        && let Ok(path) = dialog.get_selected_item()
                    {
                        source_ref.set_text(&path.to_string_lossy());
                    }
                }
                nwg::Event::OnButtonClick if handle == create_handle => {
                    let source = std::path::PathBuf::from(source_ref.text().trim());
                    if source.as_os_str().is_empty() || !source.exists() {
                        nwg::error_message(
                            &tr2.i18n("error"),
                            &tr2.i18n("no_such_file_or_directory"),
                        );
                        return;
                    }

                    // Where to write the .torrent file.
                    let mut dialog = nwg::FileDialog::default();
                    let picked = nwg::FileDialog::builder()
                        .title(tr2.i18n("create_torrent"))
                        .action(nwg::FileDialogAction::Save)
                        .filters("Torrent(*.torrent)|Any(*.*)")
                        .build(&mut dialog)
                        .is_ok()
                        && dialog.run::<nwg::ControlHandle>(None);
                    if !picked {
                        return;
                    }
                    let Ok(mut output) = dialog.get_selected_item() else {
                        return;
                    };
                    if !output.to_string_lossy().to_lowercase().ends_with(".torrent") {
                        // OsString::push appends to the string (not a path
                        // component).
                        output.push(".torrent");
                    }

                    let trackers: Vec<String> = trackers_ref
                        .text()
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(str::to_string)
                        .collect();

                    let piece_length = piece_ref
                        .selection()
                        .and_then(|idx| piece_sizes.get(idx))
                        .and_then(|(_, len)| *len);

                    let version = crate::bittorrent::torrent_create::TorrentVersion::from_index(
                        version_ref.selection().unwrap_or(0),
                    );

                    *result_ref.borrow_mut() =
                        DialogResult::CreateTorrent(CreateTorrentParams {
                            source,
                            trackers,
                            comment: comment_ref.text().trim().to_string(),
                            private: private_ref.check_state()
                                == nwg::CheckBoxState::Checked,
                            piece_length,
                            version,
                            output: std::path::PathBuf::from(output),
                            add_to_session: add_ref.check_state()
                                == nwg::CheckBoxState::Checked,
                        });
                    nwg::stop_thread_dispatch();
                }
                nwg::Event::OnButtonClick if handle == cancel_handle => {
                    nwg::stop_thread_dispatch();
                }
                nwg::Event::OnWindowClose if handle == window_handle => {
                    nwg::stop_thread_dispatch();
                }
                _ => {}
            }
        });

        run_dialog_window(&window, parent);
        nwg::unbind_event_handler(&handler);

        let result = Rc::try_unwrap(result)
            .map(RefCell::into_inner)
            .unwrap_or(DialogResult::Cancelled);
        let _ = tx.send(result);
        notice.notice();
    })
}

// Add torrent - port of addtorrentdialog.cpp

struct ParsedTorrent {
    name: String,
    total_size: i64,
    files: Vec<(String, u64)>,
}

fn parse_torrent(bytes: &[u8]) -> Result<ParsedTorrent, String> {
    let torrent = torrent_from_bytes::<ByteBufOwned>(bytes)
        .map_err(|err| format!("Failed to parse torrent file: {err:#}"))?;

    let name = torrent
        .info
        .name
        .as_ref()
        .map(|b| String::from_utf8_lossy(b.as_ref()).into_owned())
        .unwrap_or_else(|| String::from("(unnamed torrent)"));

    let mut files = Vec::new();
    if let Ok(details) = torrent.info.iter_file_details() {
        for fd in details {
            files.push((
                fd.filename
                    .to_string()
                    .unwrap_or_else(|_| String::from("(invalid name)")),
                fd.len,
            ));
        }
    }

    let total_size = files.iter().map(|f| f.1 as i64).sum();

    Ok(ParsedTorrent {
        name,
        total_size,
        files,
    })
}

pub fn spawn_add_torrent(
    tr: Translator,
    bytes: Vec<u8>,
    default_save_path: String,
    labels: Vec<Label>,
    parent: usize,
    tx: Sender<DialogResult>,
    notice: nwg::NoticeSender,
) -> DialogHandle {
    std::thread::spawn(move || {
        let parsed = match parse_torrent(&bytes) {
            Ok(p) => p,
            Err(err) => {
                nwg::error_message(&tr.i18n("error"), &err);
                let _ = tx.send(DialogResult::Cancelled);
                notice.notice();
                return;
            }
        };

        let result = Rc::new(RefCell::new(DialogResult::Cancelled));
        // Which file indices are included (toggled by double click).
        let included = Rc::new(RefCell::new(vec![true; parsed.files.len()]));

        let bold = caption_font();

        let mut window = nwg::Window::default();
        let mut name_caption = nwg::Label::default();
        let mut name_label = nwg::Label::default();
        let mut size_caption = nwg::Label::default();
        let mut size_label = nwg::Label::default();
        let mut path_caption = nwg::Label::default();
        let mut path_input = nwg::TextInput::default();
        let mut browse_btn = nwg::Button::default();
        let mut label_caption = nwg::Label::default();
        let mut label_combo: nwg::ComboBox<String> = nwg::ComboBox::default();
        let mut start_check = nwg::CheckBox::default();
        let mut files_list = nwg::ListView::default();
        let mut add_btn = nwg::Button::default();
        let mut cancel_btn = nwg::Button::default();

        nwg::Window::builder()
            .title(&tr.i18n("add_torrent_s"))
            .size((560, 470))
            .center(true)
            // Built hidden: run_dialog_window themes the content first, then
            // shows it (the default WS_VISIBLE would flash white in dark mode).
            .flags(nwg::WindowFlags::MAIN_WINDOW)
            .build(&mut window)
            .expect("dialog window");

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            .text(&tr.i18n("name"))
            .position((10, 12))
            .size((100, 20))
            .font(Some(&bold))
            .build(&mut name_caption)
            .unwrap();
        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            .text(&parsed.name)
            .position((120, 12))
            .size((430, 20))
            .build(&mut name_label)
            .unwrap();

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            .text(&tr.i18n("size"))
            .position((10, 38))
            .size((100, 20))
            .font(Some(&bold))
            .build(&mut size_caption)
            .unwrap();
        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            .text(&utils::to_human_file_size(parsed.total_size))
            .position((120, 38))
            .size((430, 20))
            .build(&mut size_label)
            .unwrap();

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            .text(&tr.i18n("save_path"))
            .position((10, 64))
            .size((100, 20))
            .font(Some(&bold))
            .build(&mut path_caption)
            .unwrap();
        nwg::TextInput::builder()
            .parent(&window)
            .text(&default_save_path)
            .position((120, 62))
            .size((380, 24))
            .build(&mut path_input)
            .unwrap();
        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("browse"))
            .position((506, 61))
            .size((44, 26))
            .build(&mut browse_btn)
            .unwrap();

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            .text(&tr.i18n("label"))
            .position((10, 94))
            .size((100, 20))
            .font(Some(&bold))
            .build(&mut label_caption)
            .unwrap();

        let mut label_names: Vec<String> = vec![tr.i18n("none")];
        label_names.extend(labels.iter().map(|l| l.name.clone()));
        nwg::ComboBox::builder()
            .parent(&window)
            // Same as the language combo: the height is the dropped list's.
            // Labels are user-created so this list has no fixed upper bound.
            .collection(label_names)
            .selected_index(Some(0))
            .position((120, 92))
            .size((240, 24))
            .build(&mut label_combo)
            .unwrap();
        if let Some(hwnd) = label_combo.handle.hwnd() {
            super::set_dropdown_visible_items(hwnd, 12);
        }

        nwg::CheckBox::builder()
            .parent(&window)
            .text(&tr.i18n("start_torrent"))
            .check_state(nwg::CheckBoxState::Checked)
            .position((120, 122))
            .size((240, 22))
            .build(&mut start_check)
            .unwrap();

        nwg::ListView::builder()
            .parent(&window)
            .list_style(nwg::ListViewStyle::Detailed)
            .ex_flags(nwg::ListViewExFlags::FULL_ROW_SELECT)
            .position((10, 152))
            .size((540, 260))
            .build(&mut files_list)
            .unwrap();

        files_list.insert_column(nwg::InsertListViewColumn {
            index: Some(0),
            fmt: None,
            width: Some(340),
            text: Some(tr.i18n("name")),
        });
        files_list.insert_column(nwg::InsertListViewColumn {
            index: Some(1),
            fmt: None,
            width: Some(100),
            text: Some(tr.i18n("size")),
        });
        files_list.insert_column(nwg::InsertListViewColumn {
            index: Some(2),
            fmt: None,
            width: Some(90),
            text: Some(tr.i18n("include")),
        });
        files_list.set_headers_enabled(true);

        for (idx, (name, len)) in parsed.files.iter().enumerate() {
            files_list.insert_item(nwg::InsertListViewItem {
                index: Some(idx as i32),
                column_index: 0,
                text: Some(name.clone()),
                image: None,
            });
            files_list.update_item(
                idx,
                nwg::InsertListViewItem {
                    index: Some(idx as i32),
                    column_index: 1,
                    text: Some(utils::to_human_file_size(*len as i64)),
                    image: None,
                },
            );
            files_list.update_item(
                idx,
                nwg::InsertListViewItem {
                    index: Some(idx as i32),
                    column_index: 2,
                    text: Some(tr.i18n("yes")),
                    image: None,
                },
            );
        }

        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("add"))
            .position((360, 424))
            .size((90, 28))
            .build(&mut add_btn)
            .unwrap();
        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("cancel"))
            .position((458, 424))
            .size((90, 28))
            .build(&mut cancel_btn)
            .unwrap();

        let window_handle = window.handle;
        let add_handle = add_btn.handle;
        let cancel_handle = cancel_btn.handle;
        let browse_handle = browse_btn.handle;
        let files_handle = files_list.handle;

        let files_list = Rc::new(files_list);
        let path_input = Rc::new(path_input);
        let label_combo = Rc::new(label_combo);
        let start_check = Rc::new(start_check);

        let result_ref = result.clone();
        let included_ref = included.clone();
        let files_list_ref = files_list.clone();
        let path_input_ref = path_input.clone();
        let label_combo_ref = label_combo.clone();
        let start_check_ref = start_check.clone();
        let tr2 = tr.clone();
        let labels2 = labels.clone();
        let bytes = RefCell::new(bytes);
        let file_count = parsed.files.len();

        let handler = nwg::full_bind_event_handler(&window_handle, move |evt, data, handle| {
            match evt {
                nwg::Event::OnButtonClick if handle == browse_handle => {
                    let mut dialog = nwg::FileDialog::default();
                    if nwg::FileDialog::builder()
                        .title(tr2.i18n("save_path"))
                        .action(nwg::FileDialogAction::OpenDirectory)
                        .build(&mut dialog)
                        .is_ok()
                        && dialog.run::<nwg::ControlHandle>(None)
                        && let Ok(dir) = dialog.get_selected_item()
                    {
                        path_input_ref.set_text(&dir.to_string_lossy());
                    }
                }
                nwg::Event::OnListViewDoubleClick if handle == files_handle => {
                    let (row, _col) = data.on_list_view_item_index();
                    if row < file_count {
                        let mut inc = included_ref.borrow_mut();
                        inc[row] = !inc[row];
                        files_list_ref.update_item(
                            row,
                            nwg::InsertListViewItem {
                                index: Some(row as i32),
                                column_index: 2,
                                text: Some(if inc[row] {
                                    tr2.i18n("yes")
                                } else {
                                    tr2.i18n("no")
                                }),
                                image: None,
                            },
                        );
                    }
                }
                nwg::Event::OnButtonClick if handle == add_handle => {
                    let inc = included_ref.borrow();
                    let only_files = if inc.iter().all(|i| *i) || inc.is_empty() {
                        None
                    } else {
                        Some(
                            inc.iter()
                                .enumerate()
                                .filter(|(_, i)| **i)
                                .map(|(idx, _)| idx)
                                .collect(),
                        )
                    };

                    let label_id = label_combo_ref
                        .selection()
                        .filter(|idx| *idx > 0)
                        .and_then(|idx| labels2.get(idx - 1))
                        .map(|l| l.id);

                    *result_ref.borrow_mut() = DialogResult::AddTorrent {
                        bytes: std::mem::take(&mut bytes.borrow_mut()),
                        params: AddParams {
                            save_path: Some(path_input_ref.text()),
                            start_torrent: start_check_ref.check_state()
                                == nwg::CheckBoxState::Checked,
                            only_files,
                            label_id,
                        },
                    };
                    nwg::stop_thread_dispatch();
                }
                nwg::Event::OnButtonClick if handle == cancel_handle => {
                    nwg::stop_thread_dispatch();
                }
                nwg::Event::OnWindowClose if handle == window_handle => {
                    nwg::stop_thread_dispatch();
                }
                _ => {}
            }
        });

        run_dialog_window(&window, parent);
        nwg::unbind_event_handler(&handler);

        let result = Rc::try_unwrap(result)
            .map(RefCell::into_inner)
            .unwrap_or(DialogResult::Cancelled);
        let _ = tx.send(result);
        notice.notice();
    })
}

// Preferences - port of preferencesdialog.cpp (General / Downloads /
// Connection / Proxy pages; labels are managed via the label context menu).

pub fn spawn_preferences(
    tr: Translator,
    cfg: Arc<Configuration>,
    languages: Vec<(String, String)>,
    parent: usize,
    tx: Sender<DialogResult>,
    notice: nwg::NoticeSender,
) -> DialogHandle {
    std::thread::spawn(move || {
        let result = Rc::new(RefCell::new(DialogResult::Cancelled));

        let bold = caption_font();

        let mut window = nwg::Window::default();
        nwg::Window::builder()
            .title(&tr.i18n("preferences"))
            .size((520, 530))
            .center(true)
            // Built hidden: run_dialog_window themes the content first, then
            // shows it (the default WS_VISIBLE would flash white in dark mode).
            .flags(nwg::WindowFlags::MAIN_WINDOW)
            .build(&mut window)
            .expect("prefs window");

        let mut tabs = nwg::TabsContainer::default();
        nwg::TabsContainer::builder()
            .parent(&window)
            .position((10, 10))
            .size((500, 460))
            .build(&mut tabs)
            .unwrap();

        let mut tab_general = nwg::Tab::default();
        let mut tab_downloads = nwg::Tab::default();
        let mut tab_connection = nwg::Tab::default();
        let mut tab_proxy = nwg::Tab::default();

        nwg::Tab::builder()
            .parent(&tabs)
            .text(&tr.i18n("general"))
            .build(&mut tab_general)
            .unwrap();
        nwg::Tab::builder()
            .parent(&tabs)
            .text(&tr.i18n("downloads"))
            .build(&mut tab_downloads)
            .unwrap();
        nwg::Tab::builder()
            .parent(&tabs)
            .text(&tr.i18n("connection"))
            .build(&mut tab_connection)
            .unwrap();
        nwg::Tab::builder()
            .parent(&tabs)
            .text(&tr.i18n("proxy"))
            .build(&mut tab_proxy)
            .unwrap();

        // --- General page --------------------------------------------------
        let mut lang_caption = nwg::Label::default();
        let mut lang_combo: nwg::ListBox<String> = nwg::ListBox::default();
        let mut theme_caption = nwg::Label::default();
        let mut theme_combo: nwg::ComboBox<String> = nwg::ComboBox::default();
        let mut skip_dialog_check = nwg::CheckBox::default();
        let mut show_tray_check = nwg::CheckBox::default();
        let mut close_action_caption = nwg::Label::default();
        let mut close_action_combo: nwg::ComboBox<String> = nwg::ComboBox::default();
        let mut min_tray_check = nwg::CheckBox::default();
        let mut assoc_btn = nwg::Button::default();

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&tab_general)
            .text(&tr.i18n("language"))
            .position((12, 16))
            .size((140, 20))
            .font(Some(&bold))
            .build(&mut lang_caption)
            .unwrap();

        let current_locale = cfg
            .get_string("locale_name")
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| String::from(crate::DEFAULT_LOCALE));
        let lang_index = languages
            .iter()
            .position(|(loc, _)| loc.eq_ignore_ascii_case(&current_locale))
            .unwrap_or(0);
        // A LIST, not a combo box. A Win32 drop-down shows a minimum of 30
        // items and takes its scroll range from the height it was created
        // with, which left the top of a 41-entry list unreachable however it
        // was sized. A list box just scrolls. ~184px shows 10 languages.
        nwg::ListBox::builder()
            .parent(&tab_general)
            .collection(languages.iter().map(|(_, n)| n.clone()).collect())
            .selected_index(Some(lang_index))
            .position((160, 14))
            .size((200, 170))
            .build(&mut lang_combo)
            .unwrap();

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&tab_general)
            .text(&tr.i18n("theme"))
            .position((12, 208))
            .size((140, 20))
            .font(Some(&bold))
            .build(&mut theme_caption)
            .unwrap();

        let themes = ["system", "light", "dark"];
        let current_theme = cfg
            .get_string("theme_id")
            .unwrap_or_else(|| String::from("system"));
        let theme_index = themes
            .iter()
            .position(|t| *t == current_theme)
            .unwrap_or(0);
        nwg::ComboBox::builder()
            .parent(&tab_general)
            .collection(themes.iter().map(|t| t.to_string()).collect())
            .selected_index(Some(theme_index))
            .position((160, 206))
            .size((200, 24))
            .build(&mut theme_combo)
            .unwrap();

        let checkbox = |parent: &nwg::Tab,
                        text: &str,
                        y: i32,
                        checked: bool,
                        out: &mut nwg::CheckBox| {
            nwg::CheckBox::builder()
                .parent(parent)
                .text(text)
                .check_state(if checked {
                    nwg::CheckBoxState::Checked
                } else {
                    nwg::CheckBoxState::Unchecked
                })
                .position((12, y))
                .size((440, 22))
                .build(out)
                .unwrap();
        };

        checkbox(
            &tab_general,
            &tr.i18n("skip_add_torrent_dialog"),
            244,
            cfg.get_bool("skip_add_torrent_dialog"),
            &mut skip_dialog_check,
        );
        checkbox(
            &tab_general,
            &tr.i18n("show_picotorrent_in_notification_area"),
            272,
            cfg.get_bool("show_in_notification_area"),
            &mut show_tray_check,
        );
        // What pressing the window's close button does.
        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&tab_general)
            .text(&tr.i18n("close_action_label"))
            .position((12, 302))
            .size((160, 20))
            .font(Some(&bold))
            .build(&mut close_action_caption)
            .unwrap();
        let close_action_idx = match cfg.get_persistent("ui.close_action").as_deref() {
            Some("minimize") => 1,
            Some("exit") => 2,
            _ => 0,
        };
        nwg::ComboBox::builder()
            .parent(&tab_general)
            .collection(vec![
                String::from("Ask every time"),
                String::from("Minimize to tray"),
                String::from("Exit"),
            ])
            .selected_index(Some(close_action_idx))
            .position((180, 300))
            .size((180, 24))
            .build(&mut close_action_combo)
            .unwrap();

        checkbox(
            &tab_general,
            &tr.i18n("minimize_to_notification_area"),
            328,
            cfg.get_bool("minimize_to_notification_area"),
            &mut min_tray_check,
        );

        // Register NanoTorrent as the default handler for .torrent files and
        // magnet links (literal text: the Translator would rebrand "PicoTorrent"
        // but there's no product-name reference to protect here).
        nwg::Button::builder()
            .parent(&tab_general)
            .text(&tr.i18n("set_default_associations"))
            .position((12, 364))
            .size((320, 46))
            .build(&mut assoc_btn)
            .unwrap();
        super::set_button_multiline(&assoc_btn);

        // --- Downloads page ------------------------------------------------
        let mut save_path_caption = nwg::Label::default();
        let mut save_path_input = nwg::TextInput::default();
        let mut save_path_browse = nwg::Button::default();
        let mut dl_limit_check = nwg::CheckBox::default();
        let mut dl_limit_input = nwg::TextInput::default();
        let mut ul_limit_check = nwg::CheckBox::default();
        let mut ul_limit_input = nwg::TextInput::default();
        let mut active_dl_caption = nwg::Label::default();
        let mut active_dl_input = nwg::TextInput::default();
        let mut active_seed_caption = nwg::Label::default();
        let mut active_seed_input = nwg::TextInput::default();
        let mut active_limit_caption = nwg::Label::default();
        let mut active_limit_input = nwg::TextInput::default();
        let mut low_disk_check = nwg::CheckBox::default();
        let mut low_disk_caption = nwg::Label::default();
        let mut low_disk_input = nwg::TextInput::default();

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&tab_downloads)
            .text(&tr.i18n("save_path"))
            .position((12, 16))
            .size((140, 20))
            .font(Some(&bold))
            .build(&mut save_path_caption)
            .unwrap();
        nwg::TextInput::builder()
            .parent(&tab_downloads)
            .text(&cfg.get_string("default_save_path").unwrap_or_default())
            .position((12, 38))
            .size((400, 24))
            .build(&mut save_path_input)
            .unwrap();
        nwg::Button::builder()
            .parent(&tab_downloads)
            .text(&tr.i18n("browse"))
            .position((420, 37))
            .size((44, 26))
            .build(&mut save_path_browse)
            .unwrap();

        checkbox(
            &tab_downloads,
            &format!("{} (KB/s)", tr.i18n("dl_rate_limit")),
            80,
            cfg.get_bool("libtorrent.enable_download_rate_limit"),
            &mut dl_limit_check,
        );
        nwg::TextInput::builder()
            .parent(&tab_downloads)
            .text(
                &cfg.get_int("libtorrent.download_rate_limit")
                    .unwrap_or(1024)
                    .to_string(),
            )
            .position((12, 106))
            .size((120, 24))
            .build(&mut dl_limit_input)
            .unwrap();

        checkbox(
            &tab_downloads,
            &format!("{} (KB/s)", tr.i18n("ul_rate_limit")),
            144,
            cfg.get_bool("libtorrent.enable_upload_rate_limit"),
            &mut ul_limit_check,
        );
        nwg::TextInput::builder()
            .parent(&tab_downloads)
            .text(
                &cfg.get_int("libtorrent.upload_rate_limit")
                    .unwrap_or(1024)
                    .to_string(),
            )
            .position((12, 170))
            .size((120, 24))
            .build(&mut ul_limit_input)
            .unwrap();

        // Queue limits (0 = unlimited).
        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&tab_downloads)
            .text(&tr.i18n("active_downloads"))
            .position((12, 212))
            .size((160, 20))
            .font(Some(&bold))
            .build(&mut active_dl_caption)
            .unwrap();
        nwg::TextInput::builder()
            .parent(&tab_downloads)
            .text(
                &cfg.get_int("libtorrent.active_downloads")
                    .unwrap_or(3)
                    .to_string(),
            )
            .position((180, 210))
            .size((80, 24))
            .build(&mut active_dl_input)
            .unwrap();
        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&tab_downloads)
            .text(&tr.i18n("active_seeds"))
            .position((12, 244))
            .size((160, 20))
            .font(Some(&bold))
            .build(&mut active_seed_caption)
            .unwrap();
        nwg::TextInput::builder()
            .parent(&tab_downloads)
            .text(
                &cfg.get_int("libtorrent.active_seeds")
                    .unwrap_or(5)
                    .to_string(),
            )
            .position((180, 242))
            .size((80, 24))
            .build(&mut active_seed_input)
            .unwrap();
        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&tab_downloads)
            .text(&tr.i18n("active_limit"))
            .position((12, 276))
            .size((160, 20))
            .font(Some(&bold))
            .build(&mut active_limit_caption)
            .unwrap();
        nwg::TextInput::builder()
            .parent(&tab_downloads)
            .text(
                &cfg.get_int("libtorrent.active_limit")
                    .unwrap_or(15)
                    .to_string(),
            )
            .position((180, 274))
            .size((80, 24))
            .build(&mut active_limit_input)
            .unwrap();

        // Pause on low disk space (percentage of the volume that must stay free).
        checkbox(
            &tab_downloads,
            &tr.i18n("pause_on_low_disk_space"),
            308,
            cfg.get_bool("pause_on_low_disk_space"),
            &mut low_disk_check,
        );
        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&tab_downloads)
            .text(&format!("{} (%)", tr.i18n("pause_on_low_disk_space_limit")))
            .position((12, 340))
            .size((160, 20))
            .font(Some(&bold))
            .build(&mut low_disk_caption)
            .unwrap();
        nwg::TextInput::builder()
            .parent(&tab_downloads)
            .text(
                &cfg.get_int("pause_on_low_disk_space_limit")
                    .unwrap_or(5)
                    .to_string(),
            )
            .position((180, 338))
            .size((80, 24))
            .build(&mut low_disk_input)
            .unwrap();

        // --- Connection page -----------------------------------------------
        let mut listen_caption = nwg::Label::default();
        let mut listen_addr_input = nwg::TextInput::default();
        let mut listen_port_input = nwg::TextInput::default();
        let mut dht_check = nwg::CheckBox::default();
        let mut lsd_check = nwg::CheckBox::default();
        let mut pex_check = nwg::CheckBox::default();
        let mut geoip_check = nwg::CheckBox::default();
        let mut ipfilter_check = nwg::CheckBox::default();
        let mut ipfilter_path_input = nwg::TextInput::default();
        let mut ipfilter_browse = nwg::Button::default();
        let mut encryption_check = nwg::CheckBox::default();
        let mut incoming_encryption_check = nwg::CheckBox::default();
        let mut anonymous_check = nwg::CheckBox::default();

        let ifaces = cfg.get_listen_interfaces();
        let (addr, port) = ifaces
            .first()
            .map(|i| (i.address.clone(), i.port))
            .unwrap_or((String::from("0.0.0.0"), 6881));

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&tab_connection)
            .text(&tr.i18n("listen_interface"))
            .position((12, 16))
            .size((200, 20))
            .font(Some(&bold))
            .build(&mut listen_caption)
            .unwrap();
        nwg::TextInput::builder()
            .parent(&tab_connection)
            .text(&addr)
            .position((12, 38))
            .size((200, 24))
            .build(&mut listen_addr_input)
            .unwrap();
        nwg::TextInput::builder()
            .parent(&tab_connection)
            .text(&port.to_string())
            .position((220, 38))
            .size((80, 24))
            .build(&mut listen_port_input)
            .unwrap();

        checkbox(
            &tab_connection,
            &tr.i18n("enable_dht"),
            80,
            cfg.get_bool("libtorrent.enable_dht"),
            &mut dht_check,
        );
        // LSD has no librqbit 8 equivalent, so this one is stored and never
        // applied. Show it greyed out rather than letting it imply a feature
        // that isn't there; librqbit 9 exposes it and this goes live again.
        // The saved value still round-trips - the disabled box keeps whatever
        // state it was built with, so nobody's stored preference is lost.
        checkbox(
            &tab_connection,
            &format!("{} (requires librqbit 9)", tr.i18n("enable_lsd")),
            108,
            cfg.get_bool("libtorrent.enable_lsd"),
            &mut lsd_check,
        );
        // NOT nwg's set_enabled: that pokes the WS_DISABLED style bit straight
        // in with SetWindowLong, so the control stops taking input but never
        // gets WM_ENABLE and keeps painting itself as live - a checkbox that
        // looks clickable and silently isn't. EnableWindow tells it properly.
        if let Some(hwnd) = lsd_check.handle.hwnd() {
            unsafe { winapi::um::winuser::EnableWindow(hwnd, 0) };
        }
        checkbox(
            &tab_connection,
            &tr.i18n("enable_pex"),
            136,
            cfg.get_bool("libtorrent.enable_pex"),
            &mut pex_check,
        );
        checkbox(
            &tab_connection,
            "Resolve peer countries (GeoIP)",
            164,
            cfg.get_bool("geoip.enabled"),
            &mut geoip_check,
        );

        // IP filter (eMule/PeerGuardian format)
        checkbox(
            &tab_connection,
            &tr.i18n("ip_filter"),
            204,
            cfg.get_bool("ipfilter.enabled"),
            &mut ipfilter_check,
        );
        nwg::TextInput::builder()
            .parent(&tab_connection)
            .text(&cfg.get_string("ipfilter.file_path").unwrap_or_default())
            .position((12, 230))
            .size((400, 24))
            .build(&mut ipfilter_path_input)
            .unwrap();
        nwg::Button::builder()
            .parent(&tab_connection)
            .text(&tr.i18n("browse"))
            .position((420, 229))
            .size((44, 26))
            .build(&mut ipfilter_browse)
            .unwrap();

        // Outgoing peer encryption (MSE/PE via the StreamTransform seam).
        checkbox(
            &tab_connection,
            "Require outgoing peer encryption (MSE/PE)",
            270,
            cfg.get_bool("libtorrent.require_outgoing_encryption"),
            &mut encryption_check,
        );

        // Incoming peer encryption (MSE/PE via the accept-path seam). Encrypted
        // peers are always accepted; this checkbox refuses *plaintext* inbound.
        checkbox(
            &tab_connection,
            "Require incoming peer encryption (MSE/PE)",
            298,
            cfg.get_bool("libtorrent.require_incoming_encryption"),
            &mut incoming_encryption_check,
        );

        // Anonymous mode: random peer id + no client version in the handshake.
        checkbox(
            &tab_connection,
            "Anonymous mode (hide client identity)",
            326,
            cfg.get_bool("libtorrent.anonymous_mode"),
            &mut anonymous_check,
        );

        // --- Proxy page ----------------------------------------------------
        let mut proxy_type_caption = nwg::Label::default();
        let mut proxy_combo: nwg::ComboBox<String> = nwg::ComboBox::default();
        let mut proxy_host_caption = nwg::Label::default();
        let mut proxy_host_input = nwg::TextInput::default();
        let mut proxy_port_caption = nwg::Label::default();
        let mut proxy_port_input = nwg::TextInput::default();
        let mut proxy_user_caption = nwg::Label::default();
        let mut proxy_user_input = nwg::TextInput::default();
        let mut proxy_pass_caption = nwg::Label::default();
        let mut proxy_pass_input = nwg::TextInput::default();
        let mut proxy_peers_check = nwg::CheckBox::default();
        let mut proxy_trackers_check = nwg::CheckBox::default();
        let mut proxy_hostnames_check = nwg::CheckBox::default();

        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&tab_proxy)
            .text(&tr.i18n("type"))
            .position((12, 16))
            .size((140, 20))
            .font(Some(&bold))
            .build(&mut proxy_type_caption)
            .unwrap();

        let proxy_types = vec![
            tr.i18n("none"),
            String::from("SOCKS4"),
            String::from("SOCKS5"),
            String::from("SOCKS5 (auth)"),
            String::from("HTTP"),
            String::from("HTTP (auth)"),
        ];
        let proxy_index = cfg.get_int("libtorrent.proxy_type").unwrap_or(0) as usize;
        nwg::ComboBox::builder()
            .parent(&tab_proxy)
            .collection(proxy_types)
            .selected_index(Some(proxy_index.min(5)))
            .position((160, 14))
            .size((200, 24))
            .build(&mut proxy_combo)
            .unwrap();

        let text_field = |parent: &nwg::Tab,
                          caption: &str,
                          y: i32,
                          value: &str,
                          cap: &mut nwg::Label,
                          input: &mut nwg::TextInput| {
            nwg::Label::builder()
                .background_color(Some(label_bg()))
                .parent(parent)
                .text(caption)
                .position((12, y + 2))
                .size((140, 20))
                .font(Some(&bold))
                .build(cap)
                .unwrap();
            nwg::TextInput::builder()
                .parent(parent)
                .text(value)
                .position((160, y))
                .size((200, 24))
                .build(input)
                .unwrap();
        };

        text_field(
            &tab_proxy,
            &tr.i18n("host"),
            48,
            &cfg.get_string("libtorrent.proxy_host").unwrap_or_default(),
            &mut proxy_host_caption,
            &mut proxy_host_input,
        );
        text_field(
            &tab_proxy,
            &tr.i18n("port"),
            80,
            &cfg.get_int("libtorrent.proxy_port").unwrap_or(0).to_string(),
            &mut proxy_port_caption,
            &mut proxy_port_input,
        );
        text_field(
            &tab_proxy,
            &tr.i18n("username"),
            112,
            &cfg
                .get_string("libtorrent.proxy_username")
                .unwrap_or_default(),
            &mut proxy_user_caption,
            &mut proxy_user_input,
        );
        text_field(
            &tab_proxy,
            &tr.i18n("password"),
            144,
            &cfg
                .get_string("libtorrent.proxy_password")
                .unwrap_or_default(),
            &mut proxy_pass_caption,
            &mut proxy_pass_input,
        );

        // Proxy scope (only used when a proxy type is selected above).
        checkbox(
            &tab_proxy,
            &tr.i18n("proxy_peers"),
            182,
            cfg.get_bool("libtorrent.proxy_peers"),
            &mut proxy_peers_check,
        );
        checkbox(
            &tab_proxy,
            &tr.i18n("proxy_trackers"),
            210,
            cfg.get_bool("libtorrent.proxy_trackers"),
            &mut proxy_trackers_check,
        );
        checkbox(
            &tab_proxy,
            &tr.i18n("proxy_hostnames"),
            238,
            cfg.get_bool("libtorrent.proxy_hostnames"),
            &mut proxy_hostnames_check,
        );

        // --- OK / Cancel -----------------------------------------------------
        let mut ok_btn = nwg::Button::default();
        let mut cancel_btn = nwg::Button::default();
        let mut restart_label = nwg::Label::default();

        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("ok"))
            .position((320, 480))
            .size((90, 28))
            .build(&mut ok_btn)
            .unwrap();
        nwg::Button::builder()
            .parent(&window)
            .text(&tr.i18n("cancel"))
            .position((418, 480))
            .size((90, 28))
            .build(&mut cancel_btn)
            .unwrap();
        nwg::Label::builder()
            .background_color(Some(label_bg()))
            .parent(&window)
            // Settings are applied live now (the session is rebuilt on OK).
            .text(&tr.i18n("changes_applied_on_ok"))
            .position((10, 484))
            .size((300, 40))
            .build(&mut restart_label)
            .unwrap();

        let window_handle = window.handle;
        let ok_handle = ok_btn.handle;
        let cancel_handle = cancel_btn.handle;
        let browse_handle = save_path_browse.handle;
        let assoc_handle = assoc_btn.handle;

        let lang_combo = Rc::new(lang_combo);
        let theme_combo = Rc::new(theme_combo);
        let skip_dialog_check = Rc::new(skip_dialog_check);
        let show_tray_check = Rc::new(show_tray_check);
        let close_action_combo = Rc::new(close_action_combo);
        let min_tray_check = Rc::new(min_tray_check);
        let save_path_input = Rc::new(save_path_input);
        let dl_limit_check = Rc::new(dl_limit_check);
        let dl_limit_input = Rc::new(dl_limit_input);
        let ul_limit_check = Rc::new(ul_limit_check);
        let ul_limit_input = Rc::new(ul_limit_input);
        let active_dl_input = Rc::new(active_dl_input);
        let active_seed_input = Rc::new(active_seed_input);
        let active_limit_input = Rc::new(active_limit_input);
        let low_disk_check = Rc::new(low_disk_check);
        let low_disk_input = Rc::new(low_disk_input);
        let listen_addr_input = Rc::new(listen_addr_input);
        let listen_port_input = Rc::new(listen_port_input);
        let dht_check = Rc::new(dht_check);
        let lsd_check = Rc::new(lsd_check);
        let pex_check = Rc::new(pex_check);
        let geoip_check = Rc::new(geoip_check);
        let ipfilter_check = Rc::new(ipfilter_check);
        let ipfilter_path_input = Rc::new(ipfilter_path_input);
        let encryption_check = Rc::new(encryption_check);
        let incoming_encryption_check = Rc::new(incoming_encryption_check);
        let anonymous_check = Rc::new(anonymous_check);
        let proxy_combo = Rc::new(proxy_combo);
        let proxy_host_input = Rc::new(proxy_host_input);
        let proxy_port_input = Rc::new(proxy_port_input);
        let proxy_user_input = Rc::new(proxy_user_input);
        let proxy_pass_input = Rc::new(proxy_pass_input);
        let proxy_peers_check = Rc::new(proxy_peers_check);
        let proxy_trackers_check = Rc::new(proxy_trackers_check);
        let proxy_hostnames_check = Rc::new(proxy_hostnames_check);

        let result_ref = result.clone();
        let cfg2 = cfg.clone();
        let tr2 = tr.clone();
        let languages2 = languages.clone();
        let current_locale2 = current_locale.clone();

        let c = (
            lang_combo.clone(),
            theme_combo.clone(),
            skip_dialog_check.clone(),
            show_tray_check.clone(),
            close_action_combo.clone(),
            min_tray_check.clone(),
            save_path_input.clone(),
            dl_limit_check.clone(),
            dl_limit_input.clone(),
            ul_limit_check.clone(),
            ul_limit_input.clone(),
            listen_addr_input.clone(),
            listen_port_input.clone(),
            dht_check.clone(),
            lsd_check.clone(),
            pex_check.clone(),
            proxy_combo.clone(),
            proxy_host_input.clone(),
            proxy_port_input.clone(),
            proxy_user_input.clone(),
            proxy_pass_input.clone(),
            geoip_check.clone(),
            ipfilter_check.clone(),
            ipfilter_path_input.clone(),
            active_dl_input.clone(),
            active_seed_input.clone(),
            encryption_check.clone(),
            incoming_encryption_check.clone(),
            active_limit_input.clone(),
            low_disk_check.clone(),
            low_disk_input.clone(),
            proxy_peers_check.clone(),
            proxy_trackers_check.clone(),
            proxy_hostnames_check.clone(),
            anonymous_check.clone(),
        );

        let save_path_input_browse = save_path_input.clone();
        let ipfilter_path_browse = ipfilter_path_input.clone();
        let ipfilter_browse_handle = ipfilter_browse.handle;
        let tr = tr.clone();

        let handler = nwg::full_bind_event_handler(&window_handle, move |evt, _data, handle| {
            match evt {
                nwg::Event::OnButtonClick if handle == browse_handle => {
                    let mut dialog = nwg::FileDialog::default();
                    if nwg::FileDialog::builder()
                        .title(tr2.i18n("save_path"))
                        .action(nwg::FileDialogAction::OpenDirectory)
                        .build(&mut dialog)
                        .is_ok()
                        && dialog.run::<nwg::ControlHandle>(None)
                        && let Ok(dir) = dialog.get_selected_item()
                    {
                        save_path_input_browse.set_text(&dir.to_string_lossy());
                    }
                }
                nwg::Event::OnButtonClick if handle == ipfilter_browse_handle => {
                    let mut dialog = nwg::FileDialog::default();
                    if nwg::FileDialog::builder()
                        .title(tr2.i18n("select_ip_filter_file"))
                        .action(nwg::FileDialogAction::Open)
                        .filters("IP filter(*.dat;*.p2p;*.txt)|Any(*.*)")
                        .build(&mut dialog)
                        .is_ok()
                        && dialog.run::<nwg::ControlHandle>(None)
                        && let Ok(path) = dialog.get_selected_item()
                    {
                        ipfilter_path_browse.set_text(&path.to_string_lossy());
                    }
                }
                nwg::Event::OnButtonClick if handle == assoc_handle => {
                    match crate::core::file_assoc::register_torrent() {
                        Ok(()) => {
                            // Windows forbids an app from silently taking over a
                            // URL protocol (magnet) default - especially when
                            // another client registered it system-wide. Open the
                            // Default apps page so the user can confirm it.
                            let _ = open::that("ms-settings:defaultapps");
                            nwg::modal_info_message(
                                &window_handle,
                                &tr.i18n("file_association"),
                                &tr.i18n1("file_association_body", "PicoTorrent"),
                            )
                        }
                        Err(e) => nwg::modal_error_message(
                            &window_handle,
                            &tr.i18n("error"),
                            &format!("{}: {e:#}", tr.i18n("file_association")),
                        ),
                    };
                }
                nwg::Event::OnButtonClick if handle == ok_handle => {
                    let checked =
                        |cb: &nwg::CheckBox| cb.check_state() == nwg::CheckBoxState::Checked;

                    // The UI is built from the translator at startup, so a
                    // language change only takes full effect on a restart.
                    let mut locale_changed = false;
                    if let Some(idx) = c.0.selection()
                        && let Some((locale, _)) = languages2.get(idx)
                    {
                        locale_changed = !locale.eq_ignore_ascii_case(&current_locale2);
                        cfg2.set("locale_name", locale);
                    }
                    if let Some(idx) = c.1.selection() {
                        let themes = ["system", "light", "dark"];
                        cfg2.set("theme_id", &themes[idx.min(2)].to_string());
                    }
                    cfg2.set("skip_add_torrent_dialog", &checked(&c.2));
                    cfg2.set("show_in_notification_area", &checked(&c.3));
                    let close_action = match c.4.selection() {
                        Some(1) => "minimize",
                        Some(2) => "exit",
                        _ => "ask",
                    };
                    cfg2.set_persistent("ui.close_action", close_action);
                    cfg2.set("minimize_to_notification_area", &checked(&c.5));

                    cfg2.set("default_save_path", &c.6.text());
                    cfg2.set("libtorrent.enable_download_rate_limit", &checked(&c.7));
                    if let Ok(limit) = c.8.text().trim().parse::<i64>() {
                        cfg2.set("libtorrent.download_rate_limit", &limit);
                    }
                    cfg2.set("libtorrent.enable_upload_rate_limit", &checked(&c.9));
                    if let Ok(limit) = c.10.text().trim().parse::<i64>() {
                        cfg2.set("libtorrent.upload_rate_limit", &limit);
                    }

                    let mut iface = cfg2
                        .get_listen_interfaces()
                        .into_iter()
                        .next()
                        .unwrap_or(crate::core::configuration::ListenInterface {
                            id: -1,
                            address: String::from("0.0.0.0"),
                            port: 6881,
                        });
                    iface.address = c.11.text();
                    if let Ok(port) = c.12.text().trim().parse::<i32>() {
                        iface.port = port;
                    }
                    cfg2.upsert_listen_interface(&iface);

                    cfg2.set("libtorrent.enable_dht", &checked(&c.13));
                    cfg2.set("libtorrent.enable_lsd", &checked(&c.14));
                    cfg2.set("libtorrent.enable_pex", &checked(&c.15));

                    if let Some(idx) = c.16.selection() {
                        cfg2.set("libtorrent.proxy_type", &(idx as i64));
                    }
                    cfg2.set("libtorrent.proxy_host", &c.17.text());
                    if let Ok(port) = c.18.text().trim().parse::<i64>() {
                        cfg2.set("libtorrent.proxy_port", &port);
                    }
                    cfg2.set("libtorrent.proxy_username", &c.19.text());
                    cfg2.set("libtorrent.proxy_password", &c.20.text());

                    cfg2.set("geoip.enabled", &checked(&c.21));
                    cfg2.set("ipfilter.enabled", &checked(&c.22));
                    cfg2.set("ipfilter.file_path", &c.23.text());

                    if let Ok(n) = c.24.text().trim().parse::<i64>() {
                        cfg2.set("libtorrent.active_downloads", &n);
                    }
                    if let Ok(n) = c.25.text().trim().parse::<i64>() {
                        cfg2.set("libtorrent.active_seeds", &n);
                    }

                    cfg2.set("libtorrent.require_outgoing_encryption", &checked(&c.26));
                    cfg2.set("libtorrent.require_incoming_encryption", &checked(&c.27));

                    if let Ok(n) = c.28.text().trim().parse::<i64>() {
                        cfg2.set("libtorrent.active_limit", &n);
                    }
                    cfg2.set("pause_on_low_disk_space", &checked(&c.29));
                    if let Ok(n) = c.30.text().trim().parse::<i64>() {
                        cfg2.set("pause_on_low_disk_space_limit", &n);
                    }

                    cfg2.set("libtorrent.proxy_peers", &checked(&c.31));
                    cfg2.set("libtorrent.proxy_trackers", &checked(&c.32));
                    cfg2.set("libtorrent.proxy_hostnames", &checked(&c.33));
                    cfg2.set("libtorrent.anonymous_mode", &checked(&c.34));

                    *result_ref.borrow_mut() = if locale_changed {
                        DialogResult::PreferencesSavedLanguageChanged
                    } else {
                        DialogResult::PreferencesSaved
                    };
                    nwg::stop_thread_dispatch();
                }
                nwg::Event::OnButtonClick if handle == cancel_handle => {
                    nwg::stop_thread_dispatch();
                }
                nwg::Event::OnWindowClose if handle == window_handle => {
                    nwg::stop_thread_dispatch();
                }
                _ => {}
            }
        });

        run_dialog_window(&window, parent);
        nwg::unbind_event_handler(&handler);

        let result = Rc::try_unwrap(result)
            .map(RefCell::into_inner)
            .unwrap_or(DialogResult::Cancelled);
        let _ = tx.send(result);
        notice.notice();
    })
}
