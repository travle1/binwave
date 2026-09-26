//! Native window chrome shared by the workspace and future library screen.

use crate::theme::DARK;
use egui::{
    pos2, vec2, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Rect,
    ResizeDirection, Sense, Stroke, StrokeKind, Ui, ViewportCommand,
};
use std::sync::OnceLock;

fn transparent_corners() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        // Wayland compositors and the Windows/macOS window surfaces support
        // alpha. X11 has no reliable compositor query through eframe, so use
        // the conservative square frame there.
        let available = cfg!(target_os = "windows")
            || cfg!(target_os = "macos")
            || (cfg!(target_os = "linux")
                && std::env::var("XDG_SESSION_TYPE").is_ok_and(|value| value == "wayland")
                && std::env::var_os("WAYLAND_DISPLAY").is_some());
        log::info!(
            "Window frame: {} corners (platform transparency assessment)",
            if available { "rounded" } else { "square" }
        );
        available
    })
}

fn resize_direction(rect: Rect, pointer: egui::Pos2, grab: f32) -> Option<ResizeDirection> {
    if !rect.contains(pointer) {
        return None;
    }
    let west = pointer.x < rect.left() + grab;
    let east = pointer.x >= rect.right() - grab;
    let north = pointer.y < rect.top() + grab;
    let south = pointer.y >= rect.bottom() - grab;
    match (north, south, west, east) {
        (true, _, true, _) => Some(ResizeDirection::NorthWest),
        (true, _, _, true) => Some(ResizeDirection::NorthEast),
        (_, true, true, _) => Some(ResizeDirection::SouthWest),
        (_, true, _, true) => Some(ResizeDirection::SouthEast),
        (true, _, _, _) => Some(ResizeDirection::North),
        (_, true, _, _) => Some(ResizeDirection::South),
        (_, _, true, _) => Some(ResizeDirection::West),
        (_, _, _, true) => Some(ResizeDirection::East),
        _ => None,
    }
}

fn resize_cursor(direction: ResizeDirection) -> CursorIcon {
    match direction {
        ResizeDirection::North | ResizeDirection::South => CursorIcon::ResizeVertical,
        ResizeDirection::East | ResizeDirection::West => CursorIcon::ResizeHorizontal,
        ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::ResizeNeSw,
        ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::ResizeNwSe,
    }
}

fn caption_button(ui: &mut Ui, rect: Rect, id: &str, icon: &str, tip: &str, danger: bool) -> bool {
    let t = &DARK;
    let response = ui.interact(rect, ui.id().with(id), Sense::click());
    let fill = if response.hovered() {
        if danger {
            t.color.status.danger
        } else {
            t.color.surface.control_alt
        }
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(t.radius.control), fill);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        FontId::new(t.font.body, FontFamily::Name("phosphor".into())),
        if danger && response.hovered() {
            t.color.text.on_accent
        } else {
            t.color.text.secondary
        },
    );
    response.on_hover_text(tip).clicked()
}

fn header(ui: &mut Ui, maximized: bool) {
    let t = &DARK;
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), t.size.header_height),
        Sense::hover(),
    );
    let button_width = t.size.caption_button[0];
    let button_height = t.size.caption_button[1];
    let button_top = rect.center().y - button_height / 2.;
    let close = Rect::from_min_size(
        pos2(rect.right() - button_width, button_top),
        vec2(button_width, button_height),
    );
    let maximize = close.translate(vec2(-button_width, 0.));
    let minimize = maximize.translate(vec2(-button_width, 0.));
    let grab = t.size.frame_resize_grab;
    let drag_rect = Rect::from_min_max(
        pos2(rect.left() + grab, rect.top() + grab),
        pos2(minimize.left(), rect.bottom()),
    );
    let drag = ui.interact(
        drag_rect,
        ui.id().with("header_drag"),
        Sense::click_and_drag(),
    );
    ui.painter().text(
        pos2(rect.left() + t.spacing.card_padding, rect.center().y),
        Align2::LEFT_CENTER,
        "BinWave",
        FontId::new(t.font.title, FontFamily::Name("Geist SemiBold".into())),
        t.color.text.primary,
    );
    if drag.double_clicked() {
        ui.ctx()
            .send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    } else if drag.is_pointer_button_down_on()
        && ui.ctx().input(|input| input.pointer.primary_pressed())
    {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
    if caption_button(
        ui,
        minimize,
        "minimize",
        egui_phosphor::regular::MINUS,
        "Minimize",
        false,
    ) {
        ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true));
    }
    if caption_button(
        ui,
        maximize,
        "maximize",
        if maximized {
            egui_phosphor::regular::COPY
        } else {
            egui_phosphor::regular::SQUARE
        },
        if maximized { "Restore" } else { "Maximize" },
        false,
    ) {
        ui.ctx()
            .send_viewport_cmd(ViewportCommand::Maximized(!maximized));
    }
    if caption_button(ui, close, "close", egui_phosphor::regular::X, "Close", true) {
        ui.ctx().send_viewport_cmd(ViewportCommand::Close);
    }
}

pub fn show(ui: &mut Ui, content: impl FnOnce(&mut Ui)) {
    let t = &DARK;
    let maximized = ui
        .ctx()
        .input(|input| input.viewport().maximized.unwrap_or(false));
    let rounded = !maximized && transparent_corners();
    let viewport = ui.max_rect();
    let inset = if maximized {
        0
    } else {
        t.spacing.card_gap as i8
    };
    let frame = egui::Frame::new()
        .fill(t.color.surface.window)
        .stroke(if maximized {
            Stroke::NONE
        } else {
            Stroke::new(t.size.frame_border, t.color.border.window)
        })
        .corner_radius(CornerRadius::same(if rounded {
            t.radius.window
        } else {
            0
        }))
        .inner_margin(egui::Margin::same(inset));
    let margin = frame.total_margin();
    frame.show(ui, |ui| {
        ui.set_min_size(vec2(
            viewport.width() - margin.left - margin.right,
            viewport.height() - margin.top - margin.bottom,
        ));
        header(ui, maximized);
        content(ui);
    });
    if !maximized {
        let pointer = ui.ctx().input(|input| input.pointer.hover_pos());
        if let Some(direction) =
            pointer.and_then(|pos| resize_direction(viewport, pos, t.size.frame_resize_grab))
        {
            ui.ctx().set_cursor_icon(resize_cursor(direction));
            if ui.ctx().input(|input| input.pointer.primary_pressed()) {
                ui.ctx()
                    .send_viewport_cmd(ViewportCommand::BeginResize(direction));
            }
        }
    }
    if !maximized {
        ui.painter().rect_stroke(
            viewport,
            CornerRadius::same(if rounded { t.radius.window } else { 0 }),
            Stroke::new(t.size.frame_border, t.color.border.window),
            StrokeKind::Inside,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_resize_includes_corners_and_excludes_interior() {
        let rect = Rect::from_min_size(pos2(0., 0.), vec2(1600., 1000.));
        assert_eq!(
            resize_direction(rect, pos2(2., 2.), 6.),
            Some(ResizeDirection::NorthWest)
        );
        assert_eq!(
            resize_direction(rect, pos2(1598., 998.), 6.),
            Some(ResizeDirection::SouthEast)
        );
        assert_eq!(
            resize_direction(rect, pos2(800., 2.), 6.),
            Some(ResizeDirection::North)
        );
        assert_eq!(resize_direction(rect, pos2(800., 500.), 6.), None);
    }
}
