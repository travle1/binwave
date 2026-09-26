//! Generic, bounded interpretation of bytes using reusable structures and readable actions.
//! References are data paths, never executable expressions. Large fixed-size collections
//! retain a read plan instead of materializing their elements.
use crate::byte_store::{ByteView, READ_LIMIT};
// Native consumers retain their existing ABI; wasm file positions must not use
// the 32-bit memory index type. Only bounded buffers are converted to usize.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) type Address = usize;
#[cfg(target_arch = "wasm32")]
pub(crate) type Address = u64;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const MAX_YAML: Address = 256 * 1024;
const MAX_NODES: Address = 20_000;
const MAX_DEPTH: Address = 64;
const LAZY_THRESHOLD: Address = 128;
const MAX_PAGE: Address = 512;
const MAX_TEXT: Address = 1024 * 1024;
const MAX_PARTIAL_LEAVES: usize = 8_192;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Endian {
    #[default]
    Little,
    Big,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Program {
    #[serde(default)]
    pub endian: Endian,
    #[serde(default, with = "named_structures")]
    pub structures: BTreeMap<String, Structure>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enums: Vec<EnumDef>,
    pub actions: Vec<Action>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnumDef {
    pub name: String,
    pub cases: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otherwise: Option<String>,
}

// YAML declares named records; the execution engine retains an indexed catalog.
mod named_structures {
    use super::*;
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Named {
        name: String,
        #[serde(default)]
        fields: Vec<Field>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        actions: Vec<Action>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endian: Option<Endian>,
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<String, Structure>, D::Error> {
        struct StructureList;
        impl<'de> serde::de::Visitor<'de> for StructureList {
            type Value = Vec<Named>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(
                    "a list of declarations beginning with '- name: RecordType'; mapping-style declarations are no longer supported",
                )
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut entries = Vec::new();
                while let Some(entry) = sequence.next_element()? {
                    entries.push(entry);
                }
                Ok(entries)
            }
        }
        let entries = d.deserialize_seq(StructureList)?;
        let mut map = BTreeMap::new();
        for entry in entries {
            let name = entry.name;
            if map.contains_key(&name) {
                return Err(serde::de::Error::custom(format!("duplicate structure name {name:?}")));
            }
            map.insert(name, Structure { fields: entry.fields, actions: entry.actions, endian: entry.endian });
        }
        Ok(map)
    }
    pub fn serialize<S: serde::Serializer>(map: &BTreeMap<String, Structure>, s: S) -> Result<S::Ok, S::Error> {
        map.iter().map(|(name, structure)| Named {
            name: name.clone(), fields: structure.fields.clone(),
            actions: structure.actions.clone(), endian: structure.endian,
        }).collect::<Vec<_>>().serialize(s)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Structure {
    #[serde(default)]
    pub fields: Vec<Field>,
    #[serde(default)]
    pub actions: Vec<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endian: Option<Endian>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: TypeSpec,
    #[serde(default, skip_serializing_if = "is_false")]
    pub preview: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<Repeat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<Expr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<Expr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative: Option<Expr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Expr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endian: Option<Endian>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Literal>,
}

impl Field {
    pub fn scalar(name: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: TypeSpec::Name(kind.into()),
            preview: false,
            repeat: None,
            length: None,
            at: None,
            relative: None,
            align: None,
            endian: None,
            expect: None,
        }
    }
}
fn is_false(value: &bool) -> bool { !*value }

#[derive(Debug, Clone)]
pub enum TypeSpec {
    Name(String),
    Select { choose: TypeChoice },
}

// Explicit references are a wire-format distinction, not a second execution path.
#[derive(Debug, Clone)]
struct TypeReference(String);
impl<'de> Deserialize<'de> for TypeReference {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged, deny_unknown_fields)]
        enum Wire { Primitive(String), Structure { structure: String } }
        match Wire::deserialize(d)? {
            Wire::Primitive(name) if is_builtin_type(&name) => Ok(Self(name)),
            Wire::Primitive(name) => Err(serde::de::Error::custom(format!("{name:?} is not a built-in type; reference a declared structure with {{structure: {name}}}"))),
            Wire::Structure { structure } => {
                if is_builtin_type(&structure) {
                    return Err(serde::de::Error::custom(format!("{structure:?} is built in; use type: {structure} directly")));
                }
                valid_name(&structure).map_err(serde::de::Error::custom)?;
                Ok(Self(structure))
            }
        }
    }
}
impl Serialize for TypeReference {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        if is_builtin_type(&self.0) { return s.serialize_str(&self.0); }
        let mut map = s.serialize_map(Some(1))?;
        map.serialize_entry("structure", &self.0)?;
        map.end()
    }
}
impl<'de> Deserialize<'de> for TypeSpec {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_yaml_ng::Value::deserialize(d)?;
        if value.as_mapping().is_some_and(|m|m.contains_key(serde_yaml_ng::Value::String("choose".into()))) {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Choice { choose: TypeChoice }
            let choice: Choice = serde_yaml_ng::from_value(value).map_err(serde::de::Error::custom)?;
            Ok(Self::Select {choose: choice.choose})
        } else {
            let reference: TypeReference = serde_yaml_ng::from_value(value).map_err(serde::de::Error::custom)?;
            Ok(Self::Name(reference.0))
        }
    }
}
impl Serialize for TypeSpec {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::Name(name) => TypeReference(name.clone()).serialize(s),
            Self::Select {choose} => {
                let mut map=s.serialize_map(Some(1))?;
                map.serialize_entry("choose", choose)?; map.end()
            }
        }
    }
}
mod type_cases {
    use super::*;
    pub fn deserialize<'de,D:serde::Deserializer<'de>>(d:D)->Result<BTreeMap<String,String>,D::Error> {
        Ok(BTreeMap::<String,TypeReference>::deserialize(d)?.into_iter().map(|(k,v)|(k,v.0)).collect())
    }
    pub fn serialize<S:serde::Serializer>(cases:&BTreeMap<String,String>,s:S)->Result<S::Ok,S::Error> {
        cases.iter().map(|(k,v)|(k,TypeReference(v.clone()))).collect::<BTreeMap<_,_>>().serialize(s)
    }
}
mod optional_type {
    use super::*;
    pub fn deserialize<'de,D:serde::Deserializer<'de>>(d:D)->Result<Option<String>,D::Error> {
        Ok(Option::<TypeReference>::deserialize(d)?.map(|v|v.0))
    }
    pub fn serialize<S:serde::Serializer>(v:&Option<String>,s:S)->Result<S::Ok,S::Error> {
        v.as_ref().map(|v|TypeReference(v.clone())).serialize(s)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TypeChoice {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default, rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_name: Option<String>,
    #[serde(default, with = "type_cases")]
    pub cases: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "optional_type")]
    pub otherwise: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Repeat {
    Until(String),
    Count(Expr),
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Expr {
    Unsigned(u64),
    Signed(i64),
    Keyword(String),
    Ref {
        #[serde(rename = "ref")]
        reference: String,
    },
    Choose {
        choose: ExprChoice,
    },
    Add {
        add: Vec<Expr>,
    },
    Subtract {
        subtract: Vec<Expr>,
    },
    Multiply {
        multiply: Vec<Expr>,
    },
    Divide {
        divide: Vec<Expr>,
    },
}

impl<'de> Deserialize<'de> for Expr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ExprVisitor;

        impl<'de> serde::de::Visitor<'de> for ExprVisitor {
            type Value = Expr;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(
                    "a number, remaining, {ref: field}, {choose: ...}, or an arithmetic expression",
                )
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(Expr::Unsigned(value))
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                if value >= 0 {
                    Ok(Expr::Unsigned(value as u64))
                } else {
                    Ok(Expr::Signed(value))
                }
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(Expr::Keyword(value.into()))
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(Expr::Keyword(value))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let key: String = map.next_key()?.ok_or_else(|| {
                    serde::de::Error::custom("an expression mapping cannot be empty")
                })?;
                let expression = match key.as_str() {
                    "ref" => Expr::Ref { reference: map.next_value()? },
                    "choose" => Expr::Choose { choose: map.next_value()? },
                    "add" => Expr::Add { add: map.next_value()? },
                    "subtract" => Expr::Subtract { subtract: map.next_value()? },
                    "multiply" => Expr::Multiply { multiply: map.next_value()? },
                    "divide" => Expr::Divide { divide: map.next_value()? },
                    unknown => {
                        let _: serde::de::IgnoredAny = map.next_value()?;
                        return Err(serde::de::Error::unknown_field(
                            unknown,
                            &["ref", "choose", "add", "subtract", "multiply", "divide"],
                        ));
                    }
                };
                if let Some(extra) = map.next_key::<String>()? {
                    let _: serde::de::IgnoredAny = map.next_value()?;
                    return Err(serde::de::Error::custom(format!(
                        "an expression has one operation; remove the extra {extra:?} key"
                    )));
                }
                Ok(expression)
            }
        }

        deserializer.deserialize_any(ExprVisitor)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExprChoice {
    #[serde(rename = "ref")]
    pub reference: String,
    pub cases: BTreeMap<String, Expr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otherwise: Option<Box<Expr>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Literal {
    Unsigned(u64),
    Signed(i64),
    Text(String),
    Float(f64),
}

#[derive(Debug, Clone)]
pub enum Action {
    Read(Field),
    Define(Define),
    Skip(Expr),
    Align(Expr),
    Calculate(Calculate),
    Choose(ChooseActions),
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionWire {
    read: Option<Field>,
    define: Option<Define>,
    skip: Option<Expr>,
    align: Option<Expr>,
    calculate: Option<Calculate>,
    choose: Option<ChooseActions>,
}
impl<'de> Deserialize<'de> for Action {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let wire = ActionWire::deserialize(deserializer)?;
        let count = Address::from(wire.read.is_some())
            + Address::from(wire.define.is_some())
            + Address::from(wire.skip.is_some())
            + Address::from(wire.align.is_some())
            + Address::from(wire.calculate.is_some())
            + Address::from(wire.choose.is_some());
        if count != 1 {
            return Err(serde::de::Error::custom("each action must contain exactly one of read, define, skip, align, calculate or choose"));
        }
        Ok(if let Some(v) = wire.read {
            Self::Read(v)
        } else if let Some(v) = wire.define {
            Self::Define(v)
        } else if let Some(v) = wire.skip {
            Self::Skip(v)
        } else if let Some(v) = wire.align {
            Self::Align(v)
        } else if let Some(v) = wire.calculate {
            Self::Calculate(v)
        } else {
            Self::Choose(wire.choose.unwrap())
        })
    }
}
impl Serialize for Action {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            Self::Read(v) => map.serialize_entry("read", v)?,
            Self::Define(v) => map.serialize_entry("define", v)?,
            Self::Skip(v) => map.serialize_entry("skip", v)?,
            Self::Align(v) => map.serialize_entry("align", v)?,
            Self::Calculate(v) => map.serialize_entry("calculate", v)?,
            Self::Choose(v) => map.serialize_entry("choose", v)?,
        }
        map.end()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Calculate {
    pub name: String,
    pub value: Expr,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChooseActions {
    #[serde(rename = "ref")]
    pub reference: String,
    pub cases: BTreeMap<String, Vec<Action>>,
    #[serde(default)]
    pub otherwise: Vec<Action>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Define {
    pub name: String,
    pub from: String,
    pub fields: FieldMapping,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FieldMapping {
    #[serde(default)]
    pub name: Option<Reference>,
    #[serde(rename = "type")]
    pub kind: TypeChoice,
    #[serde(default)]
    pub repeat: Option<Expr>,
    #[serde(default)]
    pub items_per_group: Option<Expr>,
    #[serde(default)]
    pub length: Option<Expr>,
    #[serde(default)]
    pub at: Option<Expr>,
    #[serde(default)]
    pub relative: Option<Expr>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default, rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}
impl Value {
    pub fn numeric(&self) -> Option<f64> {
        match self {
            Self::Signed(v) => Some(*v as f64),
            Self::Unsigned(v) => Some(*v as f64),
            Self::Float(v) => Some(*v),
            _ => None,
        }
    }
    fn integer(&self) -> Result<i128> {
        match self {
            Self::Signed(v) => Ok(*v as i128),
            Self::Unsigned(v) => Ok(*v as i128),
            _ => bail!("reference must contain an integer"),
        }
    }
    fn key(&self) -> String {
        match self {
            Self::Text(v) => v.clone(),
            _ => self.to_string(),
        }
    }
}
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Signed(v) => write!(f, "{v}"),
            Self::Unsigned(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::Text(v) => write!(f, "{v}"),
            Self::Bytes(v) => {
                for (i, byte) in v.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{byte:02X}")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub path: String,
    pub range: Range<Address>,
    pub kind: String,
    pub value: Option<Value>,
    pub children: Vec<Node>,
    pub lazy: Option<LazyArray>,
    pub endian: Endian,
    pub references: Vec<String>,
    pub preview: Option<String>,
}
impl Node {
    pub fn tree_label(&self) -> String {
        if self.name.starts_with('[') && self.name.ends_with(']') {
            if let Some((parent, index)) = self.path.rsplit_once('[') {
                if let Ok(index) = index.trim_end_matches(']').parse::<usize>() {
                    let parent = parent.rsplit('.').next().unwrap_or(parent);
                    return format!("{parent}[{index}]");
                }
            }
        }
        self.name.clone()
    }
    pub fn tree_summary(&self) -> String {
        if self.lazy.is_some() || self.value.is_some() {
            return format!("{} · {}", self.kind, self.display());
        }
        let bytes = self.range.end.saturating_sub(self.range.start);
        let preview = self.preview.as_ref().map_or_else(String::new, |value| format!(" · {value}"));
        if self.kind == "partial collection" {
            format!("{} items decoded · incomplete · starts 0x{:X}", self.children.len(), self.range.start)
        } else if self.kind == "partial" {
            format!("{bytes} bytes decoded{preview} · incomplete · starts 0x{:X}", self.range.start)
        } else if self.kind.ends_with("[]") {
            format!("{} items · {bytes} bytes · starts 0x{:X}", self.children.len(), self.range.start)
        } else {
            format!("{}{preview} · {bytes} bytes · starts 0x{:X}", self.kind, self.range.start)
        }
    }
    pub fn numeric_tooltip(&self) -> Option<String> {
        let (decimal, raw) = match self.value.as_ref()? {
            Value::Signed(value) => (value.to_string(), *value as u64),
            Value::Unsigned(value) => (value.to_string(), *value),
            _ => return None,
        };
        let bits = match self.kind.as_str() {
            "u8" | "i8" => 8,
            "u16" | "i16" => 16,
            "u24" | "i24" => 24,
            "u32" | "i32" => 32,
            "u64" | "i64" => 64,
            _ => self.range.end.saturating_sub(self.range.start).saturating_mul(8).clamp(1, 64) as usize,
        };
        let raw = if bits == 64 { raw } else { raw & ((1_u64 << bits) - 1) };
        Some(format!(
            "{} · {bits} bits\nDecimal: {decimal}\nHex: 0x{raw:0hex_width$X}\nBinary: 0b{raw:0binary_width$b}",
            self.kind,
            hex_width = (bits + 3) / 4,
            binary_width = bits,
        ))
    }
    pub fn numeric(&self) -> Option<f64> {
        self.value.as_ref().and_then(Value::numeric)
    }
    pub fn display(&self) -> String {
        if let Some(array) = &self.lazy {
            if array.segmented.is_some() {
                format!("{} items · across groups", array.count)
            } else if array.offsets.is_some() {
                format!("{} items · variable size", array.count)
            } else {
                format!("{} items · {} bytes each", array.count, array.stride)
            }
        } else if let Some(value) = &self.value {
            value.to_string()
        } else {
            format!("{} bytes", (self.range.end - self.range.start))
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParseOutput {
    pub nodes: Vec<Node>,
    pub definitions: Vec<ResolvedDefinition>,
    pub revision: u64,
    pub consumed: Address,
}
#[derive(Debug)]
pub struct ParseFailure {
    pub error: anyhow::Error,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub output: ParseOutput,
}
#[derive(Debug, Clone)]
pub struct ResolvedDefinition {
    pub name: String,
    pub origin: Option<Range<Address>>,
    pub fields: Vec<ResolvedField>,
}
#[derive(Debug, Clone)]
pub struct ResolvedField {
    pub name: String,
    pub kind: String,
    pub repeat: Option<String>,
    pub length: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LazyArray {
    pub count: Address,
    pub stride: Address,
    /// Boundaries for variable-size records, including the final end offset.
    /// Fixed-size arrays calculate boundaries from their stride instead.
    offsets: Option<Arc<Vec<Address>>>,
    segmented: Option<SegmentedArray>,
    plan: Arc<LazyPlan>,
}
#[derive(Debug, Clone)]
struct SegmentedArray {
    outer_start: Address,
    outer_stride: Address,
    inner_offset: Address,
    inner_count: Address,
    inner_stride: Address,
}
#[derive(Debug, Clone)]
struct LazyPlan {
    field: Field,
    start: Address,
    end: Address,
    path: String,
    endian: Endian,
    values: Arc<HashMap<String, Value>>,
    structures: Arc<BTreeMap<String, Structure>>,
    enums: Arc<Vec<EnumDef>>,
    revision: u64,
}
/// A conservative numeric projection of a constant decoded structure. Unsupported
/// features fall back to the ordinary parser; a sampled record never establishes
/// the layout of other records.
#[derive(Clone)]
pub(crate) struct NumericPlan {
    pub start: Address,
    pub stride: Address,
    segmented: Option<SegmentedArray>,
    pub path: String,
    pub revision: u64,
    pub fields: Vec<NumericField>,
}
impl NumericPlan {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn cache_layout_key(&self) -> String {
        let segment = self.segmented.as_ref().map(|part| [
            part.outer_start, part.outer_stride, part.inner_offset,
            part.inner_count, part.inner_stride,
        ]);
        let fields: Vec<_> = self.fields.iter().map(|field| serde_json::json!([
            field.name, field.suffix, field.offset, field.width, field.span,
            field.source_path, field.kind, field.endian,
        ])).collect();
        serde_json::json!({
            "version": 1, "start": self.start, "stride": self.stride,
            "segment": segment, "path": self.path, "fields": fields,
        }).to_string()
    }
    /// Combine projections that address different fields of the same fixed
    /// record sequence. The layout comes from decoded array metadata, not from
    /// a file-format name or a sampled record.
    pub(crate) fn combine(plans: &[NumericPlan]) -> Option<Self> {
        let first = plans.first()?;
        let reference = first.segmented.as_ref()?;
        if plans.iter().any(|plan| {
            let Some(segment) = plan.segmented.as_ref() else { return true; };
            plan.revision != first.revision
                || segment.outer_start != reference.outer_start
                || segment.outer_stride != reference.outer_stride
                || segment.inner_count != reference.inner_count
                || segment.inner_stride != reference.inner_stride
        }) { return None; }
        let base = plans.iter().filter_map(|plan| plan.segmented.as_ref().map(|s| s.inner_offset)).min()?;
        let mut combined = first.clone();
        combined.segmented.as_mut()?.inner_offset = base;
        combined.start = reference.outer_start.checked_add(base)?;
        combined.fields.clear();
        let mut end = 0;
        for plan in plans {
            let shift = plan.segmented.as_ref()?.inner_offset.checked_sub(base)?;
            for field in &plan.fields {
                let mut field = field.clone();
                field.offset = field.offset.checked_add(shift)?;
                end = end.max(field.offset.checked_add(field.width)?);
                field.name = format!("{} / {}", plan.path, field.name);
                field.source_path = Some(plan.path.clone());
                combined.fields.push(field);
            }
        }
        if end == 0 || end > reference.inner_stride || end > crate::byte_store::READ_LIMIT { return None; }
        combined.stride = end;
        Some(combined)
    }
    pub(crate) fn physical_stride(&self) -> Address {
        self.segmented.as_ref().map_or(self.stride, |segment| {
            if segment.inner_count == 1 { segment.outer_stride } else { segment.inner_stride }
        })
    }
    pub(crate) fn offset(&self, index: Address) -> Option<Address> {
        if let Some(segment) = &self.segmented {
            let group = index / segment.inner_count;
            let within = index % segment.inner_count;
            segment.outer_start.checked_add(group.checked_mul(segment.outer_stride)?)?
                .checked_add(segment.inner_offset)?
                .checked_add(within.checked_mul(segment.inner_stride)?)
        } else {
            self.start.checked_add(index.checked_mul(self.stride)?)
        }
    }
    pub(crate) fn contiguous_count(&self, index: Address) -> Address {
        self.segmented.as_ref().map_or(Address::MAX, |segment|
            if segment.inner_count == 1 { Address::MAX }
            else { segment.inner_count - index % segment.inner_count })
    }
}
#[derive(Clone)]
pub(crate) struct NumericField {
    pub name: String,
    pub suffix: String,
    pub offset: Address,
    pub width: Address,
    pub span: Address,
    pub source_path: Option<String>,
    kind: String,
    endian: Endian,
}
impl NumericField {
    pub fn decode(&self, record: &[u8]) -> Result<f64> {
        let bytes = record
            .get(
                usize::try_from(self.offset).context("field offset exceeds memory range")?
                    ..usize::try_from(
                        self.offset
                            .checked_add(self.width)
                            .context("field offset overflow")?,
                    )
                    .context("field end exceeds memory range")?,
            )
            .context("numeric field exceeds its record")?;
        decode_primitive(&self.kind, bytes, self.endian)?
            .numeric()
            .context("field is not numeric")
    }
}
fn constant_size(expr: &Expr) -> Option<Address> {
    match expr {
        Expr::Unsigned(value) => Address::try_from(*value).ok(),
        Expr::Signed(value) => Address::try_from(*value).ok(),
        _ => None,
    }
}
fn project_numeric(
    field: &Field,
    structures: &BTreeMap<String, Structure>,
    offset: Address,
    prefix: &str,
    inherited: Endian,
    depth: Address,
    budget: &mut Address,
    fields: &mut Vec<NumericField>,
) -> Option<Address> {
    if depth > MAX_DEPTH
        || *budget == 0
        || field.at.is_some()
        || field.relative.is_some()
        || field.align.is_some()
        || field.expect.is_some()
    {
        return None;
    }
    *budget -= 1;
    if let Some(repeat) = &field.repeat {
        let Repeat::Count(count) = repeat else { return None; };
        let count = constant_size(count)?;
        if count > *budget { return None; }
        let mut element = field.clone();
        element.repeat = None;
        let mut consumed = 0 as Address;
        for index in 0..count {
            let item_prefix = if prefix.is_empty() {
                format!("[{index}]")
            } else {
                format!("{prefix}.[{index}]")
            };
            consumed = consumed.checked_add(project_numeric(
                &element, structures, offset.checked_add(consumed)?, &item_prefix,
                inherited, depth + 1, budget, fields,
            )?)?;
        }
        return Some(consumed);
    }
    let TypeSpec::Name(kind) = &field.kind else {
        return None;
    };
    let length = match &field.length {
        Some(expr) => Some(constant_size(expr)?),
        None => None,
    };
    let inherited = field.endian.unwrap_or(inherited);
    let consumed = if let Some(width) = primitive_width(kind) {
        {
            fields.push(NumericField {
                name: if prefix.is_empty() {
                    "value".into()
                } else {
                    prefix.into()
                },
                suffix: if prefix.is_empty() {
                    String::new()
                } else {
                    format!(".{prefix}")
                },
                offset,
                width,
                span: length.unwrap_or(width),
                source_path: None,
                kind: kind.clone(),
                endian: inherited,
            });
        }
        width
    } else if kind == "bytes" {
        length?
    } else {
        let structure = structures.get(kind)?;
        if !structure.actions.is_empty() {
            return None;
        }
        let endian = field.endian.or(structure.endian).unwrap_or(inherited);
        let mut consumed = 0 as Address;
        for child in &structure.fields {
            let prefix = if prefix.is_empty() {
                child.name.clone()
            } else {
                format!("{prefix}.{}", child.name)
            };
            consumed = consumed.checked_add(project_numeric(
                child,
                structures,
                offset.checked_add(consumed)?,
                &prefix,
                endian,
                depth + 1,
                budget,
                fields,
            )?)?;
        }
        consumed
    };
    if length.is_some_and(|length| length < consumed) {
        return None;
    }
    Some(length.unwrap_or(consumed))
}
impl LazyArray {
    /// A view of the same fixed-size child collection in every fixed-size
    /// parent record. No sample data or per-sample offsets are materialized.
    pub(crate) fn flattened_child(parent: &Node, child: &Node) -> Option<Node> {
        let outer = parent.lazy.as_ref()?;
        let inner = child.lazy.as_ref()?;
        if outer.count == 0 || inner.count == 0 || outer.stride == 0 || inner.stride == 0
            || outer.offsets.is_some() || outer.segmented.is_some()
            || inner.offsets.is_some() || inner.segmented.is_some() {
            return None;
        }
        let outer_start = outer.bounds(0)?.start;
        let inner_start = inner.bounds(0)?.start;
        let inner_offset = inner_start.checked_sub(outer_start)?;
        let inner_bytes = inner.count.checked_mul(inner.stride)?;
        if inner_offset.checked_add(inner_bytes)? > outer.stride {
            return None;
        }
        let count = outer.count.checked_mul(inner.count)?;
        let item_prefix = format!("{}[0]", parent.path);
        let child_suffix = child.path.strip_prefix(&item_prefix)?;
        if !child_suffix.starts_with('.') { return None; }
        let path = format!("{}{}", parent.path, child_suffix);
        let mut plan = (*inner.plan).clone();
        plan.start = inner_start;
        plan.end = parent.range.end;
        plan.path = path.clone();
        let array = Self {
            count,
            stride: inner.stride,
            offsets: None,
            segmented: Some(SegmentedArray {
                outer_start,
                outer_stride: outer.stride,
                inner_offset,
                inner_count: inner.count,
                inner_stride: inner.stride,
            }),
            plan: Arc::new(plan),
        };
        let end = array.bounds(count - 1)?.end;
        Some(Node {
            name: format!("{} across {}", child.name, parent.name),
            path,
            range: inner_start..end,
            kind: child.kind.clone(),
            value: None,
            children: Vec::new(),
            lazy: Some(array),
            endian: child.endian,
            references: child.references.clone(),
            preview: None,
        })
    }
    /// One indexed value from every fixed-size group in every parent record.
    pub(crate) fn flattened_group_sensor(parent: &Node, group: &Node, child: &Node, index: Address) -> Option<Node> {
        let outer = parent.lazy.as_ref()?;
        let groups = group.lazy.as_ref()?;
        let (inner_count, width, value_start, mut plan) = if let Some(inner) = child.lazy.as_ref() {
            (inner.count, inner.stride, inner.bounds(index)?.start, (*inner.plan).clone())
        } else {
            let item = child.children.get(usize::try_from(index).ok()?)?;
            let width = item.range.end.checked_sub(item.range.start)?;
            if width == 0 || child.children.iter().enumerate().any(|(position, value)|
                value.range.end.checked_sub(value.range.start) != Some(width)
                    || (position as Address).checked_mul(width)
                        .and_then(|offset| child.range.start.checked_add(offset)) != Some(value.range.start)) {
                return None;
            }
            let mut plan = (*groups.plan).clone();
            plan.field = Field::scalar("value", item.kind.clone());
            plan.endian = item.endian;
            (child.children.len() as Address, width, item.range.start, plan)
        };
        if index >= inner_count || outer.count == 0 || groups.count == 0 || inner_count == 0
            || outer.stride == 0 || groups.stride == 0 || width == 0
            || outer.offsets.is_some() || outer.segmented.is_some()
            || groups.offsets.is_some() || groups.segmented.is_some()
            || child.lazy.as_ref().is_some_and(|inner| inner.offsets.is_some() || inner.segmented.is_some()) {
            return None;
        }
        let outer_start = outer.bounds(0)?.start;
        let inner_offset = value_start.checked_sub(outer_start)?;
        let last_group_offset = (groups.count - 1).checked_mul(groups.stride)?;
        if inner_offset.checked_add(last_group_offset)?.checked_add(width)? > outer.stride { return None; }
        let item_prefix = format!("{}[0]", parent.path);
        let group_suffix = group.path.strip_prefix(&item_prefix)?;
        let child_suffix = child.path.strip_prefix(&format!("{}[0]", group.path))?;
        if !group_suffix.starts_with('.') || !child_suffix.starts_with('.') { return None; }
        let path = format!("{}{}[{index}]{}", parent.path, group_suffix, child_suffix);
        let count = outer.count.checked_mul(groups.count)?;
        plan.start = outer_start.checked_add(inner_offset)?;
        plan.end = parent.range.end;
        plan.path = path.clone();
        let array = Self {
            count,
            stride: width,
            offsets: None,
            segmented: Some(SegmentedArray {
                outer_start,
                outer_stride: outer.stride,
                inner_offset,
                inner_count: groups.count,
                inner_stride: groups.stride,
            }),
            plan: Arc::new(plan),
        };
        let end = array.bounds(count - 1)?.end;
        Some(Node {
            name: format!("{}[{index}] across {}", group.name, parent.name),
            path,
            range: outer_start.checked_add(inner_offset)?..end,
            kind: if child.lazy.is_some() { child.kind.clone() } else { child.children[0].kind.clone() },
            value: None,
            children: Vec::new(),
            lazy: Some(array),
            endian: child.endian,
            references: child.references.clone(),
            preview: None,
        })
    }
    pub(crate) fn indices_for_bytes(&self, bytes: &Range<Address>) -> Option<Range<Address>> {
        if self.count == 0 || bytes.is_empty() { return None; }
        if self.segmented.is_some() {
            let mut low = 0;
            let mut high = self.count;
            while low < high {
                let mid = low + (high - low) / 2;
                if self.bounds(mid)?.end <= bytes.start { low = mid + 1; } else { high = mid; }
            }
            let first = low;
            high = self.count;
            while low < high {
                let mid = low + (high - low) / 2;
                if self.bounds(mid)?.start < bytes.end { low = mid + 1; } else { high = mid; }
            }
            return (first < low).then_some(first..low);
        }
        if let Some(offsets) = &self.offsets {
            let first = offsets.partition_point(|offset| *offset <= bytes.start).saturating_sub(1);
            let end = offsets.partition_point(|offset| *offset < bytes.end).min(offsets.len() - 1);
            return (first < end).then_some(first as Address..end as Address);
        }
        if self.stride == 0 { return None; }
        let first = (bytes.start - self.plan.start) / self.stride;
        let end = (bytes.end - self.plan.start).div_ceil(self.stride).min(self.count);
        (first < end).then_some(first..end)
    }
    pub(crate) fn bounds(&self, index: Address) -> Option<Range<Address>> {
        if index >= self.count { return None; }
        if let Some(segment) = &self.segmented {
            let group = index / segment.inner_count;
            let within = index % segment.inner_count;
            let start = segment.outer_start
                .checked_add(group.checked_mul(segment.outer_stride)?)?
                .checked_add(segment.inner_offset)?
                .checked_add(within.checked_mul(segment.inner_stride)?)?;
            return Some(start..start.checked_add(self.stride)?);
        }
        if let Some(offsets) = &self.offsets {
            let index = usize::try_from(index).ok()?;
            return Some(*offsets.get(index)?..*offsets.get(index + 1)?);
        }
        let start = self.plan.start.checked_add(index.checked_mul(self.stride)?)?;
        Some(start..start.checked_add(self.stride)?)
    }

    pub(crate) fn numeric_plan(&self) -> Option<NumericPlan> {
        let mut fields = Vec::new();
        let mut budget = MAX_NODES;
        let size = project_numeric(
            &self.plan.field,
            &self.plan.structures,
            0,
            "",
            self.plan.endian,
            0,
            &mut budget,
            &mut fields,
        )?;
        if size != self.stride || fields.is_empty() {
            return None;
        }
        Some(NumericPlan {
            start: self.plan.start,
            stride: self.stride,
            segmented: self.segmented.clone(),
            path: self.plan.path.clone(),
            revision: self.plan.revision,
            fields,
        })
    }
    /// A decode plan is shared by cloned collection nodes and replaced when a
    /// definition is applied. Together with the byte revision this identifies
    /// cached interpretations, including redefinitions with the same path.
    pub(crate) fn identity(&self) -> Address {
        Arc::as_ptr(&self.plan) as Address
    }

    pub fn page(
        &self,
        source: &ByteView,
        cancel: &AtomicBool,
        start: Address,
        count: Address,
    ) -> Result<Vec<Node>> {
        ensure!(
            source.revision == self.plan.revision,
            "Bytes changed; apply the definition again before expanding this collection."
        );
        source.validate()?;
        ensure!(
            count <= MAX_PAGE,
            "request at most {MAX_PAGE} items per page"
        );
        ensure!(start <= self.count, "collection index is out of bounds");
        ensure!(!cancel.load(Ordering::Relaxed), "parsing cancelled");
        let page_end = start.saturating_add(count).min(self.count);
        let offset = if start < page_end { self.bounds(start).map(|r| r.start) } else { None };
        let length = if start < page_end {
            self.bounds(page_end - 1).and_then(|r| offset.map(|start| r.end - start))
        } else { None };
        // A page normally occupies a small contiguous region. Decode its fields
        // from one bounded read rather than reopening the file for every scalar.
        // Explicit field references outside that region still use the source.
        let window = match (offset, length) {
            (Some(start), Some(length)) if length <= READ_LIMIT => Some(PageSource {
                source,
                start,
                bytes: source.read(start, length)?,
            }),
            _ => None,
        };
        let input: &dyn Source = window.as_ref().map_or(source as &dyn Source, |w| w);
        let nodes = self.page_from(input, cancel, start, count)?;
        source.validate()?;
        Ok(nodes)
    }
    fn page_from(
        &self,
        source: &dyn Source,
        cancel: &AtomicBool,
        start: Address,
        count: Address,
    ) -> Result<Vec<Node>> {
        ensure!(
            count <= MAX_PAGE,
            "request at most {MAX_PAGE} items per page"
        );
        ensure!(start <= self.count, "collection index is out of bounds");
        let mut engine = Engine::new(
            source,
            cancel,
            self.plan.endian,
            (*self.plan.structures).clone(),
            (*self.plan.enums).clone(),
            self.plan.revision,
        );
        engine.values = (*self.plan.values).clone();
        let end = start.saturating_add(count).min(self.count);
        let mut nodes =
            Vec::with_capacity(usize::try_from(end - start).context("page exceeds memory range")?);
        for index in start..end {
            let bounds = self.bounds(index).context("array offset overflow")?;
            let mut cursor = bounds.start;
            // Each lazy item has its own parsing budget. A page of individually
            // valid records must not fail because their node counts add up.
            engine.nodes = 0;
            if self.offsets.is_some() {
                engine.values = (*self.plan.values).clone();
                engine.collections.clear();
            }
            let path = format!("{}[{index}]", self.plan.path);
            let mut node = engine.single(
                &self.plan.field,
                &path,
                &mut cursor,
                if self.offsets.is_some() { bounds.end } else { self.plan.end },
                self.plan.endian,
                0,
            )?;
            ensure!((self.offsets.is_none() && self.segmented.is_none()) || cursor == bounds.end, "{path}: record size changed since indexing");
            node.name = format!("[{index}]");
            nodes.push(node);
        }
        Ok(nodes)
    }
}

struct PageSource<'a> {
    source: &'a ByteView,
    start: Address,
    bytes: Vec<u8>,
}
impl Source for PageSource<'_> {
    fn len(&self) -> Address {
        self.source.len()
    }
    fn read(&self, offset: Address, length: Address) -> Result<Vec<u8>> {
        if let Some(relative) = offset.checked_sub(self.start) {
            if let Some(bytes) = relative.checked_add(length).and_then(|end| {
                self.bytes
                    .get(usize::try_from(relative).ok()?..usize::try_from(end).ok()?)
            }) {
                return Ok(bytes.to_vec());
            }
        }
        self.source.read(offset, length)
    }
}

/// Sequential scans reuse one bounded read for many scalar fields.
struct ScanSource<'a> {
    source: &'a dyn Source,
    cache: std::cell::RefCell<(Address, Vec<u8>)>,
}
impl<'a> ScanSource<'a> {
    fn new(source: &'a dyn Source) -> Self {
        Self { source, cache: std::cell::RefCell::new((0, Vec::new())) }
    }
}
impl Source for ScanSource<'_> {
    fn len(&self) -> Address { self.source.len() }
    fn read(&self, offset: Address, length: Address) -> Result<Vec<u8>> {
        const WINDOW: Address = 64 * 1024;
        if length > WINDOW { return self.source.read(offset, length); }
        {
            let cache = self.cache.borrow();
            if let Some(start) = offset.checked_sub(cache.0) {
                if let Some(bytes) = start.checked_add(length).and_then(|end| {
                    cache.1.get(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
                }) {
                    return Ok(bytes.to_vec());
                }
            }
        }
        ensure!(offset <= self.source.len(), "scan offset is outside the file");
        let bytes = self.source.read(offset, WINDOW.min(self.source.len() - offset))?;
        let result = bytes.get(..usize::try_from(length).context("scan length exceeds memory range")?)
            .context("truncated source during record scan")?.to_vec();
        *self.cache.borrow_mut() = (offset, bytes);
        Ok(result)
    }
}

trait Source {
    fn len(&self) -> Address;
    fn read(&self, offset: Address, length: Address) -> Result<Vec<u8>>;
}
impl Source for ByteView {
    fn len(&self) -> Address {
        self.len()
    }
    fn read(&self, offset: Address, length: Address) -> Result<Vec<u8>> {
        self.read(offset, length)
    }
}

impl Program {
    pub fn parse(yaml: &str) -> Result<Self> {
        ensure!(
            yaml.len() as Address <= MAX_YAML,
            "definition exceeds 256 KiB"
        );
        let program: Self = serde_yaml_ng::from_str(yaml).context("read structure YAML")?;
        ensure!(!program.actions.is_empty(), "define at least one action");
        ensure!(
            program.structures.len() <= 256,
            "at most 256 structures are allowed"
        );
        ensure!(program.enums.len() <= 256, "at most 256 enums are allowed");
        let mut enum_names = HashSet::new();
        for declared in &program.enums {
            valid_name(&declared.name)?;
            ensure!(enum_names.insert(&declared.name), "duplicate enum {:?}", declared.name);
            ensure!(!declared.cases.is_empty(), "enum {:?} has no cases", declared.name);
            ensure!(declared.cases.len() <= 4096, "enum {:?} has too many cases", declared.name);
        }
        for (name, structure) in &program.structures {
            valid_name(name)?;
            ensure!(
                !is_builtin_type(name),
                "structure name {name:?} is reserved"
            );
            ensure!(
                structure.fields.len() <= 4096,
                "structure {name} has too many fields"
            );
            let mut seen = std::collections::HashSet::new();
            let mut preview_count = 0;
            for field in &structure.fields {
                valid_field(field)?;
                if field.preview { preview_count += 1; }
                ensure!(
                    seen.insert(&field.name),
                    "duplicate field {} in {name}",
                    field.name
                );
            }
            ensure!(preview_count <= 1, "structure {name} marks more than one preview field");
            validate_actions(&structure.actions, 0)?;
        }
        validate_actions(&program.actions, 0)?;
        Ok(program)
    }
    pub fn run(&self, source: &ByteView, cancel: &AtomicBool) -> Result<ParseOutput> {
        self.run_with_partial(source, cancel).map_err(|failure| failure.error)
    }
    pub fn run_with_partial(
        &self,
        source: &ByteView,
        cancel: &AtomicBool,
    ) -> std::result::Result<ParseOutput, ParseFailure> {
        source.validate().map_err(|error| ParseFailure {
            error,
            output: ParseOutput { nodes: Vec::new(), definitions: Vec::new(), revision: source.revision, consumed: 0 },
        })?;
        let output = self.run_from_with_partial(source, cancel, source.revision)?;
        source.validate().map_err(|error| ParseFailure { error, output: output.clone() })?;
        Ok(output)
    }
    #[cfg(test)]
    fn run_from(
        &self,
        source: &dyn Source,
        cancel: &AtomicBool,
        revision: u64,
    ) -> Result<ParseOutput> {
        self.run_from_with_partial(source, cancel, revision).map_err(|failure| failure.error)
    }
    fn run_from_with_partial(
        &self,
        source: &dyn Source,
        cancel: &AtomicBool,
        revision: u64,
    ) -> std::result::Result<ParseOutput, ParseFailure> {
        let mut engine = Engine::new(
            source,
            cancel,
            self.endian,
            self.structures.clone(),
            self.enums.clone(),
            revision,
        );
        for (name, structure) in &self.structures {
            engine.definitions.push(describe(name, structure, None));
        }
        let mut cursor = 0;
        let mut nodes = Vec::new();
        if let Err(error) = engine.actions(
            &self.actions,
            "",
            &mut cursor,
            source.len(),
            self.endian,
            &mut nodes,
            0,
        ) {
            return Err(ParseFailure {
                error,
                output: ParseOutput {
                    nodes: partial_tree(&nodes, &engine.progress),
                    definitions: engine.definitions,
                    revision,
                    consumed: cursor,
                },
            });
        }
        Ok(ParseOutput {
            nodes,
            definitions: engine.definitions,
            revision,
            consumed: cursor,
        })
    }
}

fn valid_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-'),
        "name {name:?} must contain only letters, digits, '_' or '-'"
    );
    Ok(())
}
fn valid_field(field: &Field) -> Result<()> {
    valid_name(&field.name)?;
    ensure!(
        field.at.is_none() || field.relative.is_none(),
        "{}: use either at or relative",
        field.name
    );
    if let Some(Repeat::Until(mode)) = &field.repeat {
        ensure!(
            mode == "until end",
            "{}: repeat must be a count or 'until end'",
            field.name
        );
    }
    if let Some(length) = &field.length {
        valid_expr(length, true)?;
    }
    for expression in [&field.at, &field.relative, &field.align]
        .into_iter()
        .flatten()
    {
        valid_expr(expression, false)?;
    }
    if let Some(Repeat::Count(count)) = &field.repeat {
        valid_expr(count, false)?;
    }
    Ok(())
}

fn valid_expr(expr: &Expr, allow_remaining: bool) -> Result<()> {
    match expr {
        Expr::Keyword(keyword) if keyword == "remaining" => ensure!(
            allow_remaining,
            "`remaining` is only valid as a field length"
        ),
        Expr::Keyword(keyword) => {
            bail!("unknown expression keyword {keyword:?}; use a number, reference, or `remaining` length")
        }
        Expr::Choose { choose } => {
            ensure!(
                !choose.cases.is_empty() || choose.otherwise.is_some(),
                "length choose needs at least one case or otherwise"
            );
            for branch in choose.cases.values() {
                valid_expr(branch, allow_remaining)?;
            }
            if let Some(branch) = &choose.otherwise {
                valid_expr(branch, allow_remaining)?;
            }
        }
        Expr::Add { add }
        | Expr::Subtract { subtract: add }
        | Expr::Multiply { multiply: add }
        | Expr::Divide { divide: add } => {
            for operand in add {
                valid_expr(operand, false)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn validate_actions(actions: &[Action], depth: Address) -> Result<()> {
    ensure!(
        depth <= MAX_DEPTH && actions.len() <= 4096,
        "too many or too deeply nested actions"
    );
    for action in actions {
        match action {
            Action::Read(f) => valid_field(f)?,
            Action::Define(d) => {
                valid_name(&d.name)?;
                for expression in [
                    &d.fields.repeat,
                    &d.fields.length,
                    &d.fields.at,
                    &d.fields.relative,
                ]
                .into_iter()
                .flatten()
                {
                    valid_expr(expression, false)?;
                }
            }
            Action::Calculate(c) => {
                valid_name(&c.name)?;
                valid_expr(&c.value, false)?;
            }
            Action::Choose(c) => {
                for branch in c.cases.values() {
                    validate_actions(branch, depth + 1)?;
                }
                validate_actions(&c.otherwise, depth + 1)?;
            }
            Action::Skip(expression) | Action::Align(expression) => {
                valid_expr(expression, false)?;
            }
        }
    }
    Ok(())
}
fn describe(
    name: &str,
    structure: &Structure,
    origin: Option<Range<Address>>,
) -> ResolvedDefinition {
    ResolvedDefinition {
        name: name.to_owned(),
        origin,
        fields: structure
            .fields
            .iter()
            .map(|field| ResolvedField {
                name: field.name.clone(),
                kind: match &field.kind {
                    TypeSpec::Name(n) => n.clone(),
                    TypeSpec::Select { .. } => "choose".into(),
                },
                repeat: field.repeat.as_ref().map(|v| format!("{v:?}")),
                length: field.length.as_ref().map(|v| format!("{v:?}")),
            })
            .collect(),
    }
}

fn partial_tree(completed: &[Node], progress: &[Node]) -> Vec<Node> {
    fn segments(path: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        for component in path.split('.') {
            let mut rest = component;
            while let Some(open) = rest.find('[') {
                if open > 0 { parts.push(&rest[..open]); }
                let Some(close) = rest[open..].find(']') else { break; };
                let close = open + close + 1;
                parts.push(&rest[open..close]);
                rest = &rest[close..];
            }
            if !rest.is_empty() { parts.push(rest); }
        }
        parts
    }
    fn insert(nodes: &mut Vec<Node>, parts: &[&str], leaf: &Node, parent: &str) {
        let Some((name, rest)) = parts.split_first() else { return; };
        let path = if parent.is_empty() || name.starts_with('[') {
            format!("{parent}{name}")
        } else {
            format!("{parent}.{name}")
        };
        let index = nodes.iter().position(|node| node.path == path).unwrap_or_else(|| {
            nodes.push(Node {
                name: (*name).to_owned(), path: path.clone(), range: leaf.range.clone(),
                kind: if rest.first().is_some_and(|part| part.starts_with('[')) {
                    "partial collection"
                } else {
                    "partial"
                }.into(), value: None, children: Vec::new(), lazy: None,
                endian: leaf.endian, references: Vec::new(), preview: None,
            });
            nodes.len() - 1
        });
        let node = &mut nodes[index];
        node.range.start = node.range.start.min(leaf.range.start);
        node.range.end = node.range.end.max(leaf.range.end);
        if rest.is_empty() {
            let children = std::mem::take(&mut node.children);
            *node = leaf.clone();
            for child in children {
                if !node.children.iter().any(|preview| preview.path == child.path) {
                    node.children.push(child);
                }
            }
        } else {
            insert(&mut node.children, rest, leaf, &path);
        }
    }

    let mut roots = Vec::new();
    for leaf in progress {
        let parts = segments(&leaf.path);
        insert(&mut roots, &parts, leaf, "");
    }
    for node in completed {
        if let Some(index) = roots.iter().position(|root| root.path == node.path) {
            roots[index] = node.clone();
        } else {
            roots.push(node.clone());
        }
    }
    roots
}

/// On a size mismatch, decode only the first item and its first nested item.
/// Keep the lazy collections for the rest; a bad tentative item must not hide
/// the partial tree or make the failed run expensive on large files.
fn preview_first_items(node: &mut Node, source: &dyn Source, cancel: &AtomicBool, depth: u8) {
    if depth == 0 { return; }
    if let Some(array) = &node.lazy {
        if array.count == 0 { return; }
        if let Ok(mut first) = array.page_from(source, cancel, 0, 1) {
            for child in &mut first {
                preview_first_items(child, source, cancel, depth - 1);
            }
            node.children = first;
        }
    } else {
        for child in &mut node.children {
            preview_first_items(child, source, cancel, depth);
        }
    }
}

struct Engine<'a> {
    source: &'a dyn Source,
    cancel: &'a AtomicBool,
    revision: u64,
    regions: Vec<Range<Address>>,
    structures: BTreeMap<String, Structure>,
    enums: Vec<EnumDef>,
    values: HashMap<String, Value>,
    collections: HashMap<String, Node>,
    definitions: Vec<ResolvedDefinition>,
    progress: Vec<Node>,
    nodes: Address,
}
impl<'a> Engine<'a> {
    fn new(
        source: &'a dyn Source,
        cancel: &'a AtomicBool,
        _endian: Endian,
        structures: BTreeMap<String, Structure>,
        enums: Vec<EnumDef>,
        revision: u64,
    ) -> Self {
        Self {
            source,
            cancel,
            revision,
            regions: vec![],
            structures,
            enums,
            values: HashMap::new(),
            collections: HashMap::new(),
            definitions: Vec::new(),
            progress: Vec::new(),
            nodes: 0,
        }
    }
    fn record_progress(&mut self, node: &Node) {
        if node.value.is_some() && self.progress.len() < MAX_PARTIAL_LEAVES {
            self.progress.push(node.clone());
        }
    }
    fn check(&mut self, depth: Address) -> Result<()> {
        ensure!(!self.cancel.load(Ordering::Relaxed), "parsing cancelled");
        ensure!(
            depth <= MAX_DEPTH,
            "structure nesting exceeds {MAX_DEPTH} levels (possibly a recursive definition)"
        );
        self.nodes += 1;
        ensure!(self.nodes <= MAX_NODES, "variable-size parsing exceeds {MAX_NODES} elements; narrow the region or use a fixed-size collection");
        Ok(())
    }
    fn lookup(&self, reference: &str, scope: &str) -> Result<&Value> {
        for path in candidates(reference, scope) {
            if let Some(value) = self.values.get(&path) {
                return Ok(value);
            }
            if let Some((collection, selector, expected, tail)) = selected_reference(&path) {
                let prefix = format!("{collection}[");
                let suffix = format!("].{selector}");
                let mut matches = self.values.iter().filter_map(|(key, value)| {
                    let index = key.strip_prefix(&prefix)?.strip_suffix(&suffix)?;
                    (index.parse::<Address>().is_ok() && value.key() == expected)
                        .then_some(index)
                });
                if let Some(index) = matches.next() {
                    ensure!(matches.next().is_none(),
                        "{scope}: reference {reference:?} matches multiple earlier {collection} items");
                    let target = format!("{collection}[{index}]{tail}");
                    if let Some(value) = self.values.get(&target) {
                        return Ok(value);
                    }
                }
            }
        }
        bail!("{scope}: reference {reference:?} is not an earlier decoded value")
    }
    fn integer(&self, expr: &Expr, scope: &str) -> Result<i128> {
        let result = match expr {
            Expr::Unsigned(n) => *n as i128,
            Expr::Signed(n) => *n as i128,
            Expr::Keyword(keyword) => {
                bail!("{scope}: {keyword:?} is only valid as a field length")
            }
            Expr::Ref { reference } => {
                if reference == "$position" {
                    bail!("use a named earlier value or 'relative' for positions");
                }
                self.lookup(reference, scope)?.integer()?
            }
            Expr::Choose { choose } => self.integer(self.selected_expr(choose, scope)?, scope)?,
            Expr::Add { add } => self.arithmetic(add, scope, "add", i128::checked_add)?,
            Expr::Subtract { subtract } => {
                self.arithmetic(subtract, scope, "subtract", i128::checked_sub)?
            }
            Expr::Multiply { multiply } => {
                self.arithmetic(multiply, scope, "multiply", i128::checked_mul)?
            }
            Expr::Divide { divide } => {
                ensure!(divide.len() == 2, "divide takes exactly two inputs");
                let left = self.integer(&divide[0], scope)?;
                let right = self.integer(&divide[1], scope)?;
                ensure!(right != 0, "division by zero");
                ensure!(
                    left.checked_rem(right) == Some(0),
                    "division must produce an exact integer"
                );
                left.checked_div(right).context("integer overflow")?
            }
        };
        Ok(result)
    }
    fn selected_expr<'b>(&self, choice: &'b ExprChoice, scope: &str) -> Result<&'b Expr> {
        let key = self.lookup(&choice.reference, scope)?.key();
        choice
            .cases
            .get(&key)
            .or(choice.otherwise.as_deref())
            .with_context(|| {
                format!(
                    "{scope}: no length mapping for {} = {key:?}",
                    choice.reference
                )
            })
    }
    fn field_length(
        &self,
        expr: &Expr,
        scope: &str,
        start: Address,
        end: Address,
    ) -> Result<Address> {
        match expr {
            Expr::Keyword(keyword) if keyword == "remaining" => Ok(end - start),
            Expr::Choose { choose } => {
                self.field_length(self.selected_expr(choose, scope)?, scope, start, end)
            }
            _ => self.size(expr, scope),
        }
    }
    fn arithmetic(
        &self,
        args: &[Expr],
        scope: &str,
        name: &str,
        op: fn(i128, i128) -> Option<i128>,
    ) -> Result<i128> {
        ensure!(args.len() == 2, "{name} takes exactly two inputs");
        op(
            self.integer(&args[0], scope)?,
            self.integer(&args[1], scope)?,
        )
        .context("integer arithmetic overflow")
    }
    fn size(&self, expr: &Expr, scope: &str) -> Result<Address> {
        Address::try_from(self.integer(expr, scope)?)
            .context("length, count or offset is negative or exceeds the platform address range")
    }
    fn selected_type(&self, kind: &TypeSpec, scope: &str) -> Result<String> {
        match kind {
            TypeSpec::Name(name) => Ok(name.clone()),
            TypeSpec::Select { choose } => self.choice(choose, scope),
        }
    }
    fn choice(&self, choice: &TypeChoice, scope: &str) -> Result<String> {
        let key = self.lookup(&choice.reference, scope)?.key();
        if choice.cases.is_empty() && choice.otherwise.is_none() && choice.enum_name.is_none() {
            return Ok(key);
        }
        let declared = choice.enum_name.as_ref()
            .map(|name| self.enum_by_name(name)).transpose()?;
        choice
            .cases
            .get(&key)
            .or_else(|| declared.and_then(|e| e.cases.get(&key)))
            .or(choice.otherwise.as_ref())
            .or_else(|| declared.and_then(|e| e.otherwise.as_ref()))
            .cloned()
            .with_context(|| {
                format!(
                    "{scope}: no type mapping for {} = {key:?}",
                    choice.reference
                )
            })
    }
    fn enum_by_name(&self, name: &str) -> Result<&EnumDef> {
        self.enums.iter().find(|declared| declared.name == name)
            .with_context(|| format!("enum {name:?} is not defined"))
    }
    fn actions(
        &mut self,
        actions: &[Action],
        scope: &str,
        cursor: &mut Address,
        end: Address,
        endian: Endian,
        nodes: &mut Vec<Node>,
        depth: Address,
    ) -> Result<()> {
        ensure!(depth <= MAX_DEPTH, "actions are nested too deeply");
        for action in actions {
            ensure!(!self.cancel.load(Ordering::Relaxed), "parsing cancelled");
            match action {
                Action::Read(field) => {
                    let path = child_path(scope, &field.name);
                    ensure!(
                        !nodes.iter().any(|n| n.name == field.name),
                        "{path}: duplicate element name"
                    );
                    let node = self
                        .field(field, &path, cursor, end, endian, depth + 1)
                        .with_context(|| format!("read {path} at byte {cursor}"))?;
                    nodes.push(node);
                }
                Action::Define(definition) => {
                    self.define(definition, scope).with_context(|| {
                        format!("define {} from {}", definition.name, definition.from)
                    })?
                }
                Action::Skip(length) => {
                    *cursor = checked_end(*cursor, self.size(length, scope)?, end)?;
                }
                Action::Align(alignment) => {
                    *cursor = align(*cursor, self.size(alignment, scope)?, end)?;
                }
                Action::Calculate(calculation) => {
                    self.check(depth)?;
                    let path = child_path(scope, &calculation.name);
                    ensure!(
                        !nodes.iter().any(|n| n.name == calculation.name),
                        "{path}: duplicate element name"
                    );
                    let number = self.integer(&calculation.value, scope)?;
                    let value = if number < 0 {
                        Value::Signed(i64::try_from(number).context("calculation exceeds i64")?)
                    } else {
                        Value::Unsigned(u64::try_from(number).context("calculation exceeds u64")?)
                    };
                    self.values.insert(path.clone(), value.clone());
                    nodes.push(Node {
                        name: calculation.name.clone(),
                        path,
                        range: *cursor..*cursor,
                        kind: "calculation".into(),
                        value: Some(value),
                        children: vec![],
                        lazy: None,
                        endian,
                        references: expr_references(&calculation.value),
                        preview: None,
                    });
                }
                Action::Choose(choice) => {
                    let key = self.lookup(&choice.reference, scope)?.key();
                    let branch = choice.cases.get(&key).unwrap_or(&choice.otherwise);
                    self.actions(branch, scope, cursor, end, endian, nodes, depth + 1)?;
                }
            }
        }
        Ok(())
    }
    fn position(
        &self,
        field: &Field,
        path: &str,
        cursor: Address,
        end: Address,
    ) -> Result<Address> {
        let mut start = cursor;
        if let Some(at) = &field.at {
            start = self.size(at, path)?;
        }
        if let Some(relative) = &field.relative {
            let position = (cursor as i128)
                .checked_add(self.integer(relative, path)?)
                .context("offset overflow")?;
            start = Address::try_from(position)
                .context("relative position is before the file or too large")?;
        }
        ensure!(
            start <= end,
            "{path}: start byte {start} exceeds region end {end}"
        );
        if let Some(region) = self.regions.last() {
            ensure!(
                start >= region.start,
                "{path}: start byte {start} precedes bounded region start {}",
                region.start
            );
        }
        if let Some(alignment) = &field.align {
            start = align(start, self.size(alignment, path)?, end)?;
        }
        Ok(start)
    }
    fn scan_field(
        &mut self,
        field: &Field,
        path: &str,
        cursor: &mut Address,
        end: Address,
        endian: Endian,
        depth: Address,
        needed: &HashSet<String>,
    ) -> Result<()> {
        if field.repeat.is_some() {
            self.field(field, path, cursor, end, endian, depth)?;
            return Ok(());
        }
        *cursor = self.position(field, path, *cursor, end)?;
        self.scan_single(field, path, cursor, end, endian, depth, needed)
    }

    /// Find a record boundary without building a tree for fields that a later
    /// offset, length, or choice does not reference.
    fn scan_single(
        &mut self,
        field: &Field,
        path: &str,
        cursor: &mut Address,
        outer_end: Address,
        inherited: Endian,
        depth: Address,
        needed: &HashSet<String>,
    ) -> Result<()> {
        self.check(depth)?;
        let start = *cursor;
        let kind = self.selected_type(&field.kind, path)?;
        let length = field
            .length
            .as_ref()
            .map(|value| self.field_length(value, path, start, outer_end))
            .transpose()?;
        let end = if let Some(length) = length {
            checked_end(start, length, outer_end)?
        } else {
            outer_end
        };
        let endian = field.endian.unwrap_or(inherited);
        let keep_value = needed.contains(&field.name) || field.expect.is_some();
        let mut value = None;
        if let Some(width) = primitive_width(&kind) {
            let value_end = checked_end(start, width, end)?;
            if keep_value {
                value = Some(decode_primitive(&kind, &self.exact(start, width)?, endian)?);
            }
            *cursor = value_end;
        } else if kind == "bytes" {
            ensure!(length.is_some(), "{path}: bytes requires a length");
            if keep_value {
                value = Some(Value::Bytes(self.exact(start, (end - start).min(64))?));
            }
            *cursor = end;
        } else if kind == "string" {
            ensure!(length.is_some(), "{path}: string requires a byte length");
            ensure!(end - start <= MAX_TEXT, "{path}: string exceeds 1 MiB; inspect it as bytes instead");
            if keep_value {
                value = Some(Value::Text(String::from_utf8(self.exact(start, end - start)?)
                    .with_context(|| format!("{path}: invalid UTF-8 text"))?));
            }
            *cursor = end;
        } else if kind == "cstring" {
            let max = (end - start).min(MAX_TEXT);
            let mut text = Vec::new();
            let mut found = false;
            while *cursor - start < max {
                let size = (max - (*cursor - start)).min(4096);
                let chunk = self.exact(*cursor, size)?;
                if let Some(index) = chunk.iter().position(|byte| *byte == 0) {
                    if keep_value { text.extend_from_slice(&chunk[..index]); }
                    *cursor += index as Address + 1;
                    found = true;
                    break;
                }
                if keep_value { text.extend_from_slice(&chunk); }
                *cursor += size;
            }
            ensure!(found, "{path}: missing zero terminator within the bounded region (maximum 1 MiB)");
            if keep_value {
                value = Some(Value::Text(String::from_utf8(text).context("invalid UTF-8 text")?));
            }
        } else if kind == "varuint" {
            let mut number = 0u64;
            let mut terminated = false;
            for i in 0..10 {
                checked_end(*cursor, 1, end)?;
                let byte = self.exact(*cursor, 1)?[0];
                *cursor += 1;
                ensure!(i != 9 || byte <= 1, "{path}: unsigned LEB128 exceeds u64");
                number |= u64::from(byte & 0x7f) << (7 * i);
                if byte & 0x80 == 0 { terminated = true; break; }
            }
            ensure!(terminated, "{path}: unterminated unsigned LEB128");
            if keep_value { value = Some(Value::Unsigned(number)); }
        } else {
            let structure = self.structures.get(&kind)
                .with_context(|| format!("{path}: unknown structure or primitive type {kind:?}"))?
                .clone();
            ensure!(structure.actions.is_empty(), "{path}: indexed records cannot define structures");
            let child_endian = field.endian.or(structure.endian).unwrap_or(inherited);
            if length.is_some() { self.regions.push(start..end); }
            for child in &structure.fields {
                let child_path = child_path(path, &child.name);
                self.scan_field(child, &child_path, cursor, end, child_endian, depth + 1, needed)
                    .with_context(|| format!("read {child_path} at byte {cursor}"))?;
            }
            if length.is_some() { self.regions.pop(); }
        }
        if let Some(expect) = &field.expect {
            let actual = value.as_ref().context("expect requires a scalar value")?;
            ensure!(matches_expected(actual, expect), "{path}: expected {expect:?}, found {actual}");
        }
        if length.is_some() && *cursor < end { *cursor = end; }
        if let Some(value) = value { self.values.insert(path.into(), value); }
        Ok(())
    }

    fn index_variable(
        &self,
        element: &Field,
        path: &str,
        start: Address,
        end: Address,
        count: Option<Address>,
        endian: Endian,
        depth: Address,
    ) -> Result<Option<Vec<Address>>> {
        // Definitions inside a record can change how later records decode.
        // Keep those collections on the ordinary execution path.
        let Ok(kind) = self.selected_type(&element.kind, path) else { return Ok(None); };
        if self.has_actions(&kind, 0) { return Ok(None); }
        let source = ScanSource::new(self.source);
        let mut scanner = Engine::new(&source, self.cancel, endian, self.structures.clone(), self.enums.clone(), self.revision);
        scanner.regions = self.regions.clone();
        let needed: HashSet<String> = self.structures.values()
            .flat_map(|structure| structure.fields.iter())
            .flat_map(field_references)
            .map(|reference| reference.rsplit(|c| c == '.' || c == '/').next().unwrap_or("").to_owned())
            .collect();
        let mut offsets = vec![start];
        let mut cursor = start;
        while count.map_or(cursor < end, |count| ((offsets.len() - 1) as Address) < count) {
            ensure!(!self.cancel.load(Ordering::Relaxed), "parsing cancelled");
            let index = offsets.len() - 1;
            let before = cursor;
            scanner.values = self.values.clone();
            scanner.collections.clear();
            scanner.nodes = 0;
            let item_path = format!("{path}[{index}]");
            if scanner.scan_single(element, &item_path, &mut cursor, end, endian, depth + 1, &needed).is_err() {
                // An index is all-or-nothing. Re-run the ordinary reader on
                // failure so completed records and fields remain inspectable.
                return Ok(None);
            }
            ensure!(cursor > before, "{path}: repeated item consumed no bytes");
            offsets.push(cursor);
        }
        Ok(Some(offsets))
    }
    fn field(
        &mut self,
        field: &Field,
        path: &str,
        cursor: &mut Address,
        end: Address,
        endian: Endian,
        depth: Address,
    ) -> Result<Node> {
        valid_field(field)?;
        *cursor = self.position(field, path, *cursor, end)?;
        let start = *cursor;
        let mut element = field.clone();
        element.repeat = None;
        element.at = None;
        element.relative = None;
        element.align = None;
        let Some(repeat) = &field.repeat else {
            return self.single(&element, path, cursor, end, endian, depth);
        };
        self.check(depth)?;
        let stride = self.fixed_size(&element, path, 0);
        let count = match repeat {
            Repeat::Count(expr) => Some(self.size(expr, path)?),
            Repeat::Until(_) => {
                if let Some(width) = stride.filter(|w| *w > 0) {
                    let region = end - start;
                    let complete = region / width;
                    let remainder = region % width;
                    if remainder != 0 {
                        if complete > 0 {
                            let complete_end = start + complete * width;
                            let mut node = self.fixed_array_node(
                                field, &element, path, start, complete_end, complete, width, endian,
                            )?;
                            preview_first_items(&mut node, self.source, self.cancel, 2);
                            if self.progress.len() == MAX_PARTIAL_LEAVES {
                                self.progress.pop();
                            }
                            self.progress.push(node);
                            *cursor = complete_end;
                        }
                        let per_item = (complete > 0 && remainder % complete == 0)
                            .then(|| {
                                let extra = remainder / complete;
                                format!(" The excess divides evenly into {extra} {} per complete item; check for an unmodeled per-item header or trailer.", if extra == 1 { "byte" } else { "bytes" })
                            })
                            .unwrap_or_default();
                        anyhow::bail!(
                            "{path}: fixed-size repeat at byte {start} has {region} bytes available; {complete} complete {width}-byte items fit; {remainder} bytes remain. Their ranges are available as a tentative prefix in Decoded structure; expand an item to decode its fields.{per_item}"
                        );
                    }
                    Some((end - start) / width)
                } else {
                    None
                }
            }
        };
        if let (Some(count), Some(stride)) = (count, stride.filter(|w| *w > 0)) {
            let length = count
                .checked_mul(stride)
                .context("collection byte length overflow")?;
            let array_end = checked_end(start, length, end)?;
            if count > LAZY_THRESHOLD {
                *cursor = array_end;
                let node = self.fixed_array_node(field, &element, path, start, array_end, count, stride, endian)?;
                self.collections.insert(path.into(), node.clone());
                return Ok(node);
            }
        }
        if stride.is_none() && count.is_none_or(|count| count > LAZY_THRESHOLD) {
            if let Some(offsets) = self.index_variable(
                &element, path, start, end, count, field.endian.unwrap_or(endian), depth,
            )? {
                let indexed_count = (offsets.len() - 1) as Address;
                if indexed_count > LAZY_THRESHOLD {
                    let array_end = *offsets.last().unwrap();
                    *cursor = array_end;
                    let node = Node {
                        name: field.name.clone(),
                        path: path.into(),
                        range: start..array_end,
                        kind: format!("{}[]", self.selected_type(&field.kind, path)?),
                        value: None,
                        children: vec![],
                        lazy: Some(LazyArray {
                            count: indexed_count,
                            stride: 0,
                            offsets: Some(Arc::new(offsets)),
                            segmented: None,
                            plan: Arc::new(LazyPlan {
                                field: element,
                                start,
                                end: array_end,
                                path: path.into(),
                                endian: field.endian.unwrap_or(endian),
                                values: Arc::new(self.values.clone()),
                                structures: Arc::new(self.structures.clone()),
                                enums: Arc::new(self.enums.clone()),
                                revision: self.revision,
                            }),
                        }),
                        endian: field.endian.unwrap_or(endian),
                        references: field_references(field),
                        preview: None,
                    };
                    self.collections.insert(path.into(), node.clone());
                    return Ok(node);
                }
            }
        }
        if let Some(count) = count {
            ensure!(
                count <= MAX_NODES,
                "{path}: variable-size collection exceeds {MAX_NODES} items"
            );
        }
        let mut children = Vec::new();
        while count
            .map(|count| (children.len() as Address) < count)
            .unwrap_or(*cursor < end)
        {
            let before = *cursor;
            let index = children.len();
            let child_path = format!("{path}[{index}]");
            let mut child = self
                .single(
                    &element,
                    &child_path,
                    cursor,
                    end,
                    field.endian.unwrap_or(endian),
                    depth + 1,
                )
                .with_context(|| format!("item {index} at byte {before}"))?;
            ensure!(*cursor > before, "{path}: repeated item consumed no bytes");
            child.name = format!("[{index}]");
            if self.progress.len() < MAX_PARTIAL_LEAVES {
                self.progress.push(child.clone());
            }
            children.push(child);
        }
        let node = Node {
            name: field.name.clone(),
            path: path.into(),
            range: start..*cursor,
            kind: format!("{}[]", self.selected_type(&field.kind, path)?),
            value: None,
            children,
            lazy: None,
            endian: field.endian.unwrap_or(endian),
            references: field_references(field),
            preview: None,
        };
        self.collections.insert(path.into(), node.clone());
        Ok(node)
    }

    fn fixed_array_node(
        &self, field: &Field, element: &Field, path: &str, start: Address,
        end: Address, count: Address, stride: Address, endian: Endian,
    ) -> Result<Node> {
        Ok(Node {
            name: field.name.clone(),
            path: path.into(),
            range: start..end,
            kind: format!("{}[]", self.selected_type(&field.kind, path)?),
            value: None,
            children: vec![],
            lazy: Some(LazyArray {
                count,
                stride,
                offsets: None,
                segmented: None,
                plan: Arc::new(LazyPlan {
                    field: element.clone(), start, end, path: path.into(),
                    endian: field.endian.unwrap_or(endian),
                    values: Arc::new(self.values.clone()),
                    structures: Arc::new(self.structures.clone()),
                    enums: Arc::new(self.enums.clone()),
                    revision: self.revision,
                }),
            }),
            endian: field.endian.unwrap_or(endian),
            references: field_references(field),
            preview: None,
        })
    }
    fn single(
        &mut self,
        field: &Field,
        path: &str,
        cursor: &mut Address,
        outer_end: Address,
        inherited: Endian,
        depth: Address,
    ) -> Result<Node> {
        self.check(depth)?;
        let start = *cursor;
        let kind = self.selected_type(&field.kind, path)?;
        let length = field
            .length
            .as_ref()
            .map(|value| self.field_length(value, path, start, outer_end))
            .transpose()?;
        let end = if let Some(length) = length {
            checked_end(start, length, outer_end)?
        } else {
            outer_end
        };
        let mut endian = field.endian.unwrap_or(inherited);
        let mut children = Vec::new();
        let mut preview = None;
        let value;
        if let Some(width) = primitive_width(&kind) {
            let value_end = checked_end(start, width, end)?;
            let bytes = self.exact(start, width)?;
            value = Some(decode_primitive(&kind, &bytes, endian)?);
            *cursor = value_end;
        } else if kind == "bytes" {
            ensure!(length.is_some(), "{path}: bytes requires a length");
            value = Some(Value::Bytes(self.exact(start, (end - start).min(64))?));
            *cursor = end;
        } else if kind == "string" {
            ensure!(length.is_some(), "{path}: string requires a byte length");
            ensure!(
                end - start <= MAX_TEXT,
                "{path}: string exceeds 1 MiB; inspect it as bytes instead"
            );
            let bytes = self.exact(start, end - start)?;
            value = Some(Value::Text(
                String::from_utf8(bytes).with_context(|| format!("{path}: invalid UTF-8 text"))?,
            ));
            *cursor = end;
        } else if kind == "cstring" {
            let max = (end - start).min(MAX_TEXT);
            let mut bytes = Vec::new();
            let mut found = false;
            while (bytes.len() as Address) < max {
                ensure!(!self.cancel.load(Ordering::Relaxed), "parsing cancelled");
                let chunk = self.exact(
                    start + bytes.len() as Address,
                    (max - bytes.len() as Address).min(4096),
                )?;
                if let Some(index) = chunk.iter().position(|b| *b == 0) {
                    bytes.extend_from_slice(&chunk[..index]);
                    *cursor = start + bytes.len() as Address + 1;
                    found = true;
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            ensure!(
                found,
                "{path}: missing zero terminator within the bounded region (maximum 1 MiB)"
            );
            value = Some(Value::Text(
                String::from_utf8(bytes).context("invalid UTF-8 text")?,
            ));
        } else if kind == "varuint" {
            let mut number = 0u64;
            let mut terminated = false;
            for i in 0..10 {
                checked_end(*cursor, 1, end)?;
                let byte = self.exact(*cursor, 1)?[0];
                *cursor += 1;
                ensure!(i != 9 || byte <= 1, "{path}: unsigned LEB128 exceeds u64");
                number |= u64::from(byte & 0x7f) << (7 * i);
                if byte & 0x80 == 0 {
                    terminated = true;
                    break;
                }
            }
            ensure!(terminated, "{path}: unterminated unsigned LEB128");
            value = Some(Value::Unsigned(number));
        } else {
            let structure = self
                .structures
                .get(&kind)
                .with_context(|| format!("{path}: unknown structure or primitive type {kind:?}"))?
                .clone();
            endian = field.endian.or(structure.endian).unwrap_or(inherited);
            if length.is_some() {
                self.regions.push(start..end);
            }
            for child in &structure.fields {
                let child_path = child_path(path, &child.name);
                let node = self
                    .field(child, &child_path, cursor, end, endian, depth + 1)
                    .with_context(|| format!("read {child_path} at byte {cursor}"))?;
                children.push(node);
            }
            self.actions(
                &structure.actions,
                path,
                cursor,
                end,
                endian,
                &mut children,
                depth + 1,
            )?;
            preview = structure.fields.iter().find(|field| field.preview)
                .and_then(|field| children.iter().find(|child| child.name == field.name))
                .map(|child| {
                    let content = child.value.as_ref().map_or_else(|| child.kind.clone(), ToString::to_string);
                    let short: String = content.chars().take(80).collect();
                    format!("{}={short}", child.name)
                });
            if length.is_some() {
                self.regions.pop();
            }
            value = None;
        }
        if let Some(expect) = &field.expect {
            let actual = value.as_ref().context("expect requires a scalar value")?;
            ensure!(
                matches_expected(actual, expect),
                "{path}: expected {expect:?}, found {actual}"
            );
        }
        if length.is_some() && *cursor < end {
            // Region length owns the remaining padding as well as the decoded prefix.
            children.push(Node {
                name: "remaining_bytes".into(),
                path: child_path(path, "remaining_bytes"),
                range: *cursor..end,
                kind: "bytes".into(),
                value: None,
                children: vec![],
                lazy: None,
                endian,
                references: vec![],
                preview: None,
            });
            *cursor = end;
        }
        let range_start = children
            .iter()
            .map(|n| n.range.start)
            .min()
            .unwrap_or(start)
            .min(start);
        let range_end = children
            .iter()
            .map(|n| n.range.end)
            .max()
            .unwrap_or(*cursor)
            .max(*cursor);
        let node = Node {
            name: field.name.clone(),
            path: path.into(),
            range: range_start..range_end,
            kind,
            value: value.clone(),
            children,
            lazy: None,
            endian,
            references: field_references(field),
            preview,
        };
        if let Some(value) = value {
            self.values.insert(path.into(), value);
        }
        if !node.children.is_empty() {
            self.collections.insert(path.into(), node.clone());
        }
        self.record_progress(&node);
        Ok(node)
    }
    fn exact(&self, offset: Address, length: Address) -> Result<Vec<u8>> {
        ensure!(!self.cancel.load(Ordering::Relaxed), "parsing cancelled");
        let bytes = self.source.read(offset, length)?;
        ensure!(
            bytes.len() as Address == length,
            "truncated source at byte {offset}: expected {length} bytes, got {}",
            bytes.len()
        );
        Ok(bytes)
    }
    fn fixed_size(&self, field: &Field, scope: &str, depth: Address) -> Option<Address> {
        if depth > MAX_DEPTH
            || field.at.is_some()
            || field.relative.is_some()
            || field.align.is_some()
            || field.expect.is_some()
        {
            return None;
        }
        // A declared region length fixes its extent, but a structure with actions must
        // still execute those actions (they may publish definitions for later reads).
        let kind = self.selected_type(&field.kind, scope).ok()?;
        if primitive_width(&kind).is_none()
            && !matches!(kind.as_str(), "string" | "bytes" | "cstring" | "varuint")
            && !self.structures.contains_key(&kind)
        {
            return None;
        }
        if self.has_actions(&kind, 0) {
            return None;
        }
        let base = if let Some(length) = &field.length {
            // `remaining` depends on the current enclosing boundary, so it can
            // never establish a reusable fixed record stride.
            if contains_remaining(length) {
                return None;
            }
            self.size(length, scope).ok()?
        } else if let Some(width) = primitive_width(&kind) {
            width
        } else {
            let structure = self.structures.get(&kind)?;
            let names: Vec<&str> = structure
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect();
            // A value inside this item can shadow an earlier value with the
            // same name. Such a stride must be discovered from each item.
            if structure
                .fields
                .iter()
                .flat_map(field_references)
                .any(|reference| {
                    !reference.starts_with('/')
                        && (!reference.contains('.')
                            || names.contains(&reference.split('.').next().unwrap_or("")))
                })
            {
                return None;
            }
            let mut total = 0 as Address;
            for child in &structure.fields {
                total = total.checked_add(self.fixed_size(
                    child,
                    &child_path(scope, &child.name),
                    depth + 1,
                )?)?;
            }
            total
        };
        match &field.repeat {
            None => Some(base),
            Some(Repeat::Count(count)) => base.checked_mul(self.size(count, scope).ok()?),
            Some(Repeat::Until(_)) => None,
        }
    }
    fn has_actions(&self, kind: &str, depth: Address) -> bool {
        if depth > MAX_DEPTH {
            return true;
        }
        let Some(structure) = self.structures.get(kind) else {
            return false;
        };
        if !structure.actions.is_empty() {
            return true;
        }
        structure.fields.iter().any(|field| match &field.kind {
            TypeSpec::Name(name) => self.has_actions(name, depth + 1),
            TypeSpec::Select { choose } => choose
                .cases
                .values()
                .chain(choose.otherwise.iter())
                .any(|name| self.has_actions(name, depth + 1)),
        })
    }
    fn define(&mut self, definition: &Define, scope: &str) -> Result<()> {
        valid_name(&definition.name)?;
        ensure!(
            primitive_width(&definition.name).is_none()
                && !matches!(
                    definition.name.as_str(),
                    "bytes" | "string" | "cstring" | "varuint"
                ),
            "structure name {:?} is reserved",
            definition.name
        );
        ensure!(
            !self.structures.contains_key(&definition.name),
            "structure {:?} is already defined; choose a unique name",
            definition.name
        );
        ensure!(
            self.structures.len() < 512,
            "at most 512 static and discovered structures are allowed"
        );
        let collection = candidates(&definition.from, scope)
            .iter()
            .find_map(|path| self.collections.get(path))
            .cloned()
            .with_context(|| {
                format!(
                    "{} is not an earlier decoded collection or structure",
                    definition.from
                )
            })?;
        let children = if let Some(lazy) = &collection.lazy {
            ensure!(
                lazy.count <= 4096,
                "at most 4096 discovered fields are allowed"
            );
            let mut elements = Vec::new();
            for start in (0..lazy.count).step_by(MAX_PAGE as usize) {
                elements.extend(lazy.page_from(
                    self.source,
                    self.cancel,
                    start,
                    MAX_PAGE.min(lazy.count - start),
                )?);
            }
            for element in &elements {
                register_values(element, &mut self.values);
            }
            elements
        } else {
            collection.children.clone()
        };
        ensure!(!children.is_empty(), "definition input is empty");
        ensure!(
            children.len() <= 4096,
            "at most 4096 discovered fields are allowed"
        );
        let mut structure = Structure::default();
        let mut names = std::collections::HashSet::new();
        let mut group_item_types = HashMap::<(String, Address, Option<Address>), String>::new();
        for (index, element) in children.iter().enumerate() {
            let name = if let Some(reference) = &definition.fields.name {
                let key = self.lookup(&reference.reference, &element.path)?.key();
                let mapped = reference.enum_name.as_ref()
                    .map(|name| self.enum_by_name(name))
                    .transpose()?
                    .and_then(|declared| declared.cases.get(&key).or(declared.otherwise.as_ref()))
                    .cloned()
                    .unwrap_or(key);
                mapped.trim_end_matches('\0').to_string()
            } else {
                format!("field_{index}")
            };
            valid_name(&name)?;
            ensure!(
                names.insert(name.clone()),
                "duplicate discovered field name {name:?}"
            );
            let kind = self.choice(&definition.fields.kind, &element.path)?;
            ensure!(
                primitive_width(&kind).is_some()
                    || matches!(kind.as_str(), "bytes" | "string" | "cstring" | "varuint")
                    || self.structures.contains_key(&kind),
                "discovered type {kind:?} is not a defined structure or primitive"
            );
            let mut field = Field::scalar(name, kind);
            let resolve = |expr: &Expr| -> Result<Expr> {
                Ok(Expr::Unsigned(
                    u64::try_from(self.size(expr, &element.path)?).context("value exceeds u64")?,
                ))
            };
            field.repeat = definition
                .fields
                .repeat
                .as_ref()
                .map(|v| resolve(v).map(Repeat::Count))
                .transpose()?;
            field.length = definition.fields.length.as_ref().map(resolve).transpose()?;
            field.at = definition.fields.at.as_ref().map(resolve).transpose()?;
            field.relative = definition
                .fields
                .relative
                .as_ref()
                .map(|v| {
                    Ok::<_, anyhow::Error>(Expr::Signed(
                        i64::try_from(self.integer(v, &element.path)?)
                            .context("relative offset exceeds i64")?,
                    ))
                })
                .transpose()?;
            if let Some(items_per_group) = &definition.fields.items_per_group {
                ensure!(field.repeat.is_some(), "items_per_group requires repeat for the group count");
                let count = resolve(items_per_group)?;
                ensure!(constant_size(&count).is_some_and(|count| count > 0), "items_per_group must be positive");
                let value_kind = match &field.kind {
                    TypeSpec::Name(kind) => kind.clone(),
                    TypeSpec::Select { .. } => unreachable!(),
                };
                let key = (value_kind.clone(), constant_size(&count).unwrap(), field.length.as_ref().and_then(constant_size));
                let item_kind = if let Some(existing) = group_item_types.get(&key) {
                    existing.clone()
                } else {
                    let item_kind = format!("{}__item_{index}", definition.name);
                    ensure!(!self.structures.contains_key(&item_kind), "structure {item_kind:?} is already defined");
                    ensure!(self.structures.len() < 512, "at most 512 static and discovered structures are allowed");
                    let mut item_field = Field::scalar("values", value_kind);
                    item_field.repeat = Some(Repeat::Count(count));
                    item_field.length = field.length.clone();
                    valid_field(&item_field)?;
                    self.structures.insert(item_kind.clone(), Structure {
                        fields: vec![item_field], ..Structure::default()
                    });
                    group_item_types.insert(key, item_kind.clone());
                    item_kind
                };
                field.length = None;
                field.kind = TypeSpec::Name(item_kind);
            }
            valid_field(&field)?;
            structure.fields.push(field);
        }
        self.definitions.push(describe(
            &definition.name,
            &structure,
            Some(collection.range),
        ));
        self.structures.insert(definition.name.clone(), structure);
        Ok(())
    }
}

fn candidates(reference: &str, scope: &str) -> Vec<String> {
    if let Some(absolute) = reference.strip_prefix('/') {
        return vec![absolute.into()];
    }
    let mut result = vec![];
    let mut parent = scope;
    loop {
        result.push(child_path(parent, reference));
        if let Some(index) = parent.rfind('.') {
            parent = &parent[..index];
        } else {
            break;
        }
    }
    result.push(reference.into());
    result
}
/// Resolve a collection item by a scalar field already decoded in that item.
/// The selector is deliberately limited to one equality test so references
/// remain readable and cannot become an expression language.
fn selected_reference(path: &str) -> Option<(&str, &str, &str, &str)> {
    let (collection, rest) = path.split_once('[')?;
    let (selector, tail) = rest.split_once(']')?;
    let (field, expected) = selector.split_once('=')?;
    let field = field.trim();
    let expected = expected.trim();
    (!collection.is_empty() && !field.is_empty() && !expected.is_empty() && tail.starts_with('.'))
        .then_some((collection, field, expected, tail))
}
fn child_path(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.into()
    } else {
        format!("{scope}.{name}")
    }
}
fn register_values(node: &Node, values: &mut HashMap<String, Value>) {
    if let Some(value) = &node.value {
        values.insert(node.path.clone(), value.clone());
    }
    for child in &node.children {
        register_values(child, values);
    }
}
fn checked_end(start: Address, length: Address, end: Address) -> Result<Address> {
    let actual = start.checked_add(length).context("byte range overflow")?;
    ensure!(
        actual <= end,
        "byte range {start}..{actual} exceeds enclosing region end {end}"
    );
    Ok(actual)
}
fn align(cursor: Address, alignment: Address, end: Address) -> Result<Address> {
    ensure!(
        alignment > 0 && alignment <= 1024 * 1024,
        "alignment must be from 1 to 1048576 bytes"
    );
    let padding = (alignment - cursor % alignment) % alignment;
    checked_end(cursor, padding, end)
}
/// The editor uses the same built-in type vocabulary as the interpreter.
pub(crate) fn is_builtin_type(kind: &str) -> bool {
    primitive_width(kind).is_some() || matches!(kind, "bytes" | "string" | "cstring" | "varuint")
}

fn primitive_width(kind: &str) -> Option<Address> {
    match kind {
        "u8" | "i8" => Some(1),
        "u16" | "i16" => Some(2),
        "u32" | "i32" | "f32" => Some(4),
        "u64" | "i64" | "f64" => Some(8),
        _ => None,
    }
}
fn decode_primitive(kind: &str, bytes: &[u8], endian: Endian) -> Result<Value> {
    let mut buffer = [0u8; 8];
    match endian {
        Endian::Little => buffer[..bytes.len()].copy_from_slice(bytes),
        Endian::Big => buffer[8 - bytes.len()..].copy_from_slice(bytes),
    }
    let raw = match endian {
        Endian::Little => u64::from_le_bytes(buffer),
        Endian::Big => u64::from_be_bytes(buffer),
    };
    Ok(match kind {
        "u8" | "u16" | "u32" | "u64" => Value::Unsigned(raw),
        "i8" => Value::Signed(raw as i8 as i64),
        "i16" => Value::Signed(raw as i16 as i64),
        "i32" => Value::Signed(raw as i32 as i64),
        "i64" => Value::Signed(raw as i64),
        "f32" => Value::Float(f32::from_bits(raw as u32) as f64),
        "f64" => Value::Float(f64::from_bits(raw)),
        _ => bail!("unknown primitive {kind}"),
    })
}
fn matches_expected(actual: &Value, expected: &Literal) -> bool {
    match expected {
        Literal::Unsigned(v) => actual.integer().ok() == Some(*v as i128),
        Literal::Signed(v) => actual.integer().ok() == Some(*v as i128),
        Literal::Float(v) => actual.numeric() == Some(*v),
        Literal::Text(v) => matches!(actual, Value::Text(a) if a == v),
    }
}
fn expr_references(expr: &Expr) -> Vec<String> {
    match expr {
        Expr::Ref { reference } => vec![reference.clone()],
        Expr::Choose { choose } => {
            let mut references = vec![choose.reference.clone()];
            references.extend(choose.cases.values().flat_map(expr_references));
            if let Some(otherwise) = &choose.otherwise {
                references.extend(expr_references(otherwise));
            }
            references
        }
        Expr::Add { add }
        | Expr::Subtract { subtract: add }
        | Expr::Multiply { multiply: add }
        | Expr::Divide { divide: add } => add.iter().flat_map(expr_references).collect(),
        _ => vec![],
    }
}
fn contains_remaining(expr: &Expr) -> bool {
    match expr {
        Expr::Keyword(keyword) => keyword == "remaining",
        Expr::Choose { choose } => {
            choose.cases.values().any(contains_remaining)
                || choose.otherwise.as_deref().is_some_and(contains_remaining)
        }
        Expr::Add { add }
        | Expr::Subtract { subtract: add }
        | Expr::Multiply { multiply: add }
        | Expr::Divide { divide: add } => add.iter().any(contains_remaining),
        _ => false,
    }
}
fn field_references(field: &Field) -> Vec<String> {
    let mut refs = Vec::new();
    for expr in [&field.length, &field.at, &field.relative, &field.align]
        .into_iter()
        .flatten()
    {
        refs.extend(expr_references(expr));
    }
    if let Some(Repeat::Count(expr)) = &field.repeat {
        refs.extend(expr_references(expr));
    }
    if let TypeSpec::Select { choose } = &field.kind {
        refs.push(choose.reference.clone());
    }
    refs.sort();
    refs.dedup();
    refs
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn one_item_segments_are_one_strided_read_run() {
        let plan = NumericPlan {
            start: 6,
            stride: 2,
            segmented: Some(SegmentedArray {
                outer_start: 0,
                outer_stride: 10,
                inner_offset: 6,
                inner_count: 1,
                inner_stride: 2,
            }),
            path: "rows.values".into(),
            revision: 1,
            fields: Vec::new(),
        };
        assert_eq!(plan.physical_stride(), 10);
        assert_eq!(plan.contiguous_count(1), Address::MAX);
        assert_eq!(plan.offset(4097), Some(4097 * 10 + 6));
    }

    #[test]
    fn reference_selects_an_earlier_section_by_decoded_field_not_position() {
        let yaml = r#"
structures:
  - name: Config
    fields:
      - {name: count, type: u8}
  - name: Data
    fields:
      - name: values
        type: u8
        repeat: {ref: "/sections[kind=1].payload.count"}
  - name: Section
    fields:
      - {name: kind, type: u8}
      - name: payload
        type:
          choose:
            ref: kind
            cases: {'1': {structure: Config}, '2': {structure: Data}}
            otherwise: bytes
        length:
          choose:
            ref: kind
            cases: {'1': 1, '2': 3}
            otherwise: 1
actions:
  - read: {name: sections, type: {structure: Section}, repeat: 3}
"#;
        for bytes in [
            vec![0, 99, 1, 3, 2, 10, 11, 12],
            vec![1, 3, 0, 99, 2, 10, 11, 12],
        ] {
            let output = run(yaml, bytes).unwrap();
            let values = &output.nodes[0].children[2].children[1].children[0];
            assert_eq!(values.children.len(), 3);
            assert_eq!(values.children.iter().map(|node| node.numeric().unwrap() as u8).collect::<Vec<_>>(), [10, 11, 12]);
        }
    }
    struct Bytes(Vec<u8>);
    impl Source for Bytes {
        fn len(&self) -> Address {
            self.0.len()
        }
        fn read(&self, offset: Address, length: Address) -> Result<Vec<u8>> {
            Ok(self
                .0
                .get(offset..offset + length)
                .context("out of bounds")?
                .to_vec())
        }
    }
    fn run(yaml: &str, bytes: Vec<u8>) -> Result<ParseOutput> {
        Program::parse(yaml)?.run_from(&Bytes(bytes), &AtomicBool::new(false), 1)
    }
    #[test]
    fn lazy_page_with_many_fields_uses_record_limit_per_item() {
        let mut yaml = String::from("structures:\n  - name: Record\n    fields:\n");
        for index in 0..200 {
            yaml.push_str(&format!("      - {{name: value_{index}, type: u8}}\n"));
        }
        yaml.push_str("actions:\n  - read: {name: records, type: {structure: Record}, repeat: 256}\n");
        let source = Bytes(vec![7; 256 * 200]);
        let output = Program::parse(&yaml).unwrap().run_from(&source, &AtomicBool::new(false), 1).unwrap();
        let array = output.nodes[0].lazy.as_ref().unwrap();
        let rows = array.page_from(&source, &AtomicBool::new(false), 0, 128).unwrap();
        assert_eq!(rows.len(), 128);
        assert_eq!(rows[127].children.len(), 200);
        assert_eq!(rows[127].children[199].numeric(), Some(7.));
    }
    const DYNAMIC: &str = r#"
endian: little
structures:
  - name: Entry
    fields:
      - {name: label, type: string, length: 4}
      - {name: code, type: u8}
      - {name: count, type: u8}
  - name: Catalog
    fields:
      - {name: count, type: u8}
      - {name: entries, type: {structure: Entry}, repeat: {ref: count}}
actions:
  - read: {name: catalog, type: {structure: Catalog}}
  - define:
      name: Discovered
      from: catalog.entries
      fields:
        name: {ref: label}
        type: {ref: code, cases: {'1': u16, '2': i16}}
        repeat: {ref: count}
  - read: {name: payload, type: {structure: Discovered}}
"#;
    #[test]
    fn reusable_enums_name_discovered_fields_and_choose_types() {
        let yaml = r#"
enums:
  - name: Labels
    cases: {'7': First, '9': Second}
  - name: Widths
    cases: {'1': u8, '2': u16}
structures:
  - name: Entry
    fields:
      - {name: id, type: u8}
      - {name: width, type: u8}
actions:
  - read: {name: entries, type: {structure: Entry}, repeat: 2}
  - define:
      name: Discovered
      from: entries
      fields:
        name: {ref: id, enum: Labels}
        type: {ref: width, enum: Widths}
  - read: {name: payload, type: {structure: Discovered}}
  - read: {name: width, type: u8}
  - read: {name: selected, type: {choose: {ref: width, enum: Widths}}}
"#;
        let out = run(yaml, vec![7, 1, 9, 2, 3, 4, 5, 1, 6]).unwrap();
        assert_eq!(out.nodes[1].children[0].name, "First");
        assert_eq!(out.nodes[1].children[1].name, "Second");
        assert_eq!(out.nodes[1].children[1].numeric(), Some(0x0504 as f64));
        assert_eq!(out.nodes[3].numeric(), Some(6.));
        let saved = serde_yaml_ng::to_string(&Program::parse(yaml).unwrap()).unwrap();
        assert!(saved.contains("enum: Labels"));
        assert!(saved.contains("enum: Widths"));
    }
    #[test]
    fn explicit_names_references_and_nested_byte_paths_round_trip() {
        let yaml = include_str!("../samples/decoders/yaml/nested_records.yaml");
        let program = Program::parse(yaml).unwrap();
        let saved = serde_yaml_ng::to_string(&program).unwrap();
        assert!(saved.contains("- name: Position"));
        assert!(saved.contains("structure: Position"));
        let output = run(&saved, vec![7,10,0,20,0,8,30,0,40,0]).unwrap();
        assert_eq!(output.consumed, 10);
        let records = &output.nodes[0].children;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].children[1].children[0].path, "records[0].position.x");
        assert_eq!(records[0].children[1].children[0].numeric(), Some(10.));
        assert_eq!(records[1].children[1].children[1].numeric(), Some(40.));
        assert_eq!(records[1].children[1].children[1].range, 8..10);
    }

    #[test]
    fn old_and_ambiguous_type_forms_are_rejected() {
        for yaml in [
            "structures: {Record: {fields: []}}\nactions: [{skip: 0}]",
            "structures: [{fields: []}]\nactions: [{skip: 0}]",
            "structures: [{name: Record}, {name: Record}]\nactions: [{skip: 0}]",
            "structures: [{name: u32}]\nactions: [{skip: 0}]",
            "actions: [{read: {name: x, type: Record}}]",
            "actions: [{read: {name: x, type: {structure: u32}}}]",
            "actions: [{read: {name: x, type: {choose: {ref: tag, cases: {'1': Record}}}}}]",
            "actions: [{read: {name: x, type: {choose: {ref: tag, otherwise: Record}}}}]",
            "actions: [{read: {name: label, type: text, length: 4}}]",
        ] { assert!(Program::parse(yaml).is_err(), "unexpectedly accepted {yaml}"); }
    }

    #[test]
    fn tree_shows_array_indices_structure_discriminators_and_integer_formats() {
        let yaml = "structures:\n  - name: Section\n    fields:\n      - {name: section_type, type: u32, preview: true}\n      - {name: signed_value, type: i16}\nactions:\n  - read: {name: sections, type: {structure: Section}, repeat: 1}\n";
        let output = run(yaml, vec![4, 0, 0, 0, 0xFE, 0xFF]).unwrap();
        let section = &output.nodes[0].children[0];
        assert_eq!(section.tree_label(), "sections[0]");
        assert!(section.tree_summary().contains("Section · section_type=4"));
        assert_eq!(section.children[0].tree_summary(), "u32 · 4");
        assert_eq!(section.children[0].numeric_tooltip().as_deref(), Some("u32 · 32 bits\nDecimal: 4\nHex: 0x00000004\nBinary: 0b00000000000000000000000000000100"));
        assert_eq!(section.children[1].tree_summary(), "i16 · -2");
        assert_eq!(section.children[1].numeric_tooltip().as_deref(), Some("i16 · 16 bits\nDecimal: -2\nHex: 0xFFFE\nBinary: 0b1111111111111110"));
    }
    #[test]
    fn collapsed_record_preview_uses_only_the_marked_field() {
        let yaml = "structures:\n  - name: Data\n    fields:\n      - {name: value, type: u8}\n  - name: Record\n    fields:\n      - {name: kind, type: u8}\n      - {name: payload, type: {choose: {ref: kind, cases: {'1': {structure: Data}}}}, preview: true}\nactions:\n  - read: {name: records, type: {structure: Record}, repeat: 2}\n";
        let out = run(yaml, vec![1, 7, 1, 8]).unwrap();
        let rows = &out.nodes[0].children;
        assert!(rows[0].tree_summary().contains("payload=Data"));
        assert!(!rows[0].tree_summary().contains("kind=1"));
        assert!(rows[1].tree_summary().contains("payload=Data"));
        let unmarked = yaml.replace(", preview: true", "");
        let out = run(&unmarked, vec![1, 7, 1, 8]).unwrap();
        assert!(!out.nodes[0].children[0].tree_summary().contains("kind=1"));
        let duplicate = yaml.replace("name: kind, type: u8", "name: kind, type: u8, preview: true");
        assert!(Program::parse(&duplicate).unwrap_err().to_string().contains("more than one preview"));
    }

    #[test]
    fn nested_description_collection_builds_reusable_structure_and_tracks_edits() {
        let mut bytes = vec![2];
        bytes.extend_from_slice(b"left\x01\x02rght\x02\x01\xff\xff\x07\x00\xfe\xff");
        let output = run(DYNAMIC, bytes.clone()).unwrap();
        assert_eq!(output.definitions.last().unwrap().name, "Discovered");
        assert_eq!(output.definitions.last().unwrap().origin, Some(1..13));
        assert_eq!(
            output.nodes[1].children[0].children[0].value,
            Some(Value::Unsigned(65535))
        );
        assert_eq!(
            output.nodes[1].children[1].children[0].value,
            Some(Value::Signed(-2))
        );
        bytes[5] = 2;
        let changed = run(DYNAMIC, bytes).unwrap();
        assert_eq!(
            changed.nodes[1].children[0].children[0].value,
            Some(Value::Signed(-1))
        );
        assert_eq!(changed.nodes[1].range, 13..19);
    }
    #[test]
    fn decoded_text_can_name_types_and_definition_can_be_reused() {
        let yaml = r#"
structures:
  - name: Entry
    fields:
      - {name: format, type: string, length: 3}
actions:
  - read: {name: definitions, type: {structure: Entry}, repeat: 2}
  - define:
      name: Pair
      from: definitions
      fields:
        type: {ref: format}
  - read: {name: first, type: {structure: Pair}}
  - read: {name: second, type: {structure: Pair}}
"#;
        let out = run(yaml, b"u16i16\x01\x00\xff\xff\x02\x00\xfe\xff".to_vec()).unwrap();
        assert_eq!(out.nodes[1].children[1].value, Some(Value::Signed(-1)));
        assert_eq!(out.nodes[2].children[1].value, Some(Value::Signed(-2)));
    }
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn huge_fixed_arrays_are_lazy_bounded_and_cancelable() {
        struct Sparse {
            reads: Cell<Address>,
            size: Address,
        }
        impl Source for Sparse {
            fn len(&self) -> Address {
                self.size
            }
            fn read(&self, offset: Address, length: Address) -> Result<Vec<u8>> {
                ensure!(
                    offset.checked_add(length).is_some_and(|e| e <= self.size),
                    "outside sparse source"
                );
                self.reads.set(self.reads.get() + length);
                Ok(vec![0; length])
            }
        }
        let source = Sparse {
            reads: Cell::new(0),
            size: 20 * 1024 * 1024 * 1024,
        };
        let program =
            Program::parse("actions:\n  - read: {name: values, type: u64, repeat: until end}")
                .unwrap();
        let out = program
            .run_from(&source, &AtomicBool::new(false), 1)
            .unwrap();
        assert_eq!(source.reads.get(), 0);
        assert_eq!(out.nodes.len(), 1);
        let array = out.nodes[0].lazy.as_ref().unwrap();
        assert_eq!(array.count, source.size / 8);
        let last = array
            .page_from(&source, &AtomicBool::new(false), array.count - 1, 1)
            .unwrap();
        assert_eq!(last[0].range, source.size - 8..source.size);
        assert_eq!(source.reads.get(), 8);
        assert!(array
            .page_from(&source, &AtomicBool::new(false), 0, MAX_PAGE + 1)
            .is_err());
        assert!(program
            .run_from(&source, &AtomicBool::new(true), 1)
            .is_err());
    }
    #[test]
    fn interleaved_choose_records_page_and_map_bytes_beyond_node_cap() {
        let yaml = r#"
structures:
  - name: Short
    fields:
      - {name: a, type: u16}
  - name: Long
    fields:
      - {name: b, type: u32}
  - name: Record
    fields:
      - {name: tag, type: u8}
      - name: payload
        type: {choose: {ref: tag, cases: {'1': {structure: Short}, '2': {structure: Long}}}}
actions:
  - read: {name: records, type: {structure: Record}, repeat: until end}
"#;
        let mut bytes = Vec::new();
        for index in 0..30_000u32 {
            if index % 2 == 0 {
                bytes.push(1);
                bytes.extend_from_slice(&(index as u16).to_le_bytes());
            } else {
                bytes.push(2);
                bytes.extend_from_slice(&index.to_le_bytes());
            }
        }
        let source = Bytes(bytes.clone());
        let output = Program::parse(yaml).unwrap()
            .run_from(&source, &AtomicBool::new(false), 1).unwrap();
        assert_eq!(output.consumed, bytes.len());
        let node = &output.nodes[0];
        let array = node.lazy.as_ref().expect("variable records should be indexed");
        assert_eq!(array.count, 30_000);
        assert_eq!(node.children.len(), 0);
        let page = array.page_from(&source, &AtomicBool::new(false), 1_455, 3).unwrap();
        assert_eq!(page[0].children[1].kind, "Long");
        assert_eq!(page[1].children[1].kind, "Short");
        assert_eq!(page[0].children[1].children[0].value, Some(Value::Unsigned(1_455)));
        let bytes = array.bounds(1_455).unwrap();
        assert_eq!(array.indices_for_bytes(&bytes),
            Some(1_455..1_456));
        assert_eq!(bytes.start..array.bounds(1_456).unwrap().end,
            page[0].range.start..page[1].range.end);
        assert_eq!(array.bounds(29_999).unwrap().end, source.len());
    }

    #[test]
    fn indexed_lengths_from_prior_fields_keep_record_boundaries() {
        let yaml = r#"
structures:
  - name: Item
    fields:
      - {name: length, type: u8}
      - {name: payload, type: bytes, length: {ref: length}}
actions:
  - read: {name: items, type: {structure: Item}, repeat: until end}
"#;
        let mut data = Vec::new();
        for index in 0..2_000 {
            let length = (index % 4 + 1) as u8;
            data.push(length);
            data.extend(std::iter::repeat_n(0xA5, length as usize));
        }
        let source = Bytes(data);
        let output = Program::parse(yaml).unwrap()
            .run_from(&source, &AtomicBool::new(false), 1).unwrap();
        let array = output.nodes[0].lazy.as_ref().unwrap();
        assert_eq!(array.count, 2_000);
        let page = array.page_from(&source, &AtomicBool::new(false), 1_999, 1).unwrap();
        assert_eq!(page[0].children[0].value, Some(Value::Unsigned(4)));
        assert_eq!(page[0].range.end, source.len());
    }
    #[test]
    fn regions_choices_alignment_and_prior_lengths_decode_chunks() {
        let yaml = r#"
structures:
  - name: Chunk
    fields:
      - {name: id, type: u8}
      - {name: size, type: u8}
      - name: payload
        type: {choose: {ref: id, cases: {'1': {structure: Pair}}, otherwise: bytes}}
        length: {ref: size}
    actions:
      - align: 2
  - name: Pair
    fields:
      - {name: first, type: u16}
      - {name: second, type: u16}
actions:
  - read: {name: chunks, type: {structure: Chunk}, repeat: until end}
"#;
        let out = run(yaml, vec![1, 4, 2, 0, 3, 0, 9, 1, 255, 0]).unwrap();
        assert_eq!(out.nodes[0].children.len(), 2);
        assert_eq!(
            out.nodes[0].children[0].children[2].children[1].value,
            Some(Value::Unsigned(3))
        );
        assert_eq!(out.nodes[0].children[1].range, 6..10);
        assert_eq!(out.nodes[0].children[1].children[2].range, 8..9);
    }
    #[test]
    fn conditional_length_can_use_stored_sizes_or_the_remaining_region() {
        let yaml = r#"
structures:
  - name: KnownPayload
    fields:
      - {name: value, type: u16}
  - name: TaggedBlock
    fields:
      - {name: kind, type: u8}
      - {name: stored_length, type: u16}
      - name: payload
        type:
          choose:
            ref: kind
            cases: {'1': {structure: KnownPayload}}
            otherwise: bytes
        length:
          choose:
            ref: kind
            cases: {'0': remaining}
            otherwise: {ref: stored_length}
actions:
  - read: {name: blocks, type: {structure: TaggedBlock}, repeat: until end}
"#;
        let out = run(
            yaml,
            vec![1, 2, 0, 0x34, 0x12, 0, 0xff, 0xff, 0xaa, 0xbb, 0xcc, 0xdd],
        )
        .unwrap();
        let blocks = &out.nodes[0].children;
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].children[2].range, 3..5);
        assert_eq!(blocks[0].children[2].children[0].value, Some(Value::Unsigned(0x1234)));
        assert_eq!(blocks[1].children[1].value, Some(Value::Unsigned(0xffff)));
        assert_eq!(blocks[1].children[2].range, 8..12);
        assert_eq!(
            blocks[1].children[2].value,
            Some(Value::Bytes(vec![0xaa, 0xbb, 0xcc, 0xdd]))
        );

        let saved = serde_yaml_ng::to_string(&Program::parse(yaml).unwrap()).unwrap();
        assert!(saved.contains("remaining"));
        Program::parse(&saved).unwrap();

        let bounded = run(
            "structures:\n  - name: Envelope\n    fields:\n      - {name: body, type: bytes, length: remaining}\nactions:\n  - read: {name: envelope, type: {structure: Envelope}, length: 3}\n  - read: {name: footer, type: u8}\n",
            vec![1, 2, 3, 4],
        )
        .unwrap();
        assert_eq!(bounded.nodes[0].children[0].range, 0..3);
        assert_eq!(bounded.nodes[1].value, Some(Value::Unsigned(4)));
    }
    #[test]
    fn remaining_is_only_valid_for_lengths_and_choose_needs_a_matching_branch() {
        for yaml in [
            "actions: [{skip: remaining}]",
            "actions: [{read: {name: value, type: u8, at: remaining}}]",
            "actions: [{read: {name: value, type: bytes, length: leftover}}]",
        ] {
            assert!(Program::parse(yaml).is_err(), "accepted invalid YAML: {yaml}");
        }
        let yaml = "actions:\n  - read: {name: kind, type: u8}\n  - read:\n      name: payload\n      type: bytes\n      length: {choose: {ref: kind, cases: {'0': remaining}}}\n";
        let error = run(yaml, vec![1, 0xaa]).unwrap_err();
        assert!(format!("{error:#}").contains("no length mapping"));
    }
    #[test]
    fn counts_offsets_arithmetic_and_exact_u64_values_are_preserved() {
        let yaml = r#"
actions:
  - read: {name: count, type: u8}
  - calculate: {name: start, value: {add: [{ref: count}, 2]}}
  - read: {name: payload, type: u64, at: {ref: start}, endian: big}
"#;
        let out = run(yaml, vec![1, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255]).unwrap();
        assert_eq!(out.nodes[2].range, 3..11);
        assert_eq!(out.nodes[2].display(), u64::MAX.to_string());
        assert_eq!(out.nodes[2].value, Some(Value::Unsigned(u64::MAX)));
        assert!(run("actions:\n - skip: {subtract: [0, 1]}", vec![1]).is_err());
        assert!(run("actions:\n - skip: {divide: [3, 2]}", vec![1]).is_err());
    }
    #[test]
    fn malformed_definitions_and_bad_input_return_contextual_errors() {
        for yaml in [
            "actions: [{read: {name: x, type: u8, tyop: 2}}]",
            "actions: [{read: {name: x, type: u8}, skip: 1}]",
            "actions: [{skip: {ref: x, extra: 1}}]",
            "actions: [{read: {name: x, type: u8, repeat: nonsense}}]",
        ] {
            assert!(
                Program::parse(yaml).is_err(),
                "accepted invalid YAML: {yaml}"
            );
        }
        let err = run("actions: [{read: {name: payload, type: u64}}]", vec![1]).unwrap_err();
        assert!(format!("{err:#}").contains("payload"));
        assert!(run(
            "actions: [{read: {name: payload, type: u8, repeat: {ref: future}}}]",
            vec![1]
        )
        .is_err());
        assert!(run(
            "actions: [{read: {name: payload, type: u64, repeat: 18446744073709551615}}]",
            vec![1]
        )
        .is_err());
        assert!(run("structures: [{name: Loop, fields: [{name: loop, type: {structure: Loop}}]}]\nactions: [{read: {name: x, type: {structure: Loop}}}]", vec![1]).is_err());
        assert!(run("actions: [{read: {name: x, type: u8, expect: 2}}]", vec![1]).is_err());
    }
    #[test]
    fn failed_run_retains_completed_fields_as_a_partial_tree() {
        let program = Program::parse(
            "structures:\n  - name: Packet\n    fields:\n      - {name: tag, type: u8}\n      - {name: value, type: u32}\nactions:\n  - read: {name: packet, type: {structure: Packet}}\n",
        ).unwrap();
        let failure = program
            .run_from_with_partial(&Bytes(vec![7, 1]), &AtomicBool::new(false), 9)
            .unwrap_err();

        assert!(format!("{:#}", failure.error).contains("byte range"));
        assert_eq!(failure.output.consumed, 1);
        assert_eq!(failure.output.revision, 9);
        assert_eq!(failure.output.nodes[0].name, "packet");
        assert_eq!(failure.output.nodes[0].kind, "partial");
        assert_eq!(failure.output.nodes[0].children[0].name, "tag");
        assert_eq!(failure.output.nodes[0].children[0].value, Some(Value::Unsigned(7)));
        assert_eq!(failure.output.nodes[0].children[0].range, 0..1);
    }
    #[test]
    fn failed_variable_section_keeps_prior_items_and_partial_item() {
        let yaml = r#"
structures:
  - name: Section
    fields:
      - {name: tag, type: u8}
      - {name: size, type: u8}
      - {name: payload, type: bytes, length: {ref: size}}
actions:
  - read: {name: signature, type: u8}
  - read: {name: sections, type: {structure: Section}, repeat: until end}
"#;
        let failure = Program::parse(yaml).unwrap()
            .run_from_with_partial(&Bytes(vec![9, 1, 1, 42, 2, 3, 99]), &AtomicBool::new(false), 1)
            .unwrap_err();
        assert!(format!("{:#}", failure.error).contains("byte range"));
        let roots = &failure.output.nodes;
        assert_eq!(roots[0].path, "signature");
        assert_eq!(roots[1].path, "sections");
        assert_eq!(roots[1].tree_summary(), "2 items decoded · incomplete · starts 0x1");
        assert_eq!(roots[1].children[0].tree_label(), "sections[0]");
        assert_eq!(roots[1].children[0].children[2].value, Some(Value::Bytes(vec![42])));
        assert_eq!(roots[1].children[1].tree_label(), "sections[1]");
        assert_eq!(roots[1].children[1].children[0].value, Some(Value::Unsigned(2)));
    }
    #[test]
    fn incomplete_final_block_keeps_previous_sections() {
        let yaml = r#"
structures:
  - name: Data
    fields:
      - {name: blocks, type: u16, repeat: until end}
  - name: Section
    fields:
      - {name: kind, type: u8}
      - {name: size, type: u8}
      - name: payload
        type: {choose: {ref: kind, cases: {'0': {structure: Data}}, otherwise: bytes}}
        length: {ref: size}
actions:
  - read: {name: signature, type: u8}
  - read: {name: sections, type: {structure: Section}, repeat: until end}
"#;
        let failure = Program::parse(yaml).unwrap()
            .run_from_with_partial(&Bytes(vec![9, 1, 1, 42, 0, 3, 1, 2, 3]), &AtomicBool::new(false), 1)
            .unwrap_err();
        assert!(format!("{:#}", failure.error).contains("fixed-size repeat"));
        let sections = &failure.output.nodes[1];
        assert_eq!(sections.children.len(), 2);
        assert_eq!(sections.children[0].children[2].value, Some(Value::Bytes(vec![42])));
        assert_eq!(sections.children[1].children[0].value, Some(Value::Unsigned(0)));
        assert_eq!(sections.children[1].children[1].value, Some(Value::Unsigned(3)));
        let completed = &sections.children[1].children[2].children[0];
        assert_eq!(completed.path, "sections[1].payload.blocks");
        assert_eq!(completed.lazy.as_ref().unwrap().count, 1);
        assert_eq!(completed.range, 6..8);
    }
    #[test]
    fn fixed_repeat_size_error_keeps_inspectable_complete_blocks() {
        let yaml = "structures:\n  - name: Block\n    fields:\n      - {name: samples, type: u16, repeat: 2}\nactions:\n  - read: {name: blocks, type: {structure: Block}, repeat: until end}\n";
        let bytes = Bytes(vec![1, 0, 2, 0, 3, 0, 4, 0, 9, 9]);
        let failure = Program::parse(yaml).unwrap()
            .run_from_with_partial(&bytes, &AtomicBool::new(false), 1)
            .unwrap_err();
        let message = format!("{:#}", failure.error);
        assert!(message.contains("10 bytes available; 2 complete 4-byte items fit; 2 bytes remain"), "{message}");
        assert!(message.contains("1 byte per complete item"), "{message}");
        assert_eq!(failure.output.consumed, 8);
        let blocks = &failure.output.nodes[0];
        assert_eq!(blocks.lazy.as_ref().unwrap().count, 2);
        assert_eq!(blocks.range, 0..8);
        assert_eq!(blocks.children[0].path, "blocks[0]");
        assert_eq!(blocks.children[0].children[0].children[0].value, Some(Value::Unsigned(1)));
        let first = blocks.lazy.as_ref().unwrap().page_from(&bytes, &AtomicBool::new(false), 0, 1).unwrap();
        assert_eq!(first[0].children[0].children[0].value, Some(Value::Unsigned(1)));
    }
    #[test]
    fn text_terminators_leb128_and_bounded_offsets_are_checked() {
        let out = run(
            "actions: [{read: {name: label, type: cstring}}, {read: {name: size, type: varuint}}]",
            b"hello\0\xac\x02".to_vec(),
        )
        .unwrap();
        assert_eq!(out.nodes[0].range, 0..6);
        assert_eq!(out.nodes[1].value, Some(Value::Unsigned(300)));
        assert!(run(
            "actions: [{read: {name: x, type: cstring}}]",
            b"hello".to_vec()
        )
        .is_err());
        assert!(run("actions: [{read: {name: x, type: varuint}}]", vec![255; 10]).is_err());
        let yaml = "structures: [{name: Region, fields: [{name: x, type: u8, at: 0}]}]\nactions: [{skip: 2}, {read: {name: region, type: {structure: Region}, length: 1}}]";
        assert!(run(yaml, vec![1, 2, 3]).is_err());
    }
    #[test]
    fn actions_round_trip_as_readable_yaml_mappings() {
        let program = Program::parse(DYNAMIC).unwrap();
        let yaml = serde_yaml_ng::to_string(&program).unwrap();
        assert!(!yaml.contains("!read"));
        assert!(yaml.contains("- read:"));
        Program::parse(&yaml).unwrap();
    }
    #[test]
    fn nested_section_actions_publish_definitions_for_later_sections() {
        let yaml = include_str!("../samples/decoders/yaml/generic_sections.yaml");
        let bytes = b"\x01\x0cleft\x01\x02rght\x02\x01\x02\x06\xff\xff\x07\x00\xfe\xff".to_vec();
        let out = run(yaml, bytes).unwrap();
        let sections = &out.nodes[0].children;
        assert_eq!(sections.len(), 2);
        assert_eq!(
            sections[1].children[2].children[0].children[0].value,
            Some(Value::Unsigned(65535))
        );
        assert_eq!(
            sections[1].children[2].children[1].children[0].value,
            Some(Value::Signed(-2))
        );
    }
    #[test]
    fn sample_png_and_riff_containers_decode_known_and_unknown_regions() {
        let mut png = 0x89504E470D0A1A0Au64.to_be_bytes().to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        png.extend_from_slice(&[8, 2, 0, 0, 0]);
        png.extend_from_slice(&0u32.to_be_bytes());
        png.extend_from_slice(&0u32.to_be_bytes());
        png.extend_from_slice(b"IEND");
        png.extend_from_slice(&0u32.to_be_bytes());
        let out = run(
            include_str!("../samples/decoders/yaml/png_chunks.yaml"),
            png,
        )
        .unwrap();
        assert_eq!(
            out.nodes[1].children[0].children[2].children[0].value,
            Some(Value::Unsigned(640))
        );
        assert_eq!(out.nodes[1].children[1].children[2].range.len(), 0);
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&40u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&[1, 0, 2, 0]);
        wav.extend_from_slice(&48000u32.to_le_bytes());
        wav.extend_from_slice(&192000u32.to_le_bytes());
        wav.extend_from_slice(&[4, 0, 16, 0]);
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(&[0; 4]);
        let out = run(
            include_str!("../samples/decoders/yaml/riff_chunks.yaml"),
            wav,
        )
        .unwrap();
        assert_eq!(
            out.nodes[2].children[1].children[0].children[2].children[2].value,
            Some(Value::Unsigned(48000))
        );
    }

    #[test]
    fn record_local_lengths_never_reuse_an_outer_namesake_as_fixed_stride() {
        let yaml = "structures: [{name: Record, fields: [{name: count, type: u8}, {name: data, type: bytes, length: {ref: count}}]}]\nactions: [{read: {name: count, type: u8}}, {read: {name: records, type: {structure: Record}, repeat: 129}}]";
        let mut bytes = vec![255];
        bytes.extend(std::iter::repeat_n(0, 129));
        let source = Bytes(bytes);
        let out = Program::parse(yaml).unwrap()
            .run_from(&source, &AtomicBool::new(false), 1).unwrap();
        let array = out.nodes[1].lazy.as_ref().unwrap();
        assert_eq!(array.count, 129);
        assert_eq!(array.bounds(128), Some(129..130));
        assert_eq!(array.page_from(&source, &AtomicBool::new(false), 128, 1).unwrap()[0].range, 129..130);
        assert_eq!(out.nodes[1].range, 1..130);
    }
}
