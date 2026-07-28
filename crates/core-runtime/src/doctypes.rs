use core_index::document::{IndexPolicy, policy::IndexKind};
use core_protocol::{
    errors::{CorelamoError, DocFailure, FailReason},
    format::Format,
};
use core_storage::{
    document_store::{ExternalDocId, StoredDocument},
    search_database::DocumentInput,
};
use rayon::prelude::*;
use serde_json::{Value, value::RawValue};
use std::collections::BTreeMap;

pub trait DocumentConversion {
    fn into_document_inputs(self, policy: &IndexPolicy) -> Result<ParseOutcome, CorelamoError>;
}

// TODO: make this configurable
const PARALLEL_PARSE_THRESHOLD: usize = 10;

pub struct Json<'a> {
    pub body: &'a str,
}
//pub struct Xml<'a>(pub &'a str);

pub struct ParseOutcome {
    pub docs: Vec<DocumentInput>,
    //to save the original index of input unfiltered docs
    pub indices: Vec<usize>,
    pub failures: Vec<DocFailure>,
}
pub fn parse_documents(
    body: &str,
    format: Format,
    policy: &IndexPolicy,
) -> Result<ParseOutcome, CorelamoError> {
    match format {
        Format::JSON => Json { body }.into_document_inputs(policy),
    }
}

//INFO: a little porno to tell if we found the id and if not then if its auto
fn extract_external_id(
    fields: &BTreeMap<String, String>,
    policy: &IndexPolicy,
) -> Result<String, FailReason> {
    let Some(id_field) = policy.id_field() else {
        return Err(FailReason::NoIdField);
    };
    match fields.get(&id_field.name) {
        Some(v) if !v.is_empty() => Ok(v.clone()),
        _ if id_field.index == IndexKind::IdAutoIncrement => Ok(String::new()),
        _ => Err(FailReason::MissingId {
            field: id_field.name.clone(),
        }),
    }
}

impl<'a> DocumentConversion for Json<'a> {
    fn into_document_inputs(self, policy: &IndexPolicy) -> Result<ParseOutcome, CorelamoError> {
        //If an array was given
        if let Ok(raw_items) = serde_json::from_str::<Vec<&RawValue>>(self.body) {
            return Ok(parse_raw_items(&raw_items, policy));
        }

        //If a single document was given
        let value: Value = serde_json::from_str(self.body).map_err(CorelamoError::from)?;
        let mut docs = Vec::new();
        let mut indices = Vec::new();
        let mut failures = Vec::new();
        match json_value_to_document_input(value, policy) {
            Ok(doc) => {
                docs.push(doc);
                indices.push(0);
            }
            Err(reason) => failures.push(DocFailure::at(0, reason)),
        }
        Ok(ParseOutcome {
            docs,
            indices,
            failures,
        })
    }
}

fn parse_raw_items(raw_items: &[&RawValue], policy: &IndexPolicy) -> ParseOutcome {
    if raw_items.len() > PARALLEL_PARSE_THRESHOLD {
        //DATABASE LOG
        parse_raw_items_parallel(raw_items, policy)
    } else {
        //println!("sequential: ");
        parse_raw_items_sequential(raw_items, policy)
    }
}

fn parse_raw_items_sequential(raw_items: &[&RawValue], policy: &IndexPolicy) -> ParseOutcome {
    let mut docs = Vec::with_capacity(raw_items.len());
    let mut indices = Vec::with_capacity(raw_items.len());
    let mut failures = Vec::new();

    for (index, raw) in raw_items.iter().enumerate() {
        match parse_one(index, raw, policy) {
            Ok(doc) => {
                docs.push(doc);
                indices.push(index);
            }
            Err(failure) => failures.push(failure),
        }
    }
    ParseOutcome {
        docs,
        indices,
        failures,
    }
}

fn parse_raw_items_parallel(raw_items: &[&RawValue], policy: &IndexPolicy) -> ParseOutcome {
    let results: Vec<Result<DocumentInput, DocFailure>> = raw_items
        .par_iter()
        .enumerate()
        .map(|(index, raw)| parse_one(index, raw, policy))
        .collect();

    let mut docs = Vec::with_capacity(results.len());
    let mut indices = Vec::with_capacity(results.len());
    let mut failures = Vec::new();

    for (index, result) in results.into_iter().enumerate() {
        match result {
            Ok(doc) => {
                docs.push(doc);
                indices.push(index);
            }
            Err(failure) => failures.push(failure),
        }
    }

    ParseOutcome {
        docs,
        indices,
        failures,
    }
}

fn parse_one(
    index: usize,
    raw: &RawValue,
    policy: &IndexPolicy,
) -> Result<DocumentInput, DocFailure> {
    let value: Value = serde_json::from_str(raw.get())
        .map_err(|e| DocFailure::at(index, FailReason::InvalidJson(e.to_string())))?;

    let source = raw.get().as_bytes().to_vec();

    let mut fields = BTreeMap::new();
    traverse_json(&value, "", &mut fields);

    let external_id =
        extract_external_id(&fields, policy).map_err(|reason| DocFailure::at(index, reason))?;

    Ok(DocumentInput {
        external_id,
        fields,
        source,
        format: Format::JSON,
    })
}

fn json_value_to_document_input(
    value: Value,
    policy: &IndexPolicy,
) -> Result<DocumentInput, FailReason> {
    let source = serde_json::to_vec(&value).map_err(|e| FailReason::InvalidJson(e.to_string()))?;

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
