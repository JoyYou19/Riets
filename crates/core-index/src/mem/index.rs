use std::collections::BTreeMap;
use std::collections::hash_map::Entry;

use crate::analyzer::analyzer::Analyzer;
use crate::array_rows::ArrayRowIndex;
use crate::document::document::{ BoolField, BoolPoint, NumericPoint };
use crate::document::{ DocumentPart, IndexedDocument, WeightInterval };
use crate::fuzzy::{ FuzzyExpansion, FuzzyOptions, candidates_within_one, split_prefix };
use crate::numeric_values::{ NumericBound, NumericPoints, NumericValue };
use crate::posting::{ Posting, PostingList };
use crate::search::{ SearchIndex, SearchNumeric, SearchStats };
use crate::types::{ ArrayRowId, DocId, FieldStats, TermKey, XPathId, encode_position };
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
        let existing = |candidate: &str, edits: u8| -> Option<FuzzyExpansion> {
            let doc_freq = self.doc_freq(candidate, xpath);
            (doc_freq > 0).then(|| FuzzyExpansion::new(candidate, edits, doc_freq))
        };

        let mut out: Vec<FuzzyExpansion> = existing(term, 0).into_iter().collect();
        if opts.max_edits == 0 {
            return out;
        }

        let (prefix, suffix) = split_prefix(term, opts.prefix_length);
        if suffix.is_empty() {
            return out;
        }

        if opts.max_edits == 1 {
            // every word one edit away, each a hash lookup
            for candidate in candidates_within_one(suffix) {
                out.extend(existing(&format!("{prefix}{candidate}"), 1));
            }
        } else {
            let dfa = levenshtein_automata::LevenshteinAutomatonBuilder::new(opts.max_edits, true)
                .build_dfa(suffix);
            let suffix_chars = suffix.chars().count();
            let max_edits = usize::from(opts.max_edits);
            for (key, list) in &self.terms {
                if key.xpath != xpath || list.is_empty() {
                    continue;
                }
                let Some(rest) = key.term.strip_prefix(prefix) else {
                    continue;
                };
                // a length difference above max_edits is more edits than allowed
                if rest.chars().count().abs_diff(suffix_chars) > max_edits {
                    continue;
                }
                if let levenshtein_automata::Distance::Exact(edits) = dfa.eval(rest) {
                    out.push(FuzzyExpansion::new(key.term.as_str(), edits, list.len() as u32));
                }
            }
        }

        out.sort_by(|a, b| a.term.cmp(&b.term).then_with(|| a.edits.cmp(&b.edits)));
        out.dedup_by(|a, b| a.term == b.term);
        out
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

    pub fn add_token(
        &mut self,
        term: impl Into<String>,
        xpath: XPathId,
        doc_id: DocId,
        position: u32
    ) {
        self.add_token_weighted(term, xpath, doc_id, position, 1);
    }

    #[timed(indexing_documents)]
    pub fn add_token_weighted(
        &mut self,
        term: impl Into<String>,
        xpath: XPathId,
        doc_id: DocId,
        position: u32,
        weight: u16
    ) {
        // one position added to a posting: doc_id + one u32 position, plus
        // per-entry posting overhead (weight, small header). Approximate —
        // this doesn't need to be exact, just proportional to real growth.
        self.estimated_bytes +=
            std::mem::size_of::<DocId>() + std::mem::size_of::<u32>() + std::mem::size_of::<u16>();

        match self.terms.entry(TermKey::new(term, xpath)) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                slot.get_mut().insert(doc_id, position, weight);
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                self.estimated_bytes += slot.key().term.len() + std::mem::size_of::<TermKey>();
                slot.insert(PostingList::new()).insert(doc_id, position, weight);
            }
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
            std::mem::size_of::<DocId>() +
            std::mem::size_of::<u16>() +
            positions.len() * std::mem::size_of::<u32>();

        match self.terms.entry(TermKey::new(term, xpath)) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                slot.get_mut().insert_posting(doc_id, positions, weight);
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                self.estimated_bytes += slot.key().term.len() + std::mem::size_of::<TermKey>();
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
    pub fn add_document(&mut self, analyzer: &Analyzer, doc_id: DocId, xpath: XPathId, text: &str) {
        for token in analyzer.analyze(text) {
            self.add_token(token.text, xpath, doc_id, token.position);
        }
    }

    #[timed(indexing_documents)]
    pub fn add_indexed_document(&mut self, analyzer: &Analyzer, document: &IndexedDocument) {
        //main document
        self.track_doc_id(document.doc_id);
        self.add_parts_and_points(
            analyzer,
            document.doc_id,
            &document.parts,
            &document.numeric_points,
            &document.bool_points
        );

        //each array in documents
        for row in &document.array_rows {
            self.array_row_index.push_row(row.array_row_id, document.doc_id, row.parent);

            self.add_parts_and_points(
                analyzer,
                row.array_row_id,
                &row.parts,
                &row.numeric_points,
                &row.bool_points
            );
        }
    }

    fn add_weighted(
        &mut self,
        analyzer: &Analyzer,
        doc_id: DocId,
        xpath: XPathId,
        weight: WeightInterval,
        segments: &[&str],
        gap: u32,
        exact: bool,
        names: bool
    ) {
        let mut token_positions: AHashMap<String, Vec<u32>> = AHashMap::new();
        let mut total_words_len = 0u32;
        let mut cur_position = 0u32;

        //do this for every string in the array of strings
        for seg in segments {
            // words: the document length this segment adds. span: the positions it uses, so the
            // next array element starts after its last word (stopwords leave gaps in positions)
            let (words, span) = if exact {
                let words: Vec<&str> = seg.split_whitespace().collect();
                // write down each word, at position (offset + its index in this string)
                for (i, word) in words.iter().enumerate() {
                    token_positions
                        .entry((*word).to_string())
                        .or_default()
                        .push(encode_position(cur_position + (i as u32), 1));
                }
                // the whole value as one term too, so a search can tell "this is the name" apart
                // from "the name contains these words" (see Analyzer::name_terms)
                for term in names.then(|| analyzer.name_terms(seg)).unwrap_or_default() {
                    token_positions.entry(term).or_default().push(encode_position(cur_position, 1));
                }
                (words.len() as u32, words.len() as u32)
            } else {
                let mut span = 0u32;
                let words = analyzer.for_each_token(seg, |term, position, length| {
                    let encoded = encode_position(cur_position + position, length);
                    let positions = token_positions.entry(term.to_owned()).or_default();
                    // two forms of one word can analyze to the same term ("king's", "kings" -> king)
                    if positions.last() != Some(&encoded) {
                        positions.push(encoded);
                    }
                    span = span.max(position + length);
                });
                (words, span)
            };
            total_words_len += words;

            //move our "cursor" forward past this string, PLUS the gap
            cur_position += span + gap;
        }

        let old = self.doc_lengths.insert((doc_id, xpath), total_words_len);

        //stats
        let stats = self.field_stats.entry(xpath).or_default();
        match old {
            Some(old_len) => {
                stats.total_doc_len =
                    stats.total_doc_len - (old_len as u64) + (total_words_len as u64);
            }
            None => {
                stats.doc_count += 1;
                stats.total_doc_len += total_words_len as u64;
            }
        }

        //write to seg
       for (term, positions) in token_positions {
            let extra_hits = positions.len().saturating_sub(1);
            let posting_weight = usize::from(weight.min)
                .saturating_add(extra_hits)
                .min(usize::from(weight.max)) as u16;
            self.add_posting_weighted(term, xpath, doc_id, positions, posting_weight);
        }
    }

    //helper
    fn add_parts_and_points(
        &mut self,
        analyzer: &Analyzer,
        target_id: DocId, //array row id uses the same type for id
        parts: &[DocumentPart],
        numeric_points: &[NumericPoint],
        bool_points: &[BoolPoint]
    ) {
        for part in parts {
            //if this part has several strings (array of string), use them else one string
            let segments: Vec<&str> = if part.is_flattened() {
                part.segments
                    .iter()
                    .map(|s| s.as_str())
                    .collect()
            } else {
                vec![part.text.as_str()]
            };

            // flattened parts skip positions between strings; normal parts don't
            let gap = if part.is_flattened() { part.gap } else { 0 };

            //one call does all the work part.exact pirck stem or step+exact
            self.add_weighted(
                analyzer,
                target_id,
                part.xpath,
                part.weight,
                &segments,
                gap,
                part.exact,
                part.names
            );
        }
        for point in numeric_points {
            self.numeric_points.insert(point.xpath, point.value, target_id);
        }

        for point in bool_points {
            let slot = self.bool_fields.entry(point.xpath).or_default();
            if point.value {
                slot.true_ids.insert(target_id);
            } else {
                slot.false_ids.insert(target_id);
            }
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

        for (key, list) in newer.terms {
            match self.terms.entry(key) {
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    slot.get_mut().append(list);
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
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
        self.estimated_bytes += newer.estimated_bytes;

        if let Some((min, max)) = newer_doc_range {
            self.track_doc_id(min);
            self.track_doc_id(max);
        }
    }
}
