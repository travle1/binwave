use super::numeric::Decimal;
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::ops::Range;

pub const MAX_LAYOUT_BYTES: usize = 64 * 1024;
const MAX_FIELDS: usize = 256;

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Endianness {
    Little,
    Big,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    Int8,
    Uint8,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Float32,
    Float64,
}

impl FieldType {
    pub fn size(self) -> usize {
        match self {
            Self::Int8 | Self::Uint8 => 1,
            Self::Int16 | Self::Uint16 => 2,
            Self::Int32 | Self::Uint32 | Self::Float32 => 4,
            Self::Float64 => 8,
        }
    }
    fn integer_bounds(self) -> Option<(i64, i64)> {
        Some(match self {
            Self::Int8 => (i8::MIN as i64, i8::MAX as i64),
            Self::Uint8 => (0, u8::MAX as i64),
            Self::Int16 => (i16::MIN as i64, i16::MAX as i64),
            Self::Uint16 => (0, u16::MAX as i64),
            Self::Int32 => (i32::MIN as i64, i32::MAX as i64),
            Self::Uint32 => (0, u32::MAX as i64),
            _ => return None,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLayout {
    endianness: Endianness,
    #[serde(default)]
    data_offset: usize,
    record_size: usize,
    fields: Vec<RawField>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawField {
    name: String,
    offset: usize,
    #[serde(rename = "type")]
    kind: FieldType,
    // Deserialize directly from the scalar, never through f64 or YAML Value.
    #[serde(default = "unit_scale")]
    scale: String,
}
fn unit_scale() -> String {
    "1".into()
}

#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    pub offset: usize,
    pub kind: FieldType,
    pub scale: f64,
    decimal_scale: Decimal,
}

impl Field {
    pub(super) fn sequential(
        name: String,
        offset: usize,
        kind: FieldType,
        text: String,
    ) -> Result<Self> {
        let scale = text
            .parse::<f64>()
            .context("scale must be a finite decimal")?;
        ensure!(
            scale.is_finite() && scale != 0.,
            "scale must be finite and nonzero"
        );
        let decimal_scale = Decimal::parse(&text).context("invalid scale")?;
        Ok(Self {
            name,
            offset,
            kind,
            scale,
            decimal_scale,
        })
    }
}

/// Identity of one decoded value in an uncompressed source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValueIdentity {
    pub section: usize,
    pub record: usize,
    pub field: usize,
    pub byte_offset: usize,
    pub byte_length: usize,
}

#[derive(Clone, Debug)]
pub struct RecordLayout {
    pub section: usize,
    pub name: String,
    pub records: usize,
    pub endianness: Endianness,
    pub data_offset: usize,
    pub record_size: usize,
    pub fields: Vec<Field>,
}

pub(super) fn reject_tags(value: &serde_yaml_ng::Value) -> Result<()> {
    use serde_yaml_ng::Value;
    match value {
        Value::Tagged(tag) => bail!("YAML tags are not supported: {}", tag.tag),
        Value::Mapping(map) => {
            for (key, value) in map {
                reject_tags(key)?;
                reject_tags(value)?;
            }
        }
        Value::Sequence(seq) => {
            for item in seq {
                reject_tags(item)?;
            }
        }
        _ => (),
    }
    Ok(())
}

impl RecordLayout {
    pub(super) fn parse_legacy(yaml: &str) -> Result<Self> {
        ensure!(
            yaml.len() <= MAX_LAYOUT_BYTES,
            "layout exceeds the 64 KiB YAML limit"
        );
        let raw: RawLayout = serde_yaml_ng::from_str(yaml).context("YAML layout")?;
        // Validate the shallow schema before building a generic tree, so nested
        // structures / alias expansion cannot allocate an unbounded Value tree.
        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).context("YAML syntax")?;
        reject_tags(&value)?;
        ensure!(raw.record_size > 0, "record_size must be greater than zero");
        raw.data_offset
            .checked_add(raw.record_size)
            .context("data_offset + record_size overflows")?;
        ensure!(
            !raw.fields.is_empty(),
            "fields must contain at least one numeric field"
        );
        ensure!(
            raw.fields.len() <= MAX_FIELDS,
            "fields exceeds the 256-field layout limit"
        );
        let mut fields: Vec<Field> = Vec::new();
        let mut names = HashSet::new();
        for field in raw.fields {
            ensure!(!field.name.trim().is_empty(), "field name cannot be empty");
            ensure!(
                names.insert(field.name.clone()),
                "duplicate field name '{}'",
                field.name
            );
            let end = field
                .offset
                .checked_add(field.kind.size())
                .with_context(|| format!("field '{}': offset + type size overflows", field.name))?;
            ensure!(
                end <= raw.record_size,
                "field '{}': bytes {}..{} exceed record_size {}",
                field.name,
                field.offset,
                end,
                raw.record_size
            );
            for other in &fields {
                ensure!(
                    end <= other.offset || field.offset >= other.offset + other.kind.size(),
                    "field '{}' overlaps field '{}'",
                    field.name,
                    other.name
                );
            }
            let scale = field.scale.parse::<f64>().with_context(|| {
                format!("field '{}': scale must be a finite decimal", field.name)
            })?;
            ensure!(
                scale.is_finite() && scale != 0.0,
                "field '{}': scale must be finite and nonzero",
                field.name
            );
            let decimal_scale = Decimal::parse(&field.scale)
                .with_context(|| format!("field '{}': invalid scale", field.name))?;
            fields.push(Field {
                name: field.name,
                offset: field.offset,
                kind: field.kind,
                scale,
                decimal_scale,
            });
        }
        Ok(Self {
            section: 0,
            name: "records".into(),
            records: 0,
            endianness: raw.endianness,
            data_offset: raw.data_offset,
            record_size: raw.record_size,
            fields,
        })
    }

    pub fn record_offset(&self, record: usize) -> Result<usize> {
        record
            .checked_mul(self.record_size)
            .and_then(|n| self.data_offset.checked_add(n))
            .context("record byte offset overflows")
    }

    pub fn field_range(&self, record: usize, field: usize) -> Result<Range<usize>> {
        let field = self
            .fields
            .get(field)
            .context("field index is out of range")?;
        ensure!(
            record < self.records,
            "section '{}': record index is out of range",
            self.name
        );
        let start = self
            .record_offset(record)?
            .checked_add(field.offset)
            .context("field byte offset overflows")?;
        let end = start
            .checked_add(field.kind.size())
            .context("field byte end overflows")?;
        Ok(start..end)
    }

    pub fn identity(&self, record: usize, field: usize) -> Result<ValueIdentity> {
        let range = self.field_range(record, field)?;
        Ok(ValueIdentity {
            section: self.section,
            record,
            field,
            byte_offset: range.start,
            byte_length: range.len(),
        })
    }

    pub fn stored_value(&self, field: usize, bytes: &[u8]) -> f64 {
        let kind = self.fields[field].kind;
        let little = matches!(self.endianness, Endianness::Little);
        macro_rules! read {
            ($ty:ty) => {{
                let a = bytes.try_into().expect("validated field width");
                if little {
                    <$ty>::from_le_bytes(a)
                } else {
                    <$ty>::from_be_bytes(a)
                }
            }};
        }
        match kind {
            FieldType::Int8 => bytes[0] as i8 as f64,
            FieldType::Uint8 => bytes[0] as f64,
            FieldType::Int16 => read!(i16) as f64,
            FieldType::Uint16 => read!(u16) as f64,
            FieldType::Int32 => read!(i32) as f64,
            FieldType::Uint32 => read!(u32) as f64,
            FieldType::Float32 => read!(f32) as f64,
            FieldType::Float64 => read!(f64),
        }
    }

    pub fn display_text(&self, field: usize, bytes: &[u8]) -> String {
        let stored = self.stored_value(field, bytes);
        let field = &self.fields[field];
        if field.kind.integer_bounds().is_some() {
            field.decimal_scale.scaled_integer_text(stored as i64)
        } else {
            (stored * field.scale).to_string()
        }
    }

    pub fn encode(&self, index: usize, displayed: &str) -> Result<Vec<u8>> {
        let field = self
            .fields
            .get(index)
            .context("field index is out of range")?;
        let little = matches!(self.endianness, Endianness::Little);
        macro_rules! bytes {
            ($v:expr) => {{
                let v = $v;
                if little {
                    v.to_le_bytes().to_vec()
                } else {
                    v.to_be_bytes().to_vec()
                }
            }};
        }
        let result = (|| -> Result<Vec<u8>> {
            if let Some((min, max)) = field.kind.integer_bounds() {
                let stored = Decimal::parse(displayed)?.stored_integer(&field.decimal_scale)?;
                ensure!(
                    (min..=max).contains(&stored),
                    "stored value {stored} is out of range ({min}..={max})"
                );
                return Ok(match field.kind {
                    FieldType::Int8 => vec![stored as i8 as u8],
                    FieldType::Uint8 => vec![stored as u8],
                    FieldType::Int16 => bytes!(stored as i16),
                    FieldType::Uint16 => bytes!(stored as u16),
                    FieldType::Int32 => bytes!(stored as i32),
                    FieldType::Uint32 => bytes!(stored as u32),
                    _ => unreachable!(),
                });
            }
            ensure!(
                displayed.len() <= 1024,
                "numeric input exceeds 1024 characters"
            );
            let text = displayed.trim();
            let value: f64 = text
                .parse()
                .context("enter a decimal number, NaN, inf, or -inf")?;
            let explicit_nonfinite = matches!(
                text.to_ascii_lowercase().as_str(),
                "nan"
                    | "+nan"
                    | "-nan"
                    | "inf"
                    | "+inf"
                    | "-inf"
                    | "infinity"
                    | "+infinity"
                    | "-infinity"
            );
            ensure!(
                value.is_finite() || explicit_nonfinite,
                "input overflows float64; use inf explicitly if intended"
            );
            let stored = value / field.scale;
            ensure!(
                !value.is_finite() || stored.is_finite(),
                "value / scale overflows the stored float"
            );
            match field.kind {
                FieldType::Float32 => {
                    let rounded = stored as f32;
                    ensure!(
                        !stored.is_finite() || rounded.is_finite(),
                        "stored value is out of float32 range"
                    );
                    Ok(bytes!(rounded))
                }
                FieldType::Float64 => Ok(bytes!(stored)),
                _ => unreachable!(),
            }
        })();
        result.with_context(|| format!("field '{}'", field.name))
    }
}
