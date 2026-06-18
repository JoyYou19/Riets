use std::hint::black_box;

use core_index::{
    analyzer::analyzer::Analyzer,
    document::{IndexedDocument, WeightInterval},
    mem::MemIndex,
};
use core_query::{Query, QueryExecutor};
use criterion::{criterion_group, criterion_main, Criterion};

fn build_mem_index() -> (Analyzer, MemIndex) {
    let analyzer = Analyzer::new();
    let mut index = MemIndex::new();

    for doc_id in 1..=10_000 {
        let doc =
            IndexedDocument::new(doc_id).with_part(1, "rust database engine", WeightInterval::TEXT);

        index.add_indexed_document(&analyzer, &doc);
    }

    (analyzer, index)
}

fn bench_term_search(c: &mut Criterion) {
    let (analyzer, index) = build_mem_index();
    let executor = QueryExecutor::new(&index, &analyzer);

    c.bench_function("term_search_10k_docs", |b| {
        b.iter(|| {
            black_box(executor.search(&Query::Term("database".into()), 1));
        });
    });
}

fn bench_and_search(c: &mut Criterion) {
    let (analyzer, index) = build_mem_index();
    let executor = QueryExecutor::new(&index, &analyzer);

    let query = Query::And(vec![
        Query::Term("rust".into()),
        Query::Term("database".into()),
    ]);

    c.bench_function("and_search_10k_docs", |b| {
        b.iter(|| {
            black_box(executor.search(&query, 1));
        });
    });
}

fn bench_and_search_scored(c: &mut Criterion) {
    let (analyzer, index) = build_mem_index();
    let executor = QueryExecutor::new(&index, &analyzer);

    let query = Query::And(vec![
        Query::Term("rust".into()),
        Query::Term("database".into()),
    ]);

    c.bench_function("and_search_scored_10k_docs", |b| {
        b.iter(|| {
            black_box(executor.search(&query, 1));
        });
    });
}

fn bench_or_search(c: &mut Criterion) {
    let (analyzer, index) = build_mem_index();
    let executor = QueryExecutor::new(&index, &analyzer);

    let query = Query::Or(vec![
        Query::Term("rust".into()),
        Query::Term("engine".into()),
    ]);

    c.bench_function("or_search_10k_docs", |b| {
        b.iter(|| black_box(executor.search(&query, 1)));
    });
}

fn bench_phrase_search(c: &mut Criterion) {
    let (analyzer, index) = build_mem_index();
    let executor = QueryExecutor::new(&index, &analyzer);

    let query = Query::Phrase(vec!["rust".into(), "database".into()]);

    c.bench_function("phrase_search_10k_docs", |b| {
        b.iter(|| black_box(executor.search(&query, 1)));
    });
}

fn bench_prefix_search(c: &mut Criterion) {
    let (analyzer, index) = build_mem_index();
    let executor = QueryExecutor::new(&index, &analyzer);

    c.bench_function("prefix_search_10k_docs", |b| {
        b.iter(|| black_box(executor.search(&Query::Prefix("dat".into()), 1)));
    });
}

fn bench_far_and_search(c: &mut Criterion) {
    let analyzer = Analyzer::new();
    let mut index = MemIndex::new();

    for doc_id in 1..=10_000 {
        let doc = IndexedDocument::new(doc_id).with_part(
            1,
            "rust x x x x x x x x x database",
            WeightInterval::TEXT,
        );

        index.add_indexed_document(&analyzer, &doc);
    }

    let executor = QueryExecutor::new(&index, &analyzer);

    let query = Query::And(vec![
        Query::Term("rust".into()),
        Query::Term("database".into()),
    ]);

    c.bench_function("and_search_far_positions_10k_docs", |b| {
        b.iter(|| black_box(executor.search(&query, 1)));
    });
}

criterion_group!(
    benches,
    bench_term_search,
    bench_and_search,
    bench_or_search,
    bench_phrase_search,
    bench_prefix_search,
    bench_far_and_search,
);
criterion_main!(benches);
