///parsed json tree -> indexed terms
use core_index::array_rows::ArrayRowAllocator;
use core_index::datetimes::parse_date;

use core_index::document::indexed::{ArrayRow, BoolPoint, NumericPoint};
use core_index::document::policy::{ARRAY_GAP, ExactMode, FieldPolicy};
use core_index::document::{DocumentPart, IndexPolicy, IndexedDocument, policy::FieldKind};
use core_index::numeric_values::{NumericValue, parse_float, parse_integer};
use core_index::types::{ArrayRowId, DocId};

use crate::json_parse::{ArrayField, LeafData, LeafValue, ParsedNode};

fn parse_bool(raw: &str) -> Option<bool> {
    //allows true false "true" "false"
    match raw.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}
/// The non-exact part for `field`: its stemmed xpath, no names, no gap.
fn plain_part(field: &FieldPolicy, policy: &IndexPolicy, text: &str) -> DocumentPart {
    DocumentPart {
        xpath: field.xpath(policy),
        text: text.to_owned(),
        weight: field.weight,
        exact_mode: None,
        names: false,
        analyzer: field.analyzer.clone(),
        gap: 0,
        segments: Vec::new(),
    }
}

/// The exact copy of `text`, when the field has an exact index.
fn exact_part(
    field: &FieldPolicy,
    policy: &IndexPolicy,
    text: &str,
    names: bool,
) -> Option<DocumentPart> {
    field.exact_xpath(policy).map(|xpath| DocumentPart {
        xpath,
        exact_mode: Some(field.exact.unwrap_or(ExactMode::CaseInsensitive)),
        names,
        ..plain_part(field, policy, text)
    })
}

fn index_text_leaf(
    field: &FieldPolicy,
    policy: &IndexPolicy,
    leaf: &LeafValue,
    parts: &mut Vec<DocumentPart>,
) {
    // text leaves always carry LeafData::Text
    let Some(text) = leaf.data.as_str() else {
        return;
    };
    parts.push(plain_part(field, policy, text));
    parts.extend(exact_part(field, policy, text, true));
}

fn index_id_leaf(
    field: &FieldPolicy,
    policy: &IndexPolicy,
    leaf: &LeafValue,
    parts: &mut Vec<DocumentPart>,
) {
    let Some(text) = leaf.data.as_str() else {
        return;
    };
    if field.searchable() {
        parts.push(plain_part(field, policy, text));
        parts.extend(exact_part(field, policy, text, false));
    }
}

/// Records a numeric point and, for searchable fields, its exact text form.
fn index_numeric_leaf(
    field: &FieldPolicy,
    policy: &IndexPolicy,
    leaf: &LeafValue,
    value: Option<NumericValue>,
    parts: &mut Vec<DocumentPart>,
    numeric: &mut Vec<NumericPoint>,
) {
    let Some(value) = value else {
        return;
    };
    numeric.push(NumericPoint {
        xpath: field.xpath(policy),
        value,
    });
    if field.searchable() {
        parts.extend(exact_part(field, policy, &leaf.data.to_text(), false));
    }
}
#[must_use]
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
        None,
        &mut indexed.parts,
        &mut indexed.array_rows,
        policy,
        allocator,
    );
    indexed
}

fn index_arrays(
    arrays: &[ArrayField],
    parent_row: Option<ArrayRowId>,
    parts: &mut Vec<DocumentPart>,
    rows: &mut Vec<ArrayRow>,
    policy: &IndexPolicy,
    allocator: &mut ArrayRowAllocator,
) {
    for array_field in arrays {
        let Some(field) = policy.field_by_path(&array_field.path) else {
            continue;
        };

        // flat repeated text-like array -> flatten into `parts`
        if matches!(
            field.kind,
            FieldKind::Text | FieldKind::Id | FieldKind::IdAuto
        ) {
            flatten_scalar_array(array_field, field, parts, policy);
            continue;
        }

        // struct (or repeated numeric/bool) -> row-keyed
        for (element_index, element) in array_field.elements.iter().enumerate() {
            let row_id = allocator.alloc();
            let position = u32::try_from(element_index).unwrap_or(u32::MAX);
            let mut row = ArrayRow::new(row_id, parent_row, position);
            index_leaves(
                &element.leaves,
                &mut row.parts,
                &mut row.numeric_points,
                &mut row.bool_points,
                policy,
            );
            index_arrays(
                &element.arrays,
                Some(row_id),
                &mut row.parts,
                rows,
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
        .flat_map(|e| {
            e.leaves
                .iter()
                .filter_map(|l| l.data.as_str().map(str::to_owned))
        })
        .collect();

    if segments.is_empty() {
        return;
    }

    // stemmed copy
    parts.push(DocumentPart::flattened(
        field.xpath(policy),
        segments.clone(),
        field.weight,
        None,
        ARRAY_GAP,
    ));
    // exact copy, if the field has an exact index
    if let Some(exact_xpath) = field.exact_xpath(policy) {
        parts.push(
            DocumentPart::flattened(
                exact_xpath,
                segments,
                field.weight,
                Some(field.exact.unwrap_or(ExactMode::CaseInsensitive)),
                ARRAY_GAP,
            )
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

        match field.kind {
            FieldKind::Text => index_text_leaf(field, policy, leaf, parts),
            FieldKind::Id | FieldKind::IdAuto => index_id_leaf(field, policy, leaf, parts),
            FieldKind::Integer => {
                let value = match &leaf.data {
                    LeafData::Integer(v) => Some(NumericValue::Int(*v)),
                    LeafData::Text(s) => parse_integer(s).ok(), // "2000" arrived as a string
                    _ => None,
                };
                index_numeric_leaf(field, policy, leaf, value, parts, numeric);
            }
            FieldKind::Float => {
                let value = match &leaf.data {
                    LeafData::Float(v) => Some(NumericValue::Float(*v)),
                    LeafData::Text(s) => parse_float(s).ok(), // "1.5" arrived as a string
                    _ => None,
                };
                index_numeric_leaf(field, policy, leaf, value, parts, numeric);
            }
            FieldKind::Bool => {
                let value = match &leaf.data {
                    LeafData::Bool(b) => Some(*b),
                    LeafData::Text(s) => parse_bool(s), // "true"/"false" as a string
                    _ => None,
                };
                if let Some(value) = value {
                    bool_points.push(BoolPoint {
                        xpath: field.xpath(policy),
                        value,
                    });
                }
            }
            FieldKind::DateTime => {
                let format = field.date_format.as_deref().unwrap_or("YYYY-MM-DD");
                let value = match (&leaf.data, format) {
                    (LeafData::Integer(v), "epoch-millis") => Some(NumericValue::Int(*v)),
                    (LeafData::Text(s), _) => parse_date(s, format).ok().map(NumericValue::Int),
                    _ => None,
                };
                index_numeric_leaf(field, policy, leaf, value, parts, numeric);
            }
            FieldKind::None | FieldKind::Struct => {}
        }
    }
}
