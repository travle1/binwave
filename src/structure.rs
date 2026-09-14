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
use std::collections::{BTreeMap, HashMap};
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
    #[serde(default)]
    pub structures: BTreeMap<String, Structure>,
    pub actions: Vec<Action>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum TypeSpec {
    Name(String),
    Select { choose: TypeChoice },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TypeChoice {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub cases: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otherwise: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Repeat {
    Count(Expr),
    Until(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Expr {
    Unsigned(u64),
    Signed(i64),
    Ref {
        #[serde(rename = "ref")]
        reference: String,
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
}
impl Node {
    pub fn numeric(&self) -> Option<f64> {
        self.value.as_ref().and_then(Value::numeric)
    }
    pub fn display(&self) -> String {
        if let Some(array) = &self.lazy {
            format!("{} items · {} bytes each", array.count, array.stride)
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
    plan: Arc<LazyPlan>,
}
#[derive(Debug, Clone)]
struct LazyPlan {
    field: Field,
    start: Address,
    end: Address,
    path: String,
    endian: Endian,
    values: HashMap<String, Value>,
    structures: BTreeMap<String, Structure>,
    revision: u64,
}
/// A conservative numeric projection of a constant decoded structure. Unsupported
/// features fall back to the ordinary parser; a sampled record never establishes
/// the layout of other records.
#[derive(Clone)]
pub(crate) struct NumericPlan {
    pub start: Address,
    pub stride: Address,
    pub path: String,
    pub revision: u64,
    pub fields: Vec<NumericField>,
}
#[derive(Clone)]
pub(crate) struct NumericField {
    pub name: String,
    pub suffix: String,
    pub offset: Address,
    pub width: Address,
    pub span: Address,
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
        || field.repeat.is_some()
    {
        return None;
    }
    *budget -= 1;
    let TypeSpec::Name(kind) = &field.kind else {
        return None;
    };
    let length = match &field.length {
        Some(expr) => Some(constant_size(expr)?),
        None => None,
    };
    let inherited = field.endian.unwrap_or(inherited);
    let consumed = if let Some(width) = primitive_width(kind) {
        if fields.len() < 32 {
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
        let length = count.min(self.count - start).checked_mul(self.stride);
        let offset = start
            .checked_mul(self.stride)
            .and_then(|offset| self.plan.start.checked_add(offset));
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
            self.plan.structures.clone(),
            self.plan.revision,
        );
        engine.values = self.plan.values.clone();
        let end = start.saturating_add(count).min(self.count);
        let mut nodes =
            Vec::with_capacity(usize::try_from(end - start).context("page exceeds memory range")?);
        for index in start..end {
            let mut cursor = self
                .plan
                .start
                .checked_add(
                    index
                        .checked_mul(self.stride)
                        .context("array offset overflow")?,
                )
                .context("array offset overflow")?;
            let path = format!("{}[{index}]", self.plan.path);
            let mut node = engine.single(
                &self.plan.field,
                &path,
                &mut cursor,
                self.plan.end,
                self.plan.endian,
                0,
            )?;
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
        let value: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(yaml).context("read structure YAML")?;
        let program: Self =
            serde_yaml_ng::from_value(value).context("invalid structure definition")?;
        ensure!(!program.actions.is_empty(), "define at least one action");
        ensure!(
            program.structures.len() <= 256,
            "at most 256 structures are allowed"
        );
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
            for field in &structure.fields {
                valid_field(field)?;
                ensure!(
                    seen.insert(&field.name),
                    "duplicate field {} in {name}",
                    field.name
                );
            }
            validate_actions(&structure.actions, 0)?;
        }
        validate_actions(&program.actions, 0)?;
        Ok(program)
    }
    pub fn run(&self, source: &ByteView, cancel: &AtomicBool) -> Result<ParseOutput> {
        source.validate()?;
        let output = self.run_from(source, cancel, source.revision)?;
        source.validate()?;
        Ok(output)
    }
    fn run_from(
        &self,
        source: &dyn Source,
        cancel: &AtomicBool,
        revision: u64,
    ) -> Result<ParseOutput> {
        let mut engine = Engine::new(
            source,
            cancel,
            self.endian,
            self.structures.clone(),
            revision,
        );
        for (name, structure) in &self.structures {
            engine.definitions.push(describe(name, structure, None));
        }
        let mut cursor = 0;
        let mut nodes = Vec::new();
        engine.actions(
            &self.actions,
            "",
            &mut cursor,
            source.len(),
            self.endian,
            &mut nodes,
            0,
        )?;
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
            Action::Define(d) => valid_name(&d.name)?,
            Action::Calculate(c) => valid_name(&c.name)?,
            Action::Choose(c) => {
                for branch in c.cases.values() {
                    validate_actions(branch, depth + 1)?;
                }
                validate_actions(&c.otherwise, depth + 1)?;
            }
            _ => {}
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

struct Engine<'a> {
    source: &'a dyn Source,
    cancel: &'a AtomicBool,
    revision: u64,
    regions: Vec<Range<Address>>,
    structures: BTreeMap<String, Structure>,
    values: HashMap<String, Value>,
    collections: HashMap<String, Node>,
    definitions: Vec<ResolvedDefinition>,
    nodes: Address,
}
impl<'a> Engine<'a> {
    fn new(
        source: &'a dyn Source,
        cancel: &'a AtomicBool,
        _endian: Endian,
        structures: BTreeMap<String, Structure>,
        revision: u64,
    ) -> Self {
        Self {
            source,
            cancel,
            revision,
            regions: vec![],
            structures,
            values: HashMap::new(),
            collections: HashMap::new(),
            definitions: Vec::new(),
            nodes: 0,
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
        }
        bail!("{scope}: reference {reference:?} is not an earlier decoded value")
    }
    fn integer(&self, expr: &Expr, scope: &str) -> Result<i128> {
        let result = match expr {
            Expr::Unsigned(n) => *n as i128,
            Expr::Signed(n) => *n as i128,
            Expr::Ref { reference } => {
                if reference == "$position" {
                    bail!("use a named earlier value or 'relative' for positions");
                }
                self.lookup(reference, scope)?.integer()?
            }
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
        if choice.cases.is_empty() && choice.otherwise.is_none() {
            return Ok(key);
        }
        choice
            .cases
            .get(&key)
            .or(choice.otherwise.as_ref())
            .cloned()
            .with_context(|| {
                format!(
                    "{scope}: no type mapping for {} = {key:?}",
                    choice.reference
                )
            })
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
                    ensure!(
                        (end - start) % width == 0,
                        "{path}: {} trailing bytes do not form a complete {width}-byte item",
                        (end - start) % width
                    );
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
                let node = Node {
                    name: field.name.clone(),
                    path: path.into(),
                    range: start..array_end,
                    kind: format!("{}[]", self.selected_type(&field.kind, path)?),
                    value: None,
                    children: vec![],
                    lazy: Some(LazyArray {
                        count,
                        stride,
                        plan: Arc::new(LazyPlan {
                            field: element,
                            start,
                            end: array_end,
                            path: path.into(),
                            endian: field.endian.unwrap_or(endian),
                            values: self.values.clone(),
                            structures: self.structures.clone(),
                            revision: self.revision,
                        }),
                    }),
                    endian: field.endian.unwrap_or(endian),
                    references: field_references(field),
                };
                self.collections.insert(path.into(), node.clone());
                return Ok(node);
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
        };
        self.collections.insert(path.into(), node.clone());
        Ok(node)
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
            .map(|v| self.size(v, path))
            .transpose()?;
        let end = if let Some(length) = length {
            checked_end(start, length, outer_end)?
        } else {
            outer_end
        };
        let mut endian = field.endian.unwrap_or(inherited);
        let mut children = Vec::new();
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
        } else if kind == "text" {
            ensure!(length.is_some(), "{path}: text requires a byte length");
            ensure!(
                end - start <= MAX_TEXT,
                "{path}: text exceeds 1 MiB; inspect it as bytes instead"
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
        };
        if let Some(value) = value {
            self.values.insert(path.into(), value);
        }
        if !node.children.is_empty() {
            self.collections.insert(path.into(), node.clone());
        }
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
            && !matches!(kind.as_str(), "text" | "bytes" | "cstring" | "varuint")
            && !self.structures.contains_key(&kind)
        {
            return None;
        }
        if self.has_actions(&kind, 0) {
            return None;
        }
        let base = if let Some(length) = &field.length {
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
                    "bytes" | "text" | "cstring" | "varuint"
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
        for (index, element) in children.iter().enumerate() {
            let name = if let Some(reference) = &definition.fields.name {
                self.lookup(&reference.reference, &element.path)?
                    .key()
                    .trim_end_matches('\0')
                    .to_string()
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
                    || matches!(kind.as_str(), "bytes" | "text" | "cstring" | "varuint")
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
    primitive_width(kind).is_some() || matches!(kind, "bytes" | "text" | "cstring" | "varuint")
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
        Expr::Add { add }
        | Expr::Subtract { subtract: add }
        | Expr::Multiply { multiply: add }
        | Expr::Divide { divide: add } => add.iter().flat_map(expr_references).collect(),
        _ => vec![],
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
    const DYNAMIC: &str = r#"
endian: little
structures:
  Entry:
    fields:
      - {name: label, type: text, length: 4}
      - {name: code, type: u8}
      - {name: count, type: u8}
  Catalog:
    fields:
      - {name: count, type: u8}
      - {name: entries, type: Entry, repeat: {ref: count}}
actions:
  - read: {name: catalog, type: Catalog}
  - define:
      name: Discovered
      from: catalog.entries
      fields:
        name: {ref: label}
        type: {ref: code, cases: {'1': u16, '2': i16}}
        repeat: {ref: count}
  - read: {name: payload, type: Discovered}
"#;
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
  Entry:
    fields:
      - {name: format, type: text, length: 3}
actions:
  - read: {name: definitions, type: Entry, repeat: 2}
  - define:
      name: Pair
      from: definitions
      fields:
        type: {ref: format}
  - read: {name: first, type: Pair}
  - read: {name: second, type: Pair}
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
    fn regions_choices_alignment_and_prior_lengths_decode_chunks() {
        let yaml = r#"
structures:
  Chunk:
    fields:
      - {name: id, type: u8}
      - {name: size, type: u8}
      - name: payload
        type: {choose: {ref: id, cases: {'1': Pair}, otherwise: bytes}}
        length: {ref: size}
    actions:
      - align: 2
  Pair:
    fields:
      - {name: first, type: u16}
      - {name: second, type: u16}
actions:
  - read: {name: chunks, type: Chunk, repeat: until end}
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
        assert!(run("structures: {Loop: {fields: [{name: loop, type: Loop}]}}\nactions: [{read: {name: x, type: Loop}}]", vec![1]).is_err());
        assert!(run("actions: [{read: {name: x, type: u8, expect: 2}}]", vec![1]).is_err());
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
        let yaml = "structures: {Region: {fields: [{name: x, type: u8, at: 0}]}}\nactions: [{skip: 2}, {read: {name: region, type: Region, length: 1}}]";
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
        let yaml = "structures: {Record: {fields: [{name: count, type: u8}, {name: data, type: bytes, length: {ref: count}}]}}\nactions: [{read: {name: count, type: u8}}, {read: {name: records, type: Record, repeat: 129}}]";
        let mut bytes = vec![255];
        bytes.extend(std::iter::repeat_n(0, 129));
        let out = run(yaml, bytes).unwrap();
        assert!(out.nodes[1].lazy.is_none());
        assert_eq!(out.nodes[1].children.len(), 129);
        assert_eq!(out.nodes[1].range, 1..130);
    }
}
