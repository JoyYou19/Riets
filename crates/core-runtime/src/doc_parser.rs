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

impl DocumentConversion for Json {
    #[timed(json_parsing)]
    fn into_document_inputs(self, policy: &IndexPolicy) -> Result<ParseOutcome, CorelamoError> {
        let mut buf = self.body.into_bytes();

        // raw sources first: simd_json's parse below unescapes strings in place
        let sources = split_raw_documents(&buf)?;

        let value: Value = simd_json::to_owned_value(&mut buf).map_err(CorelamoError::from)?;
        drop(buf);

        if let Value::Array(items) = value {
            return Ok(parse_raw_items(&items, sources, policy));
        }
        let mut outcome = ParseOutcome {
            docs: Vec::new(),
            indices: Vec::new(),
            failures: Vec::new(),
        };
        let source = sources.into_iter().next().unwrap_or_default();
        match parse_one(0, &value, source, policy) {
            Ok(doc) => {
                outcome.docs.push(doc);
                outcome.indices.push(0);
            }
            Err(failure) => outcome.failures.push(failure),
        }
        Ok(outcome)
    }
}

fn parse_raw_items(items: &[Value], sources: Vec<Vec<u8>>, policy: &IndexPolicy) -> ParseOutcome {
    if items.len() > PARALLEL_PARSE_THRESHOLD {
        parse_raw_items_parallel(items, sources, policy)
    } else {
        parse_raw_items_sequential(items, sources, policy)
    }
}

fn parse_raw_items_sequential(
    items: &[Value],
    sources: Vec<Vec<u8>>,
    policy: &IndexPolicy,
) -> ParseOutcome {
    let mut docs = Vec::with_capacity(items.len());
    let mut indices = Vec::with_capacity(items.len());
    let mut failures = Vec::new();
    for (index, (raw, source)) in items.iter().zip(sources).enumerate() {
        match parse_one(index, raw, source, policy) {
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

fn parse_raw_items_parallel(
    items: &[Value],
    sources: Vec<Vec<u8>>,
    policy: &IndexPolicy,
) -> ParseOutcome {
    let results: Vec<Result<DocumentInput, DocFailure>> = items
        .par_iter()
        .zip(sources.into_par_iter())
        .enumerate()
        .map_init(
            || String::with_capacity(64),
            |path_buf, (index, (raw, source))| {
                path_buf.clear();
                parse_one(index, raw, source, policy)
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
    source: Vec<u8>,
    policy: &IndexPolicy,
) -> Result<DocumentInput, DocFailure> {
    let node = parse_into_node(value, policy).map_err(|r| DocFailure::at(index, r))?;
    let external_id = extract_external_id(&node, policy).map_err(|r| DocFailure::at(index, r))?;
    Ok(DocumentInput {
        external_id,
        parsed: node,
        source,
        format: Format::JSON,
    })
}

fn split_raw_documents(buf: &[u8]) -> Result<Vec<Vec<u8>>, CorelamoError> {
    use serde_json::value::RawValue;
    let mut out = Vec::new();
    let mut stream = serde_json::Deserializer::from_slice(buf).into_iter::<Box<RawValue>>();
    while let Some(raw) = stream.next() {
        let raw = raw.map_err(CorelamoError::from)?;
        out.push(raw.get().as_bytes().to_vec());
    }
    Ok(out)
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
