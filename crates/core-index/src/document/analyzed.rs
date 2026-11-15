//! Documents after text analysis: everything `MemIndex` needs, with no text left.
//! Produced off the index worker so tokenizing runs in parallel with posting inserts.

use ahash::AHashMap;

use crate::analyzer::analyzer::AnalyzerSet;
use crate::document::document::{BoolPoint, NumericPoint};
use crate::document::{DocumentPart, IndexedDocument};
use crate::types::{ArrayRowId, DocId, XPathId, encode_position};

/// One term of one field, ready to become a posting.
pub struct AnalyzedTerm {
    pub term: String,
    pub positions: Vec<u32>,
    pub offsets: Vec<(u32, u32)>,
    pub weight: u16,
}

/// One field of one document or array row.
pub struct AnalyzedField {
    pub xpath: XPathId,
    /// Words this field adds to the document length (BM25 normalisation).
    pub doc_len: u32,
    pub terms: Vec<AnalyzedTerm>,
}

/// The analyzed content of a document body or of one array row.
pub struct AnalyzedTarget {
    pub fields: Vec<AnalyzedField>,
    pub numeric_points: Vec<NumericPoint>,
    pub bool_points: Vec<BoolPoint>,
}

pub struct AnalyzedArrayRow {
    pub array_row_id: ArrayRowId,
    pub parent: Option<ArrayRowId>,
    pub target: AnalyzedTarget,
    pub element_index: u32,
}

pub struct AnalyzedDocument {
    pub doc_id: DocId,
    pub main: AnalyzedTarget,
    pub array_rows: Vec<AnalyzedArrayRow>,
}

impl IndexedDocument {
    /// Runs the analyzer over every text part. Pure: no index state is touched,
    /// so this can run on any thread.
    pub fn analyze(&self, analyzers: &AnalyzerSet) -> AnalyzedDocument {
        AnalyzedDocument {
            doc_id: self.doc_id,
            main: analyze_target(
                analyzers,
                &self.parts,
                &self.numeric_points,
                &self.bool_points,
                false,
            ),
            array_rows: self
                .array_rows
                .iter()
                .map(|row| AnalyzedArrayRow {
                    array_row_id: row.array_row_id,
                    parent: row.parent,
                    element_index: row.element_index,
                    target: analyze_target(
                        analyzers,
                        &row.parts,
                        &row.numeric_points,
                        &row.bool_points,
                        true,
                    ),
                })
                .collect(),
        }
    }
}

fn analyze_target(
    analyzers: &AnalyzerSet,
    parts: &[DocumentPart],
    numeric_points: &[NumericPoint],
    bool_points: &[BoolPoint],
    is_row_keyed: bool,
) -> AnalyzedTarget {
    AnalyzedTarget {
        fields: parts
            .iter()
            .map(|part| add_weighted(analyzers, part, is_row_keyed))
            .collect(),
        numeric_points: numeric_points.to_vec(),
        bool_points: bool_points.to_vec(),
    }
}

//Moved unchanged from `MemIndex::add_weighted` (the half before any index write).
fn add_weighted(analyzers: &AnalyzerSet, part: &DocumentPart, is_row_keyed: bool) -> AnalyzedField {
    // analyzed.rs add_weighted
    //eprintln!("[idx] xpath={:?} analyzer={:?}", part.xpath, part.analyzer);

    let field_analyzer = part
        .analyzer
        .as_deref()
        .and_then(|name| analyzers.get(name))
        .unwrap_or_else(|| analyzers.default_analyzer());

    let segments: Vec<&str> = if part.is_flattened() {
        part.segments.iter().map(String::as_str).collect()
    } else {
        vec![part.text.as_str()]
    };
    let gap = if part.is_flattened() { part.gap } else { 0 };

    //when no char filters
    let store_offsets = !field_analyzer.has_char_filters() && !is_row_keyed;

    let mut occurrences: AHashMap<String, (Vec<u32>, Vec<(u32, u32)>)> = AHashMap::new();
    let mut total_words_len = 0u32;
    let mut cur_position = 0u32;

    let mut push = |term: &str, encoded: u32, offset: (u32, u32)| {
        let slot = occurrences.entry(term.to_owned()).or_default();
        if slot.0.last() != Some(&encoded) {
            slot.0.push(encoded);
            if store_offsets {
                slot.1.push(offset);
            }
        }
    };

    for seg in &segments {
        let (words, span) = if part.exact {
            let offset = (0u32, seg.len() as u32);
            let exact = if field_analyzer.lowercases() {
                seg.to_lowercase()
            } else {
                seg.to_string()
            };
            push(&exact, encode_position(cur_position, 1), offset);
            if part.names {
                for term in field_analyzer.name_terms(seg) {
                    push(&term, encode_position(cur_position, 1), offset);
                }
            }
            (1, 1)
        } else {
            let mut span = 0u32;
            let words = field_analyzer.for_each_token_with_offsets(
                seg,
                |term, position, length, start, end| {
                    let encoded = encode_position(cur_position + position, length);
                    push(term, encoded, (start, end));
                    span = span.max(position + length);
                },
            );
            (words, span)
        };
        total_words_len += words;
        cur_position += span + gap;
    }

    // eprintln!(
    //     "[idx-terms] xpath={:?} analyzer={:?} terms={:?}",
    //     part.xpath,
    //     part.analyzer,
    //     occurrences.keys().collect::<Vec<_>>()
    // );

    let terms = occurrences
        .into_iter()
        .map(|(term, (positions, offsets))| {
            let extra_hits = positions.len().saturating_sub(1);
            let weight = usize::from(part.weight.min)
                .saturating_add(extra_hits)
                .min(usize::from(part.weight.max)) as u16;
            AnalyzedTerm {
                term,
                positions,
                offsets,
                weight,
            }
        })
        .collect();

    AnalyzedField {
        xpath: part.xpath,
        doc_len: total_words_len,
        terms,
    }
}
