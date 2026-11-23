//! Integration tests for ranking: a tiny in-memory index, real queries, checked by doc id.

use core_index::{
    analyzer::analyzer::AnalyzerSet, array_rows::ArrayRowAllocator, document::IndexPolicy,
    mem::MemIndex,
};
use core_protocol::command_reponse_definitions::MatchMode;
use core_query::{executor::QueryExecutor, resolver::compile_query};
use core_storage::{json_indexing_helper::index_document, json_parse::parse_into_node};

use std::sync::atomic::{AtomicU64, Ordering};

/// A unique empty directory to hold this test's xpath registry.
fn temp_root() -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("riets-ranking-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Two text fields, no `id` (the policy adds an auto one).
const POLICY: &str = r#"{
  "analyzers": {
    "$default": { "tokenizer": "word_delimiter", "stemming": null, "ascii_folding": true }
  },
  "document": {
    "title": { "$type": "text", "$weight": { "min": 5, "max": 10 } },
    "body":  { "$type": "text", "$weight": { "min": 1, "max": 3 } }
  }
}"#;

/// (title, body) of the three test documents, doc ids 1..=3.
const DOCS: &[(&str, &str)] = &[
    ("red fox", "the quick brown fox jumps"),
    ("blue whale", "the whale swims in the ocean"),
    ("red whale", "a red whale near the fox"),
];

/// Parses a JSON string into simd-json's owned value.
fn owned(json: &str) -> simd_json::OwnedValue {
    let mut bytes = json.as_bytes().to_vec();
    simd_json::to_owned_value(&mut bytes).expect("valid json")
}

/// Builds the in-memory index and its analyzer set from `POLICY`.
fn build() -> (MemIndex, AnalyzerSet, IndexPolicy) {
    let mut policy = IndexPolicy::from_json(POLICY).expect("policy parses");
    policy.resolve(temp_root()).expect("policy resolves xpaths");
    let analyzers = AnalyzerSet::from_policy(&policy.analyzers);
    let mut index = MemIndex::new();
    let mut allocator = ArrayRowAllocator::new();
    for (i, (title, body)) in DOCS.iter().enumerate() {
        let value = owned(&format!(r#"{{"title":"{title}","body":"{body}"}}"#));
        let parsed = parse_into_node(&value, &policy).expect("doc maps to the policy");
        let doc = index_document(i as u64 + 1, &parsed, &policy, &mut allocator);
        index.add_indexed_document(&analyzers, &doc);
    }
    (index, analyzers, policy)
}

/// Runs one request through compile → rank, returning doc ids in rank order.
fn search(
    index: &MemIndex,
    analyzers: &AnalyzerSet,
    policy: &IndexPolicy,
    request: &str,
    k: usize,
) -> Vec<u64> {
    let value = owned(request);
    let groups =
        compile_query(&value, None, analyzers, policy, None, true).expect("query compiles");
    let group = &groups[0];
    let executor = QueryExecutor::new(index, &group.analyzer);
    executor
        .rank_retrieve(
            group.query.as_ref(),
            &group.ctxs,
            k,
            None,
            MatchMode::CrossFields,
        )
        .into_iter()
        .map(|hit| hit.doc_id)
        .collect()
}

/// Doc ids as a sorted set.
fn ids(index: &MemIndex, analyzers: &AnalyzerSet, policy: &IndexPolicy, request: &str) -> Vec<u64> {
    let mut ids = search(index, analyzers, policy, request, 10);
    ids.sort_unstable();
    ids
}

#[test]
fn term_matches_every_field() {
    let (index, analyzers, policy) = build();
    assert_eq!(ids(&index, &analyzers, &policy, r#""fox""#), vec![1, 3]);
}

#[test]
fn and_requires_every_word() {
    let (index, analyzers, policy) = build();
    assert_eq!(
        ids(&index, &analyzers, &policy, r#"{"AND":["red","fox"]}"#),
        vec![1, 3]
    );
}

#[test]
fn or_matches_any_word() {
    let (index, analyzers, policy) = build();
    assert_eq!(
        ids(&index, &analyzers, &policy, r#"{"OR":["whale","fox"]}"#),
        vec![1, 2, 3]
    );
}

#[test]
fn nested_and_of_or_intersects() {
    let (index, analyzers, policy) = build();
    assert_eq!(
        ids(
            &index,
            &analyzers,
            &policy,
            r#"{"AND":[{"OR":["whale","fox"]},"red"]}"#
        ),
        vec![1, 3]
    );
}

#[test]
fn phrase_matches_adjacent_words() {
    let (index, analyzers, policy) = build();
    assert_eq!(
        ids(&index, &analyzers, &policy, r#"{"phrase":"red fox"}"#),
        vec![1]
    );
}

#[test]
fn top_k_returns_at_most_k() {
    let (index, analyzers, policy) = build();
    let hits = search(
        &index,
        &analyzers,
        &policy,
        r#"{"OR":["red","whale","fox"]}"#,
        1,
    );
    assert_eq!(hits.len(), 1);
}

#[test]
fn filter_only_returns_everything_without_a_query() {
    let (index, analyzers, policy) = build();
    // `null` = blank query = match-all
    let hits = search(&index, &analyzers, &policy, "null", 10);
    assert_eq!(hits.len(), 3);
}
