use core_index::array_rows::ArrayRowAllocator;
use core_index::document::FieldPolicy;
use core_index::document::document::{ArrayRow, BoolPoint, NumericPoint};
use core_index::document::policy::ARRAY_GAP;
use core_index::document::{DocumentPart, IndexPolicy, IndexedDocument, policy::FieldKind};
use core_index::numeric_values::{parse_float, parse_integer};
use core_index::types::{ArrayRowId, DocId};

use crate::json_parse::{ArrayField, LeafValue, ParsedNode};

fn parse_bool(raw: &str) -> Option<bool> {
    //allows true false "true" "false"
    match raw.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

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
        &mut indexed.bool_points,
        policy,
    );
    index_arrays(
        &parsed.arrays,
        // doc_id,
        None,
        &mut indexed.parts,
        &mut indexed.array_rows,
        &mut indexed.bool_points,
        policy,
        allocator,
    );
    indexed
}

fn index_arrays(
    arrays: &[ArrayField],
    // doc_id: DocId,
    parent_row: Option<ArrayRowId>,
    parts: &mut Vec<DocumentPart>, 
    rows: &mut Vec<ArrayRow>,
    bool_points: &mut Vec<BoolPoint>,
    policy: &IndexPolicy,
    allocator: &mut ArrayRowAllocator,
) {
    for array_field in arrays {
        let Some(field) = policy.field_by_path(&array_field.path) else {
            continue;
        };

        // flat repeated text-like array -> flatten into `parts`
        if field.kind != FieldKind::Struct
            && matches!(
                field.kind,
                FieldKind::Text | FieldKind::Id | FieldKind::IdAuto
            )
        {
            flatten_scalar_array(array_field, field, parts, policy);
            continue;
        }

        // struct (or repeated numeric/bool) -> row-keyed, unchanged
        for element in &array_field.elements {
            let row_id = allocator.alloc();
            let mut row = ArrayRow::new(row_id, parent_row);
            index_leaves(
                &element.leaves,
                &mut row.parts,
                &mut row.numeric_points,
                &mut row.bool_points,
                policy,
            );
            index_arrays(
                &element.arrays,
                // doc_id,
                Some(row_id),
                &mut row.parts,
                rows,
                bool_points,
                policy,
                allocator,
            );
            rows.push(row);
        }
    }
}

fn flatten_scalar_array(
    array_field: &ArrayField,
    field: &FieldPolicy,
    parts: &mut Vec<DocumentPart>,
    policy: &IndexPolicy,
) {
    let segments: Vec<String> = array_field
        .elements
        .iter()
        .flat_map(|e| e.leaves.iter().map(|l| l.value.clone()))
        .collect();
    if segments.is_empty() {
        return;
    }

    // stemmed copy
    parts.push(DocumentPart::flattened(
        field.xpath(policy),
        segments.clone(),
        field.weight,
        false,
        ARRAY_GAP,
    ));
    // exact copy, if the field has an exact index
    if let Some(exact_xpath) = field.exact_xpath(policy) {
        parts.push(
            DocumentPart::flattened(exact_xpath, segments, field.weight, true, ARRAY_GAP)
                .with_names(field.kind == FieldKind::Text),
        );
    }
}

fn index_leaves(
    leaves: &[LeafValue],
    parts: &mut Vec<DocumentPart>,
    numeric: &mut Vec<NumericPoint>,
    bool_points: &mut Vec<BoolPoint>,
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
                    names: false,
                    gap: 0,
                    segments: Vec::new(),
                });
                if let Some(exact_xpath) = field.exact_xpath(policy) {
                    parts.push(DocumentPart {
                        xpath: exact_xpath,
                        text: raw.clone(),
                        weight: field.weight,
                        exact: true,
                        names: true,
                        gap: 0,
                        segments: Vec::new(),
                    });
                }
            }
            FieldKind::Id | FieldKind::IdAuto => {
                if field.searchable() {
                    // stemmed copy
                    parts.push(DocumentPart {
                        xpath: field.xpath(policy),
                        text: raw.clone(),
                        weight: field.weight,
                        exact: false,
                        names: false,
                        gap: 0,
                        segments: Vec::new(),
                    });
                    // exact copy
                    if let Some(exact_xpath) = field.exact_xpath(policy) {
                        parts.push(DocumentPart {
                            xpath: exact_xpath,
                            text: raw.clone(),
                            weight: field.weight,
                            exact: true,
                            names: false,
                            gap: 0,
                            segments: Vec::new(),
                        });
                    }
                }
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
                            names: false,
                            gap: 0,
                            segments: Vec::new(),
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
                            names: false,
                            gap: 0,
                            segments: Vec::new(),
                        });
                    }
                }
            }
            FieldKind::Bool => {
                if let Some(b) = parse_bool(raw) {
                    bool_points.push(BoolPoint {
                        xpath: field.xpath(policy),
                        value: b,
                    });
                }
            }
            _ => {}
        }
    }
}
