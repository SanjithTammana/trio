//! trio-app: a native window for a Trio room (egui on the GPU, no browser).
//!
//!   trio-app [<room-dir>]      the room in that folder (default: the current folder)
//!
//! It is a client of the room's local server, like the web page: it finds the port in
//! `<room>/.trio/server.json`, and starts `trio open <room> --no-browser` when nothing answers.
#![windows_subsystem = "windows"]

mod app;
mod chat;
mod glyph;
mod map;
mod model;
mod net;
mod paint;
mod panel;
// The server's own room-identity code, so both sides hash a room path alike.
#[path = "../../src/roomid.rs"]
mod roomid;
mod theme;

use eframe::egui;
use std::path::PathBuf;

fn main() {
    let arg = std::env::args().nth(1);
    let room = arg.map(PathBuf::from).or_else(|| std::env::current_dir().ok()).unwrap_or_default();
    let room = room.canonicalize().unwrap_or(room);
    let client = match net::attach(&room) {
        Ok(c) => c,
        Err(e) => {
            fail(&e);
            return;
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Trio")
            .with_app_id("trio")
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([420.0, 360.0])
            .with_icon(glyph::icon(64)),
        ..Default::default()
    };
    // Repaints happen only on input, a change from the server, or a running animation.
    let r = eframe::run_native(
        "trio",
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            Ok(Box::new(app::TrioApp::new(&cc.egui_ctx, client)))
        }),
    );
    if let Err(e) = r {
        fail(&e.to_string());
    }
}

/// A window-subsystem app has no console: show the error in a message box.
fn fail(msg: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (text, title) = (wide(msg), wide("Trio"));
        // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call.
        unsafe {
            MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
        }
    }
    #[cfg(not(windows))]
    eprintln!("trio-app: {msg}");
}
