//from abstract http text to our commands, useful for complex commands like search, retrieve....
use core_protocol::{errors::CorelamoError, format::Format};
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

use crate::command_response_helpers::{FieldNode, tree_to_json, unflatten};

//trait Command -> all XXXCommand should have these properties
pub trait Command: Sized {
    fn from_json(body: &str) -> Result<Self, CorelamoError>;
    //fn from_xml(body: &str) -> Result<Self, CorelamoError>;

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
pub struct SearchCommand {
    pub query: String,
    pub filter: Option<HashMap<String, String>>, //TODO: pielikt filters lidzigi kaa elastic
    pub docs: Option<usize>,
    pub offset: Option<usize>,
    pub return_fields: Option<IndexMap<String, bool>>,
}

pub struct SearchResponse {
    docs: Vec<FieldNode>,
}

impl SearchResponse {
    pub fn from_hits(docs: Vec<(String, BTreeMap<String, String>)>) -> Result<Self, CorelamoError> {
        let mut trees = Vec::with_capacity(docs.len());
        for (_id, fields) in docs {
            trees.push(unflatten(fields)?);
        }
        Ok(Self { docs: trees })
    }
}

impl ResponseData for SearchResponse {
    fn to_json(&self) -> Result<Value, CorelamoError> {
        Ok(Value::Array(self.docs.iter().map(tree_to_json).collect()))
    }

    // fn to_xml(&self, w: &mut Writer<Cursor<Vec<u8>>>) -> Result<(), io::Error> {}
}

impl Command for SearchCommand {
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
    documents: Vec<Vec<u8>>,
    not_found: Vec<String>,
    skipped: Vec<String>,
}

impl RetrieveResponse {
    pub fn new(documents: Vec<Vec<u8>>, not_found: Vec<String>, skipped: Vec<String>) -> Self {
        Self {
            documents,
            not_found,
            skipped,
        }
    }
}

impl ResponseData for RetrieveResponse {
    fn to_json(&self) -> Result<Value, CorelamoError> {
        let mut docs = Vec::with_capacity(self.documents.len());
        for bytes in &self.documents {
            let v: Value = serde_json::from_slice(bytes).map_err(|e| {
                CorelamoError::Internal(format!(
                    "stored document is not valid JSON (corruption): {e}"
                ))
            })?;
            docs.push(v);
        }

        let mut obj = serde_json::Map::new();
        obj.insert("documents".to_string(), Value::Array(docs));
        obj.insert(
            "not_found".to_string(),
            Value::Array(self.not_found.iter().cloned().map(Value::String).collect()),
        );
        obj.insert(
            "skipped_ids".to_string(),
            Value::Array(self.skipped.iter().cloned().map(Value::String).collect()),
        );
        Ok(Value::Object(obj))
    }

    // fn to_xml(&self, w: &mut Writer<Cursor<Vec<u8>>>) -> Result<(), io::Error> {
    //     todo!();
    // }
}

#[derive(Debug)]
pub struct DeleteCommand {
    pub ids: Vec<String>,
}

impl Command for DeleteCommand {
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
