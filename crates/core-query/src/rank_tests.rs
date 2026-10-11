//! Ranking checked against a brute-force model of the scoring: a search returns the exact top k of
//! the BM25+ scores, so a smaller page is always a prefix of a larger one.

use std::{collections::HashMap, sync::Arc};

use ahash::{HashSet, HashSetExt};
use core_index::{
    analyzer::Analyzer,
    document::{DocumentPart, IndexedDocument, WeightInterval},
    lsm::IndexSnapshot,
    mem::MemIndex,
    posting::DeleteSet,
    types::{DocId, ShardId, XPathId, make_doc_id},
};

use core_protocol::command_reponse_definitions::MatchMode;

use crate::{Query, QueryExecutor, resolver::FieldCtx, scorer::bm25_score_scaled};

const TITLE: XPathId = 11;
const TEXT: XPathId = 12;

const VOCABULARY: [&str; 26] = [
    "penguin", "emperor", "king", "world", "war", "english", "river", "music", "film", "island",
    "bird", "ocean", "battle", "bottle", "cattle", "castle", "crown", "queen", "north", "south",
    "winter", "summer", "garden", "train", "rocket", "violin",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    // Zipf-like: early vocabulary words are much more common, as in real text.
    fn word(&mut self) -> &'static str {
        let weights: Vec<f64> = (0..VOCABULARY.len())
            .map(|i| 1.0 / (i as f64 + 1.0))
            .collect();
        let total: f64 = weights.iter().sum();
        let mut pick = (self.next() as f64 / u64::MAX as f64) * total;
        for (index, weight) in weights.iter().enumerate() {
            if pick < *weight {
                return VOCABULARY[index];
            }
            pick -= weight;
        }
        VOCABULARY[VOCABULARY.len() - 1]
    }

    fn words(&mut self, min: usize, max: usize) -> String {
        let count = min + self.below(max - min + 1);
        (0..count)
            .map(|_| self.word())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Clone)]
struct SourceDoc {
    title: String,
    text: String,
}

fn corpus(seed: u64, docs: usize) -> Vec<SourceDoc> {
    let mut rng = Rng(seed);
    (0..docs)
        .map(|_| SourceDoc {
            title: rng.words(1, 4),
            text: rng.words(3, 30),
        })
        .collect()
}

#[derive(Clone, Copy)]
struct Weights {
    title: u16,
    text: u16,
}

struct Shard {
    snapshot: IndexSnapshot,
    // doc id -> source document index
    sources: HashMap<DocId, usize>,
}

// Builds one shard holding `members` (source indices); `deleted` members are indexed and then
// deleted, as an update or delete leaves them until compaction.
fn build_shard(
    shard: u16,
    docs: &[SourceDoc],
    members: &[usize],
    deleted: &HashSet<usize>,
    weights: Weights,
    analyzer: &Analyzer,
) -> Shard {
    let mut mem = MemIndex::new();
    let mut delete_set = DeleteSet::new();
    let mut sources = HashMap::new();
    for (local, &source) in members.iter().enumerate() {
        let doc_id = make_doc_id(ShardId(shard), local as u64);
        let mut document = IndexedDocument::new(doc_id);
        document.parts.push(DocumentPart {
            xpath: TITLE,
            text: docs[source].title.clone(),
            weight: WeightInterval::new(weights.title, weights.title),
            exact: false,
        });
        document.parts.push(DocumentPart {
            xpath: TEXT,
            text: docs[source].text.clone(),
            weight: WeightInterval::new(weights.text, weights.text),
            exact: false,
        });
        mem.add_indexed_document(analyzer, &document);
        if deleted.contains(&source) {
            delete_set.delete(doc_id);
        } else {
            sources.insert(doc_id, source);
        }
    }
    Shard {
        snapshot: IndexSnapshot::new(Arc::new(mem), Arc::new(Vec::new()), delete_set),
        sources,
    }
}

fn build_shards(
    docs: &[SourceDoc],
    shard_count: usize,
    placement: impl Fn(usize) -> usize,
    deleted: &HashSet<usize>,
    weights: Weights,
    analyzer: &Analyzer,
) -> Vec<Shard> {
    let mut members = vec![Vec::new(); shard_count];
    for source in 0..docs.len() {
        members[placement(source) % shard_count].push(source);
    }
    members
        .iter()
        .enumerate()
        .map(|(shard, members)| {
            build_shard(shard as u16, docs, members, deleted, weights, analyzer)
        })
        .collect()
}

fn ctxs() -> Vec<FieldCtx> {
    vec![
        FieldCtx {
            xpath: TITLE,
            exact_xpath: None,
            row_keyed: false,
        },
        FieldCtx {
            xpath: TEXT,
            exact_xpath: None,
            row_keyed: false,
        },
    ]
}

// The score a hit carries: the exact total in thousandths, as a float.
fn shown(total_milli: u64) -> f32 {
    total_milli as f32 / 1000.0
}

struct Searched {
    // (source index, doc id, score) in rank order
    hits: Vec<(usize, DocId, f32)>,
}

// What the shard manager does with the shards' answers: merge by (score, doc id), keep k.
fn search(
    shards: &[Shard],
    analyzer: &Analyzer,
    query: &Query,
    k: usize,
    restrict: Option<&HashSet<usize>>,
    mode: MatchMode,
) -> Searched {
    let ctxs = ctxs();
    let mut merged = Vec::new();
    for shard in shards {
        let shard_restrict: Option<HashSet<DocId>> = restrict.map(|sources| {
            shard
                .sources
                .iter()
                .filter(|(_, source)| sources.contains(source))
                .map(|(&doc_id, _)| doc_id)
                .collect()
        });
        let executor = QueryExecutor::new(&shard.snapshot, analyzer, Vec::new());
        for hit in executor.rank(Some(query), &ctxs, k, shard_restrict.as_ref(), false, mode) {
            let source = shard.sources[&hit.doc_id];
            merged.push((hit, source));
        }
    }
    merged.sort_by(|(a, _), (b, _)| {
        b.score
            .partial_cmp(&a.score)
            .unwrap()
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    });
    merged.truncate(k);
    Searched {
        hits: merged
            .into_iter()
            .map(|(hit, source)| (source, hit.doc_id, hit.score))
            .collect(),
    }
}

fn analyzed(analyzer: &Analyzer, word: &str) -> String {
    analyzer
        .analyze_query(word)
        .into_iter()
        .next()
        .map(|token| token.text)
        .expect("vocabulary words are not stopwords")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Combine {
    Any,
    // every term in the same field (best_fields)
    AllInOneField,
    // every term in some field (cross_fields)
    AllInDocument,
}

// Scores every live document straight from the definitions: tokens per field, corpus totals,
// document frequencies, BM25+ per (field, term), summed.
fn brute_force(
    docs: &[SourceDoc],
    deleted: &HashSet<usize>,
    weights: Weights,
    analyzer: &Analyzer,
    terms: &[String],
    combine: Combine,
) -> HashMap<usize, u64> {
    let live: Vec<usize> = (0..docs.len()).filter(|i| !deleted.contains(i)).collect();
    let tokens = |text: &str| -> Vec<String> {
        analyzer
            .analyze(text)
            .into_iter()
            .map(|token| token.text)
            .collect()
    };
    let fields: [(XPathId, u16, Vec<Vec<String>>); 2] = [
        (
            TITLE,
            weights.title,
            live.iter().map(|&i| tokens(&docs[i].title)).collect(),
        ),
        (
            TEXT,
            weights.text,
            live.iter().map(|&i| tokens(&docs[i].text)).collect(),
        ),
    ];

    let mut totals: HashMap<usize, u64> = HashMap::new();
    let mut found: HashMap<usize, Vec<bool>> = HashMap::new();
    for (_, weight, field_tokens) in &fields {
        let doc_count = field_tokens.len() as u64;
        let total_len: u64 = field_tokens.iter().map(|t| t.len() as u64).sum();
        let avg = if doc_count == 0 {
            0.0
        } else {
            total_len as f32 / doc_count as f32
        };
        let doc_freq = |term: &String| field_tokens.iter().filter(|t| t.contains(term)).count();
        for (position, doc_tokens) in field_tokens.iter().enumerate() {
            let tfs: Vec<u32> = terms
                .iter()
                .map(|term| doc_tokens.iter().filter(|t| *t == term).count() as u32)
                .collect();
            let matches = match combine {
                Combine::Any | Combine::AllInDocument => tfs.iter().any(|&tf| tf > 0),
                Combine::AllInOneField => tfs.iter().all(|&tf| tf > 0),
            };
            if !matches {
                continue;
            }
            let seen = found
                .entry(live[position])
                .or_insert_with(|| vec![false; terms.len()]);
            for (seen, &tf) in seen.iter_mut().zip(&tfs) {
                *seen |= tf > 0;
            }
            let mut field_score = 0u64;
            for (term, &tf) in terms.iter().zip(&tfs) {
                if tf == 0 {
                    continue;
                }
                field_score = field_score.saturating_add(bm25_score_scaled(
                    tf,
                    *weight,
                    doc_tokens.len() as u32,
                    avg,
                    doc_count,
                    doc_freq(term) as u32,
                ));
            }
            *totals.entry(live[position]).or_default() += field_score;
        }
    }
    if combine == Combine::AllInDocument {
        totals.retain(|doc, _| found[doc].iter().all(|&seen| seen));
    }
    totals
}

// The expected ranking: best display score first, then lowest doc id, as the system orders.
fn expected_top_k(
    totals: &HashMap<usize, u64>,
    shards: &[Shard],
    restrict: Option<&HashSet<usize>>,
    k: usize,
) -> Vec<(usize, DocId, f32)> {
    let doc_ids: HashMap<usize, DocId> = shards
        .iter()
        .flat_map(|shard| shard.sources.iter().map(|(&doc, &source)| (source, doc)))
        .collect();
    let mut expected: Vec<(usize, DocId, f32)> = totals
        .iter()
        .filter(|(source, _)| restrict.is_none_or(|allowed| allowed.contains(source)))
        .map(|(&source, &total)| (source, doc_ids[&source], shown(total)))
        .collect();
    expected.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap().then_with(|| a.1.cmp(&b.1)));
    expected.truncate(k);
    expected
}

fn random_terms(rng: &mut Rng, analyzer: &Analyzer, count: usize) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    while terms.len() < count {
        let term = analyzed(analyzer, rng.word());
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

fn random_restrict(rng: &mut Rng, docs: usize) -> HashSet<usize> {
    (0..docs).filter(|_| rng.below(3) == 0).collect()
}

const FEVER_LIKE: Weights = Weights { title: 40, text: 1 };
const BALANCED: Weights = Weights { title: 3, text: 2 };

#[test]
fn ranking_equals_brute_force_for_every_k() {
    let analyzer = Analyzer::new();
    let mut rng = Rng(7);
    for (round, weights) in [FEVER_LIKE, BALANCED, FEVER_LIKE, BALANCED]
        .into_iter()
        .enumerate()
    {
        let docs = corpus(100 + round as u64, 400);
        let none = HashSet::new();
        let shards = build_shards(&docs, 1, |_| 0, &none, weights, &analyzer);
        for _ in 0..16 {
            let term_count = 1 + rng.below(3);
            let terms = random_terms(&mut rng, &analyzer, term_count);
            let all = terms.len() > 1 && rng.below(2) == 0;
            let query = match (all, terms.len()) {
                (_, 1) => Query::Term(terms[0].clone()),
                (true, _) => Query::And(terms.iter().cloned().map(Query::Term).collect()),
                (false, _) => Query::Or(terms.iter().cloned().map(Query::Term).collect()),
            };
            let restrict = (rng.below(4) == 0).then(|| random_restrict(&mut rng, docs.len()));

            for (mode, combine_all) in [
                (MatchMode::BestFields, Combine::AllInOneField),
                (MatchMode::CrossFields, Combine::AllInDocument),
            ] {
                let combine = if all { combine_all } else { Combine::Any };
                let totals = brute_force(&docs, &none, weights, &analyzer, &terms, combine);
                for k in [1, 2, 5, 10, 20, 40, 1000] {
                    let got = search(&shards, &analyzer, &query, k, restrict.as_ref(), mode);
                    let want = expected_top_k(&totals, &shards, restrict.as_ref(), k);
                    assert_eq!(
                        got.hits, want,
                        "query {query:?}, {mode:?}, k {k}, round {round}"
                    );
                }
            }
        }
    }
}

#[test]
fn smaller_pages_are_prefixes_of_larger_pages() {
    let analyzer = Analyzer::new();
    let docs = corpus(31, 600);
    let none = HashSet::new();
    let shards = build_shards(&docs, 1, |_| 0, &none, FEVER_LIKE, &analyzer);
    let term = |word: &str| Query::Term(analyzed(&analyzer, word));
    let queries = [
        term("penguin"),
        Query::And(vec![term("world"), term("war")]),
        Query::Or(vec![term("emperor"), term("penguin")]),
        Query::Or(vec![term("world"), term("war"), term("battle")]),
    ];
    for mode in [MatchMode::BestFields, MatchMode::CrossFields] {
        for query in &queries {
            let all = search(&shards, &analyzer, query, 200, None, mode).hits;
            assert!(!all.is_empty(), "{query:?} matched nothing");
            for k in 1..=all.len().min(60) {
                let page = search(&shards, &analyzer, query, k, None, mode).hits;
                assert_eq!(page, all[..k], "{query:?}, {mode:?}, k {k}");
            }
        }
    }
}
