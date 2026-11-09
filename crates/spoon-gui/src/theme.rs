use egui::{Color32, Stroke};

pub const BACKGROUND: Color32 = Color32::from_rgb(0x17, 0x1C, 0x1B);
pub const SURFACE: Color32 = Color32::from_rgb(0x25, 0x2D, 0x29);
pub const SURFACE_HOVER: Color32 = Color32::from_rgb(0x30, 0x39, 0x35);
pub const SURFACE_ACTIVE: Color32 = Color32::from_rgb(0x38, 0x43, 0x3E);
pub const CONSOLE_BG: Color32 = Color32::from_rgb(0x10, 0x14, 0x13);
pub const PRIMARY: Color32 = Color32::from_rgb(0x8F, 0xE3, 0xB0);
const PRIMARY_HOVER: Color32 = Color32::from_rgb(0xA6, 0xED, 0xC0);
pub const TEXT: Color32 = Color32::from_rgb(0xF1, 0xF5, 0xF0);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x9A, 0xA5, 0x9E);
pub const WARN: Color32 = Color32::from_rgb(0xE3, 0xCF, 0x8F);
pub const ERROR: Color32 = Color32::from_rgb(0xE3, 0x8F, 0x8F);
pub const SYNTAX_NUMBER: Color32 = Color32::from_rgb(0x8F, 0xB8, 0xE3);
pub const SYNTAX_BOOL: Color32 = Color32::from_rgb(0xC7, 0xA3, 0xE3);

pub const BORDER: Color32 = Color32::from_rgb(0x34, 0x3E, 0x39);
pub const RADIUS: u8 = 8;

/// Applies the UI.md palette, rounding, spacing and typography to egui's global style.
pub fn title_family() -> egui::FontFamily {
    egui::FontFamily::Name("title".into())
}

/// Font family hosting the Font Awesome solid glyphs.
pub fn icon_family() -> egui::FontFamily {
    egui::FontFamily::Name("awesome".into())
}

/// Font family hosting Font Awesome brand glyphs.
pub fn brand_icon_family() -> egui::FontFamily {
    egui::FontFamily::Name("awesome_brands".into())
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "lilita".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../../../assets/fonts/LilitaOne.ttf"
        ))),
    );
    fonts.font_data.insert(
        "dyna_buff".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../../../assets/fonts/DynaBuff.ttf"
        ))),
    );
    // Font Awesome Free 6.5.2 (solid) — icons CC BY 4.0, font SIL OFL 1.1,
    // see https://fontawesome.com/license.
    fonts.font_data.insert(
        "fa_solid".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../../../assets/fonts/fa-solid-900.ttf"
        ))),
    );
    // Font Awesome Free 7.3.1 (brands) — icons CC BY 4.0, font SIL OFL 1.1,
    // see https://fontawesome.com/license.
    fonts.font_data.insert(
        "fa_brands".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../../../assets/fonts/fa-brands-400.otf"
        ))),
    );
    // Lilita first, with the default fonts kept as fallback for missing glyphs.
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "lilita".into());
    let mut title = vec!["dyna_buff".to_string()];
    title.extend(
        fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default(),
    );
    fonts.families.insert(title_family(), title);
    fonts
        .families
        .insert(icon_family(), vec!["fa_solid".to_string()]);
    fonts
        .families
        .insert(brand_icon_family(), vec!["fa_brands".to_string()]);
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    let mut style = (*ctx.style()).clone();

    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 6.0);
    style.spacing.interact_size.y = 30.0;
    style.spacing.window_margin = egui::Margin::same(16);
    style.spacing.menu_margin = egui::Margin::same(6);
    style.animation_time = 0.10;

    use egui::{FontFamily, FontId, TextStyle};
    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(22.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(14.5, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(12.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(13.5, FontFamily::Monospace),
        ),
    ]
    .into();

    let rounding = egui::CornerRadius::same(RADIUS);
    let v = &mut style.visuals;
    v.dark_mode = true;
    v.panel_fill = BACKGROUND;
    v.window_fill = SURFACE;
    v.window_stroke = Stroke::new(1.0_f32, BORDER);
    v.window_corner_radius = egui::CornerRadius::same(12);
    v.menu_corner_radius = rounding;
    v.window_shadow = egui::Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(110),
    };
    v.popup_shadow = v.window_shadow;
    v.extreme_bg_color = CONSOLE_BG;
    v.faint_bg_color = SURFACE;
    v.hyperlink_color = PRIMARY;
    v.striped = false;

    v.selection.bg_fill = PRIMARY.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0_f32, PRIMARY);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = SURFACE;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    w.noninteractive.corner_radius = rounding;
    for (state, fill) in [
        (&mut w.inactive, SURFACE_HOVER),
        (&mut w.hovered, SURFACE_ACTIVE),
        (&mut w.active, SURFACE_ACTIVE),
        (&mut w.open, SURFACE_HOVER),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.fg_stroke = Stroke::new(1.0_f32, TEXT);
        state.corner_radius = rounding;
        state.expansion = 0.0;
    }
    w.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    w.hovered.bg_stroke = Stroke::new(1.0_f32, PRIMARY.gamma_multiply(0.6));
    w.active.bg_stroke = Stroke::new(1.0_f32, PRIMARY);
    w.open.bg_stroke = Stroke::new(1.0_f32, BORDER);

    ctx.set_style(style);
}

/// A button whose visual colors transition between idle, hover and active states.
pub struct AnimatedButton {
    button: egui::Button<'static>,
    fills: [Color32; 4],
    strokes: [Stroke; 4],
    padding: Option<egui::Vec2>,
}

impl AnimatedButton {
    /// Overrides the style's button padding for this button.
    pub fn padding(mut self, padding: egui::Vec2) -> Self {
        self.padding = Some(padding);
        self
    }
}

impl egui::Widget for AnimatedButton {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        ui.scope(|ui| {
            if let Some(padding) = self.padding {
                ui.spacing_mut().button_padding = padding;
            }
            let widgets = &mut ui.visuals_mut().widgets;
            widgets.inactive.bg_fill = self.fills[0];
            widgets.inactive.weak_bg_fill = self.fills[0];
            widgets.inactive.bg_stroke = self.strokes[0];
            widgets.hovered.bg_fill = self.fills[1];
            widgets.hovered.weak_bg_fill = self.fills[1];
            widgets.hovered.bg_stroke = self.strokes[1];
            widgets.active.bg_fill = self.fills[2];
            widgets.active.weak_bg_fill = self.fills[2];
            widgets.active.bg_stroke = self.strokes[2];
            widgets.open.bg_fill = self.fills[3];
            widgets.open.weak_bg_fill = self.fills[3];
            widgets.open.bg_stroke = self.strokes[3];
            ui.add(self.button)
        })
        .inner
    }
}

/// Applies animated idle and hover fills/strokes to an egui button.
pub fn animated_button(
    button: egui::Button<'static>,
    idle_fill: Color32,
    hover_fill: Color32,
    idle_stroke: Stroke,
    hover_stroke: Stroke,
) -> AnimatedButton {
    AnimatedButton {
        button,
        fills: [idle_fill, hover_fill, hover_fill, idle_fill],
        strokes: [idle_stroke, hover_stroke, hover_stroke, idle_stroke],
        padding: None,
    }
}

/// Interpolates a custom-painted interactive element's background on hover.
pub fn animated_hover_fill(
    ui: &egui::Ui,
    id: egui::Id,
    hovered: bool,
    idle_fill: Color32,
    hover_fill: Color32,
) -> Color32 {
    let amount = ui
        .ctx()
        .animate_bool_with_time(id, hovered, ui.style().animation_time);
    idle_fill.lerp_to_gamma(hover_fill, amount)
}

/// A filled accent button (used for primary actions like Start).
pub fn primary_button(text: &str) -> AnimatedButton {
    animated_button(
        egui::Button::new(egui::RichText::new(text).color(BACKGROUND).strong()),
        PRIMARY,
        PRIMARY_HOVER,
        Stroke::NONE,
        Stroke::NONE,
    )
}

/// A subtle filled button for secondary actions (Stop, Restart, Copy…).
pub fn secondary_button(text: &str) -> AnimatedButton {
    animated_button(
        egui::Button::new(egui::RichText::new(text).color(TEXT)),
        SURFACE_HOVER,
        SURFACE_ACTIVE,
        Stroke::new(1.0_f32, BORDER),
        Stroke::new(1.0_f32, PRIMARY.gamma_multiply(0.6)),
    )
}

/// A transparent button with just a border (Settings, Import, Cancel…).
pub fn ghost_button(text: &str) -> AnimatedButton {
    animated_button(
        egui::Button::new(egui::RichText::new(text).color(TEXT_MUTED)),
        Color32::TRANSPARENT,
        SURFACE_HOVER,
        Stroke::new(1.0_f32, BORDER),
        Stroke::new(1.0_f32, PRIMARY.gamma_multiply(0.6)),
    )
}

/// A tinted button for destructive actions (Kill, Remove).
pub fn danger_button(text: &str) -> AnimatedButton {
    animated_button(
        egui::Button::new(egui::RichText::new(text).color(ERROR).strong()),
        ERROR.gamma_multiply(0.12),
        ERROR.gamma_multiply(0.24),
        Stroke::new(1.0_f32, ERROR.gamma_multiply(0.55)),
        Stroke::new(1.0_f32, ERROR),
    )
}

/// A small-caps, muted heading that introduces a section.
pub fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .size(11.0)
            .color(TEXT_MUTED)
            .extra_letter_spacing(1.4),
    );
}

/// Like [`section_title`], with a leading Font Awesome glyph.
pub fn section_title_with_icon(ui: &mut egui::Ui, glyph: &str, text: &str) {
    ui.horizontal(|ui| {
        ui.label(icon(glyph, 11.0).color(TEXT_MUTED));
        section_title(ui, text);
    });
}

/// Font Awesome solid glyphs (codepoints are stable since FA5).
pub mod icons {
    pub const PLAY: &str = "\u{f04b}";
    pub const STOP: &str = "\u{f04d}";
    pub const RESTART: &str = "\u{f01e}";
    pub const KILL: &str = "\u{f54c}";
    pub const GEAR: &str = "\u{f013}";
    pub const PLUS: &str = "\u{f067}";
    pub const FOLDER_OPEN: &str = "\u{f07c}";
    pub const DOWNLOAD: &str = "\u{f019}";
    pub const SAVE: &str = "\u{f0c7}";
    pub const TRASH: &str = "\u{f1f8}";
    pub const COPY: &str = "\u{f0c5}";
    pub const BROOM: &str = "\u{f515}";
    pub const SEARCH: &str = "\u{f002}";
    pub const CPU: &str = "\u{f2db}";
    pub const MEMORY: &str = "\u{f538}";
    pub const CLOCK: &str = "\u{f017}";
    pub const USERS: &str = "\u{f0c0}";
    pub const LOCATION_DOT: &str = "\u{f3c5}";
    pub const GLOBE: &str = "\u{f0ac}";
    pub const FINGERPRINT: &str = "\u{f577}";
    pub const COMMENTS: &str = "\u{f086}";
    pub const COFFEE: &str = "\u{f0f4}";
    pub const SERVER: &str = "\u{f233}";
    pub const TERMINAL: &str = "\u{f120}";
    pub const PAPER_PLANE: &str = "\u{f1d8}";
    pub const CLONE: &str = "\u{f24d}";
    pub const RESTORE: &str = "\u{f0e2}";
    pub const ARCHIVE: &str = "\u{f187}";
    pub const CHECK: &str = "\u{f00c}";
    pub const XMARK: &str = "\u{f00d}";
    pub const FILE: &str = "\u{f15b}";
    pub const SLIDERS: &str = "\u{f1de}";
    pub const GAUGE_HIGH: &str = "\u{f625}";
    pub const TAG: &str = "\u{f02b}";
    pub const FOLDER: &str = "\u{f07b}";
    pub const FILE_LINES: &str = "\u{f15c}";
    pub const PEN: &str = "\u{f304}";
    pub const ARROW_UP: &str = "\u{f062}";
    pub const ARROW_LEFT: &str = "\u{f060}";
    pub const ARROW_RIGHT: &str = "\u{f061}";
    pub const ROTATE: &str = "\u{f2f1}";
    pub const FOLDER_PLUS: &str = "\u{f65e}";
    pub const FILE_CIRCLE_PLUS: &str = "\u{e4ee}";
    pub const FILE_ZIPPER: &str = "\u{f1c6}";
    pub const ELLIPSIS: &str = "\u{f141}";
    pub const JAVA: &str = "\u{f7b6}";
    pub const PALETTE: &str = "\u{f53f}";
    pub const IMAGE: &str = "\u{f03e}";
    pub const LINK: &str = "\u{f0c1}";
}

/// Font Awesome Free brand glyphs.
pub mod brand_icons {
    pub const DISCORD: &str = "\u{f392}";
    pub const KO_FI: &str = "\u{e856}";
}

/// A single Font Awesome glyph (color it with `.color(..)` as needed).
pub fn icon(glyph: &str, size: f32) -> egui::RichText {
    egui::RichText::new(glyph).font(egui::FontId::new(size, icon_family()))
}

/// Text mixing a Font Awesome glyph with body text, for buttons, labels and menu items.
pub fn icon_text_job(glyph: &str, text: &str, color: Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        glyph,
        0.0,
        egui::TextFormat::simple(egui::FontId::new(13.0, icon_family()), color),
    );
    job.append(
        &format!(" {text}"),
        0.0,
        egui::TextFormat::simple(egui::FontId::proportional(14.5), color),
    );
    job
}

/// A dialog title: accent-colored icon followed by large text.
pub fn title_with_icon(glyph: &str, text: &str, icon_color: Color32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        glyph,
        0.0,
        egui::TextFormat::simple(egui::FontId::new(16.0, icon_family()), icon_color),
    );
    job.append(
        &format!("  {text}"),
        0.0,
        egui::TextFormat::simple(egui::FontId::proportional(20.0), TEXT),
    );
    job
}

/// A filled accent button with a leading icon (Start, Create, Save…).
pub fn primary_icon_button(glyph: &str, text: &str) -> AnimatedButton {
    animated_button(
        egui::Button::new(icon_text_job(glyph, text, BACKGROUND)),
        PRIMARY,
        PRIMARY_HOVER,
        Stroke::NONE,
        Stroke::NONE,
    )
}

/// A subtle filled button with a leading icon (Stop, Restart, Copy…).
pub fn secondary_icon_button(glyph: &str, text: &str) -> AnimatedButton {
    animated_button(
        egui::Button::new(icon_text_job(glyph, text, TEXT)),
        SURFACE_HOVER,
        SURFACE_ACTIVE,
        Stroke::new(1.0_f32, BORDER),
        Stroke::new(1.0_f32, PRIMARY.gamma_multiply(0.6)),
    )
}

/// A transparent button with a border and a leading icon (Settings, Import, Cancel…).
pub fn ghost_icon_button(glyph: &str, text: &str) -> AnimatedButton {
    animated_button(
        egui::Button::new(icon_text_job(glyph, text, TEXT_MUTED)),
        Color32::TRANSPARENT,
        SURFACE_HOVER,
        Stroke::new(1.0_f32, BORDER),
        Stroke::new(1.0_f32, PRIMARY.gamma_multiply(0.6)),
    )
}

/// A tinted destructive button with a leading icon (Kill, Remove).
pub fn danger_icon_button(glyph: &str, text: &str) -> AnimatedButton {
    animated_button(
        egui::Button::new(icon_text_job(glyph, text, ERROR)),
        ERROR.gamma_multiply(0.12),
        ERROR.gamma_multiply(0.24),
        Stroke::new(1.0_f32, ERROR.gamma_multiply(0.55)),
        Stroke::new(1.0_f32, ERROR),
    )
}

/// A tinted attention button with a leading icon (update available).
pub fn warn_icon_button(glyph: &str, text: &str) -> AnimatedButton {
    animated_button(
        egui::Button::new(icon_text_job(glyph, text, WARN)),
        WARN.gamma_multiply(0.12),
        WARN.gamma_multiply(0.24),
        Stroke::new(1.0_f32, WARN.gamma_multiply(0.55)),
        Stroke::new(1.0_f32, WARN),
    )
}

/// An animated on/off switch, `WIDTH`x`HEIGHT` points tall. Flips `on` when
/// clicked; pair with a label next to it.
pub fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let desired_size = egui::vec2(40.0, 22.0);
    let (rect, mut response) = ui.allocate_exact_size(desired_size, egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let amount = ui
        .ctx()
        .animate_bool_with_time(response.id, *on, ui.style().animation_time);

    let painter = ui.painter();
    let track = rect.shrink(1.0);
    let off_fill = CONSOLE_BG;
    let on_fill = PRIMARY;
    painter.rect_filled(
        track,
        egui::CornerRadius::same(255),
        off_fill.lerp_to_gamma(on_fill, amount),
    );
    painter.rect_stroke(
        track,
        egui::CornerRadius::same(255),
        Stroke::new(1.0_f32, BORDER),
        egui::StrokeKind::Inside,
    );
    let knob_radius = 7.0;
    let knob_x = egui::lerp(
        (track.left() + knob_radius + 2.0)..=(track.right() - knob_radius - 2.0),
        amount,
    );
    painter.circle_filled(
        egui::pos2(knob_x, track.center().y),
        knob_radius,
        TEXT.lerp_to_gamma(BACKGROUND, amount * 0.5),
    );
    response
}

/// A rounded card container.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(14, 10))
}

/// A rail-style navigation button: filled accent square when active, quiet
/// transparent icon button otherwise. Icon-only; pair with a hover tooltip.
pub fn nav_icon_tab(active: bool, glyph: &str) -> AnimatedButton {
    let rounding = egui::CornerRadius::same(8);
    let square = egui::vec2(36.0, 36.0);
    if active {
        animated_button(
            egui::Button::new(icon(glyph, 16.0).color(BACKGROUND))
                .min_size(square)
                .corner_radius(rounding),
            PRIMARY,
            PRIMARY_HOVER,
            Stroke::NONE,
            Stroke::NONE,
        )
        .padding(egui::vec2(0.0, 0.0))
    } else {
        animated_button(
            egui::Button::new(icon(glyph, 16.0).color(TEXT_MUTED))
                .min_size(square)
                .corner_radius(rounding),
            Color32::TRANSPARENT,
            SURFACE_HOVER,
            Stroke::NONE,
            Stroke::NONE,
        )
        .padding(egui::vec2(0.0, 0.0))
    }
}

/// A dashboard-style navigation tab: filled accent pill when active,
/// quiet transparent text button otherwise.
pub fn nav_tab(active: bool, glyph: &str, label: &str) -> AnimatedButton {
    let rounding = egui::CornerRadius::same(255);
    if active {
        animated_button(
            egui::Button::new(icon_text_job(glyph, label, BACKGROUND)).corner_radius(rounding),
            PRIMARY,
            PRIMARY_HOVER,
            Stroke::NONE,
            Stroke::NONE,
        )
    } else {
        animated_button(
            egui::Button::new(icon_text_job(glyph, label, TEXT_MUTED)).corner_radius(rounding),
            Color32::TRANSPARENT,
            SURFACE_HOVER,
            Stroke::NONE,
            Stroke::NONE,
        )
    }
}

/// A small pill showing a colored dot and label.
pub fn status_pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.16))
        .corner_radius(egui::CornerRadius::same(255))
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let (dot_rect, _) =
                    ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
                ui.painter().circle_filled(dot_rect.center(), 3.0, color);
                ui.label(egui::RichText::new(text).color(color).size(12.0));
            });
        });
}

/// A self-contained stat box: icon + label with the value on the same row, and a
/// consumption meter
/// that shifts from green to yellow to red as `fill` (0..1) rises. `None` renders
/// an empty track for stats without a consumption measure (e.g. uptime).
pub fn stat_card(
    ui: &mut egui::Ui,
    glyph: &str,
    label: &str,
    value: &str,
    fill: Option<f32>,
) -> egui::InnerResponse<()> {
    card().show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            ui.label(icon(glyph, 14.0).color(TEXT_MUTED));
            ui.label(
                egui::RichText::new(label.to_uppercase())
                    .size(11.0)
                    .color(TEXT_MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(value).size(15.0).strong().color(TEXT))
                        .truncate(),
                );
            });
        });
        meter_bar(ui, label, fill);
    })
}

/// A thin consumption gauge under a stat. Green below 60%, yellow to 85%, red above.
fn meter_bar(ui: &mut egui::Ui, id_salt: &str, fill: Option<f32>) {
    let height = 5.0;
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::hover(),
    );
    ui.painter().rect_filled(rect, 3.0, CONSOLE_BG);
    let Some(target) = fill else {
        return;
    };
    let smooth =
        ui.ctx()
            .animate_value_with_time(ui.id().with(("stat_meter", id_salt)), target, 0.25);
    let width = rect.width() * smooth.clamp(0.0, 1.0);
    if width < 1.0 {
        return;
    }
    let color = if smooth < 0.6 {
        PRIMARY
    } else if smooth < 0.85 {
        WARN
    } else {
        ERROR
    };
    ui.painter().rect_filled(
        egui::Rect::from_min_size(rect.min, egui::vec2(width, height)),
        3.0,
        color,
    );
}
