use core_timing::timed;
use simd_json::prelude::*;
use simd_json::{OwnedValue, StaticNode, owned::Object};
use std::collections::BTreeMap;

#[timed(json_parsing)]
pub fn traverse_json(value: &OwnedValue, path: &mut String, fields: &mut BTreeMap<String, String>) {
    match value {
        OwnedValue::Object(map) => {
            let base_len = path.len();
            for (key, val) in map.iter() {
                if !path.is_empty() {
                    path.push('/');
                }
                path.push_str(key);
                traverse_json(val, path, fields);
                path.truncate(base_len);
            }
        }
        OwnedValue::Array(arr) => {
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

pub fn escape_json_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn value_to_string(value: &OwnedValue) -> String {
    match value {
        OwnedValue::String(s) => s.clone(),
        OwnedValue::Static(simd_json::StaticNode::Null) => String::new(),
        other => other.to_string(),
    }
}

#[timed(json_parsing)]
pub fn apply_merge_patch(target: &mut OwnedValue, patch: &OwnedValue) {
    match patch {
        OwnedValue::Object(patch_obj) => {
            if !target.is_object() {
                *target = OwnedValue::Object(Box::new(Object::new()));
            }
            if let OwnedValue::Object(target_obj) = target {
                for (key, patch_value) in patch_obj.iter() {
                    if patch_value.is_null() {
                        target_obj.remove(key);
                    } else {
                        let entry = target_obj
                            .entry(key.clone())
                            .or_insert(OwnedValue::Static(StaticNode::Null));
                        apply_merge_patch(entry, patch_value);
                    }
                }
            }
        }
        _ => {
            *target = patch.clone();
        }
    }
}
