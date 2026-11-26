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

use std::{cell::RefCell, collections::BTreeMap, fmt, io, rc::Rc, sync::OnceLock};

pub const TERM_META_LEN: usize = 8 + 4 + 4 + 2+4+4+4;

use fst::{IntoStreamer, Map, MapBuilder, Streamer};
use levenshtein_automata::{DFA, Distance, LevenshteinAutomatonBuilder};

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
    /// Number of blocks, which is also the number of skip entries.
    pub block_count:u32,
     /// Bytes of the blocks area (after the skip table).
    pub block_len:u32,
    //btyes of the position stream 
    pub positions_len:u32

}

//to io:Error simplest one yer
fn fst_err(err: &fst::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err.to_string())
}
#[allow(clippy::indexing_slicing, reason = "k is clamped to 0..=3 and the array has 4 slots")]
fn builder(max_edits: u8) -> &'static LevenshteinAutomatonBuilder {
    static BUILDERS: [OnceLock<LevenshteinAutomatonBuilder>; 4] = [const { OnceLock::new() }; 4];
    let k = max_edits.min(3);
    BUILDERS[usize::from(k)].get_or_init(|| LevenshteinAutomatonBuilder::new(k, true))
}
thread_local! {
    /// The last automaton built on this thread. A query word is expanded in every segment of a
    /// snapshot one after another on the same thread, so the DFA is built once per word.
    static LAST_DFA: RefCell<Option<(u8, String, Rc<DFA>)>> = const { RefCell::new(None) };
}

/// The Levenshtein DFA for `suffix` within `max_edits`, reused while the same word is expanded.
fn cached_dfa(max_edits: u8, suffix: &str) -> Rc<DFA> {
    LAST_DFA.with(|last| {
        let mut last = last.borrow_mut();
        if let Some((edits, text, dfa)) = last.as_ref()
            && *edits == max_edits
            && text == suffix
        {
            return Rc::clone(dfa);
        }
        let dfa = Rc::new(builder(max_edits).build_dfa(suffix));
        *last = Some((max_edits, suffix.to_owned(), Rc::clone(&dfa)));
        dfa
    })
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
            .finish_non_exhaustive()
    }
}

impl TermDict {
    fn meta_for(&self, ord: u64) -> Option<TermMeta> {
        self.metas.get(usize::try_from(ord).ok()?).copied()
    }
    pub const fn empty() -> Self {
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
            let ord = u64::try_from(metas.len()).map_err(|_overflow| {
                io::Error::new(io::ErrorKind::InvalidData, "term count exceeds u64")
            })?;
            builder
                .insert(term.as_ref(), ord)
                .map_err(|err: fst::Error| fst_err(&err))?;
            metas.push(meta);
        }

        if metas.is_empty() {
            return Ok(Self::empty());
        }

        let bytes = builder.into_inner().map_err(|err: fst::Error| fst_err(&err))?;
        let map = Map::new(bytes).map_err(|err: fst::Error| fst_err(&err))?;

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

        let map = Map::new(bytes).map_err(|err: fst::Error| fst_err(&err))?;

        Ok(Self {
            map: Some(map),
            metas,
        })
    }

    //The raw FST bytes
     pub fn fst_bytes(&self) -> &[u8] {
        self.map
            .as_ref()
            .map_or_else(<&[u8]>::default, |map| map.as_fst().as_bytes())
    }

    pub fn metas(&self) -> &[TermMeta] {
        &self.metas
    }

     pub fn get(&self, term: &str) -> Option<TermMeta> {
        let map = self.map.as_ref()?;
        self.meta_for(map.get(term)?)
    }

    pub fn contains(&self, term: &str) -> bool {
        self.get(term).is_some()
    }

    pub const fn len(&self) -> usize {
        self.metas.len()
    }

    pub const fn is_empty(&self) -> bool {
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
            let meta = self.meta_for(ord).unwrap_or_default();
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

            let meta = self.meta_for(ord).unwrap_or_default();
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
            self.get(term)
                .map(|meta| vec![(FuzzyExpansion::new(term, edits, meta.doc_freq), meta)])
                .unwrap_or_default()
        };

        if opts.max_edits == 0 {
            return exact(0);
        }

        let (prefix, suffix) = split_prefix(term, opts.prefix_length);
        if suffix.is_empty() {
            return exact(0);
        }

        let dfa = cached_dfa(opts.max_edits, suffix);
        let mut out = Vec::new();

        if prefix.is_empty() {
            let mut stream = map.search_with_state(&*dfa).into_stream();
            while let Some((key, ord, state)) = stream.next() {
                let Distance::Exact(edits) = dfa.distance(state) else {
                    continue;
                };
                let meta = self.meta_for(ord).unwrap_or_default();
                let word = String::from_utf8_lossy(key).into_owned();
                out.push((FuzzyExpansion::new(word, edits, meta.doc_freq), meta));
            }
        } else {
            let mut stream = map.range().ge(prefix.as_bytes()).into_stream();
            while let Some((key, ord)) = stream.next() {
                let Ok(text) = std::str::from_utf8(key) else {
                    break;
                };
                let Some(rest) = text.strip_prefix(prefix) else {
                    break;
                };
                if let Distance::Exact(edits) = dfa.eval(rest) {
                    let meta = self.meta_for(ord).unwrap_or_default();
                    out.push((FuzzyExpansion::new(text, edits, meta.doc_freq), meta));
                }
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
