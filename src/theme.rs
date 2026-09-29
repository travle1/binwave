//! Shared design tokens and egui theme setup.
//!
//! Tokens started from `docs/design/binwave-egui-v2-design/design-tokens.json`.
//! Later phases should read these tokens instead of embedding presentation values.
#![allow(dead_code)] // Later design phases consume the remaining tokens.

use egui::{
    vec2, Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin,
    Stroke, TextStyle,
};

pub struct Surface {
    pub sidebar: Color32,
    pub window: Color32,
    pub card: Color32,
    pub raised: Color32,
    pub control: Color32,
    pub control_alt: Color32,
    pub active: Color32,
    pub sidebar_selected: Color32,
}
pub struct Border {
    pub card: Color32,
    pub window: Color32,
    pub control: Color32,
}
pub struct Text {
    pub primary: Color32,
    pub secondary: Color32,
    pub secondary_alt: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub faint_alt: Color32,
    pub line_number: Color32,
    pub zero_byte: Color32,
    pub on_accent: Color32,
}
pub struct Accent {
    pub base: Color32,
    pub soft_alpha: f32,
    pub selection_alpha: f32,
}
pub struct Status {
    pub warning: Color32,
    pub warning_text: Color32,
    pub warning_soft_alpha: f32,
    pub warning_badge_alpha: f32,
    /// Tinted badge fill, e.g. the Inspector type badge.
    pub soft_alpha: f32,
    pub success: Color32,
    pub danger: Color32,
}
pub struct Syntax {
    pub key: Color32,
    pub punctuation: Color32,
    pub value: Color32,
    pub builtin_type: Color32,
    pub number: Color32,
    pub comment: Color32,
}
pub struct HexRegions {
    pub file_header: Color32,
    pub section_header: Color32,
    pub entry_a: Color32,
    pub entry_b: Color32,
}
pub struct Colors {
    pub surface: Surface,
    pub border: Border,
    pub text: Text,
    pub accent: Accent,
    pub status: Status,
    pub syntax: Syntax,
    pub hex_regions: HexRegions,
    pub scrollbar: Color32,
    /// Plot gridlines: just above the card, so they never compete with signals.
    pub plot_grid: Color32,
    /// The overview strip's whole-file envelope.
    pub overview_band: Color32,
}
pub struct Radii {
    pub window: u8,
    pub card: u8,
    pub inset: u8,
    pub control: u8,
    pub well: u8,
    pub row: u8,
    pub chip: u8,
    pub selection: u8,
    /// Range markers such as the overview strip's viewport box.
    pub marker: u8,
    pub pill: u16,
}
pub struct Sizes {
    pub window_default: [f32; 2],
    pub window_min: [f32; 2],
    pub frame_border: f32,
    pub frame_resize_grab: f32,
    pub sidebar_width: f32,
    pub header_height: f32,
    pub header_drag_left_clearance: f32,
    pub header_drag_right_clearance: f32,
    pub card_header_height: f32,
    pub decoder_header_actions_width: f32,
    pub decoder_header_actions_min_width: f32,
    pub tab_height: f32,
    pub control_height: f32,
    pub sidebar_row_height: f32,
    pub sidebar_icon: f32,
    pub sidebar_logo: f32,
    pub sidebar_avatar: f32,
    pub recent_file_row_height: f32,
    pub table_row_height: f32,
    pub hex_row_height: f32,
    pub inspector_width: f32,
    /// The painted ⋯ button: square size, dot radius and spacing.
    pub more_button: f32,
    pub more_dot: f32,
    pub more_dot_gap: f32,
    pub inspector_min_width: f32,
    pub inspector_max_width: f32,
    /// Inspector value chips (Selected bytes grid).
    pub inspector_chip_height: f32,
    /// The field card's type badge; longer type names end in "…".
    pub inspector_badge_max_width: f32,
    /// Label column of the Inspector's Definition rows.
    pub inspector_definition_label: f32,
    pub info_icon: f32,
    /// Start screen: column width, its top as a share of the height, drop zone
    /// height and icon, recent row height, and the drop zone's dashes.
    pub start_column_width: f32,
    pub start_top_ratio: f32,
    pub start_drop_zone_height: f32,
    pub start_icon: f32,
    pub start_row_height: f32,
    pub dash_length: f32,
    pub dash_gap: f32,
    /// Table panel: row height and initial column widths (# is sized for 7 digits,
    /// Size for 10+).
    pub table_row: f32,
    pub table_index_column: f32,
    pub table_offset_column: f32,
    pub table_size_column: f32,
    pub table_field_column: f32,
    pub table_item_column: f32,
    pub inspector_min_host_width: f32,
    pub structure_type_min_width: f32,
    pub structure_status_width: f32,
    pub plot_line_width: f32,
    /// Plot tick labels need at least this much room (egui_plot's defaults);
    /// gridlines are drawn only where a label is.
    pub plot_x_label_min: f32,
    pub plot_y_label_min: f32,
    /// Gridlines fade in between these spacings, like their labels.
    pub plot_grid_fade: [f32; 2],
    /// Signal's sample-index labels are wider, so they need more room.
    pub plot_signal_label_spacing: [f32; 2],
    /// Overview strip under Signal: height, gap above it, the viewport marker's
    /// minimum width, how close to its edge a drag resizes it, and wheel zoom
    /// per point scrolled.
    pub overview_height: f32,
    pub overview_gap: f32,
    pub overview_marker_min: f32,
    pub overview_edge_grab: f32,
    pub overview_wheel_zoom: f32,
    /// The overview's series picker scrolls past this height.
    pub overview_picker_height: f32,
    pub plot_point_radius: f32,
    pub plot_selected_radius: f32,
    pub library_details_width: f32,
    pub library_row_height: f32,
    pub library_preview_height: f32,
    pub library_preview_bytes: usize,
    pub library_drop_height: f32,
    pub library_drop_dash: f32,
    pub library_drop_gap: f32,
    pub caption_button: [f32; 2],
    pub toggle: [f32; 2],
}
pub struct Gaps {
    pub card_gap: f32,
    pub card_padding: f32,
    pub tab_gap: f32,
    pub sidebar_row_gap: f32,
    /// Vertical padding inside small chips.
    pub chip_padding_y: f32,
    pub sidebar_first_section_top: f32,
    pub sidebar_section_top: f32,
    /// Indent per nesting level in trees and field lists.
    pub tree_indent: f32,
    /// Gap between Inspector value chips and between its sections.
    pub inspector_gap: f32,
}
pub struct FontSizes {
    pub title: f32,
    pub card_title: f32,
    pub body: f32,
    pub control: f32,
    pub small: f32,
    pub label: f32,
    pub axis: f32,
    pub hex: f32,
    pub code: f32,
    pub code_line_height: f32,
    /// The Inspector's large field value.
    pub value: f32,
}
pub struct Theme {
    pub color: Colors,
    pub radius: Radii,
    pub size: Sizes,
    pub spacing: Gaps,
    pub font: FontSizes,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

pub const DARK: Theme = Theme {
    color: Colors {
        surface: Surface {
            sidebar: rgb(0x18, 0x18, 0x18),
            window: rgb(0x1C, 0x1C, 0x1C),
            card: rgb(0x23, 0x23, 0x23),
            raised: rgb(0x29, 0x29, 0x29),
            control: rgb(0x30, 0x30, 0x30),
            control_alt: rgb(0x38, 0x38, 0x38),
            active: rgb(0x35, 0x35, 0x35),
            sidebar_selected: rgb(0x2D, 0x2D, 0x2D),
        },
        border: Border {
            card: rgb(0x30, 0x30, 0x30),
            window: rgb(0x3A, 0x3A, 0x3A),
            control: rgb(0x3A, 0x3A, 0x3A),
        },
        text: Text {
            primary: rgb(0xEF, 0xEE, 0xEB),
            secondary: rgb(0xC5, 0xC4, 0xC2),
            secondary_alt: rgb(0xAD, 0xAC, 0xA9),
            muted: rgb(0x95, 0x94, 0x91),
            faint: rgb(0x86, 0x85, 0x82),
            faint_alt: rgb(0x77, 0x76, 0x74),
            line_number: rgb(0x72, 0x71, 0x6F),
            zero_byte: rgb(0x76, 0x75, 0x72),
            on_accent: rgb(0x18, 0x18, 0x18),
        },
        accent: Accent {
            base: rgb(0x8C, 0x9B, 0xFF),
            soft_alpha: 0.12,
            selection_alpha: 0.30,
        },
        status: Status {
            warning: rgb(0xF0, 0xB4, 0x5E),
            warning_text: rgb(0xF7, 0xD9, 0xAE),
            warning_soft_alpha: 0.08,
            warning_badge_alpha: 0.14,
            soft_alpha: 0.12,
            success: rgb(0x5F, 0xD0, 0xBC),
            danger: rgb(0xFF, 0x95, 0x85),
        },
        syntax: Syntax {
            key: rgb(0x9A, 0xA7, 0xFF),
            punctuation: rgb(0x86, 0x85, 0x82),
            value: rgb(0xEF, 0xEE, 0xEB),
            builtin_type: rgb(0x5F, 0xD0, 0xBC),
            number: rgb(0xF0, 0xB4, 0x5E),
            comment: rgb(0x77, 0x76, 0x74),
        },
        hex_regions: HexRegions {
            file_header: rgb(0xF0, 0xB4, 0x5E),
            section_header: rgb(0x9F, 0xB0, 0xFF),
            entry_a: rgb(0xEF, 0xEE, 0xEB),
            entry_b: rgb(0xC5, 0xC4, 0xC2),
        },
        scrollbar: rgb(0x42, 0x42, 0x41),
        plot_grid: rgb(0x30, 0x30, 0x30),
        overview_band: rgb(0x48, 0x48, 0x47),
    },
    radius: Radii {
        window: 14,
        card: 12,
        inset: 10,
        control: 8,
        well: 8,
        row: 7,
        chip: 6,
        selection: 5,
        marker: 2,
        pill: 999,
    },
    size: Sizes {
        window_default: [1600., 1000.],
        window_min: [1000., 650.],
        frame_border: 1.,
        frame_resize_grab: 6.,
        sidebar_width: 240.,
        header_height: 48.,
        header_drag_left_clearance: 370.,
        header_drag_right_clearance: 160.,
        card_header_height: 40.,
        decoder_header_actions_width: 156.,
        decoder_header_actions_min_width: 320.,
        tab_height: 28.,
        control_height: 28.,
        sidebar_row_height: 36.,
        sidebar_icon: 17.,
        sidebar_logo: 24.,
        sidebar_avatar: 28.,
        recent_file_row_height: 34.,
        table_row_height: 24.,
        hex_row_height: 24.,
        inspector_width: 320.,
        more_button: 24.,
        more_dot: 1.5,
        more_dot_gap: 5.,
        inspector_min_width: 300.,
        inspector_max_width: 340.,
        inspector_chip_height: 26.,
        inspector_badge_max_width: 160.,
        inspector_definition_label: 72.,
        info_icon: 14.,
        start_column_width: 560.,
        start_top_ratio: 0.3,
        start_drop_zone_height: 176.,
        start_icon: 32.,
        start_row_height: 44.,
        dash_length: 6.,
        dash_gap: 4.,
        table_row: 24.,
        table_index_column: 60.,
        table_offset_column: 104.,
        table_size_column: 96.,
        table_field_column: 116.,
        table_item_column: 220.,
        inspector_min_host_width: 720.,
        structure_type_min_width: 520.,
        structure_status_width: 22.,
        plot_line_width: 1.5,
        plot_x_label_min: 60.,
        plot_y_label_min: 20.,
        plot_grid_fade: [20., 80.],
        plot_signal_label_spacing: [100., 150.],
        overview_height: 26.,
        overview_gap: 8.,
        overview_marker_min: 4.,
        overview_edge_grab: 5.,
        overview_wheel_zoom: 0.004,
        overview_picker_height: 320.,
        plot_point_radius: 2.5,
        plot_selected_radius: 7.,
        library_details_width: 480.,
        library_row_height: 52.,
        library_preview_height: 240.,
        library_preview_bytes: 8192,
        library_drop_height: 90.,
        library_drop_dash: 6.,
        library_drop_gap: 4.,
        caption_button: [46., 36.],
        toggle: [28., 16.],
    },
    spacing: Gaps {
        card_gap: 8.,
        card_padding: 10.,
        tab_gap: 2.,
        sidebar_row_gap: 2.,
        chip_padding_y: 2.,
        sidebar_first_section_top: 22.,
        sidebar_section_top: 16.,
        tree_indent: 12.,
        inspector_gap: 6.,
    },
    font: FontSizes {
        title: 14.5,
        card_title: 13.,
        body: 13.,
        control: 12.5,
        small: 12.,
        label: 11.5,
        axis: 10.5,
        hex: 12.5,
        code: 12.,
        code_line_height: 18.,
        value: 20.,
    },
};

/// Gridlines only at labelled ticks: egui_plot's decimal marks, minus those too
/// close together for the axis to label them.
pub fn labeled_grid(plot: egui_plot::Plot<'_>) -> egui_plot::Plot<'_> {
    labeled_grid_spaced(plot, DARK.size.plot_x_label_min)
}

/// `labeled_grid` for a plot whose x-axis labels need `x_label_min` points.
pub fn labeled_grid_spaced(plot: egui_plot::Plot<'_>, x_label_min: f32) -> egui_plot::Plot<'_> {
    let t = &DARK;
    let [fade_min, fade_max] = t.size.plot_grid_fade;
    let spacer = move |label_min: f32| move |input| labeled_marks(input, fade_min, label_min);
    plot.grid_spacing(egui::Rangef::new(fade_min, fade_max))
        .grid_color(t.color.plot_grid)
        .x_grid_spacer(spacer(x_label_min))
        .y_grid_spacer(spacer(t.size.plot_y_label_min))
}

/// Decimal grid marks at least `label_min` points apart. `base_step_size` is the
/// value span of `grid_min` points, as egui_plot passes it.
fn labeled_marks(input: egui_plot::GridInput, grid_min: f32, label_min: f32) -> Vec<egui_plot::GridMark> {
    let points_per_value = f64::from(grid_min) / input.base_step_size;
    let mut marks = egui_plot::log_grid_spacer(10)(input);
    marks.retain(|mark| mark.step_size * points_per_value > f64::from(label_min));
    marks
}

pub fn dock_style(egui_style: &egui::Style) -> egui_dock::Style {
    let t = &DARK;
    let mut style = egui_dock::Style::from_egui(egui_style);
    style.dock_area_padding = Some(Margin::same(t.spacing.card_gap as i8));
    style.main_surface_border_stroke = Stroke::new(t.size.frame_border, t.color.border.card);
    style.main_surface_border_rounding = CornerRadius::same(t.radius.card);
    style.separator.width = t.spacing.card_gap;
    style.separator.color_idle = t.color.surface.window;
    style.separator.color_hovered = t.color.border.window;
    style.separator.color_dragged = t.color.accent.base;
    style.tab_bar.bg_fill = t.color.surface.card;
    style.tab_bar.height = t.size.card_header_height;
    style.tab_bar.inner_margin = Margin::same(((t.size.card_header_height - t.size.tab_height) / 2.) as i8);
    style.tab_bar.corner_radius = CornerRadius {
        nw: t.radius.card,
        ne: t.radius.card,
        sw: 0,
        se: 0,
    };
    style.tab_bar.hline_color = t.color.surface.card;
    style.tab.spacing = t.spacing.tab_gap;
    for (tab, fill, text) in [
        (&mut style.tab.active, t.color.surface.active, t.color.text.primary),
        (&mut style.tab.focused, t.color.surface.active, t.color.text.primary),
        (&mut style.tab.active_with_kb_focus, t.color.surface.active, t.color.text.primary),
        (&mut style.tab.focused_with_kb_focus, t.color.surface.active, t.color.text.primary),
        (&mut style.tab.inactive, t.color.surface.card, t.color.text.muted),
        (&mut style.tab.inactive_with_kb_focus, t.color.surface.card, t.color.text.muted),
        (&mut style.tab.hovered, t.color.surface.control, t.color.text.primary),
    ] {
        tab.bg_fill = fill;
        tab.text_color = text;
        tab.outline_color = Color32::TRANSPARENT;
        tab.corner_radius = CornerRadius::same(t.radius.control);
    }
    style.tab.tab_body.bg_fill = t.color.surface.card;
    style.tab.tab_body.stroke = Stroke::new(t.size.frame_border, t.color.border.card);
    style.tab.tab_body.corner_radius = CornerRadius {
        nw: 0,
        ne: 0,
        sw: t.radius.card,
        se: t.radius.card,
    };
    style.tab.tab_body.inner_margin = Margin::same(t.spacing.card_padding as i8);
    // The close target remains active; app.rs paints its small icon only when
    // the pointer is over the tab pill.
    style.buttons.close_tab_color = Color32::TRANSPARENT;
    style.buttons.close_tab_active_color = Color32::TRANSPARENT;
    style
}

/// A "more" (⋯) menu button. The dots are painted, since the interface font has
/// no ⋯ glyph and some views draw without the icon font installed.
pub fn more_menu(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    let button = egui::Button::new("").min_size(egui::vec2(DARK.size.more_button, DARK.size.more_button));
    let (response, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, add_contents);
    let color = ui.style().interact(&response).text_color();
    let center = response.rect.center();
    for step in [-1., 0., 1.] {
        ui.painter().circle_filled(center + egui::vec2(step * DARK.size.more_dot_gap, 0.), DARK.size.more_dot, color);
    }
    response.on_hover_text("More actions")
}

pub fn install(ctx: &Context) {
    let mut fonts = FontDefinitions::default();
    for (name, bytes) in [
        (
            "geist-regular",
            &include_bytes!("../assets/fonts/Geist-Regular.ttf")[..],
        ),
        (
            "geist-medium",
            &include_bytes!("../assets/fonts/Geist-Medium.ttf")[..],
        ),
        (
            "geist-semibold",
            &include_bytes!("../assets/fonts/Geist-SemiBold.ttf")[..],
        ),
        (
            "geist-mono",
            &include_bytes!("../assets/fonts/GeistMono-Regular.ttf")[..],
        ),
    ] {
        fonts
            .font_data
            .insert(name.into(), FontData::from_static(bytes).into());
    }
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "geist-regular".into());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "geist-mono".into());
    for (family, name) in [
        ("Geist Medium", "geist-medium"),
        ("Geist SemiBold", "geist-semibold"),
        ("Geist Mono", "geist-mono"),
    ] {
        fonts
            .families
            .insert(FontFamily::Name(family.into()), vec![name.into()]);
    }
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);

    let t = &DARK;
    let c = &t.color;
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.override_text_color = Some(c.text.primary);
    v.weak_text_color = Some(c.text.muted);
    v.hyperlink_color = c.accent.base;
    v.faint_bg_color = c.surface.raised;
    v.extreme_bg_color = c.surface.window;
    v.text_edit_bg_color = Some(c.surface.window);
    v.code_bg_color = c.surface.window;
    v.warn_fg_color = c.status.warning;
    v.error_fg_color = c.status.danger;
    v.window_corner_radius = CornerRadius::same(t.radius.window);
    v.window_fill = c.surface.card;
    v.window_stroke = Stroke::new(1., c.border.window);
    v.menu_corner_radius = CornerRadius::same(t.radius.control);
    v.panel_fill = c.surface.window;
    v.selection.bg_fill = c.accent.base.gamma_multiply(c.accent.selection_alpha);
    v.selection.stroke = Stroke::new(1., c.accent.base);
    for (widget, fill, text) in [
        (
            &mut v.widgets.noninteractive,
            c.surface.card,
            c.text.primary,
        ),
        (&mut v.widgets.inactive, c.surface.control, c.text.secondary),
        (
            &mut v.widgets.hovered,
            c.surface.control_alt,
            c.text.primary,
        ),
        (&mut v.widgets.active, c.surface.active, c.text.primary),
        (&mut v.widgets.open, c.surface.active, c.text.primary),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = Stroke::NONE;
        widget.corner_radius = CornerRadius::same(t.radius.control);
        widget.fg_stroke = Stroke::new(1., text);
    }
    style.spacing.item_spacing = vec2(t.spacing.card_gap, t.spacing.sidebar_row_gap);
    style.spacing.button_padding = vec2(
        t.spacing.card_padding,
        (t.size.control_height - t.font.control) / 4.,
    );
    style.spacing.interact_size.y = t.size.control_height;
    style.spacing.window_margin = Margin::same(t.spacing.card_padding as i8);
    style.text_styles.insert(
        TextStyle::Body,
        FontId::new(t.font.body, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(t.font.control, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(t.font.small, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(t.font.title, FontFamily::Name("Geist SemiBold".into())),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(t.font.code, FontFamily::Monospace),
    );
    ctx.set_style_of(egui::Theme::Dark, style);
    ctx.set_theme(egui::ThemePreference::Dark);
}

#[cfg(test)]
mod tests {
    #[test]
    fn gridlines_sit_only_at_ticks_the_axis_labels() {
        // 1,000 values across 1,000 points: one value per point.
        let input = || egui_plot::GridInput { bounds: (0., 1000.), base_step_size: 20. };
        let x = labeled_marks(input(), 20., 60.);
        assert!(x.iter().all(|mark| mark.step_size >= 100.), "no unlabelled 10-value lines: {x:?}");
        assert_eq!(x.iter().filter(|mark| mark.value > 0. && mark.value < 1000.).count(), 9);
        let y = labeled_marks(input(), 20., 20.);
        assert!(y.iter().all(|mark| mark.step_size >= 100.), "10 points apart is too close to label");
        // Zoomed in: 10 values across 1,000 points puts labels every value.
        let zoomed = labeled_marks(egui_plot::GridInput { bounds: (0., 10.), base_step_size: 0.2 }, 20., 60.);
        assert!(zoomed.iter().any(|mark| mark.step_size == 1.));
    }

    use super::*;

    #[test]
    fn installs_fonts_and_dark_visuals() {
        let ctx = Context::default();
        install(&ctx);
        let style = ctx.style_of(egui::Theme::Dark);
        assert_eq!(style.visuals.panel_fill, DARK.color.surface.window);
        assert_eq!(style.visuals.selection.stroke.color, DARK.color.accent.base);
        assert_eq!(style.spacing.interact_size.y, DARK.size.control_height);
        let dock = dock_style(&style);
        assert_eq!(dock.separator.width, DARK.spacing.card_gap);
        assert_eq!(dock.separator.color_idle, DARK.color.surface.window);
        assert_eq!(dock.tab.tab_body.bg_fill, DARK.color.surface.card);
    }
}
