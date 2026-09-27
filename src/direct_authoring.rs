//! Direct byte interpretation shared by desktop and browser. No file I/O here.
#[cfg(test)]
use crate::egui_compat::ContextRun as _;
use egui::Ui;
use serde_yaml_ng::Value;
use std::ops::Range;

/// Empty source is an authoring state, not a failed parser.
#[cfg(any(test, target_arch = "wasm32"))]
pub fn empty_draft(source: &str) -> bool {
    source
        .lines()
        .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
        || serde_yaml_ng::from_str::<Value>(source)
            .ok()
            .is_some_and(|v| {
                v["actions"]
                    .as_sequence()
                    .is_some_and(|actions| actions.is_empty())
            })
}

#[cfg(test)]
pub fn results_current(source: &str, applied: &str) -> bool {
    source == applied && !empty_draft(source)
}

const TYPES: &[&str] = &[
    "string", "u8", "i8", "u16", "i16", "u32", "i32", "u64", "i64", "f32", "f64", "bytes",
];
fn width(kind: &str) -> Option<u64> {
    Some(match kind {
        "string" | "bytes" | "u8" | "i8" => 1,
        "u16" | "i16" => 2,
        "u32" | "i32" | "f32" => 4,
        "u64" | "i64" | "f64" => 8,
        _ => return None,
    })
}

/// Interpret only a bounded prefix, preserving integer precision and byte order.
pub fn preview(bytes: &[u8], kind: &str, big: bool) -> String {
    let bytes = &bytes[..bytes.len().min(64)];
    if kind == "string" {
        return format!("{:?}", String::from_utf8_lossy(bytes));
    }
    if kind == "bytes" {
        return bytes
            .iter()
            .take(8)
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
    }
    let Some(width) = width(kind) else {
        return String::new();
    };
    let values = bytes
        .chunks_exact(width as usize)
        .take(3)
        .map(|chunk| {
            macro_rules! number {
                ($t:ty) => {{
                    let bytes = chunk.try_into().unwrap();
                    (if big {
                        <$t>::from_be_bytes(bytes)
                    } else {
                        <$t>::from_le_bytes(bytes)
                    })
                    .to_string()
                }};
            }
            match kind {
                "u8" => number!(u8),
                "i8" => number!(i8),
                "u16" => number!(u16),
                "i16" => number!(i16),
                "u32" => number!(u32),
                "i32" => number!(i32),
                "u64" => number!(u64),
                "i64" => number!(i64),
                "f32" => number!(f32),
                "f64" => number!(f64),
                _ => unreachable!(),
            }
        })
        .collect::<Vec<_>>();
    if values.is_empty() {
        "Waiting for bytes...".into()
    } else {
        values.join(", ")
    }
}

#[derive(Clone)]
struct Existing {
    span: Range<usize>,
    name: String,
    kind: String,
    comment: String,
}

// Only touch simple, explicit reads. Expressions, nesting and user formatting
// outside this single flow-style read are never reserialized or guessed.
fn existing(source: &str, range: &Range<u64>) -> Option<Existing> {
    let mut offset = 0;
    let mut actions = false;
    for line in source.split_inclusive('\n') {
        if line.starts_with("actions:") {
            actions = true;
        } else if actions
            && !line.starts_with(char::is_whitespace)
            && !line.starts_with('#')
            && !line.trim().is_empty()
        {
            actions = false;
        }
        if actions && line.starts_with("  - read: {") {
            if let Ok(value) =
                serde_yaml_ng::from_str::<Value>(line.trim_start().trim_start_matches("- "))
            {
                let field = &value["read"];
                let custom = field["type"]["structure"].as_str();
                let kind = field["type"].as_str().or(custom).unwrap_or("");
                let count = field["repeat"].as_u64().unwrap_or(1);
                let size = if matches!(kind, "string" | "bytes") {
                    field["length"].as_u64().and_then(|n| n.checked_mul(count))
                } else {
                    if custom.is_some() {
                        Some(range.end - range.start)
                    } else {
                        width(kind).and_then(|w| w.checked_mul(count))
                    }
                };
                let simple = field.as_mapping().is_some_and(|m| {
                    m.keys().all(|k| {
                        matches!(
                            k.as_str(),
                            Some("name" | "type" | "at" | "length" | "repeat")
                        )
                    })
                });
                let fixed = (field["repeat"].is_null() || field["repeat"].as_u64().is_some())
                    && (matches!(kind, "string" | "bytes") || field["length"].is_null());
                if simple
                    && fixed
                    && field["at"].as_u64() == Some(range.start)
                    && size == Some(range.end - range.start)
                {
                    // Locate a trailing comment without confusing '#' inside a name.
                    let mut quote = None;
                    let mut escaped = false;
                    let mut comment = "";
                    for (i, c) in line.char_indices() {
                        if escaped {
                            escaped = false;
                            continue;
                        }
                        if quote == Some('"') && c == '\\' {
                            escaped = true;
                            continue;
                        }
                        if Some(c) == quote {
                            quote = None;
                        } else if quote.is_none() && matches!(c, '\'' | '"') {
                            quote = Some(c);
                        } else if quote.is_none() && c == '#' {
                            comment = line[i..].trim_end();
                            break;
                        }
                    }
                    return Some(Existing {
                        span: offset..offset + line.len(),
                        name: field["name"].as_str()?.into(),
                        kind: custom
                            .map(|name| format!("structure:{name}"))
                            .unwrap_or_else(|| kind.into()),
                        comment: comment.into(),
                    });
                }
            }
        }
        offset += line.len();
    }
    None
}

fn refers(value: &Value, name: &str) -> bool {
    match value {
        Value::Mapping(map) => map.iter().any(|(k, v)| {
            (k.as_str() == Some("ref")
                && v.as_str().is_some_and(|s| {
                    let s = s.trim_start_matches('/');
                    s == name
                        || s.starts_with(&format!("{name}."))
                        || s.starts_with(&format!("{name}["))
                }))
                || refers(v, name)
        }),
        Value::Sequence(items) => items.iter().any(|v| refers(v, name)),
        _ => false,
    }
}

pub fn change(source: &str, range: Range<u64>, name: &str, kind: &str) -> Result<String, String> {
    if source.len() > 256 * 1024 {
        return Err("Decoder source exceeds 256 KiB. Shorten it in Code view first.".into());
    }
    if range.end <= range.start || name.trim().is_empty() {
        return Err("Select bytes and give the field a name.".into());
    }
    let custom = kind.strip_prefix("structure:");
    let size = if custom.is_some() {
        1
    } else {
        width(kind).ok_or("Unknown interpretation")?
    };
    let length = range.end - range.start;
    if length % size != 0 {
        return Err(format!("{kind} needs a multiple of {}.", crate::format::exact_bytes(size)));
    }
    let prior = existing(source, &range);
    let value: Value = serde_yaml_ng::from_str(source)
        .map_err(|e| format!("Fix the YAML in Code view first: {e}"))?;
    if let Some(old) = &prior {
        if old.name != name && refers(&value, &old.name) {
            return Err("Other fields reference this name. Rename it and its references together in Code view.".into());
        }
    }
    if value["actions"].as_sequence().is_some_and(|items| {
        items.iter().any(|item| {
            item["read"]["name"].as_str() == Some(name)
                && prior.as_ref().is_none_or(|p| p.name != name)
        })
    }) {
        return Err("That name is already used. Choose another name.".into());
    }
    let extra = if custom.is_some() {
        String::new()
    } else if matches!(kind, "string" | "bytes") {
        format!(", length: {length}")
    } else if length > size {
        format!(", repeat: {}", length / size)
    } else {
        String::new()
    };
    let kind = custom
        .map(|name| format!("{{structure: {}}}", serde_json::to_string(name).unwrap()))
        .unwrap_or_else(|| kind.to_string());
    let line = format!(
        "  - read: {{name: {}, type: {kind}, at: {}{extra}}}",
        serde_json::to_string(name).unwrap(),
        range.start
    );
    let result = if let Some(prior) = prior {
        let mut result = source.to_string();
        result.replace_range(
            prior.span,
            &format!(
                "{line}{}\n",
                if prior.comment.is_empty() {
                    String::new()
                } else {
                    format!(" {}", prior.comment)
                }
            ),
        );
        result
    } else {
        append(source, &format!("{line}\n"))?
    };
    crate::structure::Program::parse(&result).map_err(|e| format!("{e:#}"))?;
    Ok(result)
}

fn append(source: &str, action: &str) -> Result<String, String> {
    let mut result = String::new();
    let mut found = false;
    let mut in_actions = false;
    let mut inserted = false;
    for line in source.split_inclusive('\n') {
        if line.starts_with("actions:") {
            if found {
                return Err("Duplicate actions blocks.".into());
            }
            let tail = line[8..].trim();
            if !(tail.is_empty() || tail.starts_with('#') || tail == "[]") {
                return Err("Expand actions into a YAML list in Code view first.".into());
            }
            found = true;
            in_actions = true;
            if tail == "[]" {
                result.push_str("actions:\n");
            } else {
                result.push_str(line);
                if !line.ends_with('\n') {
                    result.push('\n');
                }
            }
            continue;
        }
        if in_actions
            && !line.starts_with(char::is_whitespace)
            && !line.starts_with('#')
            && !line.trim().is_empty()
        {
            result.push_str(action);
            inserted = true;
            in_actions = false;
        }
        result.push_str(line);
    }
    if !result.is_empty() && !result.ends_with('\n') {
        result.push('\n');
    }
    if !found {
        result.push_str("actions:\n");
    }
    if !inserted {
        result.push_str(action);
    }
    Ok(result)
}

/// Candidate results are checked before either adapter publishes them.
#[derive(Clone, Debug)]
pub struct Interpretation {
    pub name: String,
    pub range: Range<u64>,
}
impl Interpretation {
    pub fn validate(&self, output: &crate::structure::ParseOutput) -> anyhow::Result<()> {
        let node = output
            .nodes
            .iter()
            .find(|n| n.name == self.name)
            .ok_or_else(|| anyhow::anyhow!("Interpretation produced no field"))?;
        anyhow::ensure!(node.range.start as u64 == self.range.start && node.range.end as u64 <= self.range.end,
            "Selection is too short: {} needs bytes {}..{}, selected {}..{}. Select more bytes and try again.",
            self.name, node.range.start, node.range.end, self.range.start, self.range.end);
        Ok(())
    }
}
pub fn structures(source: &str) -> Vec<String> {
    serde_yaml_ng::from_str::<Value>(source)
        .ok()
        .and_then(|v| v["structures"].as_sequence().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_owned))
        .collect()
}
fn fresh_name(source: &str) -> String {
    let value = serde_yaml_ng::from_str::<Value>(source).unwrap_or_default();
    for i in 1.. {
        let name = if i == 1 {
            "field".into()
        } else {
            format!("field_{i}")
        };
        if !value["actions"]
            .as_sequence()
            .is_some_and(|a| a.iter().any(|a| a["read"]["name"].as_str() == Some(&name)))
        {
            return name;
        }
    }
    unreachable!()
}
pub enum Edit {
    Commit(String),
    Interpret(String, Interpretation),
    Code,
}
#[derive(Default)]
pub struct DirectAuthoring {
    rename: Option<(String, String)>,
    focus: bool,
    error: String,
}
impl DirectAuthoring {
    #[cfg(target_arch = "wasm32")]
    pub fn editing(&self, name: &str) -> bool {
        self.rename.as_ref().is_some_and(|(n, _)| n == name)
    }
    pub fn created(&mut self, name: String) {
        self.rename = Some((name.clone(), name));
        self.focus = true;
    }
    /// Render the editable name exactly where the decoded name normally appears.
    pub fn name_ui(
        &mut self,
        ui: &mut Ui,
        source: &str,
        name: &str,
        range: Range<u64>,
    ) -> (egui::Response, Option<Edit>) {
        if self
            .rename
            .as_ref()
            .is_some_and(|(original, _)| original == name)
        {
            let (_, text) = self.rename.as_mut().unwrap();
            let response = ui.add(egui::TextEdit::singleline(text).desired_width(120.));
            if std::mem::take(&mut self.focus) {
                response.request_focus();
                response.scroll_to_me(Some(egui::Align::Center));
                if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), response.id) {
                    state
                        .cursor
                        .set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(text.chars().count()),
                        )));
                    state.store(ui.ctx(), response.id);
                }
            }
            let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
            let finish = response.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter));
            let mut edit = None;
            if escape {
                self.rename = None;
            } else if finish {
                let text = text.trim().to_string();
                if text == name {
                    self.rename = None;
                } else if let Some(old) = existing(source, &range).filter(|old| old.name == name) {
                    let kind = old.kind;
                    match change(source, range, &text, &kind) {
                        Ok(next) => {
                            edit = Some(Edit::Commit(next));
                            self.rename = None;
                            self.error.clear();
                        }
                        Err(e) => {
                            self.error = e;
                            self.focus = true;
                        }
                    }
                } else {
                    self.error =
                        "Edit this name in Code; its declaration is not a direct interpretation."
                            .into();
                    self.rename = None;
                }
            }
            (response, edit)
        } else {
            let response = ui.selectable_label(false, name);
            if response.double_clicked()
                && existing(source, &range).is_some_and(|old| old.name == name)
            {
                self.created(name.into());
            }
            response.context_menu(|ui| {
                if ui
                    .add_enabled(
                        existing(source, &range).is_some_and(|old| old.name == name),
                        egui::Button::new("Rename"),
                    )
                    .clicked()
                {
                    self.created(name.into());
                    ui.close();
                }
            });
            (response, None)
        }
    }
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        source: &str,
        range: Option<Range<u64>>,
        bytes: Option<&[u8]>,
        file_size: u64,
    ) -> Option<Edit> {
        let Some(range) = range.filter(|r| r.start < r.end && r.end <= file_size) else {
            return None;
        };
        let mut result = None;
        let mut chosen = None;
        ui.horizontal_wrapped(|ui| {
            ui.weak(format!(
                "{} at 0x{:X}",
                crate::format::exact_bytes(range.end - range.start),
                range.start
            ));
            ui.menu_button("Interpret as", |ui| {
                let big = serde_yaml_ng::from_str::<Value>(source)
                    .ok()
                    .is_some_and(|v| v["endian"].as_str() == Some("big"));
                ui.weak("Built-in types");
                for kind in TYPES {
                    let fits = (range.end - range.start) % width(kind).unwrap() == 0;
                    let value = bytes.map(|b| preview(b, kind, big)).unwrap_or_default();
                    if ui
                        .add_enabled(fits, egui::Button::new(format!("{kind}   {value}")))
                        .clicked()
                    {
                        chosen = Some(kind.to_string());
                        ui.close();
                    }
                }
                let names = structures(source);
                if !names.is_empty() {
                    ui.separator();
                    ui.weak("Your structures");
                    for name in names {
                        if ui.button(&name).clicked() {
                            chosen = Some(format!("structure:{name}"));
                            ui.close();
                        }
                    }
                }
            });
            if ui.small_button("Edit decoder").clicked() {
                result = Some(Edit::Code);
            }
        });
        if let Some(kind) = chosen {
            let name = existing(source, &range)
                .map(|old| old.name)
                .unwrap_or_else(|| fresh_name(source));
            match change(source, range.clone(), &name, &kind) {
                Ok(next) => {
                    self.error.clear();
                    result = Some(Edit::Interpret(next, Interpretation { name, range }));
                }
                Err(e) => self.error = e,
            }
        }
        if !self.error.is_empty() {
            ui.colored_label(ui.visuals().error_fg_color, &self.error);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn structures_use_real_nested_lengths_and_keep_declarations() {
        use std::sync::atomic::AtomicBool;
        let source = "# preserve this declaration\nstructures:\n  - name: Header\n    fields:\n      - {name: count, type: u8}\n      - {name: payload, type: bytes, length: {ref: count}}\nactions: []\n";
        let candidate = change(source, 0..8, "field", "structure:Header").unwrap();
        assert!(candidate.starts_with(source.split("actions:").next().unwrap()));
        assert_eq!(candidate.matches("- name: Header").count(), 1);
        assert!(candidate.contains("type: {structure: \"Header\"}"));
        let data = crate::byte_store::ByteStore::from_snapshot(
            crate::binary::Original::from_bytes(&[3, 65, 66, 67, 0, 0, 0, 0]).unwrap(),
        );
        let output = crate::structure::Program::parse(&candidate)
            .unwrap()
            .run(&data.view(), &AtomicBool::new(false))
            .unwrap();
        Interpretation {
            name: "field".into(),
            range: 0..8,
        }
        .validate(&output)
        .unwrap();
        assert_eq!(
            output.nodes[0].range,
            0..4,
            "Do not pad a variable structure to the selected length"
        );
        assert_eq!(output.nodes[0].children[1].range, 1..4);
        assert!(Interpretation {
            name: "field".into(),
            range: 0..2
        }
        .validate(&output)
        .is_err());
        let renamed = change(&candidate, 0..4, "header", "structure:Header").unwrap();
        assert_eq!(renamed.matches("- read:").count(), 1);
        assert!(renamed.contains("name: \"header\""));
        let missing = candidate.replace("ref: count", "ref: missing");
        assert!(crate::structure::Program::parse(&missing)
            .unwrap()
            .run(&data.view(), &AtomicBool::new(false))
            .is_err());
    }

    #[test]
    fn decoded_name_is_selected_for_inline_rename() {
        let ctx = egui::Context::default();
        let mut authoring = DirectAuthoring::default();
        let mut source = change("actions: []\n", 0..4, "field", "string").unwrap();
        authoring.created("field".into());
        let frame = |authoring: &mut DirectAuthoring, source: &mut String, events| {
            ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        let (_, edit) = authoring.name_ui(ui, source, "field", 0..4);
                        if let Some(Edit::Commit(next)) = edit {
                            *source = next;
                        }
                    });
                },
            )
        };
        let _ = frame(&mut authoring, &mut source, vec![]);
        let _ = frame(
            &mut authoring,
            &mut source,
            vec![egui::Event::Text("signature".into())],
        );
        let _ = frame(
            &mut authoring,
            &mut source,
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(
            source.contains("name: \"signature\""),
            "Typing should replace the selected generated name: {source}"
        );
        assert_eq!(source.matches("- read:").count(), 1);
    }

    #[test]
    fn clearing_source_releases_old_field_and_allows_starting_again() {
        let old = change("actions: []\n", 0..4, "signature", "string").unwrap();
        assert!(!results_current("", &old));
        assert!(!results_current("# starting again\n", &old));
        assert!(!results_current("actions: []\n", &old));
        assert!(results_current(&old, &old));
        assert!(!empty_draft("actions: [not_valid]"));
        let ctx = egui::Context::default();
        let mut editor = DirectAuthoring::default();
        let draw = |editor: &mut DirectAuthoring, source: &str| {
            ctx.run(egui::RawInput::default(), |_ctx, ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    editor.ui(ui, source, Some(0..4), Some(b"DEMO"), 16);
                });
            })
        };
        draw(&mut editor, &old);
        draw(&mut editor, "");
        let out = draw(&mut editor, "");
        let labels: Vec<_> = out
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some(t.galley.text()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&"Interpret as"));
        assert!(!labels.contains(&"Add field"));
        assert!(!labels
            .iter()
            .any(|text| text.contains("structure or expression")));
        assert!(
            labels.iter().all(|text| text.is_ascii()),
            "UI controls need no decorative font glyphs"
        );
        let fresh = change("", 0..4, "magic", "string").unwrap();
        assert!(fresh.contains("magic"));
        assert!(!fresh.contains("signature"));
    }

    #[test]
    fn narrow_ui_choosing_type_creates_field_without_a_form() {
        let ctx = egui::Context::default();
        let mut editor = DirectAuthoring::default();
        let mut source = "endian: little\nstructures:\n  - name: Header\n    fields: [{name: count, type: u8}]\nactions: []\n".to_string();
        let frame = |editor: &mut DirectAuthoring, source: &mut String, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(360., 380.),
                    )),
                    events,
                    ..Default::default()
                },
                |_ctx, ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        if let Some(Edit::Interpret(next, _)) =
                            editor.ui(ui, source, Some(0..4), Some(b"DEMO"), 16)
                        {
                            *source = next;
                        }
                    });
                },
            )
        };
        let position = |out: &egui::FullOutput, label: &str| {
            out.shapes
                .iter()
                .find_map(|s| match &s.shape {
                    egui::Shape::Text(t) if t.galley.text() == label => {
                        Some(t.pos + t.galley.rect.center().to_vec2())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("Missing control {label}"))
        };
        let click = |editor: &mut DirectAuthoring, source: &mut String, pos| {
            frame(editor, source, vec![egui::Event::PointerMoved(pos)]);
            for pressed in [true, false] {
                frame(
                    editor,
                    source,
                    vec![egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    }],
                );
            }
        };
        frame(&mut editor, &mut source, vec![]);
        let out = frame(&mut editor, &mut source, vec![]);
        click(&mut editor, &mut source, position(&out, "Interpret as"));
        let out = frame(&mut editor, &mut source, vec![]);
        click(&mut editor, &mut source, position(&out, "string   \"DEMO\""));
        assert!(
            source.contains("name: \"field\""),
            "Choosing a type must create the field immediately: {source}"
        );
        assert!(source.contains("type: string"));
        assert_eq!(source.matches("- read:").count(), 1);
        let out = frame(&mut editor, &mut source, vec![]);
        click(&mut editor, &mut source, position(&out, "Interpret as"));
        let out = frame(&mut editor, &mut source, vec![]);
        assert!(position(&out, "Your structures").x < 360.);
        click(&mut editor, &mut source, position(&out, "Header"));
        assert!(source.contains("type: {structure: \"Header\"}"));
        assert_eq!(source.matches("- name: Header").count(), 1);
        assert_eq!(source.matches("- read:").count(), 1);
    }

    #[test]
    fn repeated_byte_fields_match_the_whole_range_not_one_item() {
        let source =
            "actions:\n  - read: {name: blocks, type: bytes, at: 0, length: 4, repeat: 2}\n";
        assert!(existing(source, &(0..4)).is_none());
        assert!(existing(source, &(0..8)).is_some());
        let advanced =
            "actions:\n  - read: {name: values, type: u32, at: 0, repeat: {ref: count}}\n";
        assert!(existing(advanced, &(0..4)).is_none());
    }

    #[test]
    fn preview_is_exact_bounded_and_respects_byte_order() {
        assert_eq!(preview(b"DEMO", "string", false), "\"DEMO\"");
        assert_eq!(preview(&[255; 8], "u64", false), u64::MAX.to_string());
        assert_eq!(preview(&[0, 1], "u16", false), "256");
        assert_eq!(preview(&[0, 1], "u16", true), "1");
        assert_eq!(preview(&[255; 4], "i32", false), "-1");
        assert!(preview(&vec![0; 100000], "u8", false).len() < 20);
    }
    #[test]
    fn add_revise_preserves_comments_and_refuses_referenced_rename() {
        let source = "# private notes stay intact\nendian: little\nactions: []\n";
        let first = change(source, 0..4, "signature", "string").unwrap();
        let second = change(&first, 4..8, "count", "u32").unwrap();
        let renamed = change(&second, 0..4, "magic", "bytes").unwrap();
        assert!(renamed.starts_with("# private notes stay intact\n"));
        assert!(!renamed.contains("signature"));
        assert_eq!(renamed.matches("- read:").count(), 2);
        assert!(change(&renamed, 0..4, "count", "string").is_err());
        assert!(change(&renamed, 0..3, "bad", "u32").is_err());
        let referenced =
            format!("{second}  - read: {{name: payload, type: u8, repeat: {{ref: count}}}}\n");
        assert!(change(&referenced, 4..8, "length", "u32")
            .unwrap_err()
            .contains("reference"));
    }
    #[test]
    fn unrelated_nested_yaml_and_inline_comments_survive() {
        let source = "# catalog\nstructures:\n  - name: Header\n    fields: [{name: flag, type: u8}]\nactions:\n  - read: {name: \"magic\", type: string, at: 0, length: 4} # keep me\n  - read: {name: header, type: {structure: Header}}\n";
        let changed = change(source, 0..4, "signature", "string").unwrap();
        assert!(changed.contains("# keep me\n"));
        assert!(changed.contains("fields: [{name: flag, type: u8}]"));
        assert!(changed.ends_with("  - read: {name: header, type: {structure: Header}}\n"));
    }
}
