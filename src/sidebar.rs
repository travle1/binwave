//! Native workspace navigation; file operations remain owned by the app.

use crate::file_dialog::RecentPair;
use crate::theme::DARK;
use egui::{pos2, vec2, Align2, CornerRadius, FontFamily, FontId, Rect, Response, Sense, Ui};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Collapse,
    OpenFile,
    Search,
    DecoderLibrary,
    Recent(usize),
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecentSection {
    Today,
    Yesterday,
    PreviousSevenDays,
    Earlier,
}

impl RecentSection {
    const ALL: [Self; 4] = [Self::Today, Self::Yesterday, Self::PreviousSevenDays, Self::Earlier];

    fn label(self) -> &'static str {
        match self {
            Self::Today => "Today",
            Self::Yesterday => "Yesterday",
            Self::PreviousSevenDays => "Previous 7 days",
            Self::Earlier => "Earlier",
        }
    }
}

fn recent_section(opened: u64, now: u64) -> RecentSection {
    if opened == 0 {
        return RecentSection::Earlier;
    }
    let today = now / 86_400;
    let day = opened / 86_400;
    match today.saturating_sub(day) {
        0 => RecentSection::Today,
        1 => RecentSection::Yesterday,
        2..=7 => RecentSection::PreviousSevenDays,
        _ => RecentSection::Earlier,
    }
}

fn icon_font() -> FontId {
    FontId::new(DARK.size.sidebar_icon, FontFamily::Name("phosphor".into()))
}

fn icon_button(ui: &mut Ui, icon: &str, tip: &str) -> Response {
    let t = &DARK;
    let side = t.size.sidebar_avatar;
    let (rect, response) = ui.allocate_exact_size(vec2(side, side), Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(t.radius.control), t.color.surface.active);
    }
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, icon_font(), t.color.text.secondary);
    response.on_hover_text(tip)
}

fn row(ui: &mut Ui, icon: &str, label: &str, shortcut: Option<&str>, selected: bool, partial: bool) -> Response {
    let t = &DARK;
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), t.size.sidebar_row_height), Sense::click());
    if selected || response.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(t.radius.control),
            if selected { t.color.surface.sidebar_selected } else { t.color.surface.active });
    }
    let left = rect.left() + t.spacing.card_padding;
    let center = rect.center().y;
    ui.painter().text(pos2(left, center), Align2::LEFT_CENTER, icon, icon_font(), t.color.text.secondary);
    let label_left = left + t.size.sidebar_icon + t.spacing.card_padding;
    let right_reserve = if shortcut.is_some() || partial { t.size.sidebar_row_height } else { t.spacing.card_padding };
    let clip = Rect::from_min_max(pos2(label_left, rect.top()), pos2(rect.right() - right_reserve, rect.bottom()));
    ui.painter().with_clip_rect(clip).text(pos2(label_left, center), Align2::LEFT_CENTER,
        label, FontId::new(t.font.body, FontFamily::Proportional),
        if selected { t.color.text.primary } else { t.color.text.secondary });
    if let Some(shortcut) = shortcut {
        ui.painter().text(pos2(rect.right() - t.spacing.card_padding, center), Align2::RIGHT_CENTER,
            shortcut, FontId::new(t.font.label, FontFamily::Proportional), t.color.text.faint);
    }
    if partial {
        ui.painter().circle_filled(pos2(rect.right() - t.spacing.card_padding, center),
            t.size.frame_resize_grab / 2., t.color.status.warning);
    }
    response
}

fn section_label(ui: &mut Ui, label: &str, first: bool) {
    let t = &DARK;
    ui.add_space(if first { t.spacing.sidebar_first_section_top } else { t.spacing.sidebar_section_top });
    ui.label(egui::RichText::new(label).size(t.font.label).color(t.color.text.faint));
    ui.add_space(t.spacing.sidebar_row_gap);
}

pub fn show(ui: &mut Ui, recent: &[RecentPair], current: Option<&Path>, partial: bool, username: &str) -> Option<Action> {
    let t = &DARK;
    let mut action = None;
    ui.set_min_width(t.size.sidebar_width - 2. * t.spacing.card_padding);
    ui.set_max_width(t.size.sidebar_width - 2. * t.spacing.card_padding);
    ui.horizontal(|ui| {
        let (logo, _) = ui.allocate_exact_size(vec2(t.size.sidebar_logo, t.size.sidebar_logo), Sense::hover());
        ui.painter().rect_filled(logo, CornerRadius::same(t.radius.chip), t.color.accent.base);
        ui.painter().text(logo.center(), Align2::CENTER_CENTER, "B",
            FontId::new(t.font.card_title, FontFamily::Name("Geist SemiBold".into())), t.color.text.on_accent);
        ui.label(egui::RichText::new("BinWave").size(t.font.title).strong().color(t.color.text.primary));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if icon_button(ui, egui_phosphor::regular::SIDEBAR_SIMPLE, "Collapse sidebar (Ctrl+B)").clicked() {
                action = Some(Action::Collapse);
            }
        });
    });
    ui.add_space(t.spacing.sidebar_first_section_top);
    for (icon, label, shortcut, selected, value) in [
        (egui_phosphor::regular::FOLDER_OPEN, "Open file", Some("Ctrl O"), false, Action::OpenFile),
        (egui_phosphor::regular::MAGNIFYING_GLASS, "Search", Some("Ctrl K"), false, Action::Search),
        (egui_phosphor::regular::BOOK_OPEN, "Decoder library", None, false, Action::DecoderLibrary),
    ] {
        if row(ui, icon, label, shortcut, selected, false).clicked() { action = Some(value); }
        ui.add_space(t.spacing.sidebar_row_gap);
    }

    let footer_height = t.size.sidebar_row_height + t.spacing.card_padding;
    let scroll_height = (ui.available_height() - footer_height).max(t.size.sidebar_row_height);
    egui::ScrollArea::vertical().max_height(scroll_height).show(ui, |ui| {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let mut first = true;
        for section in RecentSection::ALL {
            let items = recent.iter().enumerate().filter(|(_, pair)| recent_section(pair.opened_at_unix_secs, now) == section)
                .collect::<Vec<_>>();
            if items.is_empty() { continue; }
            section_label(ui, section.label(), first);
            first = false;
            for (index, pair) in items {
                let name = pair.data.file_name().unwrap_or(pair.data.as_os_str()).to_string_lossy();
                let active = current.is_some_and(|path| path == pair.data);
                if row(ui, egui_phosphor::regular::FILE, &name, None, active, active && partial)
                    .on_hover_text(pair.data.display().to_string()).clicked() {
                    action = Some(Action::Recent(index));
                }
                ui.add_space(t.spacing.sidebar_row_gap);
            }
        }
        if recent.is_empty() {
            section_label(ui, "Recent files", true);
            ui.label(egui::RichText::new("Files you open will appear here.").size(t.font.small).color(t.color.text.faint));
        }
    });
    ui.add_space((ui.available_height() - footer_height).max(0.));
    ui.horizontal(|ui| {
        let (avatar, _) = ui.allocate_exact_size(vec2(t.size.sidebar_avatar, t.size.sidebar_avatar), Sense::hover());
        ui.painter().circle_filled(avatar.center(), avatar.width() / 2., t.color.surface.active);
        let initial = username.chars().next().unwrap_or('U').to_uppercase().to_string();
        ui.painter().text(avatar.center(), Align2::CENTER_CENTER, initial,
            FontId::new(t.font.body, FontFamily::Proportional), t.color.text.primary);
        ui.label(egui::RichText::new(username).size(t.font.body).color(t.color.text.secondary));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if icon_button(ui, egui_phosphor::regular::GEAR_SIX, "Settings").clicked() {
                action = Some(Action::Settings);
            }
        });
    });
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_sections_follow_saved_open_time() {
        let now = 20 * 86_400 + 12 * 3600;
        assert_eq!(recent_section(now, now), RecentSection::Today);
        assert_eq!(recent_section(now - 86_400, now), RecentSection::Yesterday);
        assert_eq!(recent_section(now - 7 * 86_400, now), RecentSection::PreviousSevenDays);
        assert_eq!(recent_section(0, now), RecentSection::Earlier);
    }
}
