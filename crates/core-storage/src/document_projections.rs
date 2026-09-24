use std::io;
use std::sync::Arc;

use core_index::document::IndexPolicy;
use core_protocol::document_out::DocumentOut;
use indexmap::IndexMap;
use simd_json::OwnedValue;
use simd_json::owned::Object;

use crate::document_store::StoredDocument;

pub fn project_document(
    doc: &StoredDocument,
    policy: &IndexPolicy,
    return_fields: Option<&IndexMap<String, bool>>,
) -> io::Result<DocumentOut> {
    if return_fields.is_none() && !policy.has_hidden_fields() {
        return Ok(DocumentOut::Raw(Arc::clone(&doc.source)));
    }
    if doc.source.is_empty() {
        return Ok(DocumentOut::Json(OwnedValue::Object(Box::new(
            Object::new(),
        ))));
    }

    let mut buf = doc.source.to_vec();
    let mut value = simd_json::to_owned_value(&mut buf)
        .map_err(|e| io::Error::other(format!("stored document is not valid JSON: {e}")))?;

    prune(&mut value, "", return_fields, policy);
    Ok(DocumentOut::Json(value))
}

fn prune(
    value: &mut OwnedValue,
    parent: &str,
    return_fields: Option<&IndexMap<String, bool>>,
    policy: &IndexPolicy,
) {
    match value {
        OwnedValue::Object(obj) => {
            for (key, child) in obj.iter_mut() {
                let child_path = join_path(parent, key);
                prune(child, &child_path, return_fields, policy);
            }
            obj.retain(|key, _| {
                let child_path = join_path(parent, key);
                include_path(&child_path, return_fields, policy)
            });
        }
        OwnedValue::Array(items) => {
            for child in items.iter_mut() {
                prune(child, parent, return_fields, policy);
            }
        }
        _ => {}
    }
}

fn join_path(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_string()
    } else {
        let mut out = String::with_capacity(parent.len() + 1 + key.len());
        out.push_str(parent);
        out.push('/');
        out.push_str(key);
        out
    }
}

fn include_path(
    path: &str,
    return_fields: Option<&IndexMap<String, bool>>,
    policy: &IndexPolicy,
) -> bool {
    if let Some(rf) = return_fields {
        if let Some(&include) = lookup_up(rf, path) {
            return include;
        }
    }
    if let Some(list) = policy_list(policy, path) {
        return list;
    }
    true
}

fn lookup_up<'a>(map: &'a IndexMap<String, bool>, path: &str) -> Option<&'a bool> {
    let mut candidate = path;
    loop {
        if let Some(value) = map.get(candidate) {
            return Some(value);
        }
        match candidate.rfind('/') {
            Some(idx) => candidate = &candidate[..idx],
            None => return None,
        }
    }
}

fn policy_list(policy: &IndexPolicy, path: &str) -> Option<bool> {
    let mut candidate = path;
    loop {
        if let Some(field) = policy.fields.iter().find(|f| f.name == candidate) {
            return Some(field.list);
        }
        match candidate.rfind('/') {
            Some(idx) => candidate = &candidate[..idx],
            None => return None,
        }
    }
}

pub fn id_path_to_strip<'a>(
    id_field: Option<&'a str>,
    return_fields: Option<&IndexMap<String, bool>>,
) -> Option<&'a str> {
    let id = id_field?;
    let rf = return_fields?;
    if rf.get(id) == Some(&true) {
        None
    } else {
        Some(id)
    }
}
