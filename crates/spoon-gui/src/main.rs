#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(spoon_gui::app::SPLASH_SIZE)
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_icon(spoon_gui::app::window_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "Spoon Launcher",
        options,
        Box::new(|cc| {
            spoon_gui::theme::apply(&cc.egui_ctx);
            Ok(Box::new(spoon_gui::app::SpoonApp::new(cc)))
        }),
    )
}
