//! Layout regression tests for the dashboard main panel.
//!
//! The identity band must size to its content; everything below it (RAM
//! strip, page rail, page content) has to stay visible in the window.

use std::cell::{Cell, RefCell};

use egui_kittest::Harness;
use spoon_gui::theme;

/// Renders the identity band exactly as `render_main_panel` does, followed by
/// marker buttons standing in for the icon rail and the page content.
fn dashboard(
    ui: &mut egui::Ui,
    rail_rect: &Cell<Option<egui::Rect>>,
    content_rect: &Cell<Option<egui::Rect>>,
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
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        rail_rect.set(Some(ui.add(egui::Button::new("RAIL_MARKER")).rect));
        ui.add_space(10.0);
        ui.vertical(|ui| {
            ui.set_width(ui.available_width());
            content_rect.set(Some(ui.add(egui::Button::new("CONTENT_MARKER")).rect));
        });
    });
}

#[test]
fn identity_band_sizes_to_content() {
    let rail_rect = Cell::new(None);
    let content_rect = Cell::new(None);
    let mut harness = Harness::new(|ctx| {
        theme::apply(ctx);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BACKGROUND)
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| dashboard(ui, &rail_rect, &content_rect));
    });
    harness.run();

    let rail = rail_rect.get().expect("icon rail rendered");
    let content = content_rect.get().expect("page content rendered");
    assert!(
        rail.left() < content.left(),
        "icon rail (x={:.1}) should sit left of the page content (x={:.1})",
        rail.left(),
        content.left()
    );
    assert!(
        content.top() < 300.0,
        "content below the identity band was pushed down to y={:.1}",
        content.top()
    );
}

/// The dashboard icon rail (rendered exactly as `render_page_rail` does) must
/// show one centered square icon button per page in a compact card, slim and
/// vertically centered next to the page content, which keeps the full height.
#[test]
fn icon_rail_layout() {
    let strip_rect = Cell::new(None);
    let card_rect = Cell::new(None);
    let content_rect = Cell::new(None);
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
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BACKGROUND)
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| {
                // Mirror render_main_panel: measure the content height in the
                // top-down context, then split the row into rail and content.
                let content_height = ui.available_height();
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let (strip, _) = ui.allocate_exact_size(
                        egui::vec2(54.0, content_height),
                        egui::Sense::hover(),
                    );
                    strip_rect.set(Some(strip));
                    // Mirror render_page_rail: content-sized card centered in
                    // the strip, buttons laid out inside a child ui.
                    let card_height: f32 =
                        (5.0_f32 * 36.0 + 4.0 * 6.0 + 2.0 * (10.0 + 1.0)).min(content_height);
                    let card =
                        egui::Rect::from_center_size(strip.center(), egui::vec2(54.0, card_height));
                    card_rect.set(Some(card));
                    let frame = egui::Frame::new()
                        .fill(theme::SURFACE)
                        .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                        .corner_radius(egui::CornerRadius::same(10))
                        .inner_margin(egui::Margin::symmetric(8, 10));
                    let rail_content = card - frame.total_margin();
                    ui.painter().add(frame.paint(rail_content));
                    let mut rail_ui = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(rail_content)
                            .layout(egui::Layout::top_down(egui::Align::Center)),
                    );
                    rail_ui.spacing_mut().item_spacing.y = 6.0;
                    let mut rects = Vec::new();
                    for glyph in [
                        theme::icons::GAUGE_HIGH,
                        theme::icons::FOLDER,
                        theme::icons::USERS,
                        theme::icons::ARCHIVE,
                        theme::icons::GEAR,
                    ] {
                        rects.push(rail_ui.add(theme::nav_icon_tab(false, glyph)).rect);
                    }
                    *button_rects.borrow_mut() = rects;
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        content_rect.set(Some(ui.add(egui::Button::new("CONTENT_MARKER")).rect));
                    });
                });
            });
    });
    harness.run();

    let strip = strip_rect.get().expect("rail strip rendered");
    let card = card_rect.get().expect("rail card rendered");
    let content = content_rect.get().expect("page content rendered");
    let rects = button_rects.borrow();
    assert_eq!(rects.len(), 5, "one rail button per dashboard page");
    for (index, rect) in rects.iter().enumerate() {
        assert!(
            (rect.width() - 36.0).abs() <= 1.0 && (rect.height() - 36.0).abs() <= 1.0,
            "rail button {index} should be a 36x36 square, got {:.1}x{:.1}",
            rect.width(),
            rect.height()
        );
        assert!(
            (rect.center().x - card.center().x).abs() <= 1.0,
            "rail button {index} is not centered in the card"
        );
        if index > 0 {
            assert!(
                (rect.top() - rects[index - 1].top() - 42.0).abs() <= 1.0,
                "rail button {index} is not evenly stacked below the previous one"
            );
        }
    }
    assert!(
        card.left() < content.left(),
        "rail card (x={:.1}) should sit left of the page content (x={:.1})",
        card.left(),
        content.left()
    );
    assert!(
        card.width() < 80.0,
        "rail card should stay slim, got width {:.1}",
        card.width()
    );
    // The card hugs its buttons (two 10px margins, a 1px border and 22px of
    // gaps around the five 36px buttons), it does not stretch into a bar.
    assert!(
        (card.height() - 226.0).abs() <= 2.0,
        "rail card should size to its buttons, got height {:.1}",
        card.height()
    );
    // ...and it is vertically centered next to the full-height page content.
    assert!(
        (card.center().y - strip.center().y).abs() <= 1.0,
        "rail card (center y={:.1}) should be vertically centered (y={:.1})",
        card.center().y,
        strip.center().y
    );
    assert!(
        strip.bottom() - strip.top() > 500.0,
        "the reserved strip should keep the full content height so the page \
         content is not squished, got height {:.1}",
        strip.bottom() - strip.top()
    );
}

/// The sidebar search row with an active filter must keep its clear button
/// inside the panel instead of pushing the content wider than it.
#[test]
fn sidebar_search_clear_button_stays_inside_panel() {
    let clear_rect = Cell::new(None);
    let panel_rect = Cell::new(None);
    let filter = RefCell::new(String::new());
    let mut harness = Harness::new(|ctx| {
        theme::apply(ctx);
        egui::SidePanel::left("server_list")
            .exact_width(230.0)
            .show(ctx, |ui| {
                panel_rect.set(Some(ui.max_rect()));
                ui.horizontal(|ui| {
                    ui.label("S");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut filter = filter.borrow_mut();
                        if !filter.is_empty() {
                            clear_rect.set(Some(
                                ui.add(egui::Button::new(theme::icon(theme::icons::XMARK, 12.0)))
                                    .rect,
                            ));
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut *filter)
                                .id(egui::Id::new("sidebar_search_filter_test"))
                                .desired_width(f32::INFINITY),
                        );
                    });
                });
            });
    });
    harness.run();
    *filter.borrow_mut() = String::from("P");
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

#[test]
fn sidebar_search_keeps_focus_when_clear_button_appears() {
    let search_id = egui::Id::new("sidebar_search_focus_test");
    let filter = RefCell::new(String::new());
    let has_focus = Cell::new(false);
    let clear_rect = Cell::new(None);
    let mut harness = Harness::new(|ctx| {
        theme::apply(ctx);
        egui::SidePanel::left("server_list")
            .exact_width(230.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("S");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut filter = filter.borrow_mut();
                        if !filter.is_empty() {
                            clear_rect.set(Some(
                                ui.add(egui::Button::new(theme::icon(theme::icons::XMARK, 12.0)))
                                    .rect,
                            ));
                        }
                        has_focus.set(
                            ui.add(
                                egui::TextEdit::singleline(&mut *filter)
                                    .id(search_id)
                                    .desired_width(f32::INFINITY),
                            )
                            .has_focus(),
                        );
                    });
                });
            });
    });

    harness
        .ctx
        .memory_mut(|memory| memory.request_focus(search_id));
    harness.run();
    harness
        .input_mut()
        .events
        .push(egui::Event::Text("S".to_string()));
    harness.step();
    harness.step();

    assert_eq!(&*filter.borrow(), "S");
    assert!(clear_rect.get().is_some(), "clear button should appear");
    assert!(has_focus.get(), "search field should keep keyboard focus");
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

/// The create page's software and version lists must fill the column height
/// computed from the window. Inside a horizontal row the first child only
/// sees the row's initial (small) height, so the page grows the cursor with
/// a zero-width spacer before laying out the pickers; without it both scroll
/// areas collapse to a couple of rows.
#[test]
fn create_page_picker_lists_fill_column_height() {
    let software_viewport = Cell::new(None);
    let version_viewport = Cell::new(None);
    let body_height_out = Cell::new(None);
    let column_bottom = Cell::new(None);
    let page_scroll = Cell::new(None);
    let fonts_ready = Cell::new(false);
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1200.0, 800.0))
        .build(|ctx| {
            theme::apply(ctx);
            if !fonts_ready.get() {
                fonts_ready.set(true);
                ctx.request_repaint();
                return;
            }
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(theme::BACKGROUND)
                        .inner_margin(egui::Margin::same(18)),
                )
                .show(ctx, |ui| {
                    // Header, mirroring render_create_page.
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui.label(
                        egui::RichText::new("Create Server")
                            .size(20.0)
                            .color(theme::TEXT),
                    );
                    ui.label(
                        egui::RichText::new("subtitle")
                            .size(13.0)
                            .color(theme::TEXT_MUTED),
                    );
                    let footer_height = 48.0_f32;
                    ui.add_space(6.0);
                    let body_height = (ui.available_height() - footer_height).max(0.0);
                    body_height_out.set(Some(body_height));
                    let outer = egui::ScrollArea::vertical()
                        .id_salt("create_server_page")
                        .auto_shrink([false, false])
                        .max_height(body_height)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 12.0;
                                let _ = ui.allocate_exact_size(
                                    egui::vec2(0.0, body_height),
                                    egui::Sense::hover(),
                                );
                                ui.vertical(|ui| {
                                    ui.set_width((ui.available_width() * 0.48).clamp(280.0, 430.0));
                                    column_bottom.set(Some(ui.max_rect().bottom()));
                                    // Mirror render_create_software_column's
                                    // height split: software by formula,
                                    // version measured inside its own card.
                                    const CARD_CHROME: f32 = 101.0;
                                    const GAP: f32 = 10.0;
                                    let render_list_card = |ui: &mut egui::Ui,
                                                            id: &str,
                                                            height: Option<f32>,
                                                            slot: &Cell<
                                        Option<(egui::Rect, f32)>,
                                    >| {
                                        theme::card().show(ui, |ui| {
                                            ui.set_width(ui.available_width());
                                            theme::section_title_with_icon(
                                                ui,
                                                theme::icons::SERVER,
                                                "LIST",
                                            );
                                            ui.add_space(6.0);
                                            ui.add(
                                                egui::TextEdit::singleline(&mut String::new())
                                                    .desired_width(f32::INFINITY)
                                                    .hint_text("Search…"),
                                            );
                                            ui.add_space(6.0);
                                            // `None` mirrors the version list:
                                            // measure what is left after the
                                            // card chrome, exactly like the
                                            // production code.
                                            let height = height.unwrap_or(ui.available_height());
                                            let inner = egui::ScrollArea::vertical()
                                                .id_salt(id)
                                                .auto_shrink([false, false])
                                                .hscroll(false)
                                                .max_height(height)
                                                .show(ui, |ui| {
                                                    for i in 0..40 {
                                                        ui.add_sized(
                                                            [ui.available_width(), 30.0],
                                                            egui::Button::new(format!("Row {i}")),
                                                        );
                                                    }
                                                });
                                            slot.set(Some((inner.inner_rect, height)));
                                        });
                                    };
                                    let software_height = ((ui.available_height() - GAP) * 0.5
                                        - CARD_CHROME)
                                        .max(80.0);
                                    render_list_card(
                                        ui,
                                        "sw_list",
                                        Some(software_height),
                                        &software_viewport,
                                    );
                                    ui.add_space(GAP);
                                    render_list_card(ui, "ver_list", None, &version_viewport);
                                });
                                ui.vertical(|ui| {
                                    ui.set_width(ui.available_width());
                                    // A short stand-in for the settings form,
                                    // which fits the body height in production.
                                    for _ in 0..5 {
                                        theme::card().show(ui, |ui| {
                                            ui.set_width(ui.available_width());
                                            ui.label("form row");
                                        });
                                        ui.add_space(10.0);
                                    }
                                });
                            });
                        });
                    page_scroll.set(Some((outer.content_size.y, outer.inner_rect.height())));
                });
        });
    harness.run_steps(3);

    let body_height = body_height_out.get().expect("body height measured");
    assert!(
        body_height > 400.0,
        "expected a tall page body, got {body_height:.1}"
    );
    for (name, slot) in [
        ("software", &software_viewport),
        ("version", &version_viewport),
    ] {
        let (rect, expected) = slot.get().expect("list rendered");
        assert!(
            (rect.height() - expected).abs() <= 2.0,
            "{name} list should fill its computed height ({expected:.1}), got {:.1}",
            rect.height()
        );
    }
    // The version card reaches the bottom of the column: only the card's
    // bottom margin and border sit below the list.
    let version = version_viewport.get().expect("version rendered").0;
    let bottom = column_bottom.get().expect("column measured");
    assert!(
        (version.bottom() - bottom).abs() <= 14.0,
        "version list ends {:.1}px above the column bottom {bottom:.1}",
        bottom - version.bottom()
    );
    // The whole page must not scroll: the columns exactly fill the scroll
    // area, so a content taller than the viewport would scroll the page by
    // that difference.
    let (content, viewport) = page_scroll.get().expect("page scroll area rendered");
    assert!(
        content <= viewport + 1.0,
        "the create page scrolls: content is {content:.1} but the viewport is {viewport:.1}"
    );
}

/// The Plugins & Mods results must stay inside the window: the project rows
/// live in a vertical ScrollArea below the page header, search and tab rows,
/// so a full page of results scrolls instead of pushing content past the
/// window edge. The scroll area reserves footer space so the page navigation
/// below it is never cut off.
#[test]
fn content_results_scroll_inside_page() {
    let panel_rect = Cell::new(None);
    let scroll_rect = Cell::new(None);
    let last_row_rect = Cell::new(None);
    let pager_rect = Cell::new(None);
    let mut harness = Harness::new(|ctx| {
        theme::apply(ctx);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BACKGROUND)
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| {
                panel_rect.set(Some(ui.max_rect()));
                // Mirror render_content_page above the results: title row,
                // search row, then the Browse/Installed tabs.
                for height in [30.0, 30.0, 30.0] {
                    ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), height),
                        egui::Sense::hover(),
                    );
                    ui.add_space(8.0);
                }
                let response = egui::ScrollArea::vertical()
                    .id_salt("content_results")
                    .auto_shrink([false, false])
                    .hscroll(false)
                    .max_height((ui.available_height() - 44.0).max(120.0))
                    .show(ui, |ui| {
                        for index in 0..60 {
                            let row = ui.add_sized(
                                [ui.available_width(), 56.0],
                                egui::Button::new(format!("result {index}")),
                            );
                            if index == 59 {
                                last_row_rect.set(Some(row.rect));
                            }
                        }
                    });
                scroll_rect.set(Some(response.inner_rect));
                ui.add_space(6.0);
                pager_rect.set(Some(ui.add(egui::Button::new("Page 1 of 4")).rect));
            });
    });
    harness.run();

    let panel = panel_rect.get().expect("panel rendered");
    let scroll = scroll_rect.get().expect("results scroll area rendered");
    let last_row = last_row_rect.get().expect("result rows rendered");
    let pager = pager_rect.get().expect("page navigation rendered");
    assert!(
        last_row.bottom() > scroll.bottom(),
        "row stack (bottom {:.1}) should overflow the scroll area (bottom {:.1}) so it scrolls",
        last_row.bottom(),
        scroll.bottom()
    );
    assert!(
        pager.bottom() <= panel.bottom() + 0.5,
        "page navigation (bottom {:.1}) must stay inside the panel (bottom {:.1})",
        pager.bottom(),
        panel.bottom()
    );
}
