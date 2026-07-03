use core_core::errors::CorelamoError;
use core_index::document::IndexPolicy;
use core_storage::{
    document_store::StoredDocument,
    search_database::{DocumentInput, SearchDocumentHit},
};
use serde_json::Value;
use std::collections::BTreeMap;

pub trait DocumentConversion {
    fn into_document_inputs(self) -> Result<Vec<DocumentInput>, CorelamoError>;
    fn from_res_to_document(docs: Vec<SearchDocumentHit>) -> Vec<serde_json::Value>;
    fn stored_documents_to_values(docs: &[StoredDocument]) -> Vec<serde_json::Value>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    JSON,
    XML,
}

impl TryFrom<&str> for Format {
    type Error = CorelamoError;

    fn try_from(value: &str) -> Result<Self, CorelamoError> {
        match value.to_lowercase().as_str() {
            "json" => Ok(Format::JSON),
            "xml" => Ok(Format::XML),
            other => Err(CorelamoError::UnsupportedFormat(format!(
                "unsupported format: '{other}'"
            ))),
        }
    }
}

pub struct Json<'a>(pub &'a str);
// pub struct Xml<'a>(pub &'a str);

impl<'a> DocumentConversion for Json<'a> {
    fn into_document_inputs(self) -> Result<Vec<DocumentInput>, CorelamoError> {
        let value: Value = serde_json::from_str(self.0).map_err(CorelamoError::from)?;

        match value {
            Value::Array(arr) => arr
                .into_iter()
                .map(|v| json_value_to_document_input(v).map_err(|e| CorelamoError::InvalidData(e)))
                .collect(),
            single => Ok(vec![
                json_value_to_document_input(single).map_err(|e| CorelamoError::InvalidData(e))?,
            ]),
        }
    }

    fn from_res_to_document(docs: Vec<SearchDocumentHit>) -> Vec<serde_json::Value> {
        docs.into_iter()
            .map(|hit| {
                let mut obj = res_to_json(hit.fields);
                //FIX: the id wont always be named "id" type shit
                obj.insert("id".to_string(), serde_json::Value::String(hit.external_id));
                serde_json::Value::Object(obj)
            })
            .collect()
    }

    //FIX: the id wont always be named "id" type shit part 2
    fn stored_documents_to_values(docs: &[StoredDocument]) -> Vec<serde_json::Value> {
        docs.iter()
            .map(|doc| {
                let mut obj = res_to_json(doc.fields.clone());
                obj.insert(
                    "id".to_string(),
                    serde_json::Value::String(doc.external_id.clone()),
                );
                serde_json::Value::Object(obj)
            })
            .collect()
    }
}

//MAIN FUNCTIONS ///////////////////////////////////////////////////////////////////////

pub fn parse_documents(body: &str, format: Format) -> Result<Vec<DocumentInput>, CorelamoError> {
    match format {
        Format::JSON => Json(body).into_document_inputs(),
        Format::XML => Err(CorelamoError::UnsupportedFormat(
            "xml not yet implemented".to_string(),
        )),
    }
}

pub fn serialize_hits(
    hits: Vec<SearchDocumentHit>,
    format: Format,
) -> Result<Vec<serde_json::Value>, CorelamoError> {
    match format {
        Format::JSON => Ok(Json::from_res_to_document(hits)),
        Format::XML => Err(CorelamoError::UnsupportedFormat(
            "xml not yet implemented".to_string(),
        )),
    }
}

pub fn convert_from_storage(
    docs: &[StoredDocument],
    format: Format,
) -> Result<Vec<serde_json::Value>, CorelamoError> {
    match format {
        Format::JSON => Ok(Json::stored_documents_to_values(docs)),
        Format::XML => Err(CorelamoError::UnsupportedFormat(
            "xml not yet implemented".to_string(),
        )),
    }
}

/////////////////////////////////////////////////////////////////////////
// policy is always TOML — format is irrelevant here
pub fn serialize_policy(policy: &IndexPolicy) -> Result<String, CorelamoError> {
    toml::to_string_pretty(policy).map_err(CorelamoError::from)
}

pub fn parse_policy(body: &str) -> Result<IndexPolicy, CorelamoError> {
    toml::from_str(body).map_err(CorelamoError::from)
}

/////////////////////////////////////////////////////////////////////////

fn traverse_json(value: &Value, path: &str, fields: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            for (key, val) in map {
                let new_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{}/{}", path, key)
                };
                traverse_json(val, &new_path, fields);
            }
        }
        Value::Array(arr) => {
            // TODO: figure out best way to handle arrays, for now separate with " "
            let joined = arr
                .iter()
                .map(|v| value_to_string(v))
                .collect::<Vec<_>>()
                .join(" ");
            fields.insert(path.to_string(), joined);
        }
        other => {
            fields.insert(path.to_string(), value_to_string(other));
        }
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn res_to_json(fields: BTreeMap<String, String>) -> serde_json::Map<String, serde_json::Value> {
    let mut root = serde_json::Map::new();

    for (path, value) in fields {
        let parts: Vec<&str> = path.split('/').collect();
        let mut current = &mut root;

        for (i, part) in parts.iter().enumerate() {
            if i == parts.len() - 1 {
                current.insert(part.to_string(), serde_json::Value::String(value.clone()));
            } else {
                current = current
                    .entry(part.to_string())
                    .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                    .as_object_mut()
                    .unwrap();
            }
        }
    }

    root
}

fn json_value_to_document_input(value: Value) -> Result<DocumentInput, String> {
    let mut fields = BTreeMap::new();
    traverse_json(&value, "", &mut fields);

    // TODO: make id path configurable per-database via policy
    // HACK: hardcoded "id" field
    let external_id = fields
        .remove("id")
        .ok_or_else(|| "missing 'id' field in document".to_string())?;

    Ok(DocumentInput {
        external_id,
        fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_core::errors::CorelamoError;
    use core_storage::document_store::StoredDocument;
    use core_storage::search_database::SearchDocumentHit;
    use std::collections::BTreeMap;

    // ── Format::try_from ─────────────────────────────────────────────────────

    #[test]
    fn test_format_json() {
        assert_eq!(Format::try_from("json").unwrap(), Format::JSON);
    }

    #[test]
    fn test_format_json_uppercase() {
        assert_eq!(Format::try_from("JSON").unwrap(), Format::JSON);
    }

    #[test]
    fn test_format_xml() {
        assert_eq!(Format::try_from("xml").unwrap(), Format::XML);
    }

    #[test]
    fn test_format_unsupported_returns_error() {
        let e = Format::try_from("csv").unwrap_err();
        assert!(matches!(e, CorelamoError::UnsupportedFormat(_)));
    }

    #[test]
    fn test_format_empty_returns_error() {
        assert!(Format::try_from("").is_err());
    }

    // ── parse_documents ───────────────────────────────────────────────────────

    #[test]
    fn test_parse_single_document_ok() {
        let body = r#"{"id":"1","title":"hello","body":"world"}"#;
        let docs = parse_documents(body, Format::JSON).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].external_id, "1");
        assert_eq!(docs[0].fields.get("title").unwrap(), "hello");
    }

    #[test]
    fn test_parse_array_of_documents_ok() {
        let body = r#"[{"id":"1","title":"a"},{"id":"2","title":"b"}]"#;
        let docs = parse_documents(body, Format::JSON).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].external_id, "1");
        assert_eq!(docs[1].external_id, "2");
    }

    #[test]
    fn test_parse_nested_document_ok() {
        // nested fields should be flattened to path/key format
        let body = r#"{"id":"1","meta":{"author":"normunds","date":"2026"}}"#;
        let docs = parse_documents(body, Format::JSON).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].fields.get("meta/author").unwrap(), "normunds");
        assert_eq!(docs[0].fields.get("meta/date").unwrap(), "2026");
    }

    #[test]
    fn test_parse_missing_id_returns_invalid_data() {
        let body = r#"{"title":"no id here"}"#;
        let e = parse_documents(body, Format::JSON).unwrap_err();
        assert!(matches!(e, CorelamoError::InvalidData(_)));
    }

    #[test]
    fn test_parse_invalid_json_returns_invalid_data() {
        let e = parse_documents("not json", Format::JSON).unwrap_err();
        assert!(matches!(e, CorelamoError::InvalidData(_)));
    }

    #[test]
    fn test_parse_empty_array_returns_empty_vec() {
        let docs = parse_documents("[]", Format::JSON).unwrap();
        assert!(docs.is_empty());
    }

    #[test]
    fn test_parse_xml_returns_unsupported() {
        let e = parse_documents("<doc></doc>", Format::XML).unwrap_err();
        assert!(matches!(e, CorelamoError::UnsupportedFormat(_)));
    }

    // ── serialize_hits ────────────────────────────────────────────────────────

    #[test]
    fn test_serialize_hits_empty() {
        let result = serialize_hits(vec![], Format::JSON).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_serialize_hits_contains_id() {
        let hit = SearchDocumentHit {
            external_id: "doc1".to_string(),
            internal_id: 0,
            score: 1.0,
            fields: BTreeMap::new(),
        };
        let result = serialize_hits(vec![hit], Format::JSON).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], "doc1");
    }

    #[test]
    fn test_serialize_hits_fields_present() {
        let mut fields = BTreeMap::new();
        fields.insert("title".to_string(), "rust programming".to_string());
        fields.insert("body".to_string(), "systems language".to_string());

        let hit = SearchDocumentHit {
            external_id: "doc1".to_string(),
            internal_id: 1,
            score: 0.95,
            fields,
        };
        let result = serialize_hits(vec![hit], Format::JSON).unwrap();
        assert_eq!(result[0]["title"], "rust programming");
        assert_eq!(result[0]["body"], "systems language");
    }

    #[test]
    fn test_serialize_hits_multiple() {
        let hits = vec![
            SearchDocumentHit {
                external_id: "doc1".to_string(),
                internal_id: 0,
                score: 1.0,
                fields: BTreeMap::new(),
            },
            SearchDocumentHit {
                external_id: "doc2".to_string(),
                internal_id: 1,
                score: 0.5,
                fields: BTreeMap::new(),
            },
        ];
        let result = serialize_hits(hits, Format::JSON).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["id"], "doc1");
        assert_eq!(result[1]["id"], "doc2");
    }

    #[test]
    fn test_serialize_hits_xml_returns_unsupported() {
        let e = serialize_hits(vec![], Format::XML).unwrap_err();
        assert!(matches!(e, CorelamoError::UnsupportedFormat(_)));
    }

    // ── convert_from_storage ──────────────────────────────────────────────────

    #[test]
    fn test_convert_from_storage_empty() {
        let result = convert_from_storage(&[], Format::JSON).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_convert_from_storage_contains_id() {
        let doc = StoredDocument {
            external_id: "doc1".to_string(),
            internal_id: 0,
            fields: BTreeMap::new(),
        };
        let result = convert_from_storage(&[doc], Format::JSON).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], "doc1");
    }

    #[test]
    fn test_convert_from_storage_fields_present() {
        let mut fields = BTreeMap::new();
        fields.insert("title".to_string(), "hello world".to_string());

        let doc = StoredDocument {
            external_id: "doc1".to_string(),
            internal_id: 0,
            fields,
        };
        let result = convert_from_storage(&[doc], Format::JSON).unwrap();
        assert_eq!(result[0]["title"], "hello world");
    }

    #[test]
    fn test_convert_from_storage_multiple() {
        let docs = vec![
            StoredDocument {
                external_id: "doc1".to_string(),
                internal_id: 0,
                fields: BTreeMap::new(),
            },
            StoredDocument {
                external_id: "doc2".to_string(),
                internal_id: 1,
                fields: BTreeMap::new(),
            },
        ];
        let result = convert_from_storage(&docs, Format::JSON).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["id"], "doc1");
        assert_eq!(result[1]["id"], "doc2");
    }

    #[test]
    fn test_convert_from_storage_xml_returns_unsupported() {
        let e = convert_from_storage(&[], Format::XML).unwrap_err();
        assert!(matches!(e, CorelamoError::UnsupportedFormat(_)));
    }

    // ── parse_policy / serialize_policy ──────────────────────────────────────

    #[test]
    fn test_parse_policy_valid_toml() {
        let toml = r#"
[[fields]]
name     = "title"
xpath    = 1
index    = "Text"
stored   = true
stemming = "english"

[fields.weight]
min = 65
max = 90
"#;
        let policy = parse_policy(toml).unwrap();
        assert_eq!(policy.fields.len(), 1);
        assert_eq!(policy.fields[0].name, "title");
        assert_eq!(policy.fields[0].xpath, 1);
        assert_eq!(policy.fields[0].weight.min, 65);
        assert_eq!(policy.fields[0].weight.max, 90);
    }

    #[test]
    fn test_parse_policy_multiple_fields() {
        let toml = r#"
[[fields]]
name     = "title"
xpath    = 1
index    = "Text"
stored   = true
stemming = "english"

[fields.weight]
min = 65
max = 90

[[fields]]
name     = "body"
xpath    = 2
index    = "Text"
stored   = true
stemming = "english"

[fields.weight]
min = 1
max = 75
"#;
        let policy = parse_policy(toml).unwrap();
        assert_eq!(policy.fields.len(), 2);
        assert_eq!(policy.fields[1].name, "body");
    }

    #[test]
    fn test_parse_policy_invalid_toml_returns_invalid_data() {
        let e = parse_policy("this is ][ not toml").unwrap_err();
        assert!(matches!(e, CorelamoError::InvalidData(_)));
    }

    #[test]
    fn test_serialize_policy_round_trip() {
        let toml = r#"
[[fields]]
name     = "title"
xpath    = 1
index    = "Text"
stored   = true
stemming = "english"

[fields.weight]
min = 65
max = 90
"#;
        let policy = parse_policy(toml).unwrap();
        let serialized = serialize_policy(&policy).unwrap();
        let reparsed = parse_policy(&serialized).unwrap();
        assert_eq!(policy.fields[0].name, reparsed.fields[0].name);
        assert_eq!(policy.fields[0].xpath, reparsed.fields[0].xpath);
        assert_eq!(policy.fields[0].weight.min, reparsed.fields[0].weight.min);
        assert_eq!(policy.fields[0].weight.max, reparsed.fields[0].weight.max);
        assert_eq!(policy.fields[0].stored, reparsed.fields[0].stored);
    }

    #[test]
    fn test_serialize_policy_output_is_valid_toml() {
        let toml = r#"
[[fields]]
name     = "title"
xpath    = 1
index    = "Text"
stored   = true
stemming = "english"

[fields.weight]
min = 65
max = 90
"#;
        let policy = parse_policy(toml).unwrap();
        let serialized = serialize_policy(&policy).unwrap();
        // output must be parseable as raw TOML
        assert!(toml::from_str::<toml::Value>(&serialized).is_ok());
    }
}
