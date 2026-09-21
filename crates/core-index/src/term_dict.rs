//! FST-backed term dictionary.
//!
//! replacing the Vec<TermEntry>
//!
//! Layout on disk (written by the segment writer):
//! ```text
//! u32 field_count
//! per field:
//!   u32 xpath, u32 term_count, u64 fst_len, fst_bytes[fst_len]
//!   (u64 postings_offset, u32 postings_len, u32 doc_freq,
//!    u16 max_weight) * term_count
//! ```

use std::{collections::BTreeMap, fmt, io};

pub const TERM_META_LEN: usize = 8 + 4 + 4 + 2;

use fst::{IntoStreamer, Map, MapBuilder, Streamer};
use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder};

use crate::{
    fuzzy::{FuzzyExpansion, FuzzyOptions, split_prefix},
    types::XPathId,
};

//Where a term's postings live inside the segment, plus how many docs it hits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TermMeta {
    pub postings_offset: u64,
    pub postings_len: u32,
    pub doc_freq: u32,
    pub max_weight: u16,
}

//to io:Error simplest one yer
fn fst_err(err: fst::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err.to_string())
}

#[derive(Default)]
pub struct TermDict {
    //Option for field with no ter,s
    map: Option<Map<Vec<u8>>>,
    metas: Vec<TermMeta>,
}

impl fmt::Debug for TermDict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TermDict")
            .field("terms", &self.metas.len())
            .field("fst_bytes", &self.fst_bytes().len())
            .finish()
    }
}

impl TermDict {
    pub fn empty() -> Self {
        Self {
            map: None,
            metas: Vec::new(),
        }
    }

    pub fn build<K, I>(terms: I) -> io::Result<Self>
    where
        K: AsRef<[u8]>,
        I: IntoIterator<Item = (K, TermMeta)>,
    {
        let mut builder = MapBuilder::memory();
        let mut metas: Vec<TermMeta> = Vec::new();

        for (term, meta) in terms {
            // The ord is simply the position in sorted term order.
            builder
                .insert(term.as_ref(), metas.len() as u64)
                .map_err(fst_err)?;
            metas.push(meta);
        }

        if metas.is_empty() {
            return Ok(Self::empty());
        }

        let bytes = builder.into_inner().map_err(fst_err)?;
        let map = Map::new(bytes).map_err(fst_err)?;

        Ok(Self {
            map: Some(map),
            metas,
        })
    }

    //read from bytes
    pub fn from_parts(bytes: Vec<u8>, metas: Vec<TermMeta>) -> io::Result<Self> {
        if metas.is_empty() {
            return Ok(Self::empty());
        }

        let map = Map::new(bytes).map_err(fst_err)?;

        Ok(Self {
            map: Some(map),
            metas,
        })
    }

    //The raw FST bytes
    pub fn fst_bytes(&self) -> &[u8] {
        match &self.map {
            Some(map) => map.as_fst().as_bytes(),
            None => &[],
        }
    }

    pub fn metas(&self) -> &[TermMeta] {
        &self.metas
    }

    pub fn get(&self, term: &str) -> Option<TermMeta> {
        let map = self.map.as_ref()?;
        let ord = map.get(term)?;
        self.metas.get(ord as usize).copied()
    }

    pub fn contains(&self, term: &str) -> bool {
        self.get(term).is_some()
    }

    pub fn len(&self) -> usize {
        self.metas.len()
    }

    pub fn is_empty(&self) -> bool {
        self.metas.is_empty()
    }

    //Every term in this field, ascending, as owned strings.
    pub fn entries(&self) -> Vec<(String, TermMeta)> {
        let Some(map) = self.map.as_ref() else {
            return Vec::new();
        };

        let mut out = Vec::with_capacity(self.metas.len());
        let mut stream = map.stream();

        while let Some((term, ord)) = stream.next() {
            let meta = self.metas.get(ord as usize).copied().unwrap_or_default();
            out.push((String::from_utf8_lossy(term).into_owned(), meta));
        }

        out
    }

    //All terms starting with `prefix`, ascending.
    pub fn prefix(&self, prefix: &str) -> Vec<(String, TermMeta)> {
        let Some(map) = self.map.as_ref() else {
            return Vec::new();
        };

        let mut out = Vec::new();
        let mut stream = map.range().ge(prefix.as_bytes()).into_stream();

        while let Some((term, ord)) = stream.next() {
            if !term.starts_with(prefix.as_bytes()) {
                break;
            }

            let meta = self.metas.get(ord as usize).copied().unwrap_or_default();
            out.push((String::from_utf8_lossy(term).into_owned(), meta));
        }

        out
    }

    /// Terms within `opts.max_edits` of `term`, ascending, metadata flattened to
    /// just the term name.
    pub fn fuzzy(&self, term: &str, opts: FuzzyOptions) -> Vec<(String, TermMeta)> {
        self.fuzzy_expansions(term, opts)
            .into_iter()
            .map(|(expansion, meta)| (expansion.term, meta))
            .collect()
    }

    /// Terms within `opts.max_edits` of `term`, ascending, each with the number
    /// of edits that got it there and its metadata.
    ///
    /// When there is no fixed prefix this walks the FST with a Levenshtein
    /// automaton, which prunes whole subtrees as soon as no key below them can
    /// still be in range - so cost scales with matches, not with vocabulary.
    ///
    /// NOTE: an exact hit is always included (distance 0 is within any distance).
    /// Callers that also look the term up exactly can rely on the posting merge
    /// to dedupe.
    pub fn fuzzy_expansions(
        &self,
        term: &str,
        opts: FuzzyOptions,
    ) -> Vec<(FuzzyExpansion, TermMeta)> {
        let Some(map) = self.map.as_ref() else {
            return Vec::new();
        };

        let exact = |edits: u8| -> Vec<(FuzzyExpansion, TermMeta)> {
            match self.get(term) {
                Some(meta) => vec![(FuzzyExpansion::new(term, edits, meta.doc_freq), meta)],
                None => Vec::new(),
            }
        };

        // d=0 is just an exact lookup.
        if opts.max_edits == 0 {
            return exact(0);
        }

        let (prefix, suffix) = split_prefix(term, opts.prefix_length);

        // The whole query is pinned, so fuzziness has nothing left to chew on.
        if suffix.is_empty() {
            return exact(0);
        }

        // transposition = true keeps the current Damerau behaviour.
        let builder = LevenshteinAutomatonBuilder::new(opts.max_edits, true);
        let dfa = builder.build_dfa(suffix);

        let mut out = Vec::new();

        if prefix.is_empty() {
            // Fast path: no fixed prefix, so let the FST walk the automaton.
            // `search_with_state` hands back the automaton state per match, and
            // that is where the exact edit count comes from. We pass `&dfa`
            // (fst has `impl Automaton for &A`) so `dfa` stays usable here -
            // `levenshtein_automata::DFA` is not Clone.
            let mut stream = map.search_with_state(&dfa).into_stream();

            while let Some((t, ord, state)) = stream.next() {
                let Distance::Exact(edits) = dfa.distance(state) else {
                    continue;
                };

                let meta = self.metas.get(ord as usize).copied().unwrap_or_default();
                let term = String::from_utf8_lossy(t).into_owned();

                out.push((FuzzyExpansion::new(term, edits, meta.doc_freq), meta));
            }

            return out;
        }

        // Fixed prefix: the key range is already narrow, so scan it and
        // evaluate the automaton against each key's suffix.
        let mut stream = map.range().ge(prefix.as_bytes()).into_stream();

        while let Some((t, ord)) = stream.next() {
            let Ok(t) = std::str::from_utf8(t) else {
                break;
            };

            let Some(rest) = t.strip_prefix(prefix) else {
                break;
            };

            if let Distance::Exact(edits) = dfa.eval(rest) {
                let meta = self.metas.get(ord as usize).copied().unwrap_or_default();
                out.push((FuzzyExpansion::new(t, edits, meta.doc_freq), meta));
            }
        }

        out
    }
}

/// Every field's term dictionary in one segment.
#[derive(Debug, Default)]
pub struct TermDictionary {
    fields: BTreeMap<XPathId, TermDict>,
}

impl TermDictionary {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_field(&mut self, xpath: XPathId, dict: TermDict) {
        self.fields.insert(xpath, dict);
    }

    pub fn field(&self, xpath: XPathId) -> Option<&TermDict> {
        self.fields.get(&xpath)
    }

    pub fn fields(&self) -> impl Iterator<Item = (XPathId, &TermDict)> + '_ {
        self.fields.iter().map(|(&xpath, dict)| (xpath, dict))
    }

    pub fn get(&self, xpath: XPathId, term: &str) -> Option<TermMeta> {
        self.field(xpath)?.get(term)
    }

    pub fn term_count(&self) -> usize {
        self.fields.values().map(TermDict::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.values().all(TermDict::is_empty)
    }
}
