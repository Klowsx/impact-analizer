use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgumentType {
    Null,
    Boolean,
    Integer,
    Number,
    String,
    Object,
    Array,
}

impl ArgumentType {
    // Recognize schema labels whose types can be checked from argument evidence.
    fn from_schema_label(label: &str) -> Option<Self> {
        Some(match label {
            "null" => Self::Null,
            "boolean" => Self::Boolean,
            "integer" => Self::Integer,
            "number" => Self::Number,
            "string" => Self::String,
            "object" => Self::Object,
            "array" => Self::Array,
            _ => return None,
        })
    }

    // Accept an integer where the contract allows any number.
    fn matches(self, expected: Self) -> bool {
        self == expected || (self == Self::Integer && expected == Self::Number)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Argument {
    pub line: usize,
    pub types: Vec<ArgumentType>,
}

impl Argument {
    // Keep the type and source line without retaining the original value.
    pub fn new(line: usize, kind: Option<ArgumentType>) -> Self {
        Self {
            line,
            types: kind.into_iter().collect(),
        }
    }

    // Keep the scalar types this wire text can represent.
    pub fn wire_text(line: usize, value: &str) -> Self {
        let mut types = vec![ArgumentType::String];
        if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") {
            types.push(ArgumentType::Boolean);
        }
        let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
        if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
            types.push(ArgumentType::Integer);
        } else if value.parse::<f64>().is_ok_and(f64::is_finite) {
            types.push(ArgumentType::Number);
        }
        Self { line, types }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestSection {
    pub complete: bool,
    pub root: Option<Argument>,
    pub fields: BTreeMap<String, Argument>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequestEvidence {
    pub body: RequestSection,
    pub query: RequestSection,
    pub headers: RequestSection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgumentCompatibility {
    Compatible,
    Incompatible(usize),
    Unknown(Option<usize>),
}

impl RequestSection {
    // Record that the call omitted every argument in this section.
    pub fn absent() -> Self {
        Self {
            complete: true,
            ..Self::default()
        }
    }

    // Check a field or root body against the contract and return the best source line.
    pub fn compare(
        &self,
        field: Option<&str>,
        required: bool,
        expected: &str,
        call_line: usize,
    ) -> ArgumentCompatibility {
        let argument = if let Some(field) = field {
            self.fields.get(field)
        } else {
            self.root.as_ref()
        };
        if let Some(argument) = argument {
            if argument.types.is_empty() {
                return ArgumentCompatibility::Unknown(Some(argument.line));
            }
            let mut alternatives = expected.split(" | ").map(ArgumentType::from_schema_label);
            if alternatives.clone().flatten().any(|expected| {
                argument
                    .types
                    .iter()
                    .any(|observed| observed.matches(expected))
            }) {
                return ArgumentCompatibility::Compatible;
            }
            if alternatives.any(|expected| expected.is_none()) {
                return ArgumentCompatibility::Unknown(Some(argument.line));
            }
            return ArgumentCompatibility::Incompatible(argument.line);
        }
        if !self.complete || field.is_some_and(|path| self.unknown_parent(path)) {
            return ArgumentCompatibility::Unknown(self.root.as_ref().map(|arg| arg.line));
        }
        if required {
            ArgumentCompatibility::Incompatible(
                self.root.as_ref().map_or(call_line, |arg| arg.line),
            )
        } else {
            ArgumentCompatibility::Compatible
        }
    }

    // A present wire value may encode an array even when the evidence only shows a scalar.
    pub(crate) fn compare_query(
        &self,
        name: &str,
        required: bool,
        expected: &str,
        call_line: usize,
    ) -> ArgumentCompatibility {
        let result = self.compare(Some(name), required, expected, call_line);
        if let ArgumentCompatibility::Incompatible(line) = result
            && self.fields.contains_key(name)
            && expected.split(" | ").any(|label| label == "array")
        {
            return ArgumentCompatibility::Unknown(Some(line));
        }
        result
    }

    // Keep descendants of dynamic objects and array items unverified.
    fn unknown_parent(&self, path: &str) -> bool {
        path.contains("[]")
            || path.match_indices('.').any(|(index, _)| {
                self.fields
                    .get(&path[..index])
                    .is_some_and(|argument| argument.types.is_empty())
            })
    }

    // Merge URL query arguments and leave duplicate keys or opaque overrides unverified.
    pub fn add_url_query(&mut self, url: &str, line: usize) {
        let base = url.split('#').next().unwrap_or(url).trim();
        let path_and_query = base
            .split_once("://")
            .filter(|(scheme, _)| {
                scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
            })
            .map_or(base, |(_, rest)| {
                rest.find(['/', '?']).map_or("", |index| &rest[index..])
            });
        // URL resolution inserts colon-prefixed placeholders for dynamic substitutions.
        if path_and_query.match_indices(':').any(|(index, _)| {
            path_and_query
                .as_bytes()
                .get(index + 1)
                .is_some_and(|next| {
                    next.is_ascii_alphabetic() || matches!(next, b'_' | b'$') || !next.is_ascii()
                })
        }) {
            self.complete = false;
            self.fields.values_mut().for_each(|arg| arg.types.clear());
            return;
        }
        let Some((_, query)) = path_and_query.split_once('?') else {
            return;
        };
        let complete = self.complete;
        for part in query.split('&').filter(|part| !part.is_empty()) {
            let (name, value) = part.split_once('=').unwrap_or((part, ""));
            let Some(name) = decode_query_part(name) else {
                self.complete = false;
                continue;
            };
            let argument = if complete && let Some(value) = decode_query_part(value) {
                Argument::wire_text(line, &value)
            } else {
                Argument::new(line, None)
            };
            self.fields
                .entry(name)
                .and_modify(|previous| previous.types.clear())
                .or_insert(argument);
        }
    }
}

// Decode a form-encoded query component without keeping its raw text.
fn decode_query_part(part: &str) -> Option<String> {
    let mut bytes = part.bytes();
    let mut result = Vec::new();
    while let Some(byte) = bytes.next() {
        let decoded = match byte {
            b'+' => b' ',
            b'%' => {
                let high = char::from(bytes.next()?).to_digit(16)?;
                let low = char::from(bytes.next()?).to_digit(16)?;
                (high * 16 + low) as u8
            }
            _ => byte,
        };
        result.push(decoded);
    }
    String::from_utf8(result).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ArgumentCompatibility::{Compatible, Incompatible, Unknown};

    // Exercise positive, negative, and unverified results and their source lines.
    #[test]
    fn compares_known_missing_and_dynamic_fields() {
        let mut body = RequestSection::absent();
        body.root = Some(Argument::new(4, Some(ArgumentType::Object)));
        body.fields.insert(
            "count".into(),
            Argument::new(5, Some(ArgumentType::Integer)),
        );
        body.fields.insert("owner".into(), Argument::new(6, None));

        assert_eq!(body.compare(None, true, "object", 3), Compatible);
        assert_eq!(
            body.compare(Some("count"), true, "null | number", 3),
            Compatible
        );
        assert_eq!(
            body.compare(Some("count"), false, "boolean", 3),
            Incompatible(5)
        );
        assert_eq!(
            body.compare(Some("absent"), true, "string", 3),
            Incompatible(4)
        );
        assert_eq!(body.compare(Some("absent"), false, "string", 3), Compatible);
        assert_eq!(
            body.compare(Some("owner"), true, "string", 3),
            Unknown(Some(6))
        );
        assert_eq!(
            body.compare(Some("owner.id"), true, "string", 3),
            Unknown(Some(4))
        );
        assert_eq!(
            body.compare(Some("items[].id"), true, "string", 3),
            Unknown(Some(4))
        );
        assert_eq!(
            RequestSection::absent().compare(None, true, "object", 3),
            Incompatible(3)
        );
        assert_eq!(
            RequestSection::default().compare(Some("absent"), true, "string", 3),
            Unknown(None)
        );
    }

    // Keep unsupported schema alternatives unverified while retaining known matches.
    #[test]
    fn does_not_reject_objects_when_the_expected_type_is_unresolved() {
        let mut body = RequestSection::absent();
        body.root = Some(Argument::new(4, Some(ArgumentType::Object)));
        for label in [
            "unknown",
            "#/components/schemas/Payload",
            "enum",
            "string | enum",
        ] {
            assert_eq!(body.compare(None, true, label, 3), Unknown(Some(4)));
        }
        assert_eq!(body.compare(None, true, "enum | object", 3), Compatible);
        assert_eq!(body.compare(None, true, "string", 3), Incompatible(4));
    }

    // Distinguish ambiguous query array serialization from a missing or invalid scalar.
    #[test]
    fn keeps_serialized_query_arrays_unverified() {
        let mut query = RequestSection::absent();
        query.add_url_query("/search?tags=a", 4);
        for label in ["array", "array | null", "array<string>"] {
            assert_eq!(
                query.compare_query("tags", true, label, 4),
                Unknown(Some(4))
            );
        }
        assert_eq!(
            query.compare_query("missing", true, "array", 4),
            Incompatible(4)
        );
        assert_eq!(
            query.compare_query("tags", true, "integer", 4),
            Incompatible(4)
        );
    }

    // Recognize finite numeric and boolean wire text while preserving its string interpretation.
    #[test]
    fn classifies_wire_text_without_storing_the_value() {
        use ArgumentType::*;
        for (value, expected) in [
            ("TrUe", vec![String, Boolean]),
            ("-42", vec![String, Integer]),
            ("+3", vec![String, Integer]),
            ("-2.5e1", vec![String, Number]),
            ("", vec![String]),
            ("+", vec![String]),
            ("NaN", vec![String]),
            ("1e999", vec![String]),
        ] {
            assert_eq!(
                Argument::wire_text(7, value),
                Argument {
                    line: 7,
                    types: expected
                }
            );
        }
    }

    // Decode URL query keys, flag repeated keys, and preserve collision evidence lines.
    #[test]
    fn merges_url_query_without_assuming_the_winner() {
        let mut query = RequestSection::absent();
        query
            .fields
            .insert("limit".into(), Argument::wire_text(8, "2"));
        query.add_url_query(
            "http://api:8000/orders?limit=3&dupe=1&dupe=2&some+key=%2B8&bare&bad=%GG#hidden=4",
            4,
        );

        assert_eq!(
            query.compare(Some("limit"), true, "integer", 4),
            Unknown(Some(8))
        );
        assert_eq!(
            query.compare(Some("dupe"), true, "integer", 4),
            Unknown(Some(4))
        );
        assert_eq!(
            query.compare(Some("some key"), true, "integer", 4),
            Compatible
        );
        assert_eq!(query.compare(Some("bare"), true, "string", 4), Compatible);
        assert_eq!(
            query.compare(Some("bad"), true, "string", 4),
            Unknown(Some(4))
        );
        assert_eq!(
            query.compare(Some("hidden"), true, "string", 4),
            Incompatible(4)
        );

        let mut opaque = RequestSection::default();
        opaque.add_url_query("/orders?limit=2", 5);
        assert_eq!(
            opaque.compare(Some("limit"), true, "integer", 5),
            Unknown(Some(5))
        );

        let mut dynamic = RequestSection::absent();
        dynamic.add_url_query("/orders?limit=:value", 6);
        assert_eq!(
            dynamic.compare(Some("limit"), true, "integer", 6),
            Unknown(None)
        );

        let mut relative = RequestSection::absent();
        relative.add_url_query("/orders?redirect=http://example", 3);
        assert_eq!(
            relative.compare(Some("redirect"), true, "string", 3),
            Compatible
        );
    }
}
