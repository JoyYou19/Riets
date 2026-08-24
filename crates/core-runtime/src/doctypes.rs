use core_index::document::{IndexPolicy, policy::IndexKind};
use core_protocol::{
    command_reponse_definitions::ParsedPartialReplace,
    command_response_helpers::{apply_merge_patch, traverse_json},
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
use uuid::Uuid;

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
        //INFO: we generate a random auto id for shard rounting if its auto shard_manager detects it
        //and inside the shard_db it gets a correct id
        _ if id_field.index == IndexKind::IdAuto => Ok(generate_routing_id(fields)),
        _ => Err(FailReason::MissingId {
            field: id_field.name.clone(),
        }),
    }
}

fn generate_routing_id(_fields: &BTreeMap<String, String>) -> String {
    Uuid::new_v4().simple().to_string()
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

//retrieve byte-for-byte response + skipped for format
pub fn convert_from_storage(
    docs: &[StoredDocument],
    format: Format,
) -> (Vec<(String, Vec<u8>)>, Vec<ExternalDocId>) {
    let mut output = Vec::with_capacity(docs.len());
    let mut skipped = Vec::new();
    for doc in docs {
        if doc.format == format {
            output.push((doc.external_id.clone(), doc.source.clone()));
        } else {
            skipped.push(doc.external_id.clone());
        }
    }
    (output, skipped)
}

pub fn parse_partial_replace_to_inputs(
    items: &[(String, serde_json::Value)],
    get_document: impl Fn(&str) -> Result<Option<StoredDocument>, CorelamoError>,
) -> Result<Vec<DocumentInput>, Vec<DocFailure>> {
    let mut inputs = Vec::with_capacity(items.len());
    let mut failures = Vec::new();

    for (index, (id, patch)) in items.iter().enumerate() {
        //get original
        let doc = match get_document(id) {
            Ok(Some(doc)) => doc,
            Ok(None) => {
                failures.push(DocFailure::new(
                    Some(index),
                    Some(id.clone()),
                    FailReason::NotFound,
                ));
                continue;
            }
            Err(e) => {
                failures.push(DocFailure::new(
                    Some(index),
                    Some(id.clone()),
                    FailReason::Internal(e.to_string()),
                ));
                continue;
            }
        };

        //parse
        let mut doc_value: serde_json::Value = match serde_json::from_slice(&doc.source) {
            Ok(v) => v,
            Err(e) => {
                failures.push(DocFailure::new(
                    Some(index),
                    Some(id.clone()),
                    FailReason::Internal(format!("stored document is not valid json: {e}")),
                ));
                continue;
            }
        };

        //partial-replace yoyo
        apply_merge_patch(&mut doc_value, patch);

        //get the fields
        let mut fields = BTreeMap::new();
        traverse_json(&doc_value, "", &mut fields);

        let source = match serde_json::to_vec(&doc_value) {
            Ok(v) => v,
            Err(e) => {
                failures.push(DocFailure::new(
                    Some(index),
                    Some(id.clone()),
                    FailReason::Internal(format!("failed to serialize patched document: {e}")),
                ));
                continue;
            }
        };

        inputs.push(DocumentInput {
            external_id: id.clone(),
            fields,
            source,
            format: Format::JSON,
        });
    }

    if failures.is_empty() {
        Ok(inputs)
    } else {
        Err(failures)
    }
}

//polocy is just toml so no worries
pub fn serialize_policy(policy: &IndexPolicy) -> Result<String, CorelamoError> {
    toml::to_string_pretty(policy).map_err(CorelamoError::from)
}

pub fn parse_policy(body: &str) -> Result<IndexPolicy, CorelamoError> {
    toml::from_str(body).map_err(CorelamoError::from)
}
