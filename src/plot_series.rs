//! Display grouping for decoded plot series; independent of file format.
use std::collections::BTreeMap;

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // Only the browser uses it.
fn indexed_component(path: &str) -> Option<(usize, usize)> {
    path.rfind('[').and_then(|start| {
        let end = path[start..].find(']')? + start;
        path[start + 1..end].chars().all(|c| c.is_ascii_digit())
            .then_some((start, end))
    })
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // Only the browser uses it.
pub fn group_key(name: &str) -> String {
    let path = name.rsplit_once(" / ").map_or(name, |(path, _)| path);
    indexed_component(path).map_or_else(|| path.to_owned(), |(start, _)| path[..start].to_owned())
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // Only the browser uses it.
pub fn group_label(group: &str) -> &str {
    group.rsplit('.').next().unwrap_or(group)
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // Only the browser uses it.
pub fn grouped<'a>(names: impl IntoIterator<Item = &'a str>) -> BTreeMap<String, Vec<(String, &'a str)>> {
    let mut groups: BTreeMap<String, Vec<(String, &'a str)>> = BTreeMap::new();
    for name in names {
        let (path, field) = name.rsplit_once(" / ").unwrap_or((name, "value"));
        let (group, label) = if let Some((start, end)) = indexed_component(path) {
            (path[..start].to_owned(), format!("{}{} / {field}", &path[start..=end], &path[end + 1..]))
        } else {
            (path.to_owned(), field.to_owned())
        };
        groups.entry(group).or_default().push((label, name));
    }
    for entries in groups.values_mut() {
        entries.sort_by(|a, b| {
            let index = |label: &str| label.strip_prefix('[')
                .and_then(|rest| rest.split_once(']'))
                .and_then(|(digits, _)| digits.parse::<usize>().ok());
            index(&a.0).cmp(&index(&b.0)).then_with(|| a.0.cmp(&b.0))
        });
    }
    groups
}

pub fn collection_group(name: &str, path: &str) -> Option<(String, usize)> {
    let (group_name, _) = name.split_once(" across ")?;
    // Contiguous definitions expose one named sample array per sensor.
    // Keep the complete parent path as identity so equal channel names in
    // different sections do not merge in the picker.
    if let Some((prefix, sensor)) = path.rsplit_once(".sensor_") {
        let index = sensor.parse::<usize>().ok()?;
        let (named_parent, named_sensor) = group_name.rsplit_once(".sensor_")?;
        if named_sensor.parse::<usize>().ok()? != index || !prefix.ends_with(named_parent) {
            return None;
        }
        return Some((format!("{prefix}.sensors"), index));
    }
    let (name, named_index) = group_name.rsplit_once('[')?;
    let named_index = named_index.strip_suffix(']')?.parse::<usize>().ok()?;
    let (prefix, rest) = path.rsplit_once('[')?;
    let (index, suffix) = rest.split_once(']')?;
    if !suffix.starts_with('.') { return None; }
    let index = index.parse::<usize>().ok()?;
    if index != named_index || !prefix.ends_with(name) { return None; }
    Some((format!("{prefix}{suffix}"), index))
}


#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))] // Only the browser uses it.
pub fn axis_targets(names: &[String], group: Option<&str>, series: Option<&str>) -> Vec<String> {
    if let Some(group) = group {
        names.iter().filter(|name| group_key(name) == group).cloned().collect()
    } else {
        series.into_iter().map(str::to_owned).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_groups_preserve_parent_identity_and_sensor_index() {
        assert_eq!(collection_group("Channel[10] across blocks", "sections[0].blocks.Channel[10].values"),
            Some(("sections[0].blocks.Channel.values".into(), 10)));
        assert_eq!(collection_group("Channel.sensor_2 across blocks", "sections[1].blocks.Channel.sensor_2"),
            Some(("sections[1].blocks.Channel.sensors".into(), 2)));
        assert_eq!(collection_group("records", "sections[0].records"), None);
        assert_eq!(collection_group("Channel[2] across blocks", "blocks.Channel[3].values"), None);
    }

    #[test]
    fn indexed_traces_group_by_collection_in_numeric_order() {
        let names = ["records.Channel[10].values / value", "records.Channel[2].values / value"];
        let groups = grouped(names);
        assert_eq!(groups["records.Channel"][0].0, "[2].values / value");
        assert_eq!(axis_targets(&names.map(str::to_owned), Some("records.Channel"), None).len(), 2);
    }
}
