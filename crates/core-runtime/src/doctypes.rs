use core_index::document::{IndexPolicy, policy::IndexKind};
use core_protocol::{errors::CorelamoError, format::Format};
use core_storage::{
    document_store::{ExternalDocId, StoredDocument},
    search_database::DocumentInput,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub trait DocumentConversion {
    fn into_document_inputs(
        self,
        policy: &IndexPolicy,
    ) -> Result<Vec<DocumentInput>, CorelamoError>;
}

pub struct Json<'a>(pub &'a str);

//pub struct Xml<'a>(pub &'a str);

pub fn parse_documents(
    body: &str,
    format: Format,
    policy: &IndexPolicy,
) -> Result<Vec<DocumentInput>, CorelamoError> {
    match format {
        Format::JSON => Json(body).into_document_inputs(policy),
        //Format::XML => todo!(), //Xml(body).into_document_inputs(policy),
    }
}

//INFO: a little porno to tell if we found the id and if not then if its auto
fn extract_external_id(
    fields: &BTreeMap<String, String>,
    policy: &IndexPolicy,
) -> Result<String, CorelamoError> {
    let Some(id_field) = policy.id_field() else {
        return Err(CorelamoError::InvalidData(
            "policy has no id field declared".to_string(),
        ));
    };
    match fields.get(&id_field.name) {
        Some(v) if !v.is_empty() => Ok(v.clone()),
        _ if id_field.index == IndexKind::IdAutoIncrement => Ok(String::new()),
        _ => Err(CorelamoError::InvalidData(format!(
            "missing '{}' field in document (auto_increment is off)",
            id_field.name
        ))),
    }
}

impl<'a> DocumentConversion for Json<'a> {
    fn into_document_inputs(
        self,
        policy: &IndexPolicy,
    ) -> Result<Vec<DocumentInput>, CorelamoError> {
        let value: Value = serde_json::from_str(self.0).map_err(CorelamoError::from)?;
        match value {
            Value::Array(arr) => arr
                .into_iter()
                .map(|v| json_value_to_document_input(v, policy))
                .collect(),
            single => Ok(vec![json_value_to_document_input(single, policy)?]),
        }
    }
}

fn json_value_to_document_input(
    value: Value,
    policy: &IndexPolicy,
) -> Result<DocumentInput, CorelamoError> {
    let source = serde_json::to_vec(&value).map_err(CorelamoError::from)?;

    let mut fields = BTreeMap::new();
    traverse_json(&value, "", &mut fields);

    let external_id = extract_external_id(&fields, policy)?;

    Ok(DocumentInput {
        external_id,
        fields,
        source,
        format: Format::JSON,
    })
}

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
                .map(value_to_string)
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

//retrieve byte-for-byte response + skipped for format
pub fn convert_from_storage(
    docs: &[StoredDocument],
    format: Format,
) -> (Vec<Vec<u8>>, Vec<ExternalDocId>) {
    let mut output = Vec::with_capacity(docs.len());
    let mut skipped = Vec::new();
    for doc in docs {
        if doc.format == format {
            output.push(doc.source.clone());
        } else {
            skipped.push(doc.external_id.clone());
        }
    }
    (output, skipped)
}

//polocy is just toml so no worries
pub fn serialize_policy(policy: &IndexPolicy) -> Result<String, CorelamoError> {
    toml::to_string_pretty(policy).map_err(CorelamoError::from)
}

pub fn parse_policy(body: &str) -> Result<IndexPolicy, CorelamoError> {
    toml::from_str(body).map_err(CorelamoError::from)
}
