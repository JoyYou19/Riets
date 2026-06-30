use core_index::document::IndexPolicy;
use core_storage::{
    document_store::StoredDocument,
    search_database::{DocumentInput, SearchDocumentHit},
};
use serde_json::Value;
use std::{collections::BTreeMap, io};

//TODO: ALL types for documents and policy
//trait for each filetype json/xml...
//before adding more we should get the arrays + id field figured out i believe
pub trait DocumentConversion {
    fn into_document_inputs(self) -> io::Result<Vec<DocumentInput>>; //insert
    fn from_res_to_document(docs: Vec<SearchDocumentHit>) -> io::Result<String>; //search
    fn stored_documents_to_values(docs: &[StoredDocument]) -> Vec<serde_json::Value>; //retrieve
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    JSON,
    XML,
}

//helper for enum
impl TryFrom<&str> for Format {
    type Error = io::Error;

    fn try_from(value: &str) -> io::Result<Self> {
        match value.to_lowercase().as_str() {
            "json" => Ok(Format::JSON),
            "xml" => Ok(Format::XML),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported format: '{other}'"),
            )),
        }
    }
}

//all posiible Filetype
pub struct Json<'a>(pub &'a str);
// pub struct Xml<'a>(pub &'a str);

impl<'a> DocumentConversion for Json<'a> {
    fn into_document_inputs(self) -> io::Result<Vec<DocumentInput>> {
        let value: Value = serde_json::from_str(self.0).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("data is not valid json"),
            )
        })?;

        match value {
            Value::Array(arr) => arr
                .into_iter()
                .map(|v| {
                    json_value_to_document_input(v)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
                })
                .collect(),
            single => {
                Ok(vec![json_value_to_document_input(single).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, e)
                })?])
            }
        }
    }

    fn from_res_to_document(docs: Vec<SearchDocumentHit>) -> io::Result<String> {
        let results: Vec<serde_json::Value> = docs
            .into_iter()
            .map(|hit| {
                let mut obj = res_to_json(hit.fields);
                obj.insert("id".to_string(), serde_json::Value::String(hit.external_id));
                serde_json::Value::Object(obj)
            })
            .collect();

        serde_json::to_string_pretty(&results).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    //FIX: the id wont always be named "id" type shit
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

//MAIN FUNCTIONS ////////////////////////////////////////////////////////////////////////

//determining filetype from enum + convert
pub fn parse_documents(body: &str, format: Format) -> io::Result<Vec<DocumentInput>> {
    match format {
        Format::JSON => Json(body).into_document_inputs(),
        Format::XML => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported format: xml",
        )),
    }
}

//determining filetype + convert from results
pub fn serialize_hits(hits: Vec<SearchDocumentHit>, format: Format) -> io::Result<String> {
    match format {
        Format::JSON => Json::from_res_to_document(hits),
        Format::XML => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported format: xml",
        )),
    }
}

//for retrieve convert from StoredDocument
pub fn convert_from_storage(docs: &[StoredDocument], format: Format) -> io::Result<String> {
    match format {
        Format::JSON => {
            let values = Json::stored_documents_to_values(docs);
            serde_json::to_string_pretty(&values)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
        }
        Format::XML => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported format: xml",
        )),
    }
}

//policy is always toml, not tied to request Format
pub fn serialize_policy(policy: &IndexPolicy) -> io::Result<String> {
    toml::to_string_pretty(policy).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

pub fn parse_policy(body: &str) -> io::Result<IndexPolicy> {
    toml::from_str(body).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid TOML policy: {e}"),
        )
    })
}

/////////////////////////////////////////////////////////////////////////

//specific helpers
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
            // TODO: figure out best way to handle arrays for now seperate with " "
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
        //remove returs the removed
        .remove("id")
        .ok_or_else(|| "missing 'id' field in document".to_string())?;

    Ok(DocumentInput {
        external_id,
        fields,
    })
}
