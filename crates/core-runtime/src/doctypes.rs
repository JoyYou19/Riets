use core_index::document::IndexPolicy;
use core_storage::{
    document_store::StoredDocument,
    search_database::{DocumentInput, SearchDocumentHit},
};
use serde_json::Value;
use std::{collections::BTreeMap, io};

//TODO: ALL types for documents and policy
//trait for each filetype json/xml/toml....
//before adding more we should get the arrays + id field figured out i believe
pub trait DocumentConversion {
    fn into_document_inputs(self) -> io::Result<Vec<DocumentInput>>; //insert
    fn from_res_to_document(docs: Vec<SearchDocumentHit>) -> io::Result<String>; //search
    fn from_policy(policy: &IndexPolicy) -> io::Result<String>; //policy 
    fn into_policy(self) -> io::Result<IndexPolicy>; //policy
    fn stored_documents_to_values(docs: &[StoredDocument]) -> Vec<serde_json::Value>; //retrieve
}

//all posiible Filetype
pub struct Json<'a>(pub &'a str);
// pub struct Xml<'a>(pub &'a str);
// pub struct Toml<'a>(pub &'a str);

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

    fn from_policy(policy: &IndexPolicy) -> io::Result<String> {
        serde_json::to_string_pretty(policy).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    fn into_policy(self) -> io::Result<IndexPolicy> {
        serde_json::from_str(self.0).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid JSON policy: {e}"),
            )
        })
    }

    //FIX: the id wont always be "id" type shit
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

//determining filetype from string + convert
pub fn parse_documents(body: &str, file_type: &str) -> io::Result<Vec<DocumentInput>> {
    match file_type.to_lowercase().as_str() {
        "json" => Json(body).into_document_inputs(),
        // "xml" => Xml(body).into_document_inputs(),
        // "toml" => Toml(body).into_document_inputs(),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported file type: '{other}'"),
        )),
    }
}

//determining filetype + convert from results
pub fn serialize_hits(hits: Vec<SearchDocumentHit>, filetype: &str) -> io::Result<String> {
    match filetype.to_lowercase().as_str() {
        "json" => Json::from_res_to_document(hits),
        // "xml" => Xml::from_document_inputs(hits),
        // "toml" => Toml::from_document_inputs(hits),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported filetype: '{other}'"),
        )),
    }
}

//for retrieve convert from StoredDocument
pub fn convert_from_storage(docs: &[StoredDocument], filetype: &str) -> io::Result<String> {
    match filetype.to_lowercase().as_str() {
        "json" => {
            let values = Json::stored_documents_to_values(docs);
            serde_json::to_string_pretty(&values)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
        }
        // "xml" => ...
        // "toml" => ...
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported filetype: '{other}' — supported types: json"),
        )),
    }
}

//from policy
pub fn serialize_policy(policy: &IndexPolicy, filetype: &str) -> io::Result<String> {
    match filetype.to_lowercase().as_str() {
        "json" => Json::from_policy(policy),
        // "toml" => Toml::from_policy(policy),
        // "xml" => Xml::from_policy(policy),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported filetype: '{other}'"),
        )),
    }
}

//to policy
pub fn parse_policy(body: &str, filetype: &str) -> io::Result<IndexPolicy> {
    match filetype.to_lowercase().as_str() {
        "json" => Json(body).into_policy(),
        // "toml" => Toml(body).into_policy(),
        // "xml" => Xml(body).into_policy(),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported filetype: '{other}'"),
        )),
    }
}

/////////////////////////////////////////////////////////////////////////

//Specific helpers:
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
