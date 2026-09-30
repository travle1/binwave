//! The start screen shown when no file is open: a drop zone with Open file… and
//! Try a sample file, recent files as rows, and links to decoders.
use crate::file_dialog::RecentPair;
use crate::theme::DARK;
use egui::{pos2, vec2, Align2, CornerRadius, FontFamily, FontId, Rect, RichText, Sense, Stroke, Ui};

/// Recent rows shown before "Show all".
pub const SHOWN_RECENT: usize = 8;

#[derive(Debug, PartialEq)]
pub enum Action {
    OpenFile,
    TrySample,
    Recent(usize),
    Remove(usize),
    NewDecoder,
    DecoderLibrary,
}

#[derive(Default)]
pub struct StartScreen {
    show_all: bool,
}

impl StartScreen {
    /// `enabled` is false while another operation is running.
    pub fn show(&mut self, ui: &mut Ui, recent: &[RecentPair], enabled: bool, now_unix_secs: u64) -> Option<Action> {
        let t = &DARK;
        let mut action = None;
        ui.add_space((ui.available_height() * t.size.start_top_ratio - t.size.start_drop_zone_height / 2.).max(t.spacing.card_gap));
        let width = t.size.start_column_width.min(ui.available_width() - 2. * t.spacing.card_padding);
        ui.vertical_centered(|ui| {
            ui.set_max_width(width);
            action = drop_zone(ui, enabled);
            if !recent.is_empty() {
                ui.add_space(2. * t.spacing.card_padding);
                if let Some(chosen) = self.recent(ui, recent, enabled, now_unix_secs) {
                    action = Some(chosen);
                }
            }
            ui.add_space(2. * t.spacing.card_padding);
            ui.horizontal(|ui| {
                // Centre the two links under the column.
                let links = ["New decoder", "Decoder library"];
                let font = FontId::proportional(t.font.body);
                let text = ui.fonts_mut(|fonts| links.iter().map(|link| fonts.layout_no_wrap((*link).into(),
                    font.clone(), t.color.accent.base).size().x).sum::<f32>());
                let dot = ui.fonts_mut(|fonts| fonts.layout_no_wrap(" · ".into(), font.clone(), t.color.text.faint).size().x);
                ui.add_space(((ui.available_width() - text - dot - 2. * ui.spacing().item_spacing.x) / 2.).max(0.));
                if ui.link(RichText::new(links[0]).size(t.font.body)).clicked() {
                    action = Some(Action::NewDecoder);
                }
                ui.label(RichText::new("·").size(t.font.body).color(t.color.text.faint));
                if ui.link(RichText::new(links[1]).size(t.font.body)).clicked() {
                    action = Some(Action::DecoderLibrary);
                }
            });
        });
        action
    }

    fn recent(&mut self, ui: &mut Ui, recent: &[RecentPair], enabled: bool, now: u64) -> Option<Action> {
        let t = &DARK;
        let mut action = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Recent").size(t.font.label).color(t.color.text.faint));
        });
        ui.add_space(t.spacing.sidebar_row_gap);
        let shown = if self.show_all { recent.len() } else { recent.len().min(SHOWN_RECENT) };
        for (index, pair) in recent.iter().enumerate().take(shown) {
            let response = recent_row(ui, pair, enabled, now);
            if response.clicked() {
                action = Some(Action::Recent(index));
            }
            response.context_menu(|ui| {
                if ui.button("Remove from recent").clicked() {
                    action = Some(Action::Remove(index));
                    ui.close();
                }
                if ui.button("Copy path").clicked() {
                    ui.ctx().copy_text(pair.data.display().to_string());
                    ui.close();
                }
            });
        }
        if recent.len() > SHOWN_RECENT {
            ui.add_space(t.spacing.sidebar_row_gap);
            let label = if self.show_all { "Show fewer".to_owned() } else {
                format!("Show all ({})", crate::format::count(recent.len()))
            };
            if ui.link(RichText::new(label).size(t.font.small)).clicked() {
                self.show_all = !self.show_all;
            }
        }
        action
    }
}

/// Dashed card with the file icon, Open file… and Try a sample file. Its border
/// turns accent while a file is dragged over the window.
fn drop_zone(ui: &mut Ui, enabled: bool) -> Option<Action> {
    let t = &DARK;
    let mut action = None;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), t.size.start_drop_zone_height), Sense::hover());
    let hovering = ui.input(|input| !input.raw.hovered_files.is_empty());
    ui.painter().rect_filled(rect, CornerRadius::same(t.radius.card), t.color.surface.card);
    dashed_rect(ui, rect, Stroke::new(t.size.frame_border,
        if hovering { t.color.accent.base } else { t.color.border.control }));
    let mut content = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(t.spacing.card_padding))
        .layout(egui::Layout::top_down(egui::Align::Center)));
    content.add_space(t.spacing.card_padding);
    content.label(RichText::new(egui_phosphor::regular::FILE_ARROW_UP)
        .font(FontId::new(t.size.start_icon, FontFamily::Name("phosphor".into())))
        .color(if hovering { t.color.accent.base } else { t.color.text.muted }));
    content.horizontal(|ui| {
        let text = if hovering { "Drop to open" } else { "Drop a file here to open it" };
        let font = FontId::proportional(t.font.body);
        let width = ui.fonts_mut(|fonts| fonts.layout_no_wrap(text.into(), font.clone(), t.color.text.secondary).size().x)
            + ui.spacing().item_spacing.x + t.size.info_icon;
        ui.add_space(((ui.available_width() - width) / 2.).max(0.));
        ui.label(RichText::new(text).size(t.font.body).color(t.color.text.secondary));
        ui.label(RichText::new(egui_phosphor::regular::INFO)
            .font(FontId::new(t.size.info_icon, FontFamily::Name("phosphor".into())))
            .color(t.color.text.muted))
            .on_hover_text("Files stay on this computer and open read-only until you save edits. \
                A decoder describes a file's structure so it can be shown as fields, tables and signals.");
    });
    content.add_space(t.spacing.card_padding);
    content.horizontal(|ui| {
        let open = egui::Button::new(RichText::new("Open file…").strong()).fill(ui.visuals().selection.bg_fill)
            .shortcut_text("Ctrl O").min_size(vec2(0., t.size.control_height));
        let sample = egui::Button::new("Try a sample file").min_size(vec2(0., t.size.control_height));
        // Centre the pair of buttons.
        let widths: f32 = ["Open file…  Ctrl O", "Try a sample file"].iter().map(|text| ui.fonts_mut(|fonts|
            fonts.layout_no_wrap((*text).into(), FontId::proportional(t.font.body), t.color.text.primary).size().x)
            + 2. * ui.spacing().button_padding.x).sum();
        ui.add_space(((ui.available_width() - widths - ui.spacing().item_spacing.x) / 2.).max(0.));
        if ui.add_enabled(enabled, open).clicked() {
            action = Some(Action::OpenFile);
        }
        if ui.add_enabled(enabled, sample)
            .on_hover_text("Open a small synthetic waveform with its decoder.")
            .clicked()
        {
            action = Some(Action::TrySample);
        }
    });
    action
}

/// A rounded rectangle outline drawn in dashes.
fn dashed_rect(ui: &Ui, rect: Rect, stroke: Stroke) {
    let t = &DARK;
    let radius = f32::from(t.radius.card).min(rect.height() / 2.);
    let mut points = Vec::new();
    let corners = [
        (pos2(rect.right() - radius, rect.top() + radius), -90.0_f32),
        (pos2(rect.right() - radius, rect.bottom() - radius), 0.),
        (pos2(rect.left() + radius, rect.bottom() - radius), 90.),
        (pos2(rect.left() + radius, rect.top() + radius), 180.),
    ];
    for (centre, start) in corners {
        for step in 0..=6 {
            let angle = (start + step as f32 * 15.).to_radians();
            points.push(centre + radius * vec2(angle.cos(), angle.sin()));
        }
    }
    points.push(points[0]);
    ui.painter().extend(egui::Shape::dashed_line(&points, stroke, t.size.dash_length, t.size.dash_gap));
}

/// Name, folder, decoder chip and when it was last opened. Missing files are dimmed.
fn recent_row(ui: &mut Ui, pair: &RecentPair, enabled: bool, now: u64) -> egui::Response {
    let t = &DARK;
    let exists = pair.data.is_file();
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), t.size.start_row_height),
        if enabled && exists { Sense::click() } else { Sense::hover() });
    let painter = ui.painter_at(rect);
    if response.hovered() && enabled && exists {
        painter.rect_filled(rect, CornerRadius::same(t.radius.control), t.color.surface.active);
    }
    let pad = t.spacing.card_padding;
    let centre = rect.center().y;
    let name = pair.data.file_name().unwrap_or(pair.data.as_os_str()).to_string_lossy();
    let folder = pair.data.parent().map(|parent| parent.display().to_string()).unwrap_or_default();
    let primary = if exists { t.color.text.primary } else { t.color.text.faint };
    painter.text(pos2(rect.left() + pad, centre), Align2::LEFT_CENTER, egui_phosphor::regular::FILE,
        FontId::new(t.size.sidebar_icon, FontFamily::Name("phosphor".into())), t.color.text.muted);
    let text_left = rect.left() + 2. * pad + t.size.sidebar_icon;
    // Right side, from the edge inward: when, then the decoder chip.
    let when = if exists { crate::format::ago(pair.opened_at_unix_secs, now) } else { "Not found".into() };
    let when = painter.text(pos2(rect.right() - pad, centre), Align2::RIGHT_CENTER, when,
        FontId::proportional(t.font.small), if exists { t.color.text.muted } else { t.color.status.warning });
    let mut right = when.left() - pad;
    if let Some(decoder) = &pair.decoder {
        let label = decoder.file_name().unwrap_or(decoder.as_os_str()).to_string_lossy().into_owned();
        let font = FontId::monospace(t.font.label);
        let galley = ui.fonts_mut(|fonts| fonts.layout_no_wrap(label, font, t.color.text.secondary));
        let max = t.size.inspector_badge_max_width.min(galley.size().x);
        let inset = vec2(pad / 2., t.spacing.chip_padding_y);
        let chip = Rect::from_center_size(pos2(right - (max + 2. * inset.x) / 2., centre),
            vec2(max, galley.size().y) + 2. * inset);
        painter.rect_filled(chip, CornerRadius::same(t.radius.chip), t.color.surface.control);
        painter.with_clip_rect(chip.shrink2(inset)).galley(chip.min + inset, galley, t.color.text.secondary);
        right = chip.left() - pad;
    }
    let clip = Rect::from_min_max(pos2(text_left, rect.top()), pos2(right, rect.bottom()));
    let name = painter.with_clip_rect(clip).text(pos2(text_left, centre), Align2::LEFT_CENTER, name,
        FontId::proportional(t.font.body), primary);
    // The folder fills what the name leaves, showing its end when it's too long.
    let folder_left = name.right() + pad;
    if folder_left < right {
        let font = FontId::proportional(t.font.small);
        let room = right - folder_left;
        let mut shown = folder.clone();
        let fits = |text: &str| ui.fonts_mut(|fonts| fonts.layout_no_wrap(text.into(), font.clone(),
            t.color.text.muted).size().x) <= room;
        if !fits(&shown) {
            let chars: Vec<char> = folder.chars().collect();
            let mut skip = 0;
            while skip < chars.len() && !fits(&format!("…{}", chars[skip..].iter().collect::<String>())) {
                skip += 1;
            }
            shown = format!("…{}", chars[skip..].iter().collect::<String>());
        }
        painter.text(pos2(folder_left, centre), Align2::LEFT_CENTER, shown, font, t.color.text.muted);
    }
    let hover = match &pair.decoder {
        Some(decoder) => format!("{}\nDecoder: {}", pair.data.display(), decoder.display()),
        None => pair.data.display().to_string(),
    };
    response.on_hover_text(if exists { hover } else { format!("{hover}\nThis file was moved or deleted.") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egui_compat::ContextRun as _;

    fn frame(ctx: &egui::Context, screen: &mut StartScreen, recent: &[RecentPair]) -> (Vec<String>, Option<Action>) {
        let mut action = None;
        let output = ctx.run(egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1200., 900.))),
            ..Default::default()
        }, |_ctx, ui| {
            egui::CentralPanel::default().show(ui, |ui| action = screen.show(ui, recent, true, 1_792_000_000));
        });
        let texts = output.shapes.iter().filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
            _ => None,
        }).collect();
        (texts, action)
    }

    #[test]
    fn recent_rows_show_name_decoder_and_age_then_show_all() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut recent: Vec<RecentPair> = (0..10).map(|index| {
            let data = dir.path().join(format!("capture{index}.bin"));
            std::fs::write(&data, b"x").unwrap();
            RecentPair { data, decoder: None, opened_at_unix_secs: 1_792_000_000 - 3 * 3600, plot_collection: None }
        }).collect();
        recent[0].decoder = Some(dir.path().join("layout.yaml"));
        recent[1].data = dir.path().join("moved.bin");
        let mut screen = StartScreen::default();
        let (texts, _) = frame(&ctx, &mut screen, &recent);
        for text in ["Open file…", "Try a sample file", "Recent", "capture0.bin", "layout.yaml", "3 h ago",
            "Not found", "Show all (10)", "New decoder", "Decoder library"] {
            assert!(texts.iter().any(|shown| shown == text), "missing {text}: {texts:?}");
        }
        assert!(!texts.iter().any(|shown| shown == "capture9.bin"), "8 rows before Show all");
        screen.show_all = true;
        let (texts, _) = frame(&ctx, &mut screen, &recent);
        assert!(texts.iter().any(|shown| shown == "capture9.bin"));
    }

    #[test]
    fn no_recent_files_shows_just_the_drop_zone_and_links() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let (texts, action) = frame(&ctx, &mut StartScreen::default(), &[]);
        assert!(action.is_none());
        assert!(!texts.iter().any(|shown| shown == "Recent"));
        assert!(texts.iter().any(|shown| shown == "Drop a file here to open it"));
    }
}
