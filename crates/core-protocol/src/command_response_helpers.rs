use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

use crate::errors::CorelamoError;

//FieldNode builds a nested structure from a flat BTreeMap<path, value>, rendered
//to json/xml by the response layer. Built once, consumed once.
pub enum FieldNode {
    Leaf(String),
    Branch(IndexMap<String, FieldNode>),
}

pub fn unflatten(fields: BTreeMap<String, String>) -> Result<FieldNode, CorelamoError> {
    let mut root = IndexMap::new();
    for (path, value) in fields {
        let parts: Vec<&str> = path.split('/').collect();
        insert_path(&mut root, &parts, value)?;
    }
    Ok(FieldNode::Branch(root))
}

fn insert_path(
    branch: &mut IndexMap<String, FieldNode>,
    parts: &[&str],
    value: String,
) -> Result<(), CorelamoError> {
    let (head, tail) = match parts.split_first() {
        Some(split) => split,
        None => return Ok(()),
    };

    if tail.is_empty() {
        match branch.get(*head) {
            //FIX: {a:"name", a:{b:"text"}} collides — "a" is both value and container.
            //possible fix: dont nest the response, keep flat "a":name "a/b":"text"
            Some(FieldNode::Branch(_)) => {
                return Err(CorelamoError::Internal(format!(
                    "field path collision: '{head}' is both a value and a container"
                )));
            }
            _ => {
                branch.insert(head.to_string(), FieldNode::Leaf(value));
            }
        }
        return Ok(());
    }

    let entry = branch
        .entry(head.to_string())
        .or_insert_with(|| FieldNode::Branch(IndexMap::new()));

    match entry {
        FieldNode::Branch(inner) => insert_path(inner, tail, value),
        //FIX: same collision, other direction
        FieldNode::Leaf(_) => Err(CorelamoError::Internal(format!(
            "field path collision: '{head}' is both a value and a container"
        ))),
    }
}

//json from field node
// TODO: hardcoded "id" key
pub fn tree_to_json(node: &FieldNode) -> Value {
    let obj = match node_to_json(node) {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    Value::Object(obj)
}

fn node_to_json(node: &FieldNode) -> Value {
    match node {
        FieldNode::Leaf(s) => Value::String(s.clone()),
        FieldNode::Branch(children) => {
            let mut obj = Map::new();
            for (k, child) in children {
                obj.insert(k.clone(), node_to_json(child));
            }
            Value::Object(obj)
        }
    }
}
