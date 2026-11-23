#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod app;
mod capture;
mod install;
mod network;
mod update;

pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

fn main() -> eframe::Result {
    if let Some(code) = install::helper_exit_code() {
        std::process::exit(code);
    }

    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/arcane-portal.png"))
        .expect("load the Arcane Portal icon");
    let viewport = eframe::egui::ViewportBuilder::default()
        .with_app_id("com.ooberguts.arcaneportal")
        .with_title("Arcane Portal")
        .with_inner_size([1200.0, 800.0])
        .with_min_inner_size([880.0, 640.0])
        .with_icon(icon);
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Arcane Portal",
        options,
        Box::new(|context| {
            let app = app::PortalApp::new(context);
            install::confirm_relaunch();
            Ok(Box::new(app))
        }),
    )
}
