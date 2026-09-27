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
        // eframe does not expose the chosen surface alpha mode to the app.
        // Stay square on X11 (including an explicit X11 backend override),
        // where compositor support cannot be confirmed from this API.
        let available = transparent_corners_for(
            cfg!(target_os = "windows"),
            cfg!(target_os = "macos"),
            cfg!(target_os = "linux"),
            std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
            std::env::var_os("WAYLAND_DISPLAY").is_some(),
            std::env::var("WINIT_UNIX_BACKEND").ok().as_deref(),
        );
        log::info!(
            "Window frame: {} corners (platform transparency assessment)",
            if available { "rounded" } else { "square" }
        );
        available
    })
}

fn transparent_corners_for(
    windows: bool,
    macos: bool,
    linux: bool,
    session: Option<&str>,
    wayland_display: bool,
    backend_override: Option<&str>,
) -> bool {
    windows
        || macos
        || linux && session == Some("wayland") && wayland_display && backend_override != Some("x11")
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

fn header_regions(
    rect: Rect,
    button_width: f32,
    button_height: f32,
    grab: f32,
) -> (Rect, Rect, Rect, Rect) {
    let button_top = rect.center().y - button_height / 2.;
    let close = Rect::from_min_size(
        pos2(rect.right() - button_width, button_top),
        vec2(button_width, button_height),
    );
    let maximize = close.translate(vec2(-button_width, 0.));
    let minimize = maximize.translate(vec2(-button_width, 0.));
    // Keep the draggable background clear of the file, sidebar and overflow
    // controls. An overlapping drag interaction captures their pointer press.
    let drag_left = rect.left() + DARK.size.header_drag_left_clearance;
    let drag_right = (minimize.left() - DARK.size.header_drag_right_clearance).max(drag_left);
    let drag = Rect::from_min_max(
        pos2(drag_left, rect.top() + grab),
        pos2(drag_right, rect.bottom()),
    );
    (minimize, maximize, close, drag)
}

pub fn header(ui: &mut Ui, content: impl FnOnce(&mut Ui)) {
    let t = &DARK;
    let maximized = ui.ctx().input(|input| input.viewport().maximized.unwrap_or(false));
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), t.size.header_height),
        Sense::hover(),
    );
    let button_width = t.size.caption_button[0];
    let button_height = t.size.caption_button[1];
    let grab = t.size.frame_resize_grab;
    let (minimize, maximize, close, drag_rect) =
        header_regions(rect, button_width, button_height, grab);
    let drag = ui.interact(
        drag_rect,
        ui.id().with("header_drag"),
        Sense::click_and_drag(),
    );
    let content_rect = Rect::from_min_max(
        pos2(rect.left() + t.spacing.card_padding, rect.top()),
        pos2(minimize.left() - t.spacing.card_padding, rect.bottom()),
    );
    let mut child = ui.new_child(egui::UiBuilder::new()
        .max_rect(content_rect)
        .layout(egui::Layout::left_to_right(egui::Align::Center)));
    content(&mut child);
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
    use crate::egui_compat::ContextRun as _;

    fn frame(ctx: &egui::Context, events: Vec<egui::Event>) -> Vec<ViewportCommand> {
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0., 0.), vec2(1600., 1000.))),
                events,
                ..Default::default()
            },
            |_ctx, ui| show(ui, |ui| header(ui, |_| {})),
        );
        output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .clone()
    }

    fn pointer_button(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        }
    }

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

    #[test]
    fn caption_buttons_have_documented_size_and_leave_a_drag_region() {
        let header = Rect::from_min_size(pos2(0., 0.), vec2(1600., DARK.size.header_height));
        let (minimize, maximize, close, drag) = header_regions(
            header,
            DARK.size.caption_button[0],
            DARK.size.caption_button[1],
            DARK.size.frame_resize_grab,
        );
        for button in [minimize, maximize, close] {
            assert_eq!(button.size(), vec2(46., 36.));
            assert_eq!(button.center().y, header.center().y);
        }
        assert_eq!(close.right(), header.right());
        assert_eq!(drag.right(), minimize.left() - DARK.size.header_drag_right_clearance);
        assert!(drag.contains(pos2(700., header.center().y)));
        assert!(!drag.contains(minimize.center()));
    }

    #[test]
    fn transparency_falls_back_to_square_corners_on_x11() {
        assert!(transparent_corners_for(
            true, false, false, None, false, None
        ));
        assert!(transparent_corners_for(
            false, true, false, None, false, None
        ));
        assert!(transparent_corners_for(
            false,
            false,
            true,
            Some("wayland"),
            true,
            None
        ));
        assert!(!transparent_corners_for(
            false,
            false,
            true,
            Some("x11"),
            false,
            None
        ));
        assert!(!transparent_corners_for(
            false,
            false,
            true,
            Some("wayland"),
            true,
            Some("x11")
        ));
    }

    #[test]
    fn caption_close_and_edge_resize_emit_native_viewport_commands() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        frame(&ctx, vec![]);
        let close = pos2(1560., 30.);
        frame(
            &ctx,
            vec![
                egui::Event::PointerMoved(close),
                pointer_button(close, true),
            ],
        );
        let commands = frame(&ctx, vec![pointer_button(close, false)]);
        assert!(commands.contains(&ViewportCommand::Close), "{commands:?}");

        let edge = pos2(2., 500.);
        let commands = frame(
            &ctx,
            vec![egui::Event::PointerMoved(edge), pointer_button(edge, true)],
        );
        assert!(
            commands.contains(&ViewportCommand::BeginResize(ResizeDirection::West)),
            "{commands:?}"
        );
    }

    #[test]
    fn header_drag_and_caption_buttons_emit_window_commands() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        frame(&ctx, vec![]);
        let drag = pos2(700., 30.);
        let commands = frame(
            &ctx,
            vec![egui::Event::PointerMoved(drag), pointer_button(drag, true)],
        );
        assert!(
            commands.contains(&ViewportCommand::StartDrag),
            "{commands:?}"
        );
        frame(&ctx, vec![pointer_button(drag, false)]);

        let minimize = pos2(1470., 30.);
        frame(
            &ctx,
            vec![
                egui::Event::PointerMoved(minimize),
                pointer_button(minimize, true),
            ],
        );
        let commands = frame(&ctx, vec![pointer_button(minimize, false)]);
        assert!(
            commands.contains(&ViewportCommand::Minimized(true)),
            "{commands:?}"
        );

        let maximize = pos2(1520., 30.);
        frame(
            &ctx,
            vec![
                egui::Event::PointerMoved(maximize),
                pointer_button(maximize, true),
            ],
        );
        let commands = frame(&ctx, vec![pointer_button(maximize, false)]);
        assert!(
            commands.contains(&ViewportCommand::Maximized(true)),
            "{commands:?}"
        );
    }

    #[test]
    fn header_controls_receive_clicks_outside_drag_region() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut clicked = false;
        for events in [
            vec![],
            vec![egui::Event::PointerMoved(pos2(50., 30.)), pointer_button(pos2(50., 30.), true)],
            vec![pointer_button(pos2(50., 30.), false)],
        ] {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(pos2(0., 0.), vec2(1600., 1000.))),
                    events,
                    ..Default::default()
                },
                |_ctx, ui| show(ui, |ui| header(ui, |ui| {
                    if ui.button("Open file").clicked() { clicked = true; }
                })),
            );
        }
        assert!(clicked, "header drag must not cover interactive controls");
    }

    #[test]
    fn double_clicking_header_toggles_maximize() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        frame(&ctx, vec![]);
        let header = pos2(700., 30.);
        frame(
            &ctx,
            vec![
                egui::Event::PointerMoved(header),
                pointer_button(header, true),
            ],
        );
        frame(&ctx, vec![pointer_button(header, false)]);
        frame(&ctx, vec![pointer_button(header, true)]);
        let commands = frame(&ctx, vec![pointer_button(header, false)]);
        assert!(
            commands.contains(&ViewportCommand::Maximized(true)),
            "{commands:?}"
        );
    }
}
