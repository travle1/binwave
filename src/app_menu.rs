//! One command hierarchy for desktop egui and the browser's user-activation-safe DOM bar.
#[cfg(test)]
use crate::egui_compat::ContextRun as _;
use crate::workspace_layout::Pane;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    OpenFile,
    OpenRecent,
    SimpleDemo,
    AdvancedDemo,
    SensorDemo,
    SaveCopy,
    Extract,
    Cancel,
    OpenDecoder,
    SaveDecoder,
    SaveDecoderAs,
    NewDecoder,
    Apply,
    Code,
    Example,
    Undo,
    Redo,
    CopyHex,
    CodeUndo,
    CodeRedo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Hex,
    Structures,
    Plot,
    Samples,
    Inspector,
    TogglePane,
    Reset,
    PlotWorkspace,
    Auto,
    Stacked,
    Docked,
    Steps,
    Describe,
    Python,
    PythonExample,
    CsvConfig,
    Find,
    Replace,
    Jump,
    ClearCache,
    Help,
}
impl Command {
    pub fn pane(self) -> Option<Pane> {
        Some(match self {
            Self::Code => Pane::Layout,
            Self::Hex => Pane::Hex,
            Self::Structures => Pane::Structures,
            Self::Plot => Pane::Plot,
            Self::Samples => Pane::Table,
            Self::Inspector => Pane::Inspector,
            _ => return None,
        })
    }
    pub fn editor_action(self) -> Option<&'static str> {
        Some(match self {
            Self::CodeUndo => "Undo",
            Self::CodeRedo => "Redo",
            Self::Cut => "Cut",
            Self::Copy => "Copy",
            Self::Paste => "Paste",
            Self::SelectAll => "Select all",
            _ => return None,
        })
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct Item {
    pub label: String,
    pub command: Option<Command>,
    pub argument: usize,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    pub children: Vec<Item>,
}
impl Item {
    fn action(label: &str, command: Command, enabled: bool) -> Self {
        Self {
            label: label.into(),
            command: Some(command),
            argument: 0,
            enabled,
            checked: None,
            children: vec![],
        }
    }
    fn menu(label: &str, children: Vec<Self>) -> Self {
        Self {
            label: label.into(),
            command: None,
            argument: 0,
            enabled: true,
            checked: None,
            children,
        }
    }
}
#[derive(Default)]
pub struct State {
    pub native: bool,
    pub available: bool,
    pub file: bool,
    pub selection: bool,
    pub undo: bool,
    pub redo: bool,
    pub save_decoder: bool,
    pub apply: bool,
    pub busy: bool,
    pub recent: Vec<String>,
}
pub fn menus(s: &State) -> Vec<Item> {
    use Command::*;
    let action = Item::action;
    let mut data = vec![
        action("Open file...", OpenFile, s.available),
        action("Open simple demo", SimpleDemo, s.available),
        action("Open advanced demo", AdvancedDemo, s.available),
        action("Save bytes...", SaveCopy, s.available && s.file),
        action("Extract selection...", Extract, s.available && s.selection),
        action("Cancel operation", Cancel, s.busy),
    ];
    if s.native && !s.recent.is_empty() {
        let entries = s.recent.iter().enumerate().map(|(index, label)| {
            let mut item = action(label, OpenRecent, s.available);
            item.argument = index;
            item
        }).collect();
        data.insert(1, Item::menu("Open recent", entries));
    }
    let mut decoder = vec![
        action("Open decoder...", OpenDecoder, s.available),
        action(if s.native { "Save decoder" } else { "Save decoder..." },
            SaveDecoder, s.available && s.save_decoder),
        action("New decoder", NewDecoder, s.available),
        action(crate::decoder_ui::RUN_LABEL, Apply, s.apply),
        action("Show decoder", Code, true),
        Item::menu(
            "Examples",
            crate::source_editor::YAML_EXAMPLES
                .iter()
                .enumerate()
                .map(|(i, (label, _))| {
                    let mut item = action(label, Example, s.available);
                    item.argument = i;
                    item
                })
                .collect(),
        ),
    ];
    if s.native {
        decoder.insert(2, action("Save decoder as...", SaveDecoderAs, s.available && s.save_decoder));
    }
    let mut edit = vec![
        action("Undo byte edit", Undo, s.available && s.undo),
        action("Redo byte edit", Redo, s.available && s.redo),
        action("Copy selected hex", CopyHex, s.selection),
        Item::menu(
            "Decoder editing",
            vec![
                action("Undo", CodeUndo, true),
                action("Redo", CodeRedo, true),
                action("Cut", Cut, true),
                action("Copy", Copy, true),
                action("Paste", Paste, true),
                action("Select all", SelectAll, true),
            ],
        ),
    ];
    let mut view = vec![
        action("Hex", Hex, true),
        action("Structure", Structures, true),
        action("Signal", Plot, true),
        action("Table", Samples, true),
        action("Decoder", Code, true),
        action("Inspector", Inspector, true),
        action("Reset workspace", Reset, true),
    ];
    if s.native {
        data.extend([
            action("Open sensor demo", SensorDemo, s.available),
            action("Load CSV configuration...", CsvConfig, s.available),
        ]);
        decoder.extend([
            action("Edit decoding steps...", Steps, s.available),
            action("Describe selected bytes...", Describe, s.selection),
            action("New Python script", Python, s.available),
            action("Python waveform example", PythonExample, s.available),
        ]);
        edit.extend([
            action("Go to byte...", Jump, s.file),
            action("Find bytes or text...", Find, s.file),
            action("Replace selected bytes...", Replace, s.selection),
        ]);
        view.push(action("Plot and bytes workspace", PlotWorkspace, s.file));
        return vec![
            Item::menu("File", data),
            Item::menu("Edit", edit),
            Item::menu("View", view),
            Item::menu("Decode", decoder),
            Item::menu("Tools", vec![action("Clear disk cache", ClearCache, s.available)]),
            Item::menu("Help", vec![action("Decoding guide", Help, true)]),
        ];
    } else {
        decoder.extend([
            action("New Python script", Python, s.available),
            action("Python byte preview example", PythonExample, s.available),
        ]);
        view.push(Item::menu(
            "Arrangement",
            vec![
                action("Auto", Auto, true),
                action("Stacked", Stacked, true),
                action("Docked", Docked, true),
            ],
        ));
    }
    vec![
        Item::menu("Open data", data),
        Item::menu("Decoder", decoder),
        Item::menu("Edit", edit),
        Item::menu("View", view),
        Item::menu("Help", vec![action("Decoder guide", Help, true)]),
    ]
}
#[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Desktop only.
fn menu_entries(ui: &mut egui::Ui, items: &[Item], selected: &mut Option<(Command, usize)>) {
        ui.set_min_width(200.);
        for item in items {
            if let Some(command) = item.command {
                let shortcut = match command {
                    Command::OpenFile => "Ctrl+O",
                    Command::SaveCopy => "Ctrl+Shift+S",
                    Command::SaveDecoder => "Ctrl+S",
                    Command::Jump => "Ctrl+G",
                    _ => "",
                };
                if ui
                    .add_enabled(item.enabled, egui::Button::new(&item.label).shortcut_text(shortcut))
                    .clicked()
                {
                    *selected = Some((command, item.argument));
                    ui.close();
                }
            } else {
                ui.menu_button(&item.label, |ui| menu_entries(ui, &item.children, selected));
            }
        }
}

#[derive(Clone, Copy)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Desktop only.
pub enum OverflowSelection {
    Command(Command, usize),
    Panel(Pane),
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))] // Desktop only.
pub fn overflow_menu(ui: &mut egui::Ui, menus: &[Item], panels: &[(Pane, bool)]) -> Option<OverflowSelection> {
    let mut selected = None;
    let mut panel = None;
    let mut reset = false;
    let icon = egui::RichText::new(egui_phosphor::regular::DOTS_THREE)
        .font(egui::FontId::new(
            crate::theme::DARK.size.sidebar_icon,
            egui::FontFamily::Name("phosphor".into()),
        ));
    egui::containers::menu::MenuButton::from_button(egui::Button::new(icon).frame(false)).ui(ui, |ui| {
        ui.menu_button("Panels", |ui| {
            for (pane, visible) in panels {
                let mut checked = *visible;
                let response = ui.checkbox(&mut checked, pane.title());
                if response.clicked() {
                    panel = Some(*pane);
                    ui.close();
                }
            }
        });
        if ui.button("Reset layout").clicked() { reset = true; ui.close(); }
        ui.separator();
        for menu in menus {
            ui.menu_button(&menu.label, |ui| menu_entries(ui, &menu.children, &mut selected));
        }
        ui.separator();
        ui.weak(crate::APP_VERSION);
    });
    panel.map(OverflowSelection::Panel)
        .or_else(|| reset.then_some(OverflowSelection::Command(Command::Reset, 0)))
        .or_else(|| selected.map(|(command, argument)| OverflowSelection::Command(command, argument)))
}

#[cfg(test)]
pub fn native_bar(ui: &mut egui::Ui, menus: &[Item]) -> Option<(Command, usize)> {
    let mut selected = None;
    egui::MenuBar::new().ui(ui, |ui| {
        for menu in menus {
            ui.menu_button(&menu.label, |ui| menu_entries(ui, &menu.children, &mut selected));
        }
        let row = ui.min_rect();
        ui.painter().text(
            egui::pos2(ui.clip_rect().right() - 8., row.top() + row.height() / 2.),
            egui::Align2::RIGHT_CENTER,
            crate::APP_VERSION,
            egui::TextStyle::Small.resolve(ui.style()),
            ui.visuals().weak_text_color(),
        );
    });
    selected
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desktop_menu_bar_keeps_six_menus_in_a_compact_row() {
        let ctx = egui::Context::default();
        let model = menus(&State {
            native: true,
            ..Default::default()
        });
        let frame = || {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(360., 640.),
                    )),
                    ..Default::default()
                },
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        native_bar(ui, &model);
                    });
                },
            )
        };
        let _ = frame();
        let output = frame();
        let mut positions = vec![];
        for shape in &output.shapes {
            if let egui::Shape::Text(text) = &shape.shape {
                if ["File", "Edit", "View", "Decode", "Tools", "Help"].contains(&text.galley.text()) {
                    assert!(text.pos.x + text.galley.size().x <= 360.);
                    positions.push(text.pos.y);
                }
            }
        }
        assert_eq!(positions.len(), 6);
        assert!(positions.iter().all(|y| *y < 25.));
        assert!(positions.iter().all(|y| (*y - positions[0]).abs() < 1.));
        let texts: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_owned(),
                    text.pos,
                    text.galley.size(),
                )),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|(label, pos, size)|
                label == crate::APP_VERSION && pos.x + size.x <= 360. && pos.y < 25.),
            "version is not in the top-right menu row: {texts:?}"
        );
    }
    #[test]
    fn platforms_share_commands_with_desktop_menu_organization() {
        let web = menus(&State::default());
        let native = menus(&State {
            native: true,
            ..Default::default()
        });
        assert_eq!(
            web.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(),
            ["Open data", "Decoder", "Edit", "View", "Help"]
        );
        assert_eq!(native.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(),
            ["File", "Edit", "View", "Decode", "Tools", "Help"]);
        for (wi, ni) in [(0, 0), (1, 3), (2, 1), (3, 2), (4, 5)] {
            for command in web[wi].children.iter().filter_map(|i| i.command) {
                assert!(native[ni].children.iter().any(|i| i.command == Some(command)));
            }
        }
        assert!(!web[0].children[0].enabled);
        assert!(!web[0].children[2].enabled);
        assert!(web[1].children.iter().any(|item| item.label == "Show decoder"));
        assert!(web[3].children.iter().any(|item| item.label == "Decoder"));
        let active = menus(&State {
            available: true,
            file: true,
            selection: true,
            ..Default::default()
        });
        assert!(active[0].children[0].enabled);
        assert!(active[0].children[3].enabled);
    }
}
