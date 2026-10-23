

use ahash::HashMap;
use fst::{Automaton, IntoStreamer, Streamer};

pub const DEFAULT_PREFIX_LENGTH: usize = 0;
//now max terms that the query expand to
pub const DEFAULT_MAX_EXPANSIONS: usize = 3;
/// Safety cap on distinct candidate terms gathered before ranking. Only
/// pathological queries (very short terms with high `max_edits`) reach it.
pub const DEFAULT_MAX_CANDIDATES:usize= 1000;
/// Score multiplier for a term that is not the user's exact input, so documents
/// containing the literal query term rank slightly above corrected matches.
pub const CORRECTED_TERM_BOOST: f32 = 0.8;
 
/// Hard ceiling on `max_edits`. Row cells are `u8` saturating at `max_edits + 1`.
pub const MAX_SUPPORTED_EDITS: u8 = 3;
// how storngly type reduces score 
const EDIT_WEIGHT: f64 = 2.0;
//scoring more than this below te best one is dropped 
const SCORE_MARGIN: f64 = 1.5;

//weighted distance
//cost of isnerting or removing one char 
const COST_CHAR: f64 = 1.0;
//cost of inserting char at end 
const COST_END_CHAR: f64 = 0.7;
//cost of replacing one char with an unrelated one
const COST_SUBSTITUTE: f64 = 1.0;
//cost of vowel-for-vowel swap or substitution between adjecent keyboard keys 
const COST_NEAR_SUBSTITUTE: f64 = 0.6;
//cost of swap 2 char
const COST_TRANSPOSE: f64 = 0.5;
//first char incorrect 
const FIRST_CHAR_PENALTY: f64 = 1.0;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuzzyOptions {
    pub max_edits: u8,
    pub prefix_length: usize,
    pub max_expansions: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuzzySpec {
    pub prefix_length: usize,
    pub max_expansions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyExpansion {
    pub term: String,
    pub edits: u8,
    pub doc_freq: u32,
}

impl FuzzyExpansion {
    pub fn new(term: impl Into<String>, edits: u8, doc_freq: u32) -> Self {
        Self {
            term: term.into(),
            edits,
            doc_freq,
        }
    }
    /// Multiplier the executor applies to this term's BM25 contribution.
    pub fn boost(&self) -> f32 {
        if self.edits == 0 {
            1.0
        } else {
            CORRECTED_TERM_BOOST
        }
    }
}

pub fn default_max_edits(term: &str) -> u8 {
    match term.chars().count() {
        0..=3 => 0,
        4 => 1,
        _ => 2,
    }
}

//prefixword -> (prefix word) based on prefix_chars
pub fn split_prefix(term: &str, prefix_chars: usize) -> (&str, &str) {
    let byte = term
        .char_indices()
        .nth(prefix_chars)
        .map(|(i, _)| i)
        .unwrap_or(term.len());
    term.split_at(byte)
}
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[b.len()]
}

//OPTIMAL STIRING ASLIGNMENT DISTANCE

/// Matches keys of the form `exact ++ s` where `s` is within `max_edits` of the
/// fuzzy part of the query. `exact` = an optional raw key prefix (e.g. a field
/// namespace in the term FST) followed by the first `prefix_length` chars of
/// the query. Input is consumed as UTF-8 bytes, distance is counted in chars.
#[derive(Debug, Clone)]
pub struct LevenshteinAutomaton {
    exact: Vec<u8>,
    key_prefix_len: usize,
    query: Vec<char>,
    max_edits: u8,
}
 
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LevState {
    Dead,
    Exact(usize),
    Fuzzy(FuzzyState),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuzzyState{
     /// Banded DP row: `row[t]` is the distance after `chars` input chars to the
    /// first `chars - max_edits + t` query chars. Cells outside the band can
    /// never be <= `max_edits`, so they are not stored. Saturates at
    /// `max_edits + 1`. Fixed size, so states are `Copy` and never allocate.
    row: [u8; BAND],
    /// The row before `row`, needed for transpositions.
    prev_row: [u8; BAND],
    chars: u32,
    prev_char: Option<char>,
    utf8: [u8; 4],
    utf8_len: u8,
    utf8_need: u8,
}
//the width of the banded DP row, only cells with max_editrs of the diagonal cal still match 
const BAND: usize = 2 * MAX_SUPPORTED_EDITS as usize + 1;

impl LevenshteinAutomaton{
    pub fn new(key_prefix: &[u8], term: &str, max_edits: u8, prefix_length: usize) -> Self {
        let (prefix, suffix) = split_prefix(term, prefix_length);
        let mut exact = Vec::with_capacity(key_prefix.len() + prefix.len());
        exact.extend_from_slice(key_prefix);
        exact.extend_from_slice(prefix.as_bytes());
        Self {
            exact,
            key_prefix_len: key_prefix.len(),
            query: suffix.chars().collect(),
            max_edits: max_edits.min(MAX_SUPPORTED_EDITS),
        }
    }
 
    pub fn key_prefix_len(&self) -> usize {
        self.key_prefix_len
    }
 
    pub fn max_edits(&self) -> u8 {
        self.max_edits
    }
 
    /// Query column held by band slot `t` of the row after `chars` input chars.
    fn column(&self, chars: u32, t: usize) -> Option<usize> {
        let j = chars as i64 - self.max_edits as i64 + t as i64;
        (j >= 0 && j <= self.query.len() as i64).then_some(j as usize)
    }
 
    fn initial_fuzzy(&self) -> LevState {
        let cap = self.max_edits + 1;
        let width = 2 * self.max_edits as usize + 1;
        let mut row = [cap; BAND];
        for (t, cell) in row.iter_mut().enumerate().take(width) {
            if let Some(j) = self.column(0, t) {
                *cell = j as u8;
            }
        }
        LevState::Fuzzy(FuzzyState {
            row,
            prev_row: [cap; BAND],
            chars: 0,
            prev_char: None,
            utf8: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
        })
    }
 
    fn step_char(&self, state: &FuzzyState, c: char) -> LevState {
        let cap = self.max_edits + 1;
        let width = 2 * self.max_edits as usize + 1;
        let chars = state.chars + 1;
        let old = &state.row;
        let mut next = [cap; BAND];
        let mut best = cap;
        for t in 0..width {
            let Some(j) = self.column(chars, t) else {
                continue;
            };
            // Same column in the previous row sits one slot to the right.
            let mut v = if t + 1 < width { old[t + 1].saturating_add(1) } else { cap };
            if j > 0 {
                if t > 0 {
                    v = v.min(next[t - 1].saturating_add(1));
                }
                v = v.min(old[t].saturating_add((self.query[j - 1] != c) as u8));
                if j >= 2 && state.chars >= 1 {
                    if let Some(p) = state.prev_char {
                        if c != p && c == self.query[j - 2] && p == self.query[j - 1] {
                            v = v.min(state.prev_row[t].saturating_add(1));
                        }
                    }
                }
            }
            let v = v.min(cap);
            next[t] = v;
            best = best.min(v);
        }
        if best > self.max_edits {
            return LevState::Dead;
        }
        LevState::Fuzzy(FuzzyState {
            row: next,
            prev_row: state.row,
            chars,
            prev_char: Some(c),
            utf8: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
        })
    }
 
    /// Edit distance of a matching state, `None` if the state is not a match.
    pub fn distance(&self, state: &LevState) -> Option<u8> {
        match state {
            LevState::Fuzzy(s) if s.utf8_need == 0 => {
                let t = self.query.len() as i64 - s.chars as i64 + self.max_edits as i64;
                if t < 0 || t > 2 * self.max_edits as i64 {
                    return None;
                }
                let d = s.row[t as usize];
                (d <= self.max_edits).then_some(d)
            }
            _ => None,
        }
    }
 
    /// Runs a whole word through the automaton. The word must not include the
    /// raw key prefix unless the automaton was built with one.
    pub fn distance_to(&self, word: &str) -> Option<u8> {
        let mut state = self.start();
        for &b in word.as_bytes() {
            state = self.accept(&state, b);
            if matches!(state, LevState::Dead) {
                return None;
            }
        }
        self.distance(&state)
    }
}
 
fn utf8_width(lead: u8) -> u8 {
    match lead {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 0,
    }
}
impl Automaton for LevenshteinAutomaton {
    type State = LevState;
 
    fn start(&self) -> LevState {
        if self.exact.is_empty() {
            self.initial_fuzzy()
        } else {
            LevState::Exact(0)
        }
    }
 
    fn is_match(&self, state: &LevState) -> bool {
        self.distance(state).is_some()
    }
 
    fn can_match(&self, state: &LevState) -> bool {
        !matches!(state, LevState::Dead)
    }
 
    fn accept(&self, state: &LevState, byte: u8) -> LevState {
        match state {
            LevState::Dead => LevState::Dead,
            LevState::Exact(n) => {
                if self.exact[*n] != byte {
                    LevState::Dead
                } else if n + 1 == self.exact.len() {
                    self.initial_fuzzy()
                } else {
                    LevState::Exact(n + 1)
                }
            }
            LevState::Fuzzy(s) => {
                if s.utf8_need == 0 {
                    match utf8_width(byte) {
                        0 => LevState::Dead,
                        1 => self.step_char(s, byte as char),
                        w => {
                            let mut pending = *s;
                            pending.utf8[0] = byte;
                            pending.utf8_len = 1;
                            pending.utf8_need = w;
                            LevState::Fuzzy(pending)
                        }
                    }
                } else {
                    if byte & 0xC0 != 0x80 {
                        return LevState::Dead;
                    }
                    let mut pending = *s;
                    pending.utf8[pending.utf8_len as usize] = byte;
                    pending.utf8_len += 1;
                    if pending.utf8_len < pending.utf8_need {
                        return LevState::Fuzzy(pending);
                    }
                    let bytes = &pending.utf8[..pending.utf8_len as usize];
                    match std::str::from_utf8(bytes).ok().and_then(|s| s.chars().next()) {
                        Some(c) => self.step_char(&pending, c),
                        None => LevState::Dead,
                    }
                }
            }
        }
    }
}
 
// ---------------------------------------------------------------------------
// Candidate collection
// ---------------------------------------------------------------------------
 
/// Adds every key of `map` accepted by `automaton` to `out` (term -> smallest
/// edit count seen). Keys are stripped of the automaton's raw key prefix.
/// Call once per segment with the same `out` so candidates merge before
/// ranking. Returns `true` if `limit` distinct terms were reached and the walk
/// stopped early.
pub fn collect_fst_candidates<D: AsRef<[u8]>>(
    map: &fst::Map<D>,
    automaton: &LevenshteinAutomaton,
    limit: usize,
    out: &mut HashMap<String, u8>,
) -> bool {
    let strip = automaton.key_prefix_len();
    let mut stream = map.search_with_state(automaton).into_stream();
    while let Some((key, _value, state)) = stream.next() {
        let Some(edits) = automaton.distance(&state) else {
            continue;
        };
        let Ok(term) = std::str::from_utf8(&key[strip..]) else {
            continue;
        };
        if let Some(existing) = out.get_mut(term) {
            *existing = (*existing).min(edits);
            continue;
        }
        if out.len() >= limit {
            return true;
        }
        out.insert(term.to_owned(), edits);
    }
    false
}
 
/// Same as `collect_fst_candidates` for term sources without an FST (memtable).
/// The automaton must have been built without a raw key prefix.
pub fn collect_iter_candidates<'a, I>(
    terms: I,
    automaton: &LevenshteinAutomaton,
    limit: usize,
    out: &mut HashMap<String, u8>,
) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    for term in terms {
        let Some(edits) = automaton.distance_to(term) else {
            continue;
        };
        if let Some(existing) = out.get_mut(term) {
            *existing = (*existing).min(edits);
            continue;
        }
        if out.len() >= limit {
            return true;
        }
        out.insert(term.to_owned(), edits);
    }
    false
}
 
// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------
 
fn keyboard_position(c: char) -> Option<(f64, f64)> {
    const ROWS: [(&str, f64); 3] = [("qwertyuiop", 0.0), ("asdfghjkl", 0.25), ("zxcvbnm", 0.75)];
    ROWS.iter().enumerate().find_map(|(y, (keys, offset))| {
        keys.chars()
            .position(|k| k == c)
            .map(|x| (x as f64 + offset, y as f64))
    })
}
 
fn keyboard_neighbors(a: char, b: char) -> bool {
    match (keyboard_position(a), keyboard_position(b)) {
        (Some((ax, ay)), Some((bx, by))) => {
            let dy = (ay - by).abs();
            let dx = (ax - bx).abs();
            (dy == 0.0 && dx == 1.0) || (dy == 1.0 && dx < 1.0)
        }
        _ => false,
    }
}
 
fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
}
 
fn substitute_cost(a: char, b: char) -> f64 {
    if a == b {
        0.0
    } else if (is_vowel(a) && is_vowel(b)) || keyboard_neighbors(a, b) {
        COST_NEAR_SUBSTITUTE
    } else {
        COST_SUBSTITUTE
    }
}
 
/// Weighted optimal-string-alignment cost of turning `query` into `candidate`.
/// Cheap: transpositions, vowel swaps, adjacent-key slips, missing/extra last
/// letter. Expensive: anything that changes the first letter.
pub fn weighted_distance(query: &str, candidate: &str) -> f64 {
    let q: Vec<char> = query.chars().collect();
    let c: Vec<char> = candidate.chars().collect();
    let (n, m) = (q.len(), c.len());
 
    let indel = |pos: usize, len: usize| -> f64 {
        let base = if pos == len { COST_END_CHAR } else { COST_CHAR };
        if pos == 1 { base + FIRST_CHAR_PENALTY } else { base }
    };
 
    let mut d = vec![vec![0.0f64; m + 1]; n + 1];
    for i in 1..=n {
        d[i][0] = d[i - 1][0] + indel(i, n);
    }
    for j in 1..=m {
        d[0][j] = d[0][j - 1] + indel(j, m);
    }
    for i in 1..=n {
        for j in 1..=m {
            let mut sub = substitute_cost(q[i - 1], c[j - 1]);
            if sub > 0.0 && i == 1 && j == 1 {
                sub += FIRST_CHAR_PENALTY;
            }
            let mut v = (d[i - 1][j] + indel(i, n))
                .min(d[i][j - 1] + indel(j, m))
                .min(d[i - 1][j - 1] + sub);
            if i >= 2 && j >= 2 && q[i - 1] == c[j - 2] && q[i - 2] == c[j - 1] && q[i - 1] != q[i - 2] {
                let penalty = if i == 2 { FIRST_CHAR_PENALTY } else { 0.0 };
                v = v.min(d[i - 2][j - 2] + COST_TRANSPOSE + penalty);
            }
            d[i][j] = v;
        }
    }
    d[n][m]
}
 
/// Noisy-channel score: how likely `candidate` is what the user meant.
pub fn expansion_score(query: &str, candidate: &str, doc_freq: u32) -> f64 {
    (doc_freq.max(1) as f64).log10() - EDIT_WEIGHT * weighted_distance(query, candidate)
}
 
/// Ranks candidates `(term, edits, doc_freq)` and keeps at most
/// `max_expansions`: the query term itself if it exists in the index, then the
/// best-scoring corrections within `SCORE_MARGIN` of the top score. `doc_freq`
/// should be the database-wide value so every shard picks the same terms.
/// Ties break on the term string, keeping the choice deterministic.
pub fn select_expansions<I>(term: &str, candidates: I, max_expansions: usize) -> Vec<FuzzyExpansion>
where
    I: IntoIterator<Item = (String, u8, u32)>,
{
    if max_expansions == 0 {
        return Vec::new();
    }
    let query = term.to_lowercase();
    let mut scored: Vec<(f64, FuzzyExpansion)> = candidates
        .into_iter()
        .filter(|(_, _, df)| *df > 0)
        .map(|(t, edits, df)| (expansion_score(&query, &t, df), FuzzyExpansion::new(t, edits, df)))
        .collect();
    if scored.is_empty() {
        return Vec::new();
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.term.cmp(&b.1.term)));
 
    let cutoff = scored[0].0 - SCORE_MARGIN;
    let mut selected = Vec::with_capacity(max_expansions.min(scored.len()));
    if let Some(pos) = scored.iter().position(|(_, e)| e.term == query) {
        selected.push(scored.remove(pos).1);
    }
    for (score, expansion) in scored {
        if selected.len() >= max_expansions || score < cutoff {
            break;
        }
        selected.push(expansion);
    }
    selected
}
 
// ---------------------------------------------------------------------------
// Highlighting
// ---------------------------------------------------------------------------
 
#[derive(Debug, Clone)]
pub struct FuzzyMatcher {
    automaton: LevenshteinAutomaton,
}
 
impl FuzzyMatcher {
    pub fn new(term: &str, max_edits: u8, prefix_length: usize) -> Self {
        Self {
            automaton: LevenshteinAutomaton::new(&[], &term.to_lowercase(), max_edits, prefix_length),
        }
    }
 
    pub fn matches(&self, word: &str) -> bool {
        if word.chars().any(char::is_uppercase) {
            self.automaton.distance_to(&word.to_lowercase()).is_some()
        } else {
            self.automaton.distance_to(word).is_some()
        }
    }
}
 
#[cfg(test)]
mod tests {
    use ahash::HashMapExt;

use super::*;
 
    fn osa(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in 0..=a.len() {
            d[i][0] = i;
        }
        for j in 0..=b.len() {
            d[0][j] = j;
        }
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                let cost = (a[i - 1] != b[j - 1]) as usize;
                d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
                }
            }
        }
        d[a.len()][b.len()]
    }
 
    #[test]
    fn automaton_agrees_with_reference_osa() {
        let words = [
            "search", "serach", "searc", "sear", "seat", "seraph", "serac", "reach", "robin",
            "bobin", "café", "cafe", "cafë", "shakespeare", "shakespire", "ab", "ba", "a", "",
            "abc", "acb", "bca", "naïve", "naive", "日本語", "日本", "spider-man", "spiderman",
        ];
        for q in words {
            for k in 0..=2u8 {
                let aut = LevenshteinAutomaton::new(&[], q, k, 0);
                for w in words {
                    let expected = osa(q, w);
                    let got = aut.distance_to(w);
                    if expected <= k as usize {
                        assert_eq!(got, Some(expected as u8), "q={q} w={w} k={k}");
                    } else {
                        assert_eq!(got, None, "q={q} w={w} k={k}");
                    }
                }
            }
        }
    }
 
    #[test]
    fn prefix_lock_and_key_prefix() {
        let aut = LevenshteinAutomaton::new(b"title\0", "serach", 2, 1);
        let map = fst::Map::from_iter(
            ["body\0search", "title\0rearch", "title\0search", "title\0serac", "title\0zzz"]
                .iter()
                .enumerate()
                .map(|(i, k)| (*k, i as u64)),
        )
        .unwrap();
        let mut out = HashMap::new();
        assert!(!collect_fst_candidates(&map, &aut, 100, &mut out));
        let mut got: Vec<_> = out.into_iter().collect();
        got.sort();
        assert_eq!(got, vec![("search".to_string(), 1), ("serac".to_string(), 1)]);
    }
 
    #[test]
    fn limit_reports_truncation() {
        let map = fst::Map::from_iter(["aa", "ab", "ac", "ad"].iter().map(|k| (*k, 0u64))).unwrap();
        let aut = LevenshteinAutomaton::new(&[], "a", 1, 0);
        let mut out = HashMap::new();
        assert!(collect_fst_candidates(&map, &aut, 2, &mut out));
        assert_eq!(out.len(), 2);
    }
 
    #[test]
    fn ranking_examples() {
        let pick = |q: &str, c: &[(&str, u32)]| {
            let cands = c.iter().map(|(t, df)| (t.to_string(), osa(q, t) as u8, *df));
            select_expansions(q, cands, 3).into_iter().map(|e| e.term).collect::<Vec<_>>()
        };
        assert_eq!(
            pick("serach", &[("search", 120_000), ("seraph", 900), ("serac", 30), ("reach", 40_000)]),
            vec!["search"]
        );
        assert_eq!(
            pick("searc", &[("search", 120_000), ("sear", 2_000), ("seat", 30_000), ("searcy", 50)]),
            vec!["search"]
        );
        assert_eq!(pick("shakespire", &[("shakespeare", 8_000)]), vec!["shakespeare"]);
        // A real word the user typed is always kept, first.
        let kept = pick("serac", &[("serac", 30), ("search", 120_000)]);
        assert_eq!(kept[0], "serac");
    }
 
    #[test]
    fn weighted_costs() {
        assert!(weighted_distance("serach", "search") < weighted_distance("seraph", "serach"));
        assert!(weighted_distance("searc", "search") < weighted_distance("searc", "searcy") + 1e-9);
        assert!(weighted_distance("bobin", "robin") > weighted_distance("robun", "robin"));
        assert_eq!(weighted_distance("same", "same"), 0.0);
    }
 
    #[test]
    fn matcher_lowercases_both_sides() {
        let m = FuzzyMatcher::new("Batman", 1, 0);
        assert!(m.matches("BATMEN"));
        assert!(m.matches("batamn"));
        assert!(!m.matches("superman"));
    }
}
