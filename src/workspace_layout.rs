//! Dock arrangement is view state; closing a panel never closes the document.
use egui_dock::{DockState, Node, NodeIndex, Split, SurfaceIndex};
use serde::{Deserialize, Serialize};

const STORAGE_KEY: &str = "workspace-layout-v3";
const DECODER_WIDTH: f32 = 0.40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Pane {
    Structures,
    Inspector,
    Layout,
    Plot,
    Table,
    Hex,
}
impl Pane {
    pub const ALL: [Self; 6] = [
        Self::Hex,
        Self::Structures,
        Self::Inspector,
        Self::Layout,
        Self::Plot,
        Self::Table,
    ];
    pub fn title(self) -> &'static str {
        match self {
            Self::Structures => "Decoded structure",
            Self::Inspector => "Inspector",
            Self::Layout => "Definition",
            Self::Plot => "Plot",
            Self::Table => "Samples",
            Self::Hex => "Hex",
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct WorkspaceLayout {
    pub dock: DockState<Pane>,
}
impl Default for WorkspaceLayout {
    fn default() -> Self {
        // Keep source and its result visible together. Inspector stays beside the
        // result as a supporting tab, while Hex keeps the full left column.
        let mut dock = DockState::new(vec![Pane::Hex]);
        let [_, source] = dock.main_surface_mut()
            .split_right(NodeIndex::root(), 0.48, vec![Pane::Layout]);
        dock.main_surface_mut().split_below(
            source,
            0.58,
            vec![Pane::Structures, Pane::Inspector],
        );
        Self { dock }
    }
}
impl WorkspaceLayout {
    /// Compare plots and source bytes while keeping the decoder within reach.
    /// Decoded samples are added below the right column when requested.
    pub fn plot_and_bytes() -> Self {
        let mut dock = DockState::new(vec![Pane::Layout, Pane::Inspector, Pane::Structures]);
        let [_, editor] =
            dock.main_surface_mut()
                .split_right(NodeIndex::root(), DECODER_WIDTH, vec![Pane::Plot]);
        dock.main_surface_mut()
            .split_below(editor, 0.60, vec![Pane::Hex]);
        Self { dock }
    }
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        storage.and_then(Self::decode).unwrap_or_default()
    }
    fn decode(storage: &dyn eframe::Storage) -> Option<Self> {
        if storage.get_string(STORAGE_KEY)?.len() > 65536 {
            return None;
        }
        // eframe's RON storage preserves the infinite rectangles of unopened panes.
        let result: Self = eframe::get_value(storage, STORAGE_KEY)?;
        let mut panes = std::collections::HashSet::new();
        for (_, pane) in result.dock.iter_all_tabs() {
            if !panes.insert(*pane) {
                return None;
            }
        }
        Some(result)
    }
    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, STORAGE_KEY, self);
    }

    /// Reveal the existing decoder, or reopen it in its supporting tab group.
    /// Existing user geometry and floating windows remain where the user put them.
    pub fn show_decoder(&mut self) {
        if self.dock.find_tab(&Pane::Layout).is_none() {
            let support = [Pane::Inspector, Pane::Structures]
                .into_iter()
                .find_map(|pane| self.dock.find_tab(&pane));
            if let Some((surface, node, _)) = support {
                self.dock[surface][node].append_tab(Pane::Layout);
            } else if let Some((surface, node, _)) = self.editor_location() {
                let branch = self.editor_branch(surface, node);
                self.dock.split(
                    (surface, branch),
                    Split::Left,
                    DECODER_WIDTH,
                    Node::leaf(Pane::Layout),
                );
            } else {
                self.dock.push_to_focused_leaf(Pane::Layout);
            }
        }
        self.focus(Pane::Layout);
    }

    /// Make a first plot visible above the source bytes. Once Plot has a place,
    /// keep the user's arrangement, including stacks and floating windows.
    pub fn show_initial_plot(&mut self) {
        if self.dock.find_tab(&Pane::Plot).is_none() {
            if let Some((surface, node, _)) = self.dock.find_tab(&Pane::Hex) {
                self.dock
                    .split((surface, node), Split::Above, 0.60, Node::leaf(Pane::Plot));
            } else {
                self.open_editor(Pane::Plot);
            }
        }
        self.focus(Pane::Plot);
    }

    /// Reveal samples below the content editors, keeping the decoder visible.
    /// An existing samples panel, including a floating one, is only focused.
    pub fn show_samples(&mut self) {
        if self.dock.find_tab(&Pane::Table).is_none() {
            if let Some((surface, node, _)) = self.editor_location() {
                let target = self.editor_branch(surface, node);
                self.dock.split(
                    (surface, target),
                    Split::Below,
                    0.60,
                    Node::leaf(Pane::Table),
                );
            } else {
                self.open_editor(Pane::Table);
            }
        }
        self.focus(Pane::Table);
    }

    pub fn reveal(&mut self, pane: Pane) {
        if pane == Pane::Layout {
            self.show_decoder();
            return;
        }
        if self.dock.find_tab(&pane).is_none() {
            match pane {
                Pane::Layout => unreachable!(),
                Pane::Structures | Pane::Inspector => {
                    let companion = if pane == Pane::Structures {
                        Pane::Inspector
                    } else {
                        Pane::Structures
                    };
                    let target = [Pane::Layout, companion]
                        .into_iter()
                        .find_map(|target| self.dock.find_tab(&target));
                    if let Some((surface, node, _)) = target {
                        self.dock[surface][node].append_tab(pane);
                    } else if let Some((surface, node, _)) = self.editor_location() {
                        let branch = self.editor_branch(surface, node);
                        self.dock.split(
                            (surface, branch),
                            Split::Left,
                            DECODER_WIDTH,
                            Node::leaf(pane),
                        );
                    } else {
                        self.dock.push_to_focused_leaf(pane);
                    }
                }
                Pane::Plot | Pane::Table | Pane::Hex => self.open_editor(pane),
            }
        }
        self.focus(pane);
    }

    fn editor_location(&self) -> Option<(SurfaceIndex, NodeIndex, egui_dock::TabIndex)> {
        [Pane::Hex, Pane::Plot, Pane::Table]
            .into_iter()
            .find_map(|pane| self.dock.find_tab(&pane))
    }

    /// Climb through content-only splits, stopping before the decoder/supporting
    /// panes. This puts results below both Plot and Hex in their comparison view.
    fn editor_branch(&self, surface: SurfaceIndex, mut node: NodeIndex) -> NodeIndex {
        while let Some(parent) = node.parent() {
            let contains_support = [Pane::Layout, Pane::Inspector, Pane::Structures]
                .into_iter()
                .filter_map(|pane| self.dock.find_tab(&pane))
                .any(|(other_surface, other_node, _)| {
                    other_surface == surface && is_within(other_node, parent)
                });
            if contains_support {
                break;
            }
            node = parent;
        }
        node
    }

    fn open_editor(&mut self, pane: Pane) {
        // Ordinary view commands stack editors instead of adding narrow columns.
        // show_samples is the explicit action that gives the results their own row.
        if let Some((surface, node, _)) = self.editor_location() {
            self.dock[surface][node].append_tab(pane);
        } else if let Some((surface, node, _)) = [Pane::Layout, Pane::Inspector, Pane::Structures]
            .into_iter()
            .find_map(|pane| self.dock.find_tab(&pane))
        {
            self.dock.split(
                (surface, node),
                Split::Right,
                DECODER_WIDTH,
                Node::leaf(pane),
            );
        } else {
            self.dock.push_to_focused_leaf(pane);
        }
    }

    fn focus(&mut self, pane: Pane) {
        if let Some((surface, node, tab)) = self.dock.find_tab(&pane) {
            self.dock[surface].set_active_tab(node, tab);
            self.dock.set_focused_node_and_surface((surface, node));
        }
    }
}

fn is_within(mut node: NodeIndex, ancestor: NodeIndex) -> bool {
    loop {
        if node == ancestor {
            return true;
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        node = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::Storage;
    #[derive(Default)]
    struct MemoryStorage(std::collections::HashMap<String, String>);
    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }
        fn flush(&mut self) {}
    }

    fn assert_decoder_visible(workspace: &WorkspaceLayout) {
        assert!(workspace.dock.find_tab(&Pane::Layout).is_some());
        assert!(workspace.dock.find_tab(&Pane::Inspector).is_some());
        assert!(workspace.dock.find_tab(&Pane::Structures).is_some());
        let surface = workspace.dock.find_tab(&Pane::Layout).unwrap().0;
        assert!(matches!(workspace.dock[surface][NodeIndex::root()],
            Node::Horizontal { fraction, .. } if (fraction - 0.48).abs() < 0.001 || (fraction - 0.40).abs() < 0.001));
    }

    #[test]
    fn default_shows_definition_and_structure_together_beside_hex() {
        let workspace = WorkspaceLayout::default();
        assert_eq!(workspace.dock.iter_all_tabs().count(), 4);
        assert!(workspace.dock.find_tab(&Pane::Plot).is_none());
        assert!(workspace.dock.find_tab(&Pane::Table).is_none());
        assert_eq!(
            workspace.dock.find_tab(&Pane::Hex).unwrap().1,
            NodeIndex::root().left()
        );
        let (surface, node, tab) = workspace.dock.find_tab(&Pane::Structures).unwrap();
        assert!(matches!(&workspace.dock[surface][node], Node::Leaf { active, .. } if *active == tab));
        assert_eq!(node, NodeIndex::root().right().right());
        assert_eq!(
            workspace.dock.find_tab(&Pane::Layout).unwrap().1,
            NodeIndex::root().right().left()
        );
        assert_eq!(workspace.dock.find_tab(&Pane::Inspector).unwrap().1, node);
        assert_eq!(Pane::Layout.title(), "Definition");
        assert_eq!(Pane::Structures.title(), "Decoded structure");
    }

    #[test]
    fn first_plot_opens_above_hex_and_repeated_requests_keep_geometry() {
        let mut workspace = WorkspaceLayout::default();
        workspace.show_initial_plot();
        workspace.show_decoder();
        assert_decoder_visible(&workspace);
        let plot = workspace.dock.find_tab(&Pane::Plot).unwrap();
        let hex = workspace.dock.find_tab(&Pane::Hex).unwrap();
        assert_eq!(plot.1, NodeIndex::root().left().left());
        assert_eq!(hex.1, NodeIndex::root().left().right());
        assert!(matches!(workspace.dock[plot.0][NodeIndex::root().left()],
            Node::Vertical { fraction, .. } if (fraction - 0.60).abs() < 0.001));
        assert!(workspace.dock.find_tab(&Pane::Table).is_none());
        for _ in 0..3 {
            workspace.show_initial_plot();
        }
        assert_eq!(workspace.dock.find_tab(&Pane::Plot), Some(plot));
        assert_eq!(workspace.dock.find_tab(&Pane::Hex), Some(hex));
        assert_eq!(workspace.dock.iter_all_tabs().count(), 5);
    }

    #[test]
    fn first_plot_keeps_existing_custom_stack_and_floating_geometry() {
        let mut dock = DockState::new(vec![Pane::Hex, Pane::Plot]);
        dock.main_surface_mut()
            .split_right(NodeIndex::root(), 0.81, vec![Pane::Layout]);
        let mut workspace = WorkspaceLayout { dock };
        let hex = workspace.dock.find_tab(&Pane::Hex).unwrap();
        let plot = workspace.dock.find_tab(&Pane::Plot).unwrap();
        workspace.show_initial_plot();
        assert_eq!(workspace.dock.find_tab(&Pane::Hex), Some(hex));
        assert_eq!(workspace.dock.find_tab(&Pane::Plot), Some(plot));
        assert!(matches!(workspace.dock[hex.0][NodeIndex::root()],
            Node::Horizontal { fraction, .. } if (fraction - 0.81).abs() < 0.001));
        workspace.dock.detach_tab(
            plot,
            egui::Rect::from_min_size(egui::pos2(60., 70.), egui::vec2(640., 480.)),
        );
        let floating = workspace.dock.find_tab(&Pane::Plot).unwrap();
        workspace.focus(Pane::Plot);
        let mut before = MemoryStorage::default();
        workspace.save(&mut before);
        workspace.show_initial_plot();
        let mut after = MemoryStorage::default();
        workspace.save(&mut after);
        assert_eq!(workspace.dock.find_tab(&Pane::Plot), Some(floating));
        assert_eq!(before.0, after.0);
    }

    #[test]
    fn decoder_reopens_in_its_stack_and_other_editors_do_not_add_columns() {
        let mut workspace = WorkspaceLayout::default();
        let original = workspace.dock.find_tab(&Pane::Layout).unwrap();
        workspace.dock.remove_tab(original);
        workspace.show_decoder();
        workspace.show_decoder();
        assert_decoder_visible(&workspace);
        workspace.reveal(Pane::Plot);
        workspace.reveal(Pane::Table);
        let hex = workspace.dock.find_tab(&Pane::Hex).unwrap().1;
        assert_eq!(workspace.dock.find_tab(&Pane::Plot).unwrap().1, hex);
        assert_eq!(workspace.dock.find_tab(&Pane::Table).unwrap().1, hex);
        assert_eq!(workspace.dock.iter_all_tabs().count(), Pane::ALL.len());
    }

    #[test]
    fn samples_open_below_hex_while_structure_stays_visible() {
        let mut workspace = WorkspaceLayout::default();
        workspace.show_samples();
        let table = workspace.dock.find_tab(&Pane::Table).unwrap();
        let hex = workspace.dock.find_tab(&Pane::Hex).unwrap();
        assert_eq!(hex.1.parent(), Some(NodeIndex::root().left()));
        assert_eq!(table.1, NodeIndex::root().left().right());
        assert_eq!(hex.1, NodeIndex::root().left().left());
        let (surface, node, tab) = workspace.dock.find_tab(&Pane::Structures).unwrap();
        assert!(matches!(&workspace.dock[surface][node], Node::Leaf { active, .. } if *active == tab));
        let count = workspace.dock.iter_all_tabs().count();
        workspace.show_samples();
        assert_eq!(workspace.dock.find_tab(&Pane::Table), Some(table));
        assert_eq!(workspace.dock.iter_all_tabs().count(), count);
    }

    #[test]
    fn comparison_layout_adds_samples_below_both_plot_and_hex() {
        let mut workspace = WorkspaceLayout::plot_and_bytes();
        assert_decoder_visible(&workspace);
        let plot = workspace.dock.find_tab(&Pane::Plot).unwrap();
        let hex = workspace.dock.find_tab(&Pane::Hex).unwrap();
        assert_eq!(plot.1.parent(), Some(NodeIndex::root().right()));
        assert_eq!(hex.1.parent(), plot.1.parent());
        assert_ne!(plot.1, hex.1);
        assert!(workspace.dock.find_tab(&Pane::Table).is_none());
        assert!(matches!(workspace.dock[plot.0][NodeIndex::root().right()],
            Node::Vertical { fraction, .. } if (fraction - 0.60).abs() < 0.001));
        workspace.show_samples();
        let plot = workspace.dock.find_tab(&Pane::Plot).unwrap();
        let hex = workspace.dock.find_tab(&Pane::Hex).unwrap();
        let table = workspace.dock.find_tab(&Pane::Table).unwrap();
        assert_eq!(plot.1.parent(), hex.1.parent());
        assert_eq!(plot.1.parent(), Some(NodeIndex::root().right().left()));
        assert_eq!(table.1, NodeIndex::root().right().right());
        assert_decoder_visible(&workspace);
        workspace.show_samples();
        assert_eq!(workspace.dock.find_tab(&Pane::Plot), Some(plot));
        assert_eq!(workspace.dock.find_tab(&Pane::Hex), Some(hex));
        assert_eq!(workspace.dock.find_tab(&Pane::Table), Some(table));
    }

    #[test]
    fn floating_decoder_and_samples_are_revealed_without_duplicates_or_redocking() {
        let mut workspace = WorkspaceLayout::default();
        workspace.show_samples();
        for pane in [Pane::Layout, Pane::Table] {
            let position = workspace.dock.find_tab(&pane).unwrap();
            workspace.dock.detach_tab(
                position,
                egui::Rect::from_min_size(egui::pos2(60., 60.), egui::vec2(600., 500.)),
            );
        }
        let decoder = workspace.dock.find_tab(&Pane::Layout).unwrap();
        let table = workspace.dock.find_tab(&Pane::Table).unwrap();
        let hex = workspace.dock.find_tab(&Pane::Hex).unwrap();
        for _ in 0..3 {
            workspace.show_decoder();
            workspace.show_samples();
        }
        assert_eq!(workspace.dock.find_tab(&Pane::Layout), Some(decoder));
        assert_eq!(workspace.dock.find_tab(&Pane::Table), Some(table));
        assert_eq!(workspace.dock.find_tab(&Pane::Hex), Some(hex));
        assert_eq!(workspace.dock.iter_all_tabs().count(), 5);
    }

    #[test]
    fn saved_custom_geometry_is_preserved_and_decoder_uses_existing_support_stack() {
        let mut dock = DockState::new(vec![Pane::Hex, Pane::Plot]);
        dock.main_surface_mut().split_right(
            NodeIndex::root(),
            0.81,
            vec![Pane::Inspector, Pane::Structures],
        );
        let original = WorkspaceLayout { dock };
        let mut storage = MemoryStorage::default();
        original.save(&mut storage);
        let mut restored = WorkspaceLayout::load(Some(&storage));
        assert!(
            restored.dock.find_tab(&Pane::Layout).is_none(),
            "loading must not migrate a saved arrangement"
        );
        let hex = restored.dock.find_tab(&Pane::Hex).unwrap();
        let inspector = restored.dock.find_tab(&Pane::Inspector).unwrap();
        restored.show_decoder();
        assert_eq!(restored.dock.find_tab(&Pane::Hex), Some(hex));
        assert_eq!(
            restored.dock.find_tab(&Pane::Layout).unwrap().1,
            inspector.1
        );
        assert!(matches!(restored.dock[hex.0][NodeIndex::root()],
            Node::Horizontal { fraction, .. } if (fraction - 0.81).abs() < 0.001));
    }

    #[test]
    fn decoder_reopens_beside_content_when_supporting_views_were_closed() {
        let mut workspace = WorkspaceLayout {
            dock: DockState::new(vec![Pane::Hex]),
        };
        let [top, _] = workspace.dock.main_surface_mut().split_below(
            NodeIndex::root(),
            0.60,
            vec![Pane::Plot],
        );
        assert_eq!(top, NodeIndex::root().left());
        workspace.show_decoder();
        let decoder = workspace.dock.find_tab(&Pane::Layout).unwrap();
        assert_eq!(decoder.1, NodeIndex::root().left());
        assert_eq!(
            workspace.dock.find_tab(&Pane::Hex).unwrap().1.parent(),
            Some(NodeIndex::root().right())
        );
        assert_eq!(
            workspace.dock.find_tab(&Pane::Plot).unwrap().1.parent(),
            Some(NodeIndex::root().right())
        );
        assert!(matches!(workspace.dock[decoder.0][NodeIndex::root()],
            Node::Horizontal { fraction, .. } if (fraction - 0.40).abs() < 0.001));
    }

    #[test]
    fn closing_reopening_and_persisting_panels_does_not_duplicate_them() {
        let mut workspace = WorkspaceLayout::default();
        let position = workspace.dock.find_tab(&Pane::Hex).unwrap();
        workspace.dock.remove_tab(position);
        assert!(workspace.dock.find_tab(&Pane::Hex).is_none());
        for pane in Pane::ALL {
            workspace.reveal(pane);
            workspace.reveal(pane);
        }
        let position = workspace.dock.find_tab(&Pane::Table).unwrap();
        let floating = workspace.dock.detach_tab(
            position,
            egui::Rect::from_min_size(egui::pos2(60., 60.), egui::vec2(400., 400.)),
        );
        let mut storage = MemoryStorage::default();
        workspace.save(&mut storage);
        let mut restored = WorkspaceLayout::decode(&storage).unwrap();
        assert_eq!(restored.dock.find_tab(&Pane::Table).unwrap().0, floating);
        let position = restored.dock.find_tab(&Pane::Table).unwrap();
        let (surface, node, _) = restored.dock.find_tab(&Pane::Plot).unwrap();
        restored.dock.move_tab(
            position,
            egui_dock::TabDestination::Node(surface, node, egui_dock::TabInsert::Append),
        );
        assert_eq!(restored.dock.find_tab(&Pane::Table).unwrap().0, surface);
        assert_eq!(restored.dock.iter_all_tabs().count(), Pane::ALL.len());
        storage.set_string(STORAGE_KEY, "invalid".into());
        assert!(WorkspaceLayout::decode(&storage).is_none());
        assert_eq!(
            WorkspaceLayout::load(Some(&storage))
                .dock
                .iter_all_tabs()
                .count(),
            4
        );
    }
    #[test]
    fn old_storage_key_does_not_override_current_layout() {
        let mut storage = MemoryStorage::default();
        eframe::set_value(
            &mut storage,
            "workspace-layout-v1",
            &WorkspaceLayout {
                dock: DockState::new(Pane::ALL.to_vec()),
            },
        );
        assert!(WorkspaceLayout::decode(&storage).is_none());
        assert_eq!(
            WorkspaceLayout::load(Some(&storage))
                .dock
                .iter_all_tabs()
                .count(),
            4
        );
    }
}
