//from abstract http text to our commands, useful for complex commands like search, retrieve....
use core_timing::timed;
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

use crate::{
    command_response_helpers::{FieldNode, tree_to_json, unflatten},
    errors::{CorelamoError, DocFailure},
    format::Format,
};

//trait Command -> all XXXCommand should have these properties
pub trait Command: Sized {
    fn from_json(body: &str) -> Result<Self, CorelamoError>;
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
    fn to_json(&self) -> Result<Value, CorelamoError>;
    //fn to_xml(&self, w: &mut Writer<Cursor<Vec<u8>>>) -> Result<(), io::Error>;
}

#[derive(Debug, Deserialize)]
//TODO: numbers exact-match
pub struct SearchCommand {
    pub query: String,
    pub filters: Option<HashMap<String, String>>,
    pub docs: Option<usize>,
    pub offset: Option<usize>,
    pub return_fields: Option<IndexMap<String, bool>>,
}

pub struct SearchResponse {
    docs: Vec<(String, FieldNode)>,
}

impl SearchResponse {
    pub fn from_hits(docs: Vec<(String, BTreeMap<String, String>)>) -> Result<Self, CorelamoError> {
        let mut trees = Vec::with_capacity(docs.len());
        for (id, fields) in docs {
            trees.push((id, unflatten(fields)?));
        }
        Ok(Self { docs: trees })
    }
}

impl ResponseData for SearchResponse {
    fn to_json(&self) -> Result<Value, CorelamoError> {
        Ok(Value::Array(
            self.docs
                .iter()
                .map(|(id, tree)| {
                    json!({
                        "id": id,
                        "data": tree_to_json(tree)
                    })
                })
                .collect(),
        ))
    }

    // fn to_xml(&self, w: &mut Writer<Cursor<Vec<u8>>>) -> Result<(), io::Error> {}
}

impl Command for SearchCommand {
    #[timed(command_parsing)]
    fn from_json(body: &str) -> Result<Self, CorelamoError> {
        serde_json::from_str(body).map_err(CorelamoError::from)
    }

    // fn from_xml(body: &str) -> Result<Self, CorelamoError> {
    //     todo!()
    // }
}

#[derive(Debug)]
pub struct RetrieveCommand {
    pub ids: Vec<String>,
}

impl Command for RetrieveCommand {
    // TODO: accept more shapes later, e.g. {"ids": [...]}
    #[timed(command_parsing)]
    fn from_json(body: &str) -> Result<Self, CorelamoError> {
        let ids: Vec<String> = serde_json::from_str(body)
            .map_err(|_| CorelamoError::InvalidData("expected JSON array of ids".to_string()))?;
        Ok(RetrieveCommand { ids })
    }

    // fn from_xml(body: &str) -> Result<Self, CorelamoError> {
    //     todo!();
    // }
}

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
    fn to_json(&self) -> Result<Value, CorelamoError> {
        let docs = self
            .documents
            .iter()
            .map(|(id, bytes)| {
                let data: Value = serde_json::from_slice(bytes).map_err(|e| {
                    CorelamoError::Internal(format!(
                        "stored document '{id}' is not valid JSON (corruption): {e}"
                    ))
                })?;

                Ok(serde_json::json!({
                    "id": id,
                    "data": data
                }))
            })
            .collect::<Result<Vec<Value>, CorelamoError>>()?;

        Ok(serde_json::json!({
            "documents": docs,
            "not_found": self.not_found,
            "skipped_ids": self.skipped,
        }))
    }
    // fn to_xml(&self, w: &mut Writer<Cursor<Vec<u8>>>) -> Result<(), io::Error> {
    //     todo!();
    // }
}

#[derive(Debug)]
pub struct DeleteCommand {
    pub ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct LookupCommand {
    pub ids: Vec<String>,
    pub return_fields: Option<IndexMap<String, bool>>,
}

impl Command for LookupCommand {
    #[timed(command_parsing)]
    fn from_json(body: &str) -> Result<Self, CorelamoError> {
        serde_json::from_str(body).map_err(CorelamoError::from)
    }
}

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
    fn to_json(&self) -> Result<Value, CorelamoError> {
        let documents: Vec<Value> = self
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
pub struct GetLogsRequest {
    pub date: Option<String>,
}

impl Command for DeleteCommand {
    #[timed(command_parsing)]
    fn from_json(body: &str) -> Result<Self, CorelamoError> {
        let ids: Vec<String> = serde_json::from_str(body)
            .map_err(|_| CorelamoError::InvalidData("expected JSON array of ids".to_string()))?;
        Ok(DeleteCommand { ids })
    }

    // fn from_xml(body: &str) -> Result<Self, CorelamoError> {
    //     todo!();
    // }
}

pub struct LoginResponse {
    pub token: String,
}
impl ResponseData for LoginResponse {
    fn to_json(&self) -> Result<Value, CorelamoError> {
        Ok(json!({"token":self.token}))
    }
}

#[derive(Debug, Deserialize)]
pub struct PartialReplaceItem {
    pub id: String,
    pub patch: serde_json::Value,
}

#[derive(Debug)]
pub struct PartialReplaceCommand {
    pub items: Vec<PartialReplaceItem>,
}

pub struct ParsedPartialReplace {
    pub items: Vec<(String, BTreeMap<String, String>)>, // id  -> fields
    pub failures: Vec<DocFailure>,
}

impl Command for PartialReplaceCommand {
    #[timed(command_parsing)]
    fn from_json(body: &str) -> Result<Self, CorelamoError> {
        let items: Vec<PartialReplaceItem> = serde_json::from_str(body).map_err(|e| {
            CorelamoError::InvalidData(format!("invalid partial-replace request: {e}"))
        })?;

        if items.is_empty() {
            return Err(CorelamoError::InvalidData(
                "partial-replace requires at least one document".into(),
            ));
        }

        Ok(PartialReplaceCommand { items })
    }
}

#[derive(Deserialize)]
pub struct RenameDatabaseRequest {
    pub name: String,
}

#[derive(Deserialize)]
pub struct TimingsRequest {
    pub categories: Option<Vec<String>>,
    pub file: Option<String>,
}
