// У release-збірці під Windows — без консольного вікна.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod detectable;
mod error;
mod process;
mod storage;
mod util;

use eframe::egui;

/// Прапорець командного рядка, з яким запускаються скопійовані бінарники:
/// з ним програма не створює вікна й просто «висить» у фоні.
pub const WORKER_FLAG: &str = "--background-worker";

fn main() -> eframe::Result {
    if std::env::args_os().skip(1).any(|arg| arg == WORKER_FLAG) {
        process::run_worker();
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 680.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Game Launcher",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)?))),
    )
}
