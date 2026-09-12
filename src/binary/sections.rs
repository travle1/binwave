//! YAML compilation and physical section boundaries. Byte access is supplied by
//! the caller; decoding and edit storage remain independent of file access.
use super::layout::{reject_tags, Endianness, Field, FieldType, RecordLayout, MAX_LAYOUT_BYTES};
use crate::cache::LoadProgress;
use anyhow::{bail, ensure, Context, Result};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::{collections::HashSet, fmt, ops::Range, sync::Arc};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSections {
    endian: Endianness,
    sections: Vec<RawSection>,
    #[serde(default, deserialize_with = "no_compression")]
    compression: (),
}

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

fn no_compression<'de, D: Deserializer<'de>>(_: D) -> std::result::Result<(), D::Error> {
    Err(de::Error::custom(
        "compressed sections are not supported yet",
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSection {
    name: String,
    #[serde(default, deserialize_with = "present")]
    bytes: Option<usize>,
    #[serde(default, deserialize_with = "present")]
    fields: Option<Vec<Entry>>,
    #[serde(default, deserialize_with = "present")]
    repeat: Option<Repeat>,
    #[serde(default, deserialize_with = "present")]
    at: Option<usize>,
    #[serde(default, deserialize_with = "present")]
    marker: Option<String>,
    #[serde(default, deserialize_with = "no_compression")]
    compression: (),
}

#[derive(Clone, Copy, Debug)]
enum Repeat {
    Count(usize),
    Eof,
    UntilNext,
}

impl<'de> Deserialize<'de> for Repeat {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Repeat;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a positive integer, 'eof', or 'until next'")
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> std::result::Result<Repeat, E> {
                let n = usize::try_from(n).map_err(E::custom)?;
                if n == 0 {
                    return Err(E::custom("repeat count must be positive"));
                }
                Ok(Repeat::Count(n))
            }
            fn visit_str<E: de::Error>(self, s: &str) -> std::result::Result<Repeat, E> {
                match s {
                    "eof" => Ok(Repeat::Eof),
                    "until next" => Ok(Repeat::UntilNext),
                    _ => Err(E::custom(
                        "repeat must be a positive integer, 'eof', or 'until next'",
                    )),
                }
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expanded {
    #[serde(rename = "type")]
    kind: String,
    // Direct scalar deserialization retains every decimal digit of the YAML scale.
    #[serde(default = "one")]
    scale: String,
}
fn one() -> String {
    "1".into()
}

#[derive(Debug)]
enum FieldValue {
    Compact(String),
    Expanded(Expanded),
    Padding(usize),
}
impl<'de> Deserialize<'de> for FieldValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = FieldValue;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a numeric type, {type, scale}, or padding byte count")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> std::result::Result<FieldValue, E> {
                Ok(FieldValue::Compact(s.into()))
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> std::result::Result<FieldValue, E> {
                Ok(FieldValue::Padding(usize::try_from(n).map_err(E::custom)?))
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                map: M,
            ) -> std::result::Result<FieldValue, M::Error> {
                Expanded::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(FieldValue::Expanded)
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Debug)]
struct Entry {
    name: String,
    value: FieldValue,
}
impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Entry;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("one field-name: definition entry")
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Entry, M::Error> {
                let name: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("empty field entry"))?;
                let value = map
                    .next_value()
                    .map_err(|e| de::Error::custom(format!("field '{name}': {e}")))?;
                if map.next_key::<String>()?.is_some() {
                    return Err(de::Error::custom(format!("field '{name}': expected exactly one name per entry (duplicate or conflicting keys)")));
                }
                Ok(Entry { name, value })
            }
        }
        d.deserialize_map(V)
    }
}

#[derive(Debug)]
struct Section {
    name: String,
    at: Option<usize>,
    marker: Vec<u8>,
    repeat: Repeat,
    opaque_bytes: Option<usize>,
    record: RecordLayout,
}

#[derive(Debug)]
pub struct Layout {
    sections: Vec<Section>,
    legacy: bool,
}

#[derive(Debug)]
pub struct ResolvedSection {
    /// Physical range includes the marker, once, before any payload records.
    pub physical: Range<usize>,
    pub marker: Range<usize>,
    pub opaque: bool,
    pub record: Arc<RecordLayout>,
}

#[derive(Debug)]
pub struct ResolvedLayout {
    pub sections: Vec<ResolvedSection>,
    pub gaps: Vec<Range<usize>>,
    pub trailing: Range<usize>,
    pub legacy: bool,
}

// Inspect only root keys without first expanding arbitrary YAML into a Value.
// Full typed parsing below validates nested shape before the tag-checking pass.
struct Keys(Vec<String>);
impl<'de> Deserialize<'de> for Keys {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Keys;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a YAML layout mapping")
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Keys, M::Error> {
                let mut keys = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    if keys.contains(&key) {
                        return Err(de::Error::custom(format!("duplicate key '{key}'")));
                    }
                    if key == "compression" {
                        return Err(de::Error::custom(
                            "compressed sections are not supported yet",
                        ));
                    }
                    keys.push(key);
                    map.next_value::<de::IgnoredAny>()?;
                }
                Ok(Keys(keys))
            }
        }
        d.deserialize_map(V)
    }
}

impl Layout {
    pub fn parse(yaml: &str) -> Result<Self> {
        ensure!(
            yaml.len() <= MAX_LAYOUT_BYTES,
            "layout exceeds the 64 KiB YAML limit"
        );
        let keys: Keys = serde_yaml_ng::from_str(yaml).context("YAML layout")?;
        let new = keys
            .0
            .iter()
            .any(|k| matches!(k.as_str(), "endian" | "sections"));
        let legacy = keys.0.iter().any(|k| {
            matches!(
                k.as_str(),
                "endianness" | "data_offset" | "record_size" | "fields"
            )
        });
        ensure!(!(new && legacy), "cannot mix section and legacy layout schemas (endian/sections versus endianness/data_offset/record_size/fields)");
        if !new && legacy {
            let record = RecordLayout::parse_legacy(yaml)?;
            return Ok(Self {
                sections: vec![Section {
                    name: "records".into(),
                    at: Some(record.data_offset),
                    marker: vec![],
                    repeat: Repeat::Eof,
                    opaque_bytes: None,
                    record,
                }],
                legacy: true,
            });
        }
        let raw: RawSections = serde_yaml_ng::from_str(yaml).context("YAML sections")?;
        let _ = raw.compression;
        ensure!(!raw.sections.is_empty(), "sections must not be empty");
        ensure!(
            raw.sections.len() <= 256,
            "layout exceeds the 256-section limit"
        );
        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).context("YAML syntax")?;
        reject_tags(&value)?;
        let endian = raw.endian;
        let mut names = HashSet::new();
        let mut sections = Vec::new();
        let mut total_fields = 0usize;
        for (index, raw) in raw.sections.into_iter().enumerate() {
            let name = raw.name.clone();
            let result = (|| -> Result<Section> {
                let _ = raw.compression;
                ensure!(!name.trim().is_empty(), "section name cannot be empty");
                ensure!(
                    names.insert(name.clone()),
                    "duplicate section name '{name}'"
                );
                ensure!(
                    raw.bytes.is_some() != raw.fields.is_some(),
                    "declare exactly one of bytes or fields"
                );
                ensure!(
                    raw.repeat.is_none() || raw.fields.is_some(),
                    "repeat is only allowed with fields"
                );
                let marker = raw
                    .marker
                    .as_deref()
                    .map(parse_marker)
                    .transpose()?
                    .unwrap_or_default();
                let mut fields = Vec::new();
                let mut field_names = HashSet::new();
                let mut size = 0usize;
                for entry in raw.fields.unwrap_or_default() {
                    let field_name = entry.name.clone();
                    let result = (|| -> Result<()> {
                        ensure!(!entry.name.trim().is_empty(), "field name cannot be empty");
                        if entry.name == "padding" {
                            let FieldValue::Padding(n) = entry.value else {
                                bail!("padding is reserved; use padding: nonnegative-byte-count");
                            };
                            size = size
                                .checked_add(n)
                                .context("padding length overflows record size")?;
                            return Ok(());
                        }
                        ensure!(
                            field_names.insert(entry.name.clone()),
                            "duplicate field name '{}'",
                            entry.name
                        );
                        let (kind, scale) = match entry.value {
                            FieldValue::Compact(kind) => (kind, one()),
                            FieldValue::Expanded(v) => (v.kind, v.scale),
                            FieldValue::Padding(_) => {
                                bail!("numeric byte counts are only allowed for padding")
                            }
                        };
                        let kind = match kind.as_str() {
                            "u8" => FieldType::Uint8, "i8" => FieldType::Int8,
                            "u16" => FieldType::Uint16, "i16" => FieldType::Int16,
                            "u32" => FieldType::Uint32, "i32" => FieldType::Int32,
                            "f32" => FieldType::Float32, "f64" => FieldType::Float64,
                            _ => bail!("unsupported type '{kind}' (use u8, i8, u16, i16, u32, i32, f32, f64)"),
                        };
                        let field = Field::sequential(entry.name, size, kind, scale)?;
                        size = size
                            .checked_add(kind.size())
                            .context("field size overflows record size")?;
                        fields.push(field);
                        Ok(())
                    })();
                    result.with_context(|| format!("field '{field_name}'"))?;
                }
                if raw.bytes.is_none() {
                    ensure!(size > 0, "record size must be nonzero");
                }
                total_fields = total_fields
                    .checked_add(fields.len())
                    .context("field count overflows")?;
                ensure!(total_fields <= 256, "layout exceeds the 256-field limit");
                let repeat = raw.repeat.unwrap_or(Repeat::Count(1));
                let minimum = match repeat {
                    Repeat::Count(n) => size
                        .checked_mul(n)
                        .context("repeat count × record size overflows")?,
                    _ => 0,
                };
                raw.at
                    .unwrap_or(0)
                    .checked_add(marker.len())
                    .and_then(|p| p.checked_add(raw.bytes.unwrap_or(minimum)))
                    .context("section byte range overflows")?;
                Ok(Section {
                    name: name.clone(),
                    at: raw.at,
                    marker,
                    repeat,
                    opaque_bytes: raw.bytes,
                    record: RecordLayout {
                        section: index,
                        name: name.clone(),
                        records: 0,
                        endianness: endian,
                        data_offset: 0,
                        record_size: size,
                        fields,
                    },
                })
            })();
            let section = result.with_context(|| format!("section '{name}'"))?;
            sections.push(section);
        }
        for (index, section) in sections.iter().enumerate() {
            match section.repeat {
                Repeat::Eof => ensure!(
                    index + 1 == sections.len(),
                    "section '{}': repeat: eof must be final",
                    section.name
                ),
                Repeat::UntilNext => {
                    let next = sections.get(index + 1).with_context(|| {
                        format!(
                            "section '{}': repeat: until next requires a following section",
                            section.name
                        )
                    })?;
                    ensure!(
                        next.at.is_some() || !next.marker.is_empty(),
                        "section '{}': following section '{}' must declare at or marker",
                        section.name,
                        next.name
                    );
                }
                _ => (),
            }
        }
        Ok(Self {
            sections,
            legacy: false,
        })
    }

    /// Resolve boundaries against a read-only view of current working bytes.
    /// No decoded values or per-record allocations are needed for this step.
    pub fn resolve(
        &self,
        len: usize,
        byte: impl Fn(usize) -> u8,
        progress: &LoadProgress,
    ) -> Result<ResolvedLayout> {
        progress.set_total(len as u64);
        let mut cursor = 0usize;
        let mut sections = Vec::new();
        let mut gaps = Vec::new();
        let matches = |at: usize, marker: &[u8]| -> bool {
            at.checked_add(marker.len()).is_some_and(|end| end <= len)
                && marker.iter().enumerate().all(|(i, b)| byte(at + i) == *b)
        };
        for (index, section) in self.sections.iter().enumerate() {
            let resolved = (|| -> Result<ResolvedSection> {
                progress.check()?;
                if let Some(at) = section.at {
                    ensure!(
                        at >= cursor,
                        "at {at} is backward or overlaps the previous section ending at {cursor}"
                    );
                    ensure!(at <= len, "at {at} is beyond file end {len}");
                    if at > cursor {
                        gaps.push(cursor..at);
                    }
                    cursor = at;
                }
                let start = cursor;
                ensure!(
                    matches(cursor, &section.marker),
                    "marker missing at byte {cursor} (0x{cursor:X})"
                );
                cursor = cursor
                    .checked_add(section.marker.len())
                    .context("marker end overflows")?;
                let data_start = cursor;
                let mut record = section.record.clone();
                record.data_offset = data_start;
                if let Some(n) = section.opaque_bytes {
                    cursor = cursor.checked_add(n).context("bytes length overflows")?;
                    ensure!(cursor <= len, "opaque bytes extend beyond file end {len}");
                } else {
                    let size = record.record_size;
                    let end = match section.repeat {
                        Repeat::Count(n) => {
                            let end = n
                                .checked_mul(size)
                                .and_then(|n| cursor.checked_add(n))
                                .context("repeated record byte range overflows")?;
                            ensure!(end <= len, "expected {n} complete records of {size} bytes; file ends at {len} (partial or missing record)");
                            end
                        }
                        Repeat::Eof => len,
                        Repeat::UntilNext => {
                            let next = &self.sections[index + 1];
                            if let Some(at) = next.at {
                                ensure!(
                                    at >= cursor,
                                    "next section '{}' at {at} is backward or overlaps",
                                    next.name
                                );
                                ensure!(
                                    at <= len,
                                    "next section '{}' boundary {at} is beyond EOF {len}",
                                    next.name
                                );
                                at
                            } else {
                                let mut at = cursor;
                                loop {
                                    if matches(at, &next.marker) {
                                        break at;
                                    }
                                    if (at - cursor) / size % 4096 == 0 {
                                        progress.advance(at as u64);
                                        progress.check()?;
                                    }
                                    ensure!(
                                        at < len,
                                        "marker for next section '{}' missing before EOF",
                                        next.name
                                    );
                                    let end =
                                        at.checked_add(size).context("record end overflows")?;
                                    ensure!(end <= len, "partial record at byte {at}; marker for next section '{}' missing before EOF", next.name);
                                    at = end;
                                }
                            }
                        }
                    };
                    ensure!(
                        (end - cursor) % size == 0,
                        "boundary at byte {end} splits a {size}-byte record ({} trailing bytes)",
                        (end - cursor) % size
                    );
                    record.records = (end - cursor) / size;
                    ensure!(
                        record.records as u128 <= (1u128 << 53),
                        "record count exceeds 2^53; record indices would lose plot precision"
                    );
                    cursor = end;
                }
                progress.advance(cursor as u64);
                Ok(ResolvedSection {
                    physical: start..cursor,
                    marker: start..data_start,
                    opaque: section.opaque_bytes.is_some(),
                    record: Arc::new(record),
                })
            })();
            sections.push(resolved.with_context(|| format!("section '{}'", section.name))?);
        }
        progress.check()?;
        Ok(ResolvedLayout {
            sections,
            gaps,
            trailing: cursor..len,
            legacy: self.legacy,
        })
    }
}

fn parse_marker(text: &str) -> Result<Vec<u8>> {
    let mut marker = Vec::new();
    for token in text.split_whitespace() {
        ensure!(
            token.len() == 2 && token.bytes().all(|b| b.is_ascii_hexdigit()),
            "malformed marker: use space-separated hex bytes, for example 'DE AD BE EF'"
        );
        marker.push(u8::from_str_radix(token, 16)?);
    }
    ensure!(!marker.is_empty(), "marker cannot be empty");
    Ok(marker)
}
