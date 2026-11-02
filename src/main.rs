#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod analysis;
mod app;
mod cli;
mod detect;
mod export;
mod model;
mod num;
mod preview;
mod video;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| ["analyze", "report", "csv", "cut", "help", "--help"].contains(&a.as_str())) {
        attach_console();
        if args[0] == "help" || args[0] == "--help" {
            println!("{}", cli::USAGE);
            return Ok(());
        }
        return cli::run(&args);
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("MCSR Inventory Cutter")
            .with_inner_size([1400.0, 900.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    let video = args.first().map(std::path::PathBuf::from);
    eframe::run_native(
        "mcsr-inv-cut",
        options,
        Box::new(move |cc| {
            let mut app = app::App::new(cc);
            if let Some(v) = video {
                app.open_video(v);
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Release builds use the GUI subsystem; reattach to the launching terminal so
/// CLI commands can print.
fn attach_console() {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}
