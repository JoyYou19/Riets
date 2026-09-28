use core_index::array_rows::ArrayRowAllocator;
use core_index::document::document::{ArrayRow, NumericPoint};
use core_index::document::{DocumentPart, IndexPolicy, IndexedDocument, policy::FieldKind};
use core_index::numeric_values::{parse_float, parse_integer};
use core_index::types::{ArrayRowId, DocId};

use crate::json_parse::{ArrayField, LeafValue, ParsedNode};

pub fn index_document(
    doc_id: DocId,
    parsed: &ParsedNode,
    policy: &IndexPolicy,
    allocator: &mut ArrayRowAllocator,
) -> IndexedDocument {
    let mut indexed = IndexedDocument::new(doc_id);
    index_leaves(
        &parsed.leaves,
        &mut indexed.parts,
        &mut indexed.numeric_points,
        policy,
    );
    index_arrays(
        &parsed.arrays,
        doc_id,
        None,
        &mut indexed.array_rows,
        policy,
        allocator,
    );
    indexed
}

fn index_arrays(
    arrays: &[ArrayField],
    doc_id: DocId,
    parent_row: Option<ArrayRowId>,
    rows: &mut Vec<ArrayRow>,
    policy: &IndexPolicy,
    allocator: &mut ArrayRowAllocator,
) {
    for array_field in arrays {
        for element in &array_field.elements {
            let row_id = allocator.alloc();

            let mut row = ArrayRow::new(row_id, parent_row);
            index_leaves(
                &element.leaves,
                &mut row.parts,
                &mut row.numeric_points,
                policy,
            );
            index_arrays(
                &element.arrays,
                doc_id,
                Some(row_id),
                rows,
                policy,
                allocator,
            );
            rows.push(row);
        }
    }
}

fn index_leaves(
    leaves: &[LeafValue],
    parts: &mut Vec<DocumentPart>,
    numeric: &mut Vec<NumericPoint>,
    policy: &IndexPolicy,
) {
    for leaf in leaves {
        let Some(field) = policy.leaf_by_path(&leaf.path) else {
            continue;
        };
        let raw = &leaf.value;
        match field.kind {
            FieldKind::Text => {
                parts.push(DocumentPart {
                    xpath: field.xpath(policy),
                    text: raw.clone(),
                    weight: field.weight,
                    exact: false,
                });
                if let Some(exact_xpath) = field.exact_xpath(policy) {
                    parts.push(DocumentPart {
                        xpath: exact_xpath,
                        text: raw.clone(),
                        weight: field.weight,
                        exact: true,
                    });
                }
            }
            FieldKind::Id => {
                parts.push(DocumentPart {
                    xpath: field.xpath(policy),
                    text: raw.clone(),
                    weight: field.weight,
                    exact: true,
                });
            }
            FieldKind::Integer => {
                if let Some(value) = parse_integer(raw) {
                    numeric.push(NumericPoint {
                        xpath: field.xpath(policy),
                        value,
                    });
                    if field.searchable() {
                        parts.push(DocumentPart {
                            xpath: field.xpath(policy),
                            text: raw.to_string(),
                            weight: field.weight,
                            exact: true,
                        });
                    }
                }
            }
            FieldKind::Float => {
                if let Some(value) = parse_float(raw) {
                    numeric.push(NumericPoint {
                        xpath: field.xpath(policy),
                        value,
                    });
                    if field.searchable() {
                        parts.push(DocumentPart {
                            xpath: field.xpath(policy),
                            text: raw.to_string(),
                            weight: field.weight,
                            exact: true,
                        });
                    }
                }
            }
            _ => {}
        }
    }
}
