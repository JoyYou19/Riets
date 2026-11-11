use core_index::document::{IndexPolicy, policy::FieldKind};
use core_protocol::{
    errors::{CorelamoError, DocFailure, FailReason},
    format::Format,
};
use core_storage::{
    document_store::{ExternalDocId, StoredDocument},
    json_parse::{ParsedNode, parse_into_node},
    search_database::DocumentInput,
};
use core_timing::timed;
use rayon::prelude::*;
use simd_json::OwnedValue as Value;
use uuid::Uuid;

pub(crate) trait DocumentConversion {
    fn into_document_inputs(self, policy: &IndexPolicy) -> Result<ParseOutcome, CorelamoError>;
}

// TODO: make this configurable
const PARALLEL_PARSE_THRESHOLD: usize = 10;

pub(crate) struct Json {
    pub body: String,
}
//pub struct Xml<'a>(pub &'a str);

pub(crate) struct ParseOutcome {
    pub docs: Vec<DocumentInput>,
    //to save the original index of input unfiltered docs
    pub indices: Vec<usize>,
    pub failures: Vec<DocFailure>,
}

#[timed(json_parsing)]
pub(crate) fn parse_documents(
    body: String,
    format: Format,
    policy: &IndexPolicy,
) -> Result<ParseOutcome, CorelamoError> {
    match format {
        Format::JSON => Json { body }.into_document_inputs(policy),
    }
}

//INFO: a little porno to tell if we found the id and if not then if its auto
#[timed(json_parsing)]
fn extract_external_id(node: &ParsedNode, policy: &IndexPolicy) -> Result<String, FailReason> {
    let Some(id_field) = policy.id_field() else {
        return Err(FailReason::NoIdField);
    };
    match node.leaves.iter().find(|l| l.path == id_field.full_path) {
        Some(l) if !l.value.is_empty() => Ok(l.value.clone()),
        _ if id_field.kind == FieldKind::IdAuto => Ok(generate_routing_id()),
        _ => Err(FailReason::MissingId {
            field: id_field.name.clone(),
        }),
    }
}

#[timed(json_parsing)]
fn generate_routing_id() -> String {
    Uuid::new_v4().simple().to_string()
}

impl DocumentConversion for Json{
    #[timed(json_parsing)]
    fn into_document_inputs(self, policy: &IndexPolicy) -> Result<ParseOutcome, CorelamoError> {
        // the body's own bytes are parsed in place: no copy, one parse
        let mut buf = self.body.into_bytes();
        let value: Value = simd_json::to_owned_value(&mut buf).map_err(CorelamoError::from)?;
        drop(buf); // the value tree owns everything it needs

        if let Value::Array(items) = value {
            return Ok(parse_raw_items(&items, policy));
        }
        let mut outcome = ParseOutcome { docs: Vec::new(), indices: Vec::new(), failures: Vec::new() };
        match parse_one(0, &value, policy) {
            Ok(doc) => {
                outcome.docs.push(doc);
                outcome.indices.push(0);
            }
            Err(failure) => outcome.failures.push(failure),
        }
        Ok(outcome)
    
    }
}

#[timed(json_parsing)]
fn parse_raw_items(items: &[Value], policy: &IndexPolicy) -> ParseOutcome {
    if items.len() > PARALLEL_PARSE_THRESHOLD {
        //DATABASE LOG
        parse_raw_items_parallel(items, policy)
    } else {
        //println!("sequential: ");
        parse_raw_items_sequential(items, policy)
    }
}

#[timed(json_parsing)]
fn parse_raw_items_sequential(raw_items: &[Value], policy: &IndexPolicy) -> ParseOutcome {
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

#[timed(json_parsing)]
fn parse_raw_items_parallel(raw_items: &[Value], policy: &IndexPolicy) -> ParseOutcome {
    let results: Vec<Result<DocumentInput, DocFailure>> = raw_items
        .par_iter()
        .enumerate()
        .map_init(
            || String::with_capacity(64),
            |path_buf, (index, raw)| {
                path_buf.clear();
                parse_one(index, raw, policy)
            },
        )
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

#[timed(json_parsing)]
fn parse_one(
    index: usize,
    value: &Value,
    policy: &IndexPolicy,
) -> Result<DocumentInput, DocFailure> {
    let source = simd_json::to_vec(value)
        .map_err(|e| DocFailure::at(index, FailReason::InvalidJson(e.to_string())))?;
    let node = parse_into_node(value, policy).map_err(|r| DocFailure::at(index, r))?;
    let external_id = extract_external_id(&node, policy).map_err(|r| DocFailure::at(index, r))?;
    Ok(DocumentInput {
        external_id,
        parsed: node,
        source,
        format: Format::JSON,
    })
}

//retrieve byte-for-byte response + skipped for format
#[timed(retrieve_opps)]
pub(crate) fn convert_from_storage(
    docs: &[StoredDocument],
    format: Format,
) -> (Vec<(String, Vec<u8>)>, Vec<ExternalDocId>) {
    let mut output = Vec::with_capacity(docs.len());
    let mut skipped = Vec::new();
    for doc in docs {
        if doc.format == format {
            output.push((doc.external_id.clone(), doc.source.to_vec()));
        } else {
            skipped.push(doc.external_id.clone());
        }
    }
    (output, skipped)
}

//polocy is just json so no worries
pub(crate) fn serialize_policy(policy: &IndexPolicy) -> Result<String, CorelamoError> {
    serde_json::to_string_pretty(policy).map_err(CorelamoError::from)
}

pub(crate) fn parse_policy(body: &str) -> Result<IndexPolicy, CorelamoError> {
    serde_json::from_str(body).map_err(CorelamoError::from)
}
