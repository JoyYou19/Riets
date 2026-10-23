use std::collections::BTreeMap;
use std::collections::hash_map::Entry;
use crate::document::analyzed::{ AnalyzedDocument, AnalyzedField, AnalyzedTarget };
use crate::analyzer::analyzer::Analyzer;
use crate::array_rows::ArrayRowIndex;
use crate::document::document:: BoolField  ;
use crate::document:: IndexedDocument ;
use crate::fuzzy::{ DEFAULT_MAX_CANDIDATES, FuzzyExpansion, FuzzyOptions, LevenshteinAutomaton, select_expansions, split_prefix };
use crate::numeric_values::{ NumericBound, NumericPoints, NumericValue };
use crate::posting::{ Posting, PostingList };
use crate::search::{ SearchIndex, SearchNumeric, SearchStats };
use crate::types::{ ArrayRowId, DocId, FieldStats, TermKey, XPathId };
use crate::wildcard::{ WildcardPattern, capped_union };
use ahash::AHashMap;
use ahash::HashSet;
use core_timing::timed;
use roaring::RoaringTreemap;

// Memory inverted index, the core of the index
#[derive(Debug, Clone)]
pub struct MemIndex {
    terms: AHashMap<TermKey, PostingList>,
    doc_lengths: AHashMap<(DocId, XPathId), u32>,
    field_stats: BTreeMap<XPathId, FieldStats>,
    numeric_points: NumericPoints,
    bool_fields: AHashMap<XPathId, BoolField>,
    estimated_bytes: usize,
    min_doc_id: Option<DocId>,
    max_doc_id: Option<DocId>,
    array_row_index: ArrayRowIndex,
}

impl Default for MemIndex {
    fn default() -> Self {
        Self {
            terms: AHashMap::default(),
            doc_lengths: AHashMap::default(),
            field_stats: BTreeMap::new(),
            numeric_points: NumericPoints::default(),
            bool_fields: AHashMap::new(),
            estimated_bytes: 0,
            min_doc_id: None,
            max_doc_id: None,
            array_row_index: ArrayRowIndex::default(),
        }
    }
}
impl SearchIndex for MemIndex {
    fn lookup(&self, term: &str, xpath: XPathId) -> PostingList {
        self.lookup_or_empty(term, xpath)
    }

    fn resolve_array_rows(&self, rows: &HashSet<ArrayRowId>) -> HashSet<DocId> {
        rows.iter()
            .filter_map(|&r| self.array_row_index.doc_of(r))
            .collect()
    }

    fn parent_of_row(&self, row: ArrayRowId) -> Option<ArrayRowId> {
        self.array_row_index.parent_of(row)
    }

    fn doc_of_row(&self, row: ArrayRowId) -> Option<DocId> {
        self.array_row_index.doc_of(row)
    }

    fn bool_true_ids(&self, xpath: XPathId) -> RoaringTreemap {
        self.bool_fields
            .get(&xpath)
            .map(|f| f.true_ids.clone())
            .unwrap_or_default()
    }
    fn bool_false_ids(&self, xpath: XPathId) -> RoaringTreemap {
        self.bool_fields
            .get(&xpath)
            .map(|f| f.false_ids.clone())
            .unwrap_or_default()
    }

    fn bool_value(&self, xpath: XPathId, doc_id: DocId) -> Option<bool> {
        let f = self.bool_fields.get(&xpath)?;
        if f.true_ids.contains(doc_id) {
            Some(true)
        } else if f.false_ids.contains(doc_id) {
            Some(false)
        } else {
            None
        }
    }

    fn terms(&self, xpath: XPathId) -> Vec<String> {
        self.terms
            .keys()
            .filter(|k| k.xpath == xpath)
            .map(|k| k.term.clone())
            .collect()
    }

    // The trait's version copies every term of the field into a Vec before running the automaton
    // over them (2 edits). Here the keys are walked in place, and the automaton only runs on terms
    // with the prefix and a length within `max_edits` of the typed word; a term is copied only
    // when it matches.
    fn fuzzy_expansions(
    &self,
    term: &str,
    xpath: XPathId,
    opts: FuzzyOptions,
) -> Vec<FuzzyExpansion> {
    // One automaton covers k = 0, 1 and 2 with identical semantics,
    // including transpositions and the exact-prefix lock.
    let aut = LevenshteinAutomaton::new(&[], term, opts.max_edits, opts.prefix_length);

    let mut candidates: Vec<(String, u8, u32)> = Vec::new();
    for (key, list) in &self.terms {
        if key.xpath != xpath || list.is_empty() {
            continue;
        }
        let Some(edits) = aut.distance_to(&key.term) else {
            continue;
        };
        candidates.push((key.term.clone(), edits, self.doc_freq(&key.term, xpath)));
        if candidates.len() >= DEFAULT_MAX_CANDIDATES {
            break;
        }
    }

    select_expansions(term, candidates, opts.max_expansions)
}
    // No TermMeta here — MemIndex stores full PostingLists directly, so
    // doc_freq is just the stored list's length. Uses the private lookup()
    // (returns &PostingList, no clone) rather than lookup_or_empty, since
    // there's no need to clone the whole posting list just to read its len.
    fn doc_freq(&self, term: &str, xpath: XPathId) -> u32 {
        self.terms
            .get(&TermKey::new(term, xpath))
            .map(|list| list.len() as u32)
            .unwrap_or(0)
    }
    fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        self.lookup_prefix(prefix, xpath)
    }

    fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        self.lookup_wildcard(pattern, xpath)
    }
}

impl SearchNumeric for MemIndex {
    fn numeric_range(
        &self,
        xpath: XPathId,
        lo: Option<NumericBound>,
        hi: Option<NumericBound>
    ) -> PostingList {
        let docs = self.numeric_points.range(xpath, lo, hi);
        PostingList::from_items(
            docs
                .into_iter()
                .map(|doc_id| Posting::with_weight(doc_id, Vec::new(), 0))
                .collect()
        )
    }

    fn numeric_value(&self, xpath: XPathId, doc_id: DocId) -> Option<NumericValue> {
        self.numeric_points.get(xpath, doc_id)
    }

    fn numeric_bounds(&self, xpath: XPathId) -> (Option<NumericValue>, Option<NumericValue>) {
        self.numeric_points.bounds(xpath)
    }
}

impl SearchStats for MemIndex {
    fn doc_len(&self, doc_id: DocId, xpath: XPathId) -> Option<u32> {
        self.doc_lengths.get(&(doc_id, xpath)).copied()
    }

    fn doc_count(&self, xpath: XPathId) -> u64 {
        self.field_stats
            .get(&xpath)
            .map(|s| s.doc_count)
            .unwrap_or(0)
    }

    fn total_doc_len(&self, xpath: XPathId) -> u64 {
        self.field_stats
            .get(&xpath)
            .map(|s| s.total_doc_len)
            .unwrap_or(0)
    }
    fn doc_range(&self) -> Option<(DocId, DocId)> {
        self.doc_id_range()
    }
}

impl MemIndex {
    pub fn new() -> Self {
        Self {
            terms: AHashMap::new(),
            doc_lengths: AHashMap::new(),
            field_stats: BTreeMap::new(),
            numeric_points: NumericPoints::default(),
            bool_fields: AHashMap::new(),
            estimated_bytes: 0,
            min_doc_id: None,
            max_doc_id: None,
            array_row_index: ArrayRowIndex::default(),
        }
    }
    pub fn with_capacity(expected_docs: usize, expected_terms: usize) -> Self {
        Self {
            terms: AHashMap::with_capacity(expected_terms),
            doc_lengths: AHashMap::with_capacity(expected_docs),
            field_stats: BTreeMap::new(),
            numeric_points: NumericPoints::default(),
            bool_fields: AHashMap::new(),
            estimated_bytes: 0,
            min_doc_id: None,
            max_doc_id: None,
            array_row_index: ArrayRowIndex::default(),
        }
    }

    #[timed(indexing_documents)]
    pub fn freeze(self) -> crate::segment::ImmutableSegment {
        let terms: BTreeMap<_, _> = self.terms.into_iter().collect();
        let doc_lengths: BTreeMap<_, _> = self.doc_lengths.into_iter().collect();
        let field_stats = self.field_stats;
        let bool_fields: BTreeMap<_, _> = self.bool_fields.into_iter().collect();

        crate::segment::ImmutableSegment::new(
            terms,
            doc_lengths,
            field_stats,
            self.numeric_points.build(),
            bool_fields,
            self.array_row_index
        )
    }
    //tracks min and max doc id in this segment
    fn track_doc_id(&mut self, doc_id: DocId) {
        self.min_doc_id = Some(self.min_doc_id.map_or(doc_id, |m| m.min(doc_id)));
        self.max_doc_id = Some(self.max_doc_id.map_or(doc_id, |m| m.max(doc_id)));
    }

    pub fn doc_id_range(&self) -> Option<(DocId, DocId)> {
        match (self.min_doc_id, self.max_doc_id) {
            (Some(min), Some(max)) => Some((min, max)),
            _ => None,
        }
    }

    pub fn add_posting_weighted(
        &mut self,
        term: impl Into<String>,
        xpath: XPathId,
        doc_id: DocId,
        positions: Vec<u32>,
        weight: u16
    ) {
        self.estimated_bytes +=
            size_of::<DocId>() +
            size_of::<u16>() +
            positions.len() * size_of::<u32>();

        match self.terms.entry(TermKey::new(term, xpath)) {
            Entry::Occupied(mut slot) => {
                slot.get_mut().insert_posting(doc_id, positions, weight);
            }
            Entry::Vacant(slot) => {
                self.estimated_bytes += slot.key().term.len() + size_of::<TermKey>();
                slot.insert(PostingList::new()).insert_posting(doc_id, positions, weight);
            }
        }
    }

    pub fn lookup(&self, term: &str, xpath: XPathId) -> Option<&PostingList> {
        self.terms.get(&TermKey::new(term, xpath))
    }

    pub fn term_count(&self) -> usize {
        self.terms.len()
    }
    pub fn estimated_size_bytes(&self) -> usize {
        self.estimated_bytes
    }

    #[timed(indexing_documents)]
    pub fn add_indexed_document(&mut self, analyzer: &Analyzer, document: &IndexedDocument) {
        self.add_analyzed_document(document.analyze(analyzer));
    }
    /// Inserts an already analyzed document. Only index writes happen here.
    #[timed(indexing_documents)]
    pub fn add_analyzed_document(&mut self, document: AnalyzedDocument) {
        self.track_doc_id(document.doc_id);
        self.apply_target(document.doc_id, document.main);

        for row in document.array_rows {
            self.array_row_index.push_row(row.array_row_id, document.doc_id, row.parent);
            self.apply_target(row.array_row_id, row.target);
        }
    }

    fn apply_target(&mut self, target_id: DocId, target: AnalyzedTarget) {
        for field in target.fields {
            self.apply_field(target_id, field);
        }
        for point in target.numeric_points {
            self.numeric_points.insert(point.xpath, point.value, target_id);
        }
        for point in target.bool_points {
            let slot = self.bool_fields.entry(point.xpath).or_default();
            if point.value {
                slot.true_ids.insert(target_id);
            } else {
                slot.false_ids.insert(target_id);
            }
        }
    }

    fn apply_field(&mut self, doc_id: DocId, field: AnalyzedField) {
        let old = self.doc_lengths.insert((doc_id, field.xpath), field.doc_len);

        let stats = self.field_stats.entry(field.xpath).or_default();
        match old {
            Some(old_len) => {
                stats.total_doc_len =
                    stats.total_doc_len - (old_len as u64) + (field.doc_len as u64);
            }
            None => {
                stats.doc_count += 1;
                stats.total_doc_len += field.doc_len as u64;
            }
        }

        for term in field.terms {
            self.add_posting_weighted(term.term, field.xpath, doc_id, term.positions, term.weight);
        }
    }


    pub fn lookup_or_empty(&self, term: &str, xpath: XPathId) -> PostingList {
        self.lookup(term, xpath).cloned().unwrap_or_default()
    }

    #[timed(search)]
    pub fn lookup_prefix(&self, prefix: &str, xpath: XPathId) -> PostingList {
        let matches: Vec<(&str, &PostingList)> = self.terms
            .iter()
            .filter(|(key, _)| key.xpath == xpath && key.term.starts_with(prefix))
            .map(|(key, postings)| (key.term.as_str(), postings))
            .collect();
        capped_union(matches)
    }

    #[timed(search)]
    pub fn lookup_wildcard(&self, pattern: &WildcardPattern, xpath: XPathId) -> PostingList {
        if pattern.is_prefix_only() {
            return self.lookup_prefix(pattern.prefix(), xpath);
        }
        let matches: Vec<(&str, &PostingList)> = self.terms
            .iter()
            .filter(|(key, _)| key.xpath == xpath && pattern.matches(&key.term))
            .map(|(key, postings)| (key.term.as_str(), postings))
            .collect();
        capped_union(matches)
    }

    //TEST

    #[timed(indexing_documents)]
    pub fn merge_from(&mut self, newer: MemIndex) {
        // Capture this before we start moving fields out of `newer`.
        let newer_doc_range = newer.doc_id_range();

        let mut duplicate_key_bytes = 0usize;
        for (key, list) in newer.terms {
            match self.terms.entry(key) {
                Entry::Occupied(mut slot) => {
                    duplicate_key_bytes += slot.key().term.len() + size_of::<TermKey>();
                    slot.get_mut().append(list);
                }
                Entry::Vacant(slot) => {
                    slot.insert(list);
                }
            }
        }
        self.doc_lengths.extend(newer.doc_lengths);
        for (xpath, stats) in newer.field_stats {
            self.field_stats.entry(xpath).or_default().add(&stats);
        }
        self.numeric_points.merge(newer.numeric_points);

        for (xpath, field) in newer.bool_fields {
            match self.bool_fields.entry(xpath) {
                Entry::Vacant(entry) => {
                    // Key doesn't exist yet: move the entire field in directly!
                    entry.insert(field);
                }
                Entry::Occupied(mut entry) => {
                    // Key already exists: perform the in-place union
                    let slot = entry.get_mut();
                    slot.true_ids |= &field.true_ids;
                    slot.false_ids |= &field.false_ids;
                }
            }
        }
        self.array_row_index.merge_from(&newer.array_row_index);
        self.estimated_bytes += newer.estimated_bytes.saturating_sub(duplicate_key_bytes);

        if let Some((min, max)) = newer_doc_range {
            self.track_doc_id(min);
            self.track_doc_id(max);
        }
    }
}
