use std::collections::BTreeMap;

use bincode::{Decode, Encode};
use core_index::document::{
    IndexPolicy,
    policy::{FieldKind, FieldPolicy},
};
use core_protocol::errors::FailReason;
use serde::{Deserialize, Serialize};
use simd_json::OwnedValue;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct LeafValue {
    pub path: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ArrayField {
    pub path: String,
    pub elements: Vec<ParsedNode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode, Default)]
pub struct ParsedNode {
    pub leaves: Vec<LeafValue>,
    pub arrays: Vec<ArrayField>,
}

//to get the new fields from the parsed document
impl ParsedNode {
    pub fn collect_leaf_values(&self, out: &mut BTreeMap<String, String>) {
        for leaf in &self.leaves {
            out.entry(leaf.path.clone())
                .or_insert_with(|| leaf.value.clone());
        }
        for array_field in &self.arrays {
            for element in &array_field.elements {
                element.collect_leaf_values(out);
            }
        }
    }
}

pub fn parse_into_node(value: &OwnedValue, policy: &IndexPolicy) -> Result<ParsedNode, FailReason> {
    let mut node = ParsedNode::default();
    if matches!(value, OwnedValue::Object(_)) {
        walk_fields(value, &policy.fields, &mut node)?;
    }
    Ok(node)
}

//parse raw bytes too reindex uses this
pub fn parse_source_into_node(
    source: &[u8],
    policy: &IndexPolicy,
) -> Result<ParsedNode, FailReason> {
    let mut buf = source.to_vec();
    let value: OwnedValue =
        simd_json::to_owned_value(&mut buf).map_err(|e| FailReason::InvalidJson(e.to_string()))?;
    parse_into_node(&value, policy)
}

fn walk_fields(
    obj: &OwnedValue,
    fields: &[FieldPolicy],
    node: &mut ParsedNode,
) -> Result<(), FailReason> {
    //go through poolicy fields
    for field in fields {
        //get this fields value in the given json
        let Some(value) = get_by_path(obj, &field.name) else {
            continue;
        };

        //this array of nested objects?
        if field.kind == FieldKind::Struct {
            //ensure valid array
            let OwnedValue::Array(elements) = value else {
                continue;
            };
            let mut array_field = ArrayField {
                path: field.full_path.clone(),
                elements: Vec::new(),
            };
            for element in elements.iter() {
                let OwnedValue::Object(_) = element else {
                    return Err(FailReason::InvalidField {
                        field: field.name.clone(),
                        expected: "object".to_string(),
                        got: value_type_name(element).to_string(),
                    });
                };
                let mut child = ParsedNode::default();
                walk_fields(element, &field.subfields, &mut child)?;
                array_field.elements.push(child);
            }
            node.arrays.push(array_field);

        //this is an array with string/num/bool
        } else if field.repeated {
            let OwnedValue::Array(elements) = value else {
                continue;
            };
            let mut array_field = ArrayField {
                path: field.full_path.clone(),
                elements: Vec::new(),
            };
            //poulate this array
            for element in elements.iter() {
                //validate the string/numver based on the poliy
                let Some(raw) = leaf_text(element) else {
                    continue;
                };
                validate_leaf(field, &raw)?;
                let mut child = ParsedNode::default();
                child.leaves.push(LeafValue {
                    path: field.full_path.clone(),
                    value: raw,
                });
                array_field.elements.push(child);
            }
            node.arrays.push(array_field);
        //this is a leaf value
        } else {
            let Some(raw) = leaf_text(value) else {
                continue;
            };
            validate_leaf(field, &raw)?;
            node.leaves.push(LeafValue {
                path: field.full_path.clone(),
                value: raw,
            });
        }
    }
    Ok(())
}

fn validate_leaf(field: &FieldPolicy, raw: &str) -> Result<(), FailReason> {
    if !field.kind.is_numeric() || raw.trim().is_empty() {
        return Ok(());
    }
    field
        .kind
        .validate_value(raw)
        .map_err(|_| FailReason::InvalidField {
            field: field.name.clone(),
            expected: field.kind.label().to_string(),
            got: raw.to_string(),
        })
}

fn get_by_path<'a>(mut value: &'a OwnedValue, path: &str) -> Option<&'a OwnedValue> {
    for segment in path.split('/') {
        let OwnedValue::Object(map) = value else {
            return None;
        };
        value = map.get(segment)?;
    }
    Some(value)
}

fn leaf_text(value: &OwnedValue) -> Option<String> {
    match value {
        OwnedValue::String(s) => Some(s.clone()),
        OwnedValue::Static(simd_json::StaticNode::Null) => None,
        OwnedValue::Object(_) | OwnedValue::Array(_) => Some(value.to_string()),
        other => Some(other.to_string()),
    }
}

fn value_type_name(value: &OwnedValue) -> &'static str {
    match value {
        OwnedValue::Static(node) => match node {
            simd_json::StaticNode::Null => "null",
            simd_json::StaticNode::Bool(_) => "bool",
            simd_json::StaticNode::I64(_)
            | simd_json::StaticNode::U64(_)
            | simd_json::StaticNode::F64(_) => "number",
        },
        OwnedValue::String(_) => "string",
        OwnedValue::Array(_) => "array",
        OwnedValue::Object(_) => "object",
        #[allow(unreachable_patterns)]
        _ => "unknown",
    }
}
