//! Files and links opened from the Finder, a browser or `open` on macOS.
//!
//! macOS does not put a double-clicked file on the command line. It launches
//! the app (or picks the running one) and sends an "open documents" Apple
//! event, which AppKit hands to the application delegate's
//! `application:openURLs:`. winit's delegate does not implement it, so AppKit
//! fell back to its document machinery, found no document class for a
//! .torrent and answered with "NanoTorrent cannot open files of this type" -
//! while File > Add Torrent, which never goes near any of that, worked. A
//! clicked magnet link (the Info.plist's CFBundleURLTypes) arrives the same
//! way and failed the same way.
//!
//! The method is added to winit's delegate class at run time and passes what
//! it gets to `ipc::hand_over`, the queue a second launch on Windows or Linux
//! forwards its argv into, so from there it is one path for every platform.

use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::NSApplication;
use objc2_foundation::{NSArray, NSURL};

/// `- (void)application:(NSApplication *)app openURLs:(NSArray<NSURL *> *)urls`
extern "C-unwind" fn open_urls(_this: &AnyObject, _cmd: Sel, _app: &AnyObject, urls: &NSArray<NSURL>) {
    let args: Vec<String> = (0..urls.count())
        .filter_map(|i| {
            let url = urls.objectAtIndex(i);
            // A file comes as file:///..., a magnet as itself.
            if url.isFileURL() { url.path() } else { url.absoluteString() }.map(|s| s.to_string())
        })
        .collect();
    tracing::info!("opened from macOS: {args:?}");
    crate::ipc::hand_over(args);
}

/// Teach winit's application delegate `application:openURLs:`.
///
/// After `MainWindow::new` - building the Slint backend is what creates the
/// winit event loop, and with it the delegate - and before the event loop
/// runs, because the event for the file that launched the app is delivered
/// as soon as it starts.
pub fn install() {
    let Some(mtm) = MainThreadMarker::new() else {
        return tracing::warn!("not on the main thread; Finder opens will not reach NanoTorrent");
    };
    let app = NSApplication::sharedApplication(mtm);
    let Some(delegate) = app.delegate() else {
        return tracing::warn!("no application delegate; Finder opens will not reach NanoTorrent");
    };
    let object: &AnyObject = (*delegate).as_ref();
    let class: &AnyClass = object.class();

    // SAFETY: `open_urls` has exactly the signature the type encoding states
    // (void return; self, _cmd, two objects). class_addMethod leaves an
    // existing implementation alone and returns NO, which is fine either way.
    let added = unsafe {
        let imp: Imp = std::mem::transmute(
            open_urls as extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, &NSArray<NSURL>),
        );
        objc2::ffi::class_addMethod(
            std::ptr::from_ref(class).cast_mut(),
            sel!(application:openURLs:),
            imp,
            c"v@:@@".as_ptr(),
        )
    };
    if !added.as_bool() {
        tracing::warn!("{} already answers application:openURLs:", class.name().to_string_lossy());
    }

    // AppKit records which optional methods a delegate answers when it is SET,
    // so a method added afterwards is never asked. Setting it again makes
    // AppKit look a second time. winit keeps its own strong reference, and
    // `delegate` is one more, so nothing is freed in between.
    app.setDelegate(None);
    app.setDelegate(Some(&delegate));
}
