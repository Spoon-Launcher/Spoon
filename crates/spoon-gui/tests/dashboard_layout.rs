//! Layout regression tests for the dashboard main panel.
//!
//! The identity band must size to its content; everything below it (RAM
//! strip, tab bar, page content) has to stay visible in the window.

use std::cell::{Cell, RefCell};

use egui_kittest::Harness;
use spoon_gui::theme;

/// Renders the identity band exactly as `render_main_panel` does, followed by
/// marker buttons standing in for the RAM strip and the tab bar.
fn dashboard(
    ui: &mut egui::Ui,
    ram_rect: &Cell<Option<egui::Rect>>,
    tab_rect: &Cell<Option<egui::Rect>>,
) {
    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let (dot_rect, _) =
                ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(dot_rect.center(), 5.0, theme::PRIMARY);
            ui.label(
                egui::RichText::new("test")
                    .strong()
                    .size(24.0)
                    .color(theme::TEXT),
            );
            ui.label(
                egui::RichText::new("Stopped")
                    .size(15.0)
                    .color(theme::TEXT_MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let _ = ui.button("Start");
                let _ = ui.button("Open Folder");
            });
        });
    });
    ui.add_space(10.0);
    ram_rect.set(Some(ui.add(egui::Button::new("RAM_MARKER")).rect));
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        for tab in ["Overview", "Files", "Players", "Backups", "Settings"] {
            ui.add(egui::Button::new(tab));
        }
        tab_rect.set(Some(ui.min_rect()));
    });
}

#[test]
fn identity_band_sizes_to_content() {
    let ram_rect = Cell::new(None);
    let tab_rect = Cell::new(None);
    let mut harness = Harness::new(|ctx| {
        theme::apply(ctx);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BACKGROUND)
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| dashboard(ui, &ram_rect, &tab_rect));
    });
    harness.run();

    let ram = ram_rect.get().expect("RAM marker rendered");
    let tabs = tab_rect.get().expect("tab bar rendered");
    assert!(
        ram.top() < 300.0,
        "content below the identity band was pushed down to y={:.1}",
        ram.top()
    );
    assert!(
        tabs.top() < 400.0,
        "tab bar pushed down to y={:.1}",
        tabs.top()
    );
}

/// The sidebar search row with an active filter must keep its clear button
/// inside the panel instead of pushing the content wider than it.
#[test]
fn sidebar_search_clear_button_stays_inside_panel() {
    let clear_rect = Cell::new(None);
    let panel_rect = Cell::new(None);
    let mut filter = String::from("P");
    let mut harness = Harness::new(|ctx| {
        theme::apply(ctx);
        egui::SidePanel::left("server_list")
            .exact_width(230.0)
            .show(ctx, |ui| {
                panel_rect.set(Some(ui.max_rect()));
                ui.horizontal(|ui| {
                    ui.label("S");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        clear_rect.set(Some(ui.add(egui::Button::new("x")).rect));
                        ui.add(
                            egui::TextEdit::singleline(&mut filter).desired_width(f32::INFINITY),
                        );
                    });
                });
            });
    });
    harness.run();

    let clear = clear_rect.get().expect("clear button rendered");
    let panel = panel_rect.get().expect("panel rendered");
    assert!(
        clear.right() <= panel.right() + 0.5,
        "clear button overflows the panel: {:.1} > {:.1}",
        clear.right(),
        panel.right()
    );
}

/// The pinned sidebar footer (buttons and version label) must fit inside the
/// height the server list reserves for it, or the list and footer overlap.
#[test]
fn sidebar_footer_fits_reserved_height() {
    for (update_available, buttons) in [(false, 3), (true, 4)] {
        let footer_rect = Cell::new(None);
        let version_rect = Cell::new(None);
        let button_rects = RefCell::new(Vec::new());
        let fonts_ready = Cell::new(false);
        let mut harness = Harness::new(|ctx| {
            theme::apply(ctx);
            if !fonts_ready.get() {
                // set_fonts only takes effect on the next pass; skip a frame
                // so the icon font used by the buttons is bound.
                fonts_ready.set(true);
                ctx.request_repaint();
                return;
            }
            egui::SidePanel::left("server_list").show(ctx, |ui| {
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    version_rect.set(Some(
                        ui.add_sized(
                            [ui.available_width(), 18.0],
                            egui::Label::new(
                                egui::RichText::new("v0.0.1")
                                    .size(11.5)
                                    .color(theme::TEXT_MUTED),
                            )
                            .halign(egui::Align::Center),
                        )
                        .rect,
                    ));
                    let mut rects = Vec::new();
                    for _ in 0..buttons {
                        rects.push(
                            ui.add_sized(
                                [ui.available_width(), 36.0],
                                theme::ghost_icon_button(theme::icons::GEAR, "Footer"),
                            )
                            .rect,
                        );
                    }
                    *button_rects.borrow_mut() = rects;
                    footer_rect.set(Some(ui.min_rect()));
                });
            });
        });
        harness.run_steps(2);

        let rendered = footer_rect.get().expect("footer rendered").height();
        let reserved = spoon_gui::app::sidebar_footer_height(update_available);
        let version = version_rect.get().expect("version rendered");
        let lowest_button = button_rects
            .borrow()
            .iter()
            .map(|rect| rect.bottom())
            .fold(0.0_f32, f32::max);
        assert!(
            rendered <= reserved,
            "footer with update_available={update_available} renders {rendered:.1}px \
             but the server list only reserves {reserved:.1}px"
        );
        assert!(
            version.top() > lowest_button,
            "version label is not below the footer buttons: {:.1} <= {:.1}",
            version.top(),
            lowest_button
        );
    }
}
