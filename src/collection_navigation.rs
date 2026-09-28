//! Shared Table collection navigation for desktop and browser.
use std::collections::{HashMap, HashSet};
pub struct Collection {
    pub name: String,
    pub path: String,
    pub count: u64,
    pub group: Option<(String, usize)>,
}
/// One entry of the collection picker.
pub struct Chip {
    pub label: String,
    pub target: String,
    pub hover: String,
    /// Channels discovered inside a repeated item (for example one per sensor),
    /// shown as one chip and chosen in the breadcrumb.
    pub channels: Vec<Channel>,
}

pub struct Channel {
    pub label: String,
    /// Member collections; a channel with several has an `[n]` selector.
    pub members: Vec<(Option<usize>, String)>,
}

impl Chip {
    pub fn contains(&self, target: &str) -> bool {
        self.channels.iter().any(|channel| channel.members.iter().any(|(_, path)| path == target))
    }
}

pub fn chips(collections: &[Collection], file_header: bool) -> Vec<Chip> {
        let mut chips = Vec::new();
        if file_header {
            chips.push(Chip { label: "File header".into(), target: String::new(),
                hover: "Top-level fields".into(), channels: Vec::new() });
        }
        // Channel keys look like `container.CHANNEL[i].rest` or `container.CHANNEL.sensors`:
        // dropping the last two segments gives the container they all live in.
        let container_of = |key: &str| key.rsplitn(3, '.').nth(2).map(str::to_owned);
        let keyed: Vec<(&Collection, Option<(String, usize)>)> = collections.iter()
            .map(|node| (node, node.group.clone())).collect();
        let containers: HashSet<String> = keyed.iter()
            .filter_map(|(_, key)| key.as_ref().and_then(|(key, _)| container_of(key))).collect();
        let channel_parents: HashSet<String> = keyed.iter()
            .filter_map(|(_, key)| key.as_ref().and_then(|(key, _)| key.rsplit_once('.').map(|(parent, _)| parent.to_owned())))
            .collect();
        let mut chip_of_container: HashMap<String, usize> = HashMap::new();
        for (node, key) in keyed {
            // A discovered collection directly inside a channel container is one more channel.
            let (container, channel, index) = match &key {
                Some((key, index)) => match container_of(key) {
                    Some(container) => (container, key.rsplit('.').nth(1).unwrap_or(key).to_owned(), Some(*index)),
                    None => continue,
                },
                None => match node.path.rsplit_once('.').filter(|(parent, _)| containers.contains(*parent)) {
                    Some((parent, name)) => (parent.to_owned(), name.to_owned(), None),
                    None => {
                        if channel_parents.contains(&node.path) {
                            continue;
                        }
                        let name = node.name.split_once(" across ").map_or(node.name.as_str(), |(name, _)| name);
                        chips.push(Chip {
                            label: format!("{} ({})", humanize(name.rsplit('.').next().unwrap_or(name)),
                                crate::format::count(node.count)),
                            target: node.path.clone(),
                            hover: node.path.clone(),
                            channels: Vec::new(),
                        });
                        continue;
                    }
                },
            };
            let chip = *chip_of_container.entry(container.clone()).or_insert_with(|| {
                chips.push(Chip { label: humanize(container.rsplit('.').next().unwrap_or(&container)),
                    target: node.path.clone(), hover: container.clone(), channels: Vec::new() });
                chips.len() - 1
            });
            let channels = &mut chips[chip].channels;
            match channels.iter_mut().find(|existing| existing.label == channel) {
                Some(existing) => existing.members.push((index, node.path.clone())),
                None => channels.push(Channel { label: channel, members: vec![(index, node.path.clone())] }),
            }
        }
        for &chip in chip_of_container.values() {
            let chip = &mut chips[chip];
            for channel in &mut chip.channels {
                channel.members.sort_by_key(|(index, _)| *index);
            }
            chip.target = chip.channels[0].members[0].1.clone();
            chip.label = format!("{} ({})", chip.label,
                crate::format::plural(chip.channels.len() as u64, "channel", "channels"));
        }
        // Equal names in different places keep the chips apart.
        let labels: Vec<String> = chips.iter().map(|chip| chip.label.clone()).collect();
        for chip in &mut chips {
            if labels.iter().filter(|label| **label == chip.label).count() > 1 {
                if let Some((parent, _)) = chip.hover.rsplit_once('.') {
                    chip.label = format!("{} · {parent}", chip.label);
                }
            }
        }
        chips
}

fn humanize(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}
