//! Display grouping for decoded plot series; independent of file format.
use std::collections::BTreeMap;

fn indexed_component(path: &str) -> Option<(usize, usize)> {
    path.rfind('[').and_then(|start| {
        let end = path[start..].find(']')? + start;
        path[start + 1..end].chars().all(|c| c.is_ascii_digit())
            .then_some((start, end))
    })
}

pub fn group_key(name: &str) -> String {
    let path = name.rsplit_once(" / ").map_or(name, |(path, _)| path);
    indexed_component(path).map_or_else(|| path.to_owned(), |(start, _)| path[..start].to_owned())
}

pub fn group_label(group: &str) -> &str {
    group.rsplit('.').next().unwrap_or(group)
}

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
    fn indexed_traces_group_by_collection_in_numeric_order() {
        let names = ["records.Channel[10].values / value", "records.Channel[2].values / value"];
        let groups = grouped(names);
        assert_eq!(groups["records.Channel"][0].0, "[2].values / value");
        assert_eq!(axis_targets(&names.map(str::to_owned), Some("records.Channel"), None).len(), 2);
    }
}
