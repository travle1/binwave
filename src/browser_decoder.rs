use wasm_bindgen::prelude::*;
#[wasm_bindgen(
    inline_js = "export function inWorker() { return typeof document === 'undefined'; }"
)]
extern "C" {
    #[wasm_bindgen(js_name = inWorker)]
    pub fn in_worker() -> bool;
}

use crate::byte_store::ByteView;
use crate::structure::{LazyArray, Node, ParseOutput, Program};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
#[wasm_bindgen]
pub fn python_runner() -> String {
    include_str!("../python/runner.py").into()
}
fn json_result(value: Value) -> Result<String> {
    let text = serde_json::to_string(&value)?;
    ensure!(
        text.len() <= OUTPUT_LIMIT,
        "Decoded result exceeds the browser response limit; request a smaller page"
    );
    Ok(text)
}
fn plot_channel(path: &str, name: &str, points: &[crate::structure_plot::PlotSample],
    envelopes: Option<&Vec<crate::structure_plot::PlotEnvelope>>, estimated: bool) -> Value {
    let bands: Vec<[f64; 4]> = if estimated { envelopes.into_iter().flatten().map(|band| [
        band.range.start as f64 - 0.5, band.range.end as f64 - 0.5, band.min.value, band.max.value,
    ]).collect() } else { Vec::new() };
    json!({"name":name,"path":path,
        "bytes":points.iter().map(|p|[p.bytes.start.to_string(),p.bytes.end.to_string()]).collect::<Vec<_>>(),
        "points":points.iter().map(|p|[p.index as f64,p.value]).collect::<Vec<_>>(),"bands":bands})
}
fn js_error(error: anyhow::Error) -> JsValue {
    JsValue::from_str(&format!("{error:#}"))
}
fn describe(node: &Node) -> Value {
    json!({"name":node.name,"label":node.tree_label(),"summary":node.tree_summary(),"path":node.path,"kind":node.kind,
        "numeric_tooltip":node.numeric_tooltip(),
        "start":node.range.start.to_string(),"end":node.range.end.to_string(),
        "value":node.display(),"count":node.lazy.as_ref().map(|a|a.count.to_string()),
        "children":node.children.iter().map(describe).collect::<Vec<_>>()})
}
fn find<'a>(nodes: &'a [Node], path: &str) -> Option<&'a Node> {
    for node in nodes {
        if node.path == path {
            return Some(node);
        }
        if let Some(node) = find(&node.children, path) {
            return Some(node);
        }
    }
    None
}
fn collection_node<'a>(nodes: &'a [Node], flattened: &'a BTreeMap<String, Node>, path: &str) -> Option<&'a Node> {
    find(nodes, path).or_else(|| flattened.get(path))
}
fn resolve_tree_node(nodes: &[Node], path: &str, source: &ByteView) -> Result<Option<Node>> {
    for node in nodes {
        if node.path == path { return Ok(Some(node.clone())); }
        if let Some(found) = resolve_tree_node(&node.children, path, source)? { return Ok(Some(found)); }
        if let Some(array) = &node.lazy {
            if let Some(remainder) = path.strip_prefix(&node.path).and_then(|rest| rest.strip_prefix('[')) {
                if let Some((index, _)) = remainder.split_once(']') {
                    if let Ok(index) = index.parse::<u64>() {
                        if index < array.count {
                            let item = page(node, source, index, 1)?;
                            if let Some(found) = resolve_tree_node(&item, path, source)? { return Ok(Some(found)); }
                        }
                    }
                }
            }
        }
    }
    Ok(None)
}
fn collections(nodes: &[Node], result: &mut Vec<Value>) {
    for node in nodes {
        if let Some(array) = &node.lazy {
            result.push(json!({"path":node.path,"count":array.count.to_string()}));
        } else {
            if node.children.len() > 1 && node.children.iter().all(|n| n.name.starts_with('[')) {
                result.push(json!({"path":node.path,"count":node.children.len().to_string()}));
            }
            collections(&node.children, result);
        }
    }
}
/// Expose fixed-size child arrays across their parent records, just as the
/// desktop collection browser does. Only the first parent is decoded; the
/// resulting LazyArray describes the rest without reading the file.
fn flattened_collections(nodes: &[Node], source: &ByteView) -> BTreeMap<String, Node> {
    fn parents<'a>(nodes: &'a [Node], found: &mut Vec<&'a Node>) {
        for node in nodes {
            if node.lazy.as_ref().is_some_and(|array| array.count > 0) {
                found.push(node);
            }
            parents(&node.children, found);
        }
    }
    fn descend(parent: &Node, row: &Node, source: &ByteView, found: &mut BTreeMap<String, Node>) {
        for child in &row.children {
            if let Some(flat) = LazyArray::flattened_child(parent, child) {
                found.insert(flat.path.clone(), flat);
            }
            if let Some(groups) = &child.lazy {
                if groups.count > 0 {
                    if let Ok(first) = groups.page(source, &AtomicBool::new(false), 0, 1) {
                        if let Some(group_item) = first.first() {
                            for inner in &group_item.children {
                                let count = inner.lazy.as_ref().map_or(inner.children.len() as u64, |array| array.count);
                                if count > 0 && count <= 4096 {
                                    for index in 0..count {
                                        if found.len() >= 4096 { break; }
                                        if let Some(flat) = LazyArray::flattened_group_sensor(parent, child, inner, index) {
                                            found.insert(flat.path.clone(), flat);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            descend(parent, child, source, found);
        }
    }
    let mut candidates = Vec::new();
    parents(nodes, &mut candidates);
    candidates.sort_by_key(|node| std::cmp::Reverse(node.lazy.as_ref().map_or(0, |array| array.count)));
    let mut found = BTreeMap::new();
    for parent in candidates.into_iter().take(128) {
        if found.len() >= 4096 { break; }
        let Some(array) = &parent.lazy else { continue; };
        if let Ok(first) = array.page(source, &AtomicBool::new(false), 0, 1) {
            if let Some(row) = first.first() {
                descend(parent, row, source, &mut found);
            }
        }
    }
    found
}
fn last_lazy_path(nodes: &[Node]) -> Option<&str> {
    nodes.iter().rev().find_map(|node| last_lazy_path(&node.children).or_else(|| node.lazy.as_ref().map(|_| node.path.as_str())))
}
fn decoded_json(output: &ParseOutput, partial_error: Option<&str>, flattened: &BTreeMap<String, Node>) -> Result<String> {
    let mut arrays = Vec::new();
    if partial_error.is_none() { collections(&output.nodes, &mut arrays); }
    if partial_error.is_none() {
        arrays.extend(flattened.values().filter_map(|node| node.lazy.as_ref().map(|array|
            json!({"path":node.path,"name":node.name,"count":array.count.to_string(),
                "group":crate::plot_series::collection_group(&node.name, &node.path)}))));
    }
    let preview_path = partial_error.and_then(|_| last_lazy_path(&output.nodes));
    let nodes: Vec<_> = output.nodes.iter().map(describe).collect();
    json_result(json!({"nodes":nodes,"partial_preview_path":preview_path,
        "collections":arrays,"consumed":output.consumed.to_string(),
        "types":output.definitions.len(),"partial":partial_error.is_some(),"error":partial_error}))
}
fn cells(node: &Node, prefix: &str, out: &mut BTreeMap<String, Value>) {
    if out.len() >= 64 {
        return;
    }
    if node.value.is_some() {
        out.insert(
            if prefix.is_empty() {
                "value".into()
            } else {
                prefix.into()
            },
            json!({"text":node.display(),"number":node.numeric().filter(|n|n.is_finite()),
                "start":node.range.start.to_string(),"end":node.range.end.to_string()}),
        );
    }
    for child in &node.children {
        let key = if prefix.is_empty() {
            child.name.clone()
        } else {
            format!("{prefix}.{}", child.name)
        };
        cells(child, &key, out);
    }
}
fn cell_columns(node: &Node, prefix: &str, columns: &mut Vec<String>) {
    if columns.len() >= 64 { return; }
    if node.value.is_some() { columns.push(if prefix.is_empty() { "value".into() } else { prefix.into() }); }
    for child in &node.children {
        let key = if prefix.is_empty() { child.name.clone() } else { format!("{prefix}.{}", child.name) };
        cell_columns(child, &key, columns);
    }
}
fn page(node: &Node, source: &ByteView, start: u64, count: u64) -> Result<Vec<Node>> {
    ensure!(count <= 512, "table page exceeds 512 records");
    if let Some(array) = &node.lazy {
        array.page(source, &AtomicBool::new(false), start, count)
    } else {
        let start = usize::try_from(start).context("record index exceeds memory range")?;
        ensure!(
            start <= node.children.len(),
            "record index outside collection"
        );
        Ok(node.children[start..(start + count as usize).min(node.children.len())].to_vec())
    }
}
#[wasm_bindgen]
pub struct DecoderSession {
    source: ByteView,
    output: Option<ParseOutput>,
    program: Option<Program>,
    flattened: BTreeMap<String, Node>,
}
#[wasm_bindgen]
impl DecoderSession {
    #[wasm_bindgen(constructor)]
    pub fn new(size: &str) -> Result<DecoderSession, JsValue> {
        Ok(Self {
            source: ByteView {
                revision: 1,
                length: size
                    .parse()
                    .map_err(|_| JsValue::from_str("Invalid file size"))?,
            },
            output: None,
            program: None,
            flattened: BTreeMap::new(),
        })
    }
    pub fn decode(&mut self, yaml: &str) -> Result<String, JsValue> {
        self.decode_interpretation(yaml, "", "0", "0")
    }
    /// Python's shared runner emits the same plan object used by native.
    pub fn decode_plan_json(&mut self, plan_json: &str) -> Result<String, JsValue> {
        let yaml = (|| -> Result<String> {
            let plan: serde_json::Value = serde_json::from_str(plan_json)?;
            ensure!(!plan.is_null(), "Python decoder did not define a read plan");
            Ok(serde_yaml_ng::to_string(&plan)?)
        })().map_err(js_error)?;
        self.decode(&yaml)
    }
    pub fn decode_interpretation(&mut self, yaml: &str, name: &str, start: &str, end: &str) -> Result<String, JsValue> {
        (|| {
            let program = Program::parse(yaml)?;
            let (output, error) = match program.run_with_partial(&self.source, &AtomicBool::new(false)) {
                Ok(output) => (output, None),
                Err(failure) => (failure.output, Some(format!("{:#}", failure.error))),
            };
            if error.is_none() && !name.is_empty() {
                crate::direct_authoring::Interpretation { name: name.into(), range: start.parse()?..end.parse()? }.validate(&output)?;
            }
            let flattened = if error.is_none() { flattened_collections(&output.nodes, &self.source) }
                else { BTreeMap::new() };
            let result = decoded_json(&output, error.as_deref(), &flattened)?;
            self.output = Some(output);
            self.program = Some(program);
            self.flattened = flattened;
            Ok(result)
        })()
        .map_err(js_error)
    }
    /// Locate selected original bytes within a decoded collection without scanning data.
    pub fn probe(&self, path: &str, start: &str, end: &str) -> Result<String, JsValue> {
        (|| -> Result<String> {
            let output = self.output.as_ref().context("Apply a decoder first")?;
            let range = start.parse::<u64>()?..end.parse::<u64>()?;
            let mut candidates = Vec::new();
            fn collect<'a>(nodes: &'a [Node], result: &mut Vec<&'a Node>) {
                for node in nodes {
                    if node.lazy.is_some() { result.push(node); }
                    collect(&node.children, result);
                }
            }
            collect(&output.nodes, &mut candidates);
            let node = crate::structure_selection::matching_collection(self.flattened.values(), path, &range)
                .or_else(|| crate::structure_selection::matching_collection(candidates, path, &range));
            let samples = node.and_then(|node| crate::structure_selection::sample_indices_for_bytes(node, &range));
            let path = node.map_or(path, |node| node.path.as_str());
            json_result(json!({"path":path,"first":samples.as_ref().map(|r| r.start.to_string()),
                "end":samples.map(|r| r.end.to_string())}))
        })().map_err(js_error)
    }

    pub fn pick(&self, path: &str, index: &str) -> Result<String, JsValue> {
        (|| -> Result<String> {
            let node = collection_node(&self.output.as_ref().context("Apply a decoder first")?.nodes, &self.flattened, path)
                .context("Unknown collection")?;
            let index = index.parse::<u64>()?;
            let end = index.checked_add(1).context("Sample index overflow")?;
            let bytes = crate::structure_selection::sample_bytes(node, index..end)
                .context("Sample outside collection")?;
            json_result(json!({"path":path,"index":index.to_string(),
                "start":bytes.start.to_string(),"end":bytes.end.to_string()}))
        })().map_err(js_error)
    }

    /// Select an inclusive range of collection records without materializing them.
    pub fn pick_range(&self, path: &str, first: &str, last: &str) -> Result<String, JsValue> {
        (|| -> Result<String> {
            let node = collection_node(&self.output.as_ref().context("Apply a decoder first")?.nodes, &self.flattened, path)
                .context("Unknown collection")?;
            let first = first.parse::<u64>()?;
            let last = last.parse::<u64>()?;
            let start = first.min(last);
            let end = first.max(last).checked_add(1).context("Sample index overflow")?;
            let bytes = crate::structure_selection::sample_bytes(node, start..end)
                .context("Sample range outside collection")?;
            json_result(json!({"path":path,"first":first.to_string(),"last":last.to_string(),
                "start":bytes.start.to_string(),"end":bytes.end.to_string()}))
        })().map_err(js_error)
    }

    /// Resolve the selected field on demand, paging only its containing records.
    /// `path` picks a containing node that covers the same bytes (an Inspector
    /// breadcrumb part) over the most specific field.
    pub fn inspect(&self, start: &str, end: &str, path: Option<String>) -> Result<String, JsValue> {
        (|| -> Result<String> {
            let output = self.output.as_ref().context("Apply a decoder first")?;
            let range = start.parse::<u64>()?..end.parse::<u64>()?;
            ensure!(range.start < range.end && range.end <= self.source.len(), "Invalid byte selection");
            let at_path = |path: &str| crate::structure_selection::node_at_path(&output.nodes, &self.source, path);
            let field = match path.as_deref().filter(|path| !path.is_empty()) {
                Some(path) => at_path(path)?.filter(|node| node.range == range),
                None => None,
            };
            let field = match field {
                Some(field) => Some(field),
                None => crate::structure_selection::resolved_field(&output.nodes, &self.source, &range)?,
            };
            let field = field.as_ref().map(|node| -> Result<_> {
                let sample = crate::structure_selection::sample_for_node(&output.nodes, node);
                let parent = match crate::structure_selection::parent_path(&node.path) {
                    Some(parent) => at_path(parent)?,
                    None => None,
                };
                let mut field = crate::workspace_inspector::FieldInfo::from_node(
                    node, sample, self.program.as_ref(), parent.as_ref());
                for (_, part) in crate::structure_selection::path_parts(&node.path) {
                    if let Some(part) = at_path(part)? {
                        field.crumbs.push(crate::workspace_inspector::Crumb {
                            path: part.path, start: part.range.start as u64, end: part.range.end as u64 });
                    }
                }
                Ok(field)
            }).transpose()?;
            json_result(json!({"field":field,"consumed":output.consumed.to_string(),
                "types":output.definitions.len()}))
        })().map_err(js_error)
    }

    /// Encode a whole numeric field with the same rules as the native editor.
    pub fn encode(&self, start: &str, end: &str, value: &str) -> Result<String, JsValue> {
        (|| -> Result<String> {
            let output = self.output.as_ref().context("Apply a decoder first")?;
            let range = start.parse::<u64>()?..end.parse::<u64>()?;
            ensure!(range.start < range.end && range.end <= self.source.len(), "Invalid field range");
            let node = crate::structure_selection::resolved_field(&output.nodes, &self.source, &range)?
                .context("No decoded field covers these bytes")?;
            ensure!(node.range == range, "Select the full decoded field");
            let (bytes, displayed) = crate::field_edit::encode_value(&node, value)?;
            json_result(json!({"start":start,"end":end,"bytes":bytes,"displayed":displayed}))
        })().map_err(js_error)
    }

    pub fn table(&self, path: &str, start: &str) -> Result<String, JsValue> {
        (|| {
            let node = collection_node(&self.output.as_ref().context("Apply a decoder first")?.nodes, &self.flattened, path).context("Unknown collection")?;
            let start = start.parse::<u64>()?;
            let rows = page(node, &self.source, start, 128)?;
            let mut columns = Vec::new();
            if let Some(row) = rows.first() { cell_columns(row, "", &mut columns); }
            json_result(json!({"path":path,"start":start.to_string(),"columns":columns,"rows":rows.iter().enumerate().map(|(i,node)| {
                let mut fields = BTreeMap::new(); cells(node,"", &mut fields);
                json!({"index":(start+i as u64).to_string(),"start":node.range.start.to_string(),
                    "end":node.range.end.to_string(),"cells":fields})
            }).collect::<Vec<_>>()}))
        })().map_err(js_error)
    }
    pub fn tree_page(&self, path: &str, start: &str) -> Result<String, JsValue> {
        (|| -> Result<String> {
            let output = self.output.as_ref().context("Apply a decoder first")?;
            let node = resolve_tree_node(&output.nodes, path, &self.source)?
                .context("Structure node not found")?;
            let start = start.parse::<u64>()?;
            let count = node.lazy.as_ref().map(|array| array.count)
                .unwrap_or(node.children.len() as u64);
            ensure!(start < count, "Structure index outside collection");
            let page_size = node.lazy.as_ref().map(|array| {
                if array.stride == 0 { 16 } else { (1024 * 1024 / array.stride).clamp(1, 64) }
            }).unwrap_or(64);
            let nodes = page(&node, &self.source, start, page_size)?;
            json_result(json!({"path":path,"start":start.to_string(),"count":count.to_string(),"page_size":page_size,
                "nodes":nodes.iter().map(describe).collect::<Vec<_>>() }))
        })().map_err(js_error)
    }
    pub fn plot_many(&self, paths: &str, first: &str, last: &str, width: u32) -> Result<String, JsValue> {
        (|| -> Result<String> {
            let output = self.output.as_ref().context("Apply a decoder first")?;
            let paths: Vec<String> = serde_json::from_str(paths)?;
            let mut seen = std::collections::BTreeSet::new();
            let nodes: Vec<Node> = paths.iter().filter(|path| seen.insert((*path).clone()))
                .map(|path| collection_node(&output.nodes, &self.flattened, path)
                    .cloned().context("Unknown collection")).collect::<Result<_>>()?;
            let range = first.parse::<u64>()?..last.parse::<u64>()?;
            ensure!(range.start <= range.end, "Invalid plot range");
            let results = crate::structure_plot::read_for_collections(self.source.clone(), &nodes, range, u64::from(width))?;
            let mut series = Vec::new();
            let mut scanned = 0;
            let mut estimated = false;
            let mut reduced = false;
            for (paths, data) in results {
                scanned += data.scanned;
                estimated |= data.quality == crate::structure_plot::PlotQuality::SampledPreview;
                reduced |= data.quality != crate::structure_plot::PlotQuality::Exact;
                for (name, points) in &data.series {
                    let (path, field) = if paths.len() == 1 { (paths[0].as_str(), name.as_str()) }
                        else { paths.iter().find_map(|path| name.strip_prefix(&format!("{path} / "))
                            .map(|field| (path.as_str(), field))).context("Grouped field lost its collection")? };
                    let mut channel = plot_channel(path, field, points, data.envelopes.get(name),
                        data.quality == crate::structure_plot::PlotQuality::SampledPreview);
                    channel["name"] = json!(format!("{path} / {field}"));
                    series.push(channel);
                }
            }
            // Plot geometry has a separate bound from schema/table messages.
            // Resolution is controlled by the shared native query policy.
            let text = serde_json::to_string(&json!({"series":series,"scanned":scanned,"estimated":estimated,
                "quality":if estimated {"Estimated range · zoom in for detail"} else if reduced {"Reduced · min/max retained"} else {"Exact samples"}}))?;
            ensure!(text.len() <= 64 * 1024 * 1024, "Plot response exceeds 64 MiB; select fewer collections");
            Ok(text)
        })().map_err(js_error)
    }

    pub fn plot(&self, path: &str, first: &str, last: &str, width: u32) -> Result<String, JsValue> {
        self.plot_budget(path, first, last, width)
    }
}
impl DecoderSession {
    fn plot_budget(&self, path: &str, first: &str, last: &str, width: u32) -> Result<String, JsValue> {
        (|| {
            let node = collection_node(&self.output.as_ref().context("Apply a decoder first")?.nodes, &self.flattened, path).context("Unknown collection")?;
            let total = node.lazy.as_ref().map_or(node.children.len() as u64, |a| a.count);
            let start = first.parse::<u64>()?.min(total);
            let end = last.parse::<u64>()?.min(total);
            ensure!(start <= end, "Invalid plot range");
            let data = crate::structure_plot::read_for_viewport(self.source.clone(), node.clone(), start..end, u64::from(width))?;
            let estimated = data.quality == crate::structure_plot::PlotQuality::SampledPreview;
            let series: Vec<Value> = data.series.iter().map(|(name, points)|
                plot_channel(path, name, points, data.envelopes.get(name), estimated)).collect();
            json_result(json!({"path":path,"start":start.to_string(),"end":end.to_string(),
                "scanned":data.scanned,"quality":data.quality.label(),"estimated":estimated,
                "reduced":data.quality != crate::structure_plot::PlotQuality::Exact,"series":series}))
        })().map_err(js_error)
    }
}
