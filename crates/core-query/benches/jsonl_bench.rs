use std::hint::black_box;

use core_index::{
    analyzer::analyzer::Analyzer,
    document::{IndexedDocument, WeightInterval},
    lsm::LsmIndex,
};
use core_query::{Query, QueryExecutor};
use core_testkit::corpus::load_jsonl;
use criterion::{criterion_group, criterion_main, Criterion};

fn fixture_path() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../core-testkit/fixtures/generated_10k.jsonl"
    )
}

fn build_index() -> (Analyzer, LsmIndex, std::path::PathBuf) {
    let docs = load_jsonl(fixture_path()).unwrap();
    let analyzer = Analyzer::new();

    let root = std::env::temp_dir().join(format!("corelamo-jsonl-bench-{}", std::process::id()));

    std::fs::remove_dir_all(&root).ok();

    let mut index = LsmIndex::persistent(&root, 5_000).unwrap();

    for doc in docs {
        let indexed = IndexedDocument::new(doc.id)
            .with_part(1, &doc.title, WeightInterval::TITLE)
            .with_part(2, &doc.body, WeightInterval::TEXT);

        index.add_indexed_document(&analyzer, &indexed).unwrap();
    }

    index.flush().unwrap();

    (analyzer, index, root)
}

fn bench_build_10k_jsonl_index(c: &mut Criterion) {
    c.bench_function("jsonl_build_10k_index", |b| {
        b.iter(|| {
            let (analyzer, index, root) = build_index();
            black_box((analyzer, index.segment_count()));
            std::fs::remove_dir_all(root).ok();
        });
    });
}

fn bench_search_10k_jsonl_term(c: &mut Criterion) {
    let (analyzer, index, root) = build_index();
    let executor = QueryExecutor::new(&index, &analyzer);

    c.bench_function("jsonl_search_10k_term_database", |b| {
        b.iter(|| {
            black_box(executor.search(&Query::Term("database".into()), 2));
        });
    });

    std::fs::remove_dir_all(root).ok();
}

fn bench_search_10k_jsonl_and(c: &mut Criterion) {
    let (analyzer, index, root) = build_index();
    let executor = QueryExecutor::new(&index, &analyzer);

    let query = Query::And(vec![
        Query::Term("database".into()),
        Query::Term("search".into()),
    ]);

    c.bench_function("jsonl_search_10k_and_database_search", |b| {
        b.iter(|| {
            black_box(executor.search(&query, 2));
        });
    });

    std::fs::remove_dir_all(root).ok();
}

fn bench_search_10k_jsonl_term_top_k(c: &mut Criterion) {
    let (analyzer, index, root) = build_index();
    let executor = QueryExecutor::new(&index, &analyzer);

    c.bench_function("jsonl_search_10k_term_database_top_10", |b| {
        b.iter(|| {
            black_box(executor.search_top_k(&Query::Term("database".into()), 2, 10));
        });
    });

    std::fs::remove_dir_all(root).ok();
}

fn bench_search_10k_jsonl_and_top_k(c: &mut Criterion) {
    let (analyzer, index, root) = build_index();
    let executor = QueryExecutor::new(&index, &analyzer);

    let query = Query::And(vec![
        Query::Term("database".into()),
        Query::Term("search".into()),
    ]);

    c.bench_function("jsonl_search_10k_and_database_search_top_10", |b| {
        b.iter(|| {
            black_box(executor.search_top_k(&query, 2, 10));
        });
    });

    std::fs::remove_dir_all(root).ok();
}

fn bench_reopen_10k_jsonl_index(c: &mut Criterion) {
    let (_analyzer, index, root) = build_index();
    drop(index);

    c.bench_function("jsonl_reopen_10k_index", |b| {
        b.iter(|| {
            let reopened = LsmIndex::persistent(black_box(&root), 5_000).unwrap();
            black_box(reopened.segment_count());
        });
    });

    std::fs::remove_dir_all(root).ok();
}

fn and_query(terms: &[&str]) -> Query {
    Query::And(
        terms
            .iter()
            .map(|term| Query::Term((*term).into()))
            .collect(),
    )
}

fn bench_search_10k_jsonl_and_scaling(c: &mut Criterion) {
    let (analyzer, index, root) = build_index();
    let executor = QueryExecutor::new(&index, &analyzer);

    let q1 = Query::Term("database".into());
    let q2 = and_query(&["database", "search"]);
    let q3 = and_query(&["database", "search", "engine"]);
    let qrare = and_query(&["database", "search", "term300"]);
    let qrareswap = and_query(&["database", "term300", "search"]);
    let qrareswap1 = and_query(&["term300", "search", "database"]);
    let q4 = and_query(&["database", "search", "engine", "storage"]);
    let q5 = and_query(&["database", "search", "engine", "storage", "index"]);

    c.bench_function("jsonl_search_10k_1_term_database", |b| {
        b.iter(|| {
            black_box(executor.search(&q1, 2));
        });
    });

    c.bench_function("jsonl_search_10k_2_and_database_search", |b| {
        b.iter(|| {
            black_box(executor.search(&q2, 2));
        });
    });

    c.bench_function("jsonl_search_10k_3_and_database_search_engine", |b| {
        b.iter(|| {
            black_box(executor.search(&q3, 2));
        });
    });

    c.bench_function("jsonl_search_10k_3_and_database_search_engine_rare", |b| {
        b.iter(|| {
            black_box(executor.search(&qrare, 2));
        });
    });

    c.bench_function(
        "jsonl_search_10k_3_and_database_search_engine_rare_swap",
        |b| {
            b.iter(|| {
                black_box(executor.search(&qrareswap, 2));
            });
        },
    );

    c.bench_function(
        "jsonl_search_10k_3_and_database_search_engine_rare_swap1",
        |b| {
            b.iter(|| {
                black_box(executor.search(&qrareswap1, 2));
            });
        },
    );

    c.bench_function(
        "jsonl_search_10k_4_and_database_search_engine_storage",
        |b| {
            b.iter(|| {
                black_box(executor.search(&q4, 2));
            });
        },
    );

    c.bench_function(
        "jsonl_search_10k_5_and_database_search_engine_storage_index",
        |b| {
            b.iter(|| {
                black_box(executor.search(&q5, 2));
            });
        },
    );

    std::fs::remove_dir_all(root).ok();
}

fn bench_search_10k_jsonl_and_scaling_top_10(c: &mut Criterion) {
    let (analyzer, index, root) = build_index();
    let executor = QueryExecutor::new(&index, &analyzer);

    let queries = [
        (
            "jsonl_top10_1_term_database",
            Query::Term("database".into()),
        ),
        (
            "jsonl_top10_2_and_database_search",
            and_query(&["database", "search"]),
        ),
        (
            "jsonl_top10_3_and_database_search_engine",
            and_query(&["database", "search", "engine"]),
        ),
        (
            "jsonl_top10_4_and_database_search_engine_storage",
            and_query(&["database", "search", "engine", "storage"]),
        ),
        (
            "jsonl_top10_5_and_database_search_engine_storage_index",
            and_query(&["database", "search", "engine", "storage", "index"]),
        ),
    ];

    for (name, query) in queries {
        c.bench_function(name, |b| {
            b.iter(|| {
                black_box(executor.search_top_k(&query, 2, 10));
            });
        });
    }

    std::fs::remove_dir_all(root).ok();
}

criterion_group!(
    benches,
    bench_build_10k_jsonl_index,
    bench_reopen_10k_jsonl_index,
    bench_search_10k_jsonl_term,
    bench_search_10k_jsonl_term_top_k,
    bench_search_10k_jsonl_and,
    bench_search_10k_jsonl_and_top_k,
    bench_search_10k_jsonl_and_scaling,
    bench_search_10k_jsonl_and_scaling_top_10,
);
criterion_main!(benches);
