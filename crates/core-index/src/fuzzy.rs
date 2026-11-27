use fst::Automaton;

pub const DEFAULT_PREFIX_LENGTH: usize = 0;
//now max terms that the query expand to
pub const DEFAULT_MAX_EXPANSIONS: usize = 50;
/// Safety cap on distinct candidate terms gathered before ranking. Only
/// pathological queries (very short terms with high `max_edits`) reach it.
pub const DEFAULT_MAX_CANDIDATES: usize = 1000;
/// Orders fuzzy candidates the same way for every index: fewer edits first, then more
/// documents, then the term itself so ties never depend on iteration order. Keeps at most
/// `DEFAULT_MAX_CANDIDATES`; the final choice is made after shards are merged.
#[must_use]
pub fn rank_candidates(mut candidates: Vec<(String, u8, u32)>) -> Vec<FuzzyExpansion> {
    candidates.sort_unstable_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.0.cmp(&b.0))
    });
    candidates.truncate(DEFAULT_MAX_CANDIDATES);
    candidates
        .into_iter()
        .map(|(term, edits, doc_freq)| FuzzyExpansion::new(term, edits, doc_freq))
        .collect()
}
/// A typed word in at most this many documents may be a typo of a more common word.
pub const TYPO_MAX_DOC_FREQ: u32 = 500;
/// A rare typed word is a likely typo when a candidate within the fuzzy edits is in at least
/// this many times as many documents ("holand" 33, "holland" 7,922).
pub const TYPO_RATIO: u32 = 20;
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
}

#[must_use]
pub fn default_max_edits(term: &str) -> u8 {
    match term.chars().count() {
        0..=2 => 0,
        3..=4 => 1,
        _ => 2,
    }
}

//prefixword -> (prefix word) based on prefix_chars
#[must_use]
pub fn split_prefix(term: &str, prefix_chars: usize) -> (&str, &str) {
    let byte = term
        .char_indices()
        .nth(prefix_chars)
        .map_or(term.len(), |(i, _)| i);
    term.split_at(byte)
}
#[must_use]
pub fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let left = cur.last().copied().unwrap_or_default();
            let up = prev
                .get(j + 1)
                .copied()
                .unwrap_or_default();
            let diag = prev.get(j).copied().unwrap_or_default();
            cur.push((up + 1).min(left + 1).min(diag + usize::from(ca != *cb)));
        }
        prev = cur;
    }
    prev.last().copied().unwrap_or_default()
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
pub struct FuzzyState {
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
const BAND: usize = 2 * (MAX_SUPPORTED_EDITS as usize) + 1;

impl LevenshteinAutomaton {
    #[must_use]
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

    #[must_use]
    pub const fn key_prefix_len(&self) -> usize {
        self.key_prefix_len
    }

    #[must_use]
    pub const fn max_edits(&self) -> u8 {
        self.max_edits
    }

    /// Query column held by band slot `t` of the row after `chars` input chars.
    fn column(&self, chars: u32, t: usize) -> Option<usize> {
        let slot = u32::try_from(t).ok()?;
        let j = chars.checked_add(slot)?.checked_sub(u32::from(self.max_edits))?;
        let j = usize::try_from(j).ok()?;
        (j <= self.query.len()).then_some(j)
    }

    fn initial_fuzzy(&self) -> LevState {
        let cap = self.max_edits + 1;
        let width = 2 * usize::from(self.max_edits) + 1;
        let mut row = [cap; BAND];
        for (t, cell) in row.iter_mut().enumerate().take(width) {
            if let Some(j) = self.column(0, t) {
                *cell = u8::try_from(j).unwrap_or(cap);
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
        let width = 2 * usize::from(self.max_edits) + 1;
        let chars = state.chars + 1;
        let old = &state.row;
        let mut next = [cap; BAND];
        let mut best = cap;
        let cell = |row: &[u8; BAND], at: usize| row.get(at).copied().unwrap_or(cap);
        for t in 0..width {
            let Some(j) = self.column(chars, t) else {
                continue;
            };
            // Same column in the previous row sits one slot to the right.
            let mut v = if t + 1 < width { cell(old, t + 1).saturating_add(1) } else { cap };
            if let Some(before_j) = j.checked_sub(1) {
                if let Some(left) = t.checked_sub(1) {
                    v = v.min(cell(&next, left).saturating_add(1));
                }
                let mismatch = u8::from(self.query.get(before_j) != Some(&c));
                v = v.min(cell(old, t).saturating_add(mismatch));
                if
                    let Some(p) = state.prev_char &&
                    state.chars >= 1 &&
                    c != p &&
                    let Some(two_back) = j.checked_sub(2) &&
                    self.query.get(two_back) == Some(&c) &&
                    self.query.get(before_j) == Some(&p)
                {
                    v = v.min(cell(&state.prev_row, t).saturating_add(1));
                }
            }
            let v = v.min(cap);
            if let Some(slot) = next.get_mut(t) {
                *slot = v;
            }
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
    #[must_use]
    pub fn distance(&self, state: &LevState) -> Option<u8> {
        let LevState::Fuzzy(s) = state else {
            return None;
        };
        if s.utf8_need != 0 {
            return None;
        }
        let consumed = usize::try_from(s.chars).ok()?;
        let t = (self.query.len() + usize::from(self.max_edits)).checked_sub(consumed)?;
        if t > 2 * usize::from(self.max_edits) {
            return None;
        }
        let d = *s.row.get(t)?;
        (d <= self.max_edits).then_some(d)
    }

    /// Runs a whole word through the automaton. The word must not include the
    /// raw key prefix unless the automaton was built with one.
    #[must_use]
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
    fn accept_fuzzy(&self, state: &FuzzyState, byte: u8) -> LevState {
        if state.utf8_need == 0 {
            return match utf8_width(byte) {
                0 => LevState::Dead,
                1 => self.step_char(state, char::from(byte)),
                width => {
                    let mut pending = *state;
                    pending.utf8 = [byte, 0, 0, 0];
                    pending.utf8_len = 1;
                    pending.utf8_need = width;
                    LevState::Fuzzy(pending)
                }
            };
        }
        if (byte & 0xc0) != 0x80 {
            return LevState::Dead;
        }
        let mut pending = *state;
        let Some(slot) = pending.utf8.get_mut(usize::from(pending.utf8_len)) else {
            return LevState::Dead;
        };
        *slot = byte;
        pending.utf8_len += 1;
        if pending.utf8_len < pending.utf8_need {
            return LevState::Fuzzy(pending);
        }
        let Some(bytes) = pending.utf8.get(..usize::from(pending.utf8_len)) else {
            return LevState::Dead;
        };
        match
            std::str
                ::from_utf8(bytes)
                .ok()
                .and_then(|text| text.chars().next())
        {
            Some(c) => self.step_char(&pending, c),
            None => LevState::Dead,
        }
    }
}

const fn utf8_width(lead: u8) -> u8 {
    match lead {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 0,
    }
}
impl Automaton for LevenshteinAutomaton {
    type State = LevState;

    fn start(&self) -> LevState {
        if self.exact.is_empty() { self.initial_fuzzy() } else { LevState::Exact(0) }
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
                if self.exact.get(*n) != Some(&byte) {
                    LevState::Dead
                } else if n + 1 == self.exact.len() {
                    self.initial_fuzzy()
                } else {
                    LevState::Exact(n + 1)
                }
            }
            LevState::Fuzzy(s) => self.accept_fuzzy(s, byte),
        }
    }
}

// ---------------------------------------------------------------------------
// Candidate collection
// ---------------------------------------------------------------------------

// /// Adds every key of `map` accepted by `automaton` to `out` (term -> smallest
// /// edit count seen). Keys are stripped of the automaton's raw key prefix.
// /// Call once per segment with the same `out` so candidates merge before
// /// ranking. Returns `true` if `limit` distinct terms were reached and the walk
// /// stopped early.
// pub fn collect_fst_candidates<D: AsRef<[u8]>>(
//     map: &fst::Map<D>,
//     automaton: &LevenshteinAutomaton,
//     limit: usize,
//     out: &mut HashMap<String, u8>,
// ) -> bool {
//     let strip = automaton.key_prefix_len();
//     let mut stream = map.search_with_state(automaton).into_stream();
//     while let Some((key, _value, state)) = stream.next() {
//         let Some(edits) = automaton.distance(&state) else {
//             continue;
//         };
//         let Ok(term) = std::str::from_utf8(&key[strip..]) else {
//             continue;
//         };
//         if let Some(existing) = out.get_mut(term) {
//             *existing = (*existing).min(edits);
//             continue;
//         }
//         if out.len() >= limit {
//             return true;
//         }
//         out.insert(term.to_owned(), edits);
//     }
//     false
// }
// /// Same as `collect_fst_candidates` for term sources without an FST (memtable).
// /// The automaton must have been built without a raw key prefix.
// pub fn collect_iter_candidates<'a, I>(
//     terms: I,
//     automaton: &LevenshteinAutomaton,
//     limit: usize,
//     out: &mut HashMap<String, u8>,
// ) -> bool
// where
//     I: IntoIterator<Item = &'a str>,
// {
//     for term in terms {
//         let Some(edits) = automaton.distance_to(term) else {
//             continue;
//         };
//         if let Some(existing) = out.get_mut(term) {
//             *existing = (*existing).min(edits);
//             continue;
//         }
//         if out.len() >= limit {
//             return true;
//         }
//         out.insert(term.to_owned(), edits);
//     }
//     false
// }
// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------
fn keyboard_position(c: char) -> Option<(usize, usize)> {
    // x in quarter-key units, so the row offsets 0.0 / 0.25 / 0.75 become 0 / 1 / 3
    const ROWS: [(&str, usize); 3] = [
        ("qwertyuiop", 0),
        ("asdfghjkl", 1),
        ("zxcvbnm", 3),
    ];
    ROWS.iter()
        .enumerate()
        .find_map(|(row, &(keys, shift))| {
            let key = keys.chars().position(|k| k == c)?;
            Some((key * 4 + shift, row))
        })
}

fn keyboard_neighbors(a: char, b: char) -> bool {
    let (Some((ax, ay)), Some((bx, by))) = (keyboard_position(a), keyboard_position(b)) else {
        return false;
    };
    let dx = ax.abs_diff(bx);
    let dy = ay.abs_diff(by);
    (dy == 0 && dx == 4) || (dy == 1 && dx < 4)
}

const fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
}
fn sound_alike(a: char, b: char) -> bool {
    matches!(
        (a.min(b), a.max(b)),
        ('c', 'k' | 's') | ('k', 'q') | ('s', 'z') | ('v', 'w') | ('g', 'j')
    )
}

fn substitute_cost(a: char, b: char) -> f64 {
    if a == b {
        0.0
    } else if (is_vowel(a) && is_vowel(b)) || keyboard_neighbors(a, b) || sound_alike(a, b) {
        COST_NEAR_SUBSTITUTE
    } else {
        COST_SUBSTITUTE
    }
}

/// Weighted optimal-string-alignment cost of turning `query` into `candidate`.
/// Cheap: transpositions, vowel swaps, adjacent-key slips, missing/extra last
/// letter. Expensive: anything that changes the first letter.
fn row_cell(row: &[f64], at: usize) -> f64 {
    row.get(at).copied().unwrap_or(f64::INFINITY)
}

#[must_use]
pub fn weighted_distance(query: &str, candidate: &str) -> f64 {
    let query_chars: Vec<char> = query.chars().collect();
    let cand_chars: Vec<char> = candidate.chars().collect();
    let (query_len, cand_len) = (query_chars.len(), cand_chars.len());

    let indel = |pos: usize, len: usize| -> f64 {
        let base = if pos == len { COST_END_CHAR } else { COST_CHAR };
        if pos == 1 {
            base + FIRST_CHAR_PENALTY
        } else {
            base
        }
    };

    let mut first = vec![0.0_f64];
    for j in 1..=cand_len{
        let next = row_cell(&first, j - 1) + indel(j, cand_len);
        first.push(next);
    }

    let mut before: Vec<f64> = Vec::new();
    let mut prev = first;
    for (i, &qc) in query_chars.iter().enumerate() {
        let mut cur = vec![row_cell(&prev, 0) + indel(i + 1, query_len)];
        for (j, &cc) in cand_chars.iter().enumerate() {
            let mut sub = substitute_cost(qc, cc);
            if sub > 0.0 && i == 0 && j == 0 && !sound_alike(qc, cc) {
                sub += FIRST_CHAR_PENALTY;
            }
            let mut v = (row_cell(&prev, j + 1) + indel(i + 1, query_len))
                .min(row_cell(&cur, j) + indel(j + 1, cand_len))
                .min(row_cell(&prev, j) + sub);
            if
                let (Some(i_back), Some(j_back)) = (i.checked_sub(1), j.checked_sub(1)) &&
                let (Some(&q_prev), Some(&c_prev)) = (query_chars.get(i_back), cand_chars.get(j_back)) &&
                qc == c_prev &&
                q_prev == cc &&
                qc != q_prev
            {
                v = v.min(row_cell(&before, j_back) + COST_TRANSPOSE);
            }
            cur.push(v);
        }
        before = std::mem::replace(&mut prev, cur);
    }
    row_cell(&prev, cand_len)
}

/// Noisy-channel score: how likely `candidate` is what the user meant.
#[must_use]
pub fn expansion_score(query: &str, candidate: &str, doc_freq: u32) -> f64 {
    EDIT_WEIGHT.mul_add(-weighted_distance(query, candidate), f64::from(doc_freq.max(1)).log10())
}

/// Ranks candidates `(term, edits, doc_freq)` and keeps at most
/// `max_expansions`: the query term itself if it exists in the index, then the
/// best-scoring corrections within `SCORE_MARGIN` of the top score. `doc_freq`
/// should be the database-wide value so every shard picks the same terms.
/// Ties break on the term string, keeping the choice deterministic.
pub fn select_expansions<I>(term: &str, candidates: I, max_expansions: usize) -> Vec<FuzzyExpansion>
    where I: IntoIterator<Item = (String, u8, u32)>
{
    if max_expansions == 0 {
        return Vec::new();
    }
    let query = term.to_lowercase();
    let mut scored: Vec<(f64, FuzzyExpansion)> = candidates
        .into_iter()
        .filter(|(_, _, df)| *df > 0)
        .map(|(t, edits, df)| {
            (expansion_score(&query, &t, df), FuzzyExpansion::new(t, edits, df))
        })
        .collect();
    if scored.is_empty() {
        return Vec::new();
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.term.cmp(&b.1.term)));

    let mut selected: Vec<(f64, FuzzyExpansion)> = Vec::with_capacity(
        max_expansions.min(scored.len())
    );
    if let Some(pos) = scored.iter().position(|(_, e)| e.term == query) {
        selected.push(scored.remove(pos));
    }
    // the margin is measured among the corrections: the typed word is kept anyway, and a
    // common typed word ("ronald", from "ronalds") must not push out every correction
    if let Some(best) = scored.first().map(|(score, _)| *score) {
        let cutoff = best - SCORE_MARGIN;
        for (score, expansion) in scored {
            if selected.len() >= max_expansions || score < cutoff {
                break;
            }
            selected.push((score, expansion));
        }
    }
    // the typed word is always kept, but in score order: "wass" (in 3 documents) comes after
    // "was", so did-you-mean suggests "was" first
    selected.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.term.cmp(&b.1.term)));
    selected
        .into_iter()
        .map(|(_, expansion)| expansion)
        .collect()
}

// ---------------------------------------------------------------------------
// Highlighting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FuzzyMatcher {
    automaton: LevenshteinAutomaton,
}

impl FuzzyMatcher {
    #[must_use]
    pub fn new(term: &str, max_edits: u8, prefix_length: usize) -> Self {
        Self {
            automaton: LevenshteinAutomaton::new(
                &[],
                &term.to_lowercase(),
                max_edits,
                prefix_length
            ),
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

// #[cfg(test)]
// mod tests {
//     use super::*;

//     fn osa(a: &str, b: &str) -> usize {
//         let a: Vec<char> = a.chars().collect();
//         let b: Vec<char> = b.chars().collect();
//         let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
//         for (i, row) in d.iter_mut().enumerate() {
//             row[0] = i;
//         }
//         for (j, cell) in d[0].iter_mut().enumerate() {
//             *cell = j;
//         }
//         for i in 1..=a.len() {
//             for j in 1..=b.len() {
//                 let cost = (a[i - 1] != b[j - 1]) as usize;
//                 d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
//                 if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
//                     d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
//                 }
//             }
//         }
//         d[a.len()][b.len()]
//     }

//     #[test]
//     fn automaton_agrees_with_reference_osa() {
//         let words = [
//             "search",
//             "serach",
//             "searc",
//             "sear",
//             "seat",
//             "seraph",
//             "serac",
//             "reach",
//             "robin",
//             "bobin",
//             "café",
//             "cafe",
//             "cafë",
//             "shakespeare",
//             "shakespire",
//             "ab",
//             "ba",
//             "a",
//             "",
//             "abc",
//             "acb",
//             "bca",
//             "naïve",
//             "naive",
//             "日本語",
//             "日本",
//             "spider-man",
//             "spiderman",
//         ];
//         for q in words {
//             for k in 0..=2u8 {
//                 let aut = LevenshteinAutomaton::new(&[], q, k, 0);
//                 for w in words {
//                     let expected = osa(q, w);
//                     let got = aut.distance_to(w);
//                     if expected <= (k as usize) {
//                         assert_eq!(got, Some(expected as u8), "q={q} w={w} k={k}");
//                     } else {
//                         assert_eq!(got, None, "q={q} w={w} k={k}");
//                     }
//                 }
//             }
//         }
//     }

//     #[test]
//     fn ranking_examples() {
//         let pick = |q: &str, c: &[(&str, u32)]| {
//             let cands = c.iter().map(|(t, df)| (t.to_string(), osa(q, t) as u8, *df));
//             select_expansions(q, cands, 3)
//                 .into_iter()
//                 .map(|e| e.term)
//                 .collect::<Vec<_>>()
//         };
//         assert_eq!(
//             pick(
//                 "serach",
//                 &[
//                     ("search", 120_000),
//                     ("seraph", 900),
//                     ("serac", 30),
//                     ("reach", 40_000),
//                 ]
//             ),
//             vec!["search"]
//         );
//         assert_eq!(
//             pick(
//                 "searc",
//                 &[
//                     ("search", 120_000),
//                     ("sear", 2_000),
//                     ("seat", 30_000),
//                     ("searcy", 50),
//                 ]
//             ),
//             vec!["search"]
//         );
//         assert_eq!(pick("shakespire", &[("shakespeare", 8_000)]), vec!["shakespeare"]);
//         // A real word the user typed is always kept, first.
//         let kept = pick(
//             "serac",
//             &[
//                 ("serac", 30),
//                 ("search", 120_000),
//             ]
//         );
//         assert_eq!(kept, vec!["search".to_string(), "serac".to_string()]);
//     }

//     #[test]
//     fn weighted_costs() {
//         assert!(weighted_distance("serach", "search") < weighted_distance("seraph", "serach"));
//         assert!(weighted_distance("searc", "search") < weighted_distance("searc", "searcy") + 1e-9);
//         assert!(weighted_distance("bobin", "robin") > weighted_distance("robun", "robin"));
//         assert_eq!(weighted_distance("same", "same"), 0.0);
//     }

//     #[test]
//     fn matcher_lowercases_both_sides() {
//         let m = FuzzyMatcher::new("Batman", 1, 0);
//         assert!(m.matches("BATMEN"));
//         assert!(m.matches("batamn"));
//         assert!(!m.matches("superman"));
//     }
// }
