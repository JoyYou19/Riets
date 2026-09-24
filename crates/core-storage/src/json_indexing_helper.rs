//this is so that we can parse stored->indexed from the bytes
use std::collections::BTreeMap;
use std::io;

use core_index::document::{IndexPolicy, IndexedDocument, policy::FieldKind};
use core_index::numeric_values::{parse_float, parse_integer};
use core_index::types::DocId;
use core_protocol::command_response_helpers::traverse_json;

pub fn flatten_fields(source: &[u8]) -> io::Result<BTreeMap<String, String>> {
    if source.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut buf = source.to_vec();
    let value = simd_json::to_owned_value(&mut buf)
        .map_err(|e| io::Error::other(format!("failed to parse document source: {e}")))?;
    let mut fields = BTreeMap::new();
    traverse_json(&value, &mut String::new(), &mut fields);
    Ok(fields)
}

pub fn indexed_from_fields(
    doc_id: DocId,
    fields: &BTreeMap<String, String>,
    policy: &IndexPolicy,
) -> IndexedDocument {
    let mut indexed = IndexedDocument::new(doc_id);

    for field in policy.indexed_fields() {
        match field.kind {
            FieldKind::Text => {
                let Some(text) = fields.get(&field.name) else {
                    continue;
                };
                indexed = indexed.with_part(field.xpath(policy), text, field.weight);
                if let Some(exact_xpath) = field.exact_xpath(policy) {
                    indexed = indexed.with_exact(exact_xpath, text, field.weight);
                }
            }
            FieldKind::Id => {
                let Some(text) = fields.get(&field.name) else {
                    continue;
                };
                indexed = indexed.with_exact(field.xpath(policy), text, field.weight);
            }
            FieldKind::Integer => {
                let Some(raw) = fields.get(&field.name) else {
                    continue;
                };
                if let Some(value) = parse_integer(raw) {
                    indexed = indexed.with_numeric_point(field.xpath(policy), value);
                    if field.searchable() {
                        indexed = indexed.with_exact(field.xpath(policy), raw, field.weight);
                    }
                }
            }
            FieldKind::Float => {
                let Some(raw) = fields.get(&field.name) else {
                    continue;
                };
                if let Some(value) = parse_float(raw) {
                    indexed = indexed.with_numeric_point(field.xpath(policy), value);
                    if field.searchable() {
                        indexed = indexed.with_exact(field.xpath(policy), raw, field.weight);
                    }
                }
            }
            _ => {}
        }
    }

    indexed
}
