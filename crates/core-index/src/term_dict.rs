//! FST-backed term dictionary.
//!
//! replacing the Vec<TermEntry>
//!
//! Layout on disk (written by the segment writer):
//! ```text
//! u32 field_count
//! per field:
//!   u32 xpath, u32 term_count, u64 fst_len, fst_bytes[fst_len]
//!   (u64 postings_offset, u32 postings_len, u32 doc_freq) * term_count
//! ```

use std::{collections::BTreeMap, fmt, io};

pub const TERM_META_LEN: usize = 8 + 4 + 4;

use fst::{IntoStreamer, Map, MapBuilder, Streamer};
use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder};

use crate::{
    fuzzy::{FuzzyOptions, split_prefix},
    types::XPathId,
};

//Where a term's postings live inside the segment, plus how many docs it hits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TermMeta {
    pub postings_offset: u64,
    pub postings_len: u32,
    pub doc_freq: u32,
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

    pub fn fuzzy(&self, term: &str, opts: FuzzyOptions) -> Vec<(String, TermMeta)> {
        let Some(map) = self.map.as_ref() else {
            return Vec::new();
        };

        // d=0 is just an exact lookup.
        if opts.max_edits == 0 {
            return match self.get(term) {
                Some(meta) => vec![(term.to_string(), meta)],
                None => Vec::new(),
            };
        }

        let (prefix, suffix) = split_prefix(term, opts.prefix_length);

        //no possible fuzz after the prefix -> just the query term
        if suffix.is_empty() {
            return match self.get(term) {
                Some(meta) => vec![(term.to_string(), meta)],
                None => Vec::new(),
            };
        }

        // transposition = true keeps the current Damerau behaviour.
        let builder = LevenshteinAutomatonBuilder::new(opts.max_edits, true);
        let dfa = builder.build_dfa(suffix);

        let mut out = Vec::new();

        if prefix.is_empty() {
            //Fast path: no fixed prefix, so let the FST walk the automaton.
            let mut stream = map.search(dfa).into_stream();

            while let Some((t, ord)) = stream.next() {
                let meta = self.metas.get(ord as usize).copied().unwrap_or_default();
                out.push((String::from_utf8_lossy(t).into_owned(), meta));
            }

            return out;
        }

        //Fixed prefix: the key range is already narrow, so scan it and
        //evaluate the automaton against each key's suffix.
        let mut stream = map.range().ge(prefix.as_bytes()).into_stream();

        while let Some((t, ord)) = stream.next() {
            let Ok(t) = std::str::from_utf8(t) else {
                break;
            };

            let Some(rest) = t.strip_prefix(prefix) else {
                break;
            };

            if let Distance::Exact(_) = dfa.eval(rest) {
                let meta = self.metas.get(ord as usize).copied().unwrap_or_default();
                out.push((t.to_string(), meta));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(offset: u64) -> TermMeta {
        TermMeta {
            postings_offset: offset,
            postings_len: 4,
            doc_freq: 1,
        }
    }

    fn opts(max_edits: u8, prefix_length: usize) -> FuzzyOptions {
        FuzzyOptions {
            max_edits,
            prefix_length,
            max_expansions: 50,
        }
    }

    // Deliberately listed in ascending order (MapBuilder::insert demands it).
    fn dict() -> TermDict {
        TermDict::build(vec![
            ("car", meta(10)),
            ("cart", meta(20)),
            ("cat", meta(30)),
            ("dog", meta(40)),
            ("shakespeare", meta(50)),
            ("shakespere", meta(60)),
        ])
        .unwrap()
    }

    fn names(entries: Vec<(String, TermMeta)>) -> Vec<String> {
        entries.into_iter().map(|(term, _)| term).collect()
    }

    #[test]
    fn get_roundtrips() {
        let d = dict();

        assert_eq!(d.get("cat"), Some(meta(30)));
        assert_eq!(d.get("cart"), Some(meta(20)));
        assert_eq!(d.get("missing"), None);
        assert_eq!(d.len(), 6);
        assert!(!d.is_empty());
    }

    #[test]
    fn entries_are_ascending() {
        assert_eq!(
            names(dict().entries()),
            vec!["car", "cart", "cat", "dog", "shakespeare", "shakespere"]
        );
    }

    #[test]
    fn empty_dict_is_harmless() {
        let d = TermDict::empty();

        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
        assert_eq!(d.get("cat"), None);
        assert!(d.prefix("ca").is_empty());
        assert!(d.entries().is_empty());
        assert!(d.fuzzy("cat", opts(2, 0)).is_empty());
        assert_eq!(d.fst_bytes().len(), 0);
    }

    #[test]
    fn rejects_out_of_order_terms() {
        // "cat" then "car" is descending, so the builder must complain.
        assert!(TermDict::build(vec![("cat", meta(1)), ("car", meta(2))]).is_err());
    }

    #[test]
    fn prefix_only_returns_matching_terms() {
        let d = dict();

        assert_eq!(names(d.prefix("ca")), vec!["car", "cart", "cat"]);
        assert_eq!(names(d.prefix("cart")), vec!["cart"]);
        assert_eq!(names(d.prefix("s")), vec!["shakespeare", "shakespere"]);
        assert!(d.prefix("zzz").is_empty());
        // A prefix that sorts between keys must not spill into later ones.
        assert!(d.prefix("cau").is_empty());
    }

    #[test]
    fn fuzzy_d1_agrees_with_candidate_generation() {
        let d = dict();

        // candidates_within_one(word) is "distance exactly 1"; the FST returns
        // "distance <= 1", which also covers the word itself.
        for word in ["cot", "wiliam", "teh", "shakespere", "car", "cart"] {
            let mut from_fst = names(d.fuzzy(word, opts(1, 0)));
            from_fst.sort();

            let candidates = crate::fuzzy::candidates_within_one(word);
            let mut from_candidates: Vec<String> = d
                .entries()
                .into_iter()
                .map(|(term, _)| term)
                .filter(|term| term == word || candidates.contains(term))
                .collect();
            from_candidates.sort();

            assert_eq!(from_fst, from_candidates, "d=1 mismatch for {word:?}");
        }
    }

    #[test]
    fn fuzzy_finds_transposition_and_insertion() {
        let d = dict();

        // "teh" -> "the"-style adjacent swap is exercised by the differential
        // test; here we check a real insertion.
        assert!(names(d.fuzzy("shakespere", opts(1, 0))).contains(&"shakespeare".to_string()));
    }

    #[test]
    fn fuzzy_distance_two_needs_two_edits() {
        let d = TermDict::build(vec![("abcdef", meta(1))]).unwrap();

        assert!(d.fuzzy("abcdxx", opts(1, 0)).is_empty());
        assert_eq!(names(d.fuzzy("abcdxx", opts(2, 0))), vec!["abcdef"]);

        // d=0 is an exact lookup only.
        assert!(d.fuzzy("abcdxx", opts(0, 0)).is_empty());
        assert_eq!(names(d.fuzzy("abcdef", opts(0, 0))), vec!["abcdef"]);
    }

    #[test]
    fn prefix_length_pins_the_start() {
        let d = dict();

        // Unpinned: "cot" reaches "cat" (one substitution).
        assert_eq!(names(d.fuzzy("cot", opts(1, 0))), vec!["cat"]);

        // Pinned to "co": nothing in this field starts with "co".
        assert!(d.fuzzy("cot", opts(1, 2)).is_empty());

        // Prefix swallows the whole query, so only the exact term can match.
        assert_eq!(names(d.fuzzy("cat", opts(1, 3))), vec!["cat"]);
    }

    #[test]
    fn from_parts_roundtrips_bytes() {
        let d = dict();

        let reloaded = TermDict::from_parts(d.fst_bytes().to_vec(), d.metas().to_vec()).unwrap();

        assert_eq!(reloaded.len(), d.len());
        assert_eq!(reloaded.get("cart"), Some(meta(20)));
        assert_eq!(reloaded.entries(), d.entries());
        assert_eq!(names(reloaded.prefix("ca")), names(d.prefix("ca")));
        assert_eq!(
            names(reloaded.fuzzy("cot", opts(1, 0))),
            names(d.fuzzy("cot", opts(1, 0)))
        );
    }

    #[test]
    fn dictionary_scopes_terms_per_field() {
        let mut dicts = TermDictionary::new();

        dicts.insert_field(0, TermDict::build(vec![("cat", meta(1))]).unwrap());
        dicts.insert_field(1, TermDict::build(vec![("dog", meta(2))]).unwrap());

        assert_eq!(dicts.get(0, "cat"), Some(meta(1)));
        assert_eq!(dicts.get(1, "dog"), Some(meta(2)));
        assert_eq!(dicts.get(0, "dog"), None);
        assert_eq!(dicts.get(9, "cat"), None);
        assert_eq!(dicts.term_count(), 2);
        assert!(!dicts.is_empty());
    }
}
