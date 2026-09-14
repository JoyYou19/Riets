//from abstract http text to our commands, useful for complex commands like search, retrieve....
use core_timing::timed;
use indexmap::IndexMap;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use simd_json::prelude::*;
use simd_json::{OwnedValue, json};
use std::collections::{BTreeMap, HashMap};
use strsim::levenshtein;

use crate::{
    command_response_helpers::{FieldNode, tree_to_json, unflatten},
    errors::{CorelamoError, DocFailure},
    format::Format,
};

//helper
pub fn parse_json_command<T: DeserializeOwned>(body: &str) -> Result<T, CorelamoError> {
    let mut bytes = body.as_bytes().to_vec();
    simd_json::from_slice(&mut bytes)
        .map_err(|e| CorelamoError::InvalidData(describe_parse_error(&e)))
}

//INFO: fancy hujna lai dabutu smuku error message aaraa
fn describe_parse_error(e: &simd_json::Error) -> String {
    if e.is_syntax() {
        // The document itself isn't valid JSON (bad comma/colon/brace/quote/etc.) -
        return match e.character() {
            Some(c) => format!(
                "Malformed JSON syntax at character {} (near '{c}'). Check for missing commas, colons, quotes, or braces.",
                e.index()
            ),
            None => format!(
                "Malformed JSON syntax at character {}. Check for missing commas, colons, quotes, or braces.",
                e.index()
            ),
        };
    }

    let msg = match e.error() {
        simd_json::ErrorType::Serde(msg) => msg.clone(),
        _ => e.to_string(),
    };

    format_unknown_field(&msg).unwrap_or(msg)
}

fn format_unknown_field(err_str: &str) -> Option<String> {
    if !err_str.contains("unknown field") {
        return None;
    }

    let field_start = err_str.find('`')?;
    let rest = &err_str[field_start + 1..];
    let field_end = rest.find('`')?;
    let bad_field = &rest[..field_end];

    if let Some(expected_idx) = err_str.find("expected one of ") {
        let expected_str = &err_str[expected_idx + 16..];
        let expected_fields: Vec<&str> = expected_str
            .split(',')
            .map(|s| s.trim().trim_matches('`'))
            .collect();

        if let Some(best_match) = expected_fields
            .iter()
            .min_by_key(|field| levenshtein(bad_field, field))
        {
            if levenshtein(bad_field, best_match) <= 3 {
                return Some(format!(
                    "Unknown field '{bad_field}'. Did you mean '{best_match}'?"
                ));
            }
        }
    }

    Some(format!("Unknown field '{bad_field}'."))
}

//trait Command -> all XXXCommand should have these properties
//functions with #derive Deserialize already have this unless you want to customize like in
//PartialReplace
pub trait Command: Sized + DeserializeOwned {
    fn from_json(body: &str) -> Result<Self, CorelamoError> {
        parse_json_command(body)
    }
    //fn from_xml(body: &str) -> Result<Self, CorelamoError>;

    #[timed(command_parsing)]
    fn parse(body: &str, format: Format) -> Result<Self, CorelamoError> {
        match format {
            Format::JSON => Self::from_json(body),
            //Format::XML => todo!(), //Self::from_xml(body),
        }
    }
}

pub trait ResponseData {
    fn to_json(&self) -> Result<OwnedValue, CorelamoError>;
    //fn to_xml(&self, w: &mut Writer<Cursor<Vec<u8>>>) -> Result<(), io::Error>;
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
//TODO: numbers exact-match
pub struct SearchCommand {
    pub query: QuerySpec,
    pub filters: Option<HashMap<String, FilterSpec>>,
    pub docs: Option<usize>,
    pub offset: Option<usize>,
    pub return_fields: Option<IndexMap<String, bool>>,
    pub sort: Option<IndexMap<String, SortSpec>>,
}

#[derive(Debug, Clone)]
pub enum QuerySpec {
    Plain(String),
    Exact { value: String, exact: bool },
}

use std::fmt;

impl fmt::Display for QuerySpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuerySpec::Plain(q) => write!(f, "{q}"),
            QuerySpec::Exact { value, .. } => write!(f, "{value}"),
        }
    }
}

//Hand made cuz this our favourite command that needs a lot of care
impl<'de> Deserialize<'de> for QuerySpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = OwnedValue::deserialize(deserializer)?;

        match value {
            OwnedValue::String(s) => Ok(QuerySpec::Plain(s)),
            OwnedValue::Object(obj) => {
                const EXPECTED: &[&str] = &["value", "exact"];

                for key in obj.keys() {
                    if !EXPECTED.contains(&key.as_str()) {
                        if let Some(best_match) =
                            EXPECTED.iter().min_by_key(|field| levenshtein(key, field))
                        {
                            if levenshtein(key, best_match) <= 3 {
                                return Err(serde::de::Error::custom(format!(
                                    "Unknown field '{key}' in query. Did you mean '{best_match}'?"
                                )));
                            }
                        }
                        return Err(serde::de::Error::custom(format!(
                            "Unknown field '{key}' in query. Expected 'value' and 'exact'."
                        )));
                    }
                }

                let query = obj
                    .get("value")
                    .and_then(OwnedValue::as_str)
                    .ok_or_else(|| {
                        serde::de::Error::custom(
                            "Object query form requires a string 'query' field",
                        )
                    })?
                    .to_string();

                let exact = obj
                    .get("exact")
                    .and_then(OwnedValue::as_bool)
                    .ok_or_else(|| {
                        serde::de::Error::custom(
                            "Object query form requires a boolean 'exact' field",
                        )
                    })?;

                Ok(QuerySpec::Exact {
                    value: query,
                    exact,
                })
            }
            other => Err(serde::de::Error::custom(format!(
                "query must be a string, or an object with 'value' and 'exact' fields (found {})",
                other.value_type()
            ))),
        }
    }
}

impl Command for SearchCommand {}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum FilterSpec {
    Plain(String),
    Exact { value: String, exact: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
#[serde(deny_unknown_fields)]
pub enum SortOrderRequest {
    Asc,
    #[default]
    Desc,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SortSpec {
    #[serde(default)]
    pub order: SortOrderRequest,
    pub ratio: Option<u8>,
}

pub struct SearchResponse {
    docs: Vec<(String, f32, FieldNode)>,
}

impl SearchResponse {
    pub fn from_hits(
        docs: Vec<(String, f32, BTreeMap<String, String>)>,
    ) -> Result<Self, CorelamoError> {
        let mut trees = Vec::with_capacity(docs.len());
        for (id, score, fields) in docs {
            trees.push((id, score, unflatten(fields)?));
        }
        Ok(Self { docs: trees })
    }
}

impl ResponseData for SearchResponse {
    fn to_json(&self) -> Result<OwnedValue, CorelamoError> {
        let items: Vec<OwnedValue> = self
            .docs
            .iter()
            .map(|(id, score, tree)| {
                json!({
                    "id": id,
                    "score": score,
                    "data": tree_to_json(tree)
                })
            })
            .collect();
        Ok(OwnedValue::Array(Box::new(items)))
    }

    // fn to_xml(&self, w: &mut Writer<Cursor<Vec<u8>>>) -> Result<(), io::Error> {}
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct RetrieveCommand {
    pub ids: Vec<String>,
}

impl Command for RetrieveCommand {}

pub struct RetrieveResponse {
    documents: Vec<(String, Vec<u8>)>,
    not_found: Vec<String>,
    skipped: Vec<String>,
}

impl RetrieveResponse {
    pub fn new(
        documents: Vec<(String, Vec<u8>)>,
        not_found: Vec<String>,
        skipped: Vec<String>,
    ) -> Self {
        Self {
            documents,
            not_found,
            skipped,
        }
    }
}

impl ResponseData for RetrieveResponse {
    fn to_json(&self) -> Result<OwnedValue, CorelamoError> {
        let docs = self
            .documents
            .iter()
            .map(|(id, bytes)| {
                let mut buf = bytes.clone();
                let data: OwnedValue = simd_json::from_slice(&mut buf).map_err(|e| {
                    CorelamoError::Internal(format!(
                        "stored document '{id}' is not valid JSON (corruption): {e}"
                    ))
                })?;

                Ok(simd_json::json!({
                    "id": id,
                    "data": data
                }))
            })
            .collect::<Result<Vec<OwnedValue>, CorelamoError>>()?;

        Ok(simd_json::json!({
            "documents": docs,
            "not_found": self.not_found,
            "skipped_ids": self.skipped,
        }))
    }
    // fn to_xml(&self, w: &mut Writer<Cursor<Vec<u8>>>) -> Result<(), io::Error> {
    //     todo!();
    // }
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct DeleteCommand {
    pub ids: Vec<String>,
}

impl Command for DeleteCommand {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookupCommand {
    pub ids: Vec<String>,
    pub return_fields: Option<IndexMap<String, bool>>,
}

impl Command for LookupCommand {}
pub struct LookupResponse {
    pub docs: Vec<(String, FieldNode)>,
    pub not_found: Vec<String>,
}

impl LookupResponse {
    pub fn from_hits(
        docs: Vec<(String, BTreeMap<String, String>)>,
        not_found: Vec<String>,
    ) -> Result<Self, CorelamoError> {
        let mut trees = Vec::with_capacity(docs.len());
        for (id, fields) in docs {
            trees.push((id, unflatten(fields)?));
        }
        Ok(Self {
            docs: trees,
            not_found,
        })
    }
}

impl ResponseData for LookupResponse {
    fn to_json(&self) -> Result<OwnedValue, CorelamoError> {
        let documents: Vec<OwnedValue> = self
            .docs
            .iter()
            .map(|(id, tree)| json!({ "id": id, "data": tree_to_json(tree) }))
            .collect();
        Ok(json!({
            "documents": documents,
            "not_found": self.not_found,
        }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetLogsRequest {
    pub date: Option<String>,
}

impl Command for GetLogsRequest {}

pub struct LoginResponse {
    pub token: String,
}
impl ResponseData for LoginResponse {
    fn to_json(&self) -> Result<OwnedValue, CorelamoError> {
        Ok(json!({"token":self.token}))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialReplaceItem {
    pub id: String,
    pub patch: simd_json::OwnedValue,
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct PartialReplaceCommand {
    pub items: Vec<PartialReplaceItem>,
}

pub struct ParsedPartialReplace {
    pub items: Vec<(String, BTreeMap<String, String>)>, // id  -> fields
    pub failures: Vec<DocFailure>,
}

impl Command for PartialReplaceCommand {
    fn from_json(body: &str) -> Result<Self, CorelamoError> {
        // Go through the shared fancy-error path first...
        let cmd: Self = parse_json_command(body)?;

        // ...then layer this command's own semantic validation on top.
        if cmd.items.is_empty() {
            return Err(CorelamoError::InvalidData(
                "partial-replace requires at least one document".into(),
            ));
        }

        Ok(cmd)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameDatabaseRequest {
    pub name: String,
}

impl Command for RenameDatabaseRequest {}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateDatabaseRequest {
    pub shard_count: Option<u16>,
}

impl Command for CreateDatabaseRequest {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimingsRequest {
    pub categories: Option<Vec<String>>,
    pub file: Option<String>,
}

impl Command for TimingsRequest {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InfoWordsRequest {
    pub words: Vec<String>,
}

impl Command for InfoWordsRequest {}
