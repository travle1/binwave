//! One command hierarchy for desktop egui and the browser's user-activation-safe DOM bar.
use crate::workspace_layout::Pane;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    OpenFile,
    SimpleDemo,
    SensorDemo,
    SaveCopy,
    Extract,
    Cancel,
    OpenDecoder,
    SaveDecoder,
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
    Reset,
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
    pub children: Vec<Item>,
}
impl Item {
    fn action(label: &str, command: Command, enabled: bool) -> Self {
        Self {
            label: label.into(),
            command: Some(command),
            argument: 0,
            enabled,
            children: vec![],
        }
    }
    fn menu(label: &str, children: Vec<Self>) -> Self {
        Self {
            label: label.into(),
            command: None,
            argument: 0,
            enabled: true,
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
}
pub fn menus(s: &State) -> Vec<Item> {
    use Command::*;
    let action = Item::action;
    let mut data = vec![
        action("Open file...", OpenFile, s.available),
        action("Open simple demo", SimpleDemo, s.available),
        action("Save edited copy...", SaveCopy, s.available && s.file),
        action("Extract selection...", Extract, s.available && s.selection),
        action("Cancel operation", Cancel, s.busy),
    ];
    let mut decoder = vec![
        action("Open decoder...", OpenDecoder, s.available),
        action(
            "Save decoder...",
            SaveDecoder,
            s.available && s.save_decoder,
        ),
        action("New decoder", NewDecoder, s.available),
        action("Apply decoder", Apply, s.apply),
        action("Code", Code, true),
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
    let mut edit = vec![
        action("Undo byte edit", Undo, s.available && s.undo),
        action("Redo byte edit", Redo, s.available && s.redo),
        action("Copy selected hex", CopyHex, s.selection),
        Item::menu(
            "Code editing",
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
        action("Decoded structure", Structures, true),
        action("Plot", Plot, true),
        action("Samples", Samples, true),
        action("Code", Code, true),
        action("Inspector", Inspector, true),
        action("Reset workspace", Reset, true),
    ];
    if s.native {
        data.push(Item::menu(
            "Advanced",
            vec![
                action("Open sensor demo", SensorDemo, s.available),
                action("Load CSV configuration...", CsvConfig, s.available),
            ],
        ));
        decoder.push(Item::menu(
            "Advanced",
            vec![
                action("Edit decoding steps...", Steps, s.available),
                action("Describe selected bytes...", Describe, s.selection),
                action("New Python script", Python, s.available),
                action("Python waveform example", PythonExample, s.available),
            ],
        ));
        edit.push(Item::menu(
            "Find and replace",
            vec![
                action("Go to byte...", Jump, s.file),
                action("Find bytes or text...", Find, s.file),
                action("Replace selected bytes...", Replace, s.selection),
            ],
        ));
        view.push(Item::menu(
            "Tools and help",
            vec![
                action("Clear disk cache", ClearCache, s.available),
                action("Decoding guide", Help, true),
            ],
        ));
    } else {
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
    ]
}
pub fn native_bar(ui: &mut egui::Ui, menus: &[Item]) -> Option<(Command, usize)> {
    fn entries(ui: &mut egui::Ui, items: &[Item], selected: &mut Option<(Command, usize)>) {
        ui.spacing_mut().interact_size.y = 30.;
        ui.set_min_width(220.);
        for item in items {
            if let Some(command) = item.command {
                if ui
                    .add_enabled(item.enabled, egui::Button::new(&item.label))
                    .clicked()
                {
                    *selected = Some((command, item.argument));
                    ui.close_menu();
                }
            } else {
                ui.menu_button(&item.label, |ui| entries(ui, &item.children, selected));
            }
        }
    }
    let mut selected = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().interact_size.y = 32.;
        ui.spacing_mut().button_padding = egui::vec2(10., 6.);
        ui.spacing_mut().item_spacing.x = 4.;
        if ui.available_width() > 520. {
            ui.strong("Binwave");
            ui.add_space(12.);
        }
        for menu in menus {
            ui.menu_button(&menu.label, |ui| entries(ui, &menu.children, &mut selected));
        }
    });
    selected
}
pub fn decoder_controls(ui: &mut egui::Ui, enabled: bool, busy: bool) -> Option<Command> {
    let mut result = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                enabled,
                egui::Button::new("Apply decoder").shortcut_text("F5"),
            )
            .clicked()
        {
            result = Some(Command::Apply);
        }
        if busy && ui.button("Stop").clicked() {
            result = Some(Command::Cancel);
        }
    });
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_bar_keeps_all_four_menus_on_one_row() {
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
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        native_bar(ui, &model);
                    });
                },
            )
        };
        let _ = frame();
        let output = frame();
        let mut positions = vec![];
        for shape in output.shapes {
            if let egui::Shape::Text(text) = shape.shape {
                if ["Open data", "Decoder", "Edit", "View"].contains(&text.galley.text()) {
                    assert!(text.pos.x + text.galley.size().x <= 360.);
                    positions.push(text.pos.y);
                }
            }
        }
        assert_eq!(positions.len(), 4);
        assert!(positions.iter().all(|y| (*y - positions[0]).abs() < 1.));
    }
    #[test]
    fn platforms_share_primary_commands_and_order() {
        let web = menus(&State::default());
        let native = menus(&State {
            native: true,
            ..Default::default()
        });
        assert_eq!(
            web.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(),
            ["Open data", "Decoder", "Edit", "View"]
        );
        for (a, b) in web.iter().zip(&native) {
            let a: Vec<_> = a.children.iter().filter_map(|i| i.command).collect();
            let b: Vec<_> = b.children.iter().filter_map(|i| i.command).collect();
            assert_eq!(a, b);
        }
        assert!(!web[0].children[0].enabled);
        assert!(!web[0].children[2].enabled);
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
