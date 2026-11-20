//extracts the needed fields from a json based on the policy

use std::collections::{BTreeMap, BTreeSet};

use core_index::document::{
    IndexPolicy,
    policy::{FieldKind, FieldPolicy},
};
use core_protocol::errors::FailReason;
use serde::{Deserialize, Serialize};
use simd_json::OwnedValue;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LeafData {
    Text(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
}

impl LeafData {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            LeafData::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn to_text(&self) -> String {
        match self {
            LeafData::Text(s) => s.clone(),
            LeafData::Integer(i) => i.to_string(),
            LeafData::Float(f) => f.to_string(),
            LeafData::Bool(b) => b.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeafValue {
    pub path: String,
    pub data: LeafData,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArrayField {
    pub path: String,
    pub elements: Vec<ParsedNode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ParsedNode {
    pub leaves: Vec<LeafValue>,
    pub arrays: Vec<ArrayField>,
}

//to get the new fields from the parsed document
impl ParsedNode {
    pub fn collect_leaf_values(&self, out: &mut BTreeMap<String, String>) {
        for leaf in &self.leaves {
            out.entry(leaf.path.clone())
                .or_insert_with(|| leaf.data.to_text());
        }
        for array_field in &self.arrays {
            for element in &array_field.elements {
                element.collect_leaf_values(out);
            }
        }
    }
    pub fn collect_field_names<'a>(&'a self, out: &mut BTreeSet<&'a str>) {
        for leaf in &self.leaves {
            out.insert(leaf.path.as_str());
        }
        for array_field in &self.arrays {
            for element in &array_field.elements {
                element.collect_field_names(out);
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

        if field.kind == FieldKind::Struct {
            //this array of nested objects?
            if field.repeated {
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
            } else {
                //single object
                let OwnedValue::Object(_) = value else {
                    continue;
                };
                walk_fields(value, &field.subfields, node)?;
            }

            //this is an array with string/num/bool
        } else if field.repeated {
            let OwnedValue::Array(elements) = value else {
                continue;
            };
            let mut array_field = ArrayField {
                path: field.full_path.clone(),
                elements: Vec::new(),
            };
            for element in elements.iter() {
                let Some(data) = leaf_data(element) else {
                    continue;
                };
                validate_leaf(field, element)?;
                let mut child = ParsedNode::default();
                child.leaves.push(LeafValue {
                    path: field.full_path.clone(),
                    data,
                });
                array_field.elements.push(child);
            }
            node.arrays.push(array_field);
        } else {
            let Some(data) = leaf_data(value) else {
                continue;
            };
            validate_leaf(field, value)?;
            node.leaves.push(LeafValue {
                path: field.full_path.clone(),
                data,
            });
        }
    }
    Ok(())
}

pub fn validate_leaf(field: &FieldPolicy, value: &OwnedValue) -> Result<(), FailReason> {
    use simd_json::StaticNode::{Bool as JsonBool, F64, I64, U64};

    let already_valid = match (field.kind, value) {
        (FieldKind::Integer | FieldKind::Float, OwnedValue::Static(I64(_) | U64(_))) => true,
        (FieldKind::Float, OwnedValue::Static(F64(_))) => true,
        (FieldKind::Bool, OwnedValue::Static(JsonBool(_))) => true,
        _ => false,
    };
    if already_valid {
        return Ok(());
    }
    let Some(raw) = leaf_text(value) else {
        return Ok(()); // null
    };
    if raw.trim().is_empty() {
        return Ok(());
    }
    field
        .kind
        .validate_value(&raw)
        .map_err(|_| FailReason::InvalidField {
            field: field.name.clone(),
            expected: field.kind.label().to_string(),
            got: raw,
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

pub fn leaf_text(value: &OwnedValue) -> Option<String> {
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

fn leaf_data(value: &OwnedValue) -> Option<LeafData> {
    use simd_json::StaticNode;
    match value {
        OwnedValue::String(s) => Some(LeafData::Text(s.clone())),
        OwnedValue::Static(StaticNode::Null) => None,
        OwnedValue::Static(StaticNode::Bool(b)) => Some(LeafData::Bool(*b)),
        OwnedValue::Static(StaticNode::I64(i)) => Some(LeafData::Integer(*i)),
        //u64 beyond i64::MAX stays text so the numeric values drops it, stays searchable if
        //$searchable:true
        OwnedValue::Static(StaticNode::U64(u)) => i64::try_from(*u)
            .map(LeafData::Integer)
            .ok()
            .or_else(|| Some(LeafData::Text(u.to_string()))),
        OwnedValue::Static(StaticNode::F64(f)) => Some(LeafData::Float(*f)),
        OwnedValue::Object(_) | OwnedValue::Array(_) => Some(LeafData::Text(value.to_string())),
    }
}
