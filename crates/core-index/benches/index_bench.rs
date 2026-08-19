use std::{hint::black_box, sync::Arc};

use core_index::{analyzer::analyzer::Analyzer, mem::MemIndex, search::SearchIndex};
use criterion::{Criterion, criterion_group, criterion_main};

fn bench_index_large_document(c: &mut Criterion) {
    let analyzer = Analyzer::new();
    let text = "database rust engine ".repeat(100_000);

    c.bench_function("index_large_document", |b| {
        b.iter(|| {
            let mut index = MemIndex::new();
            index.add_document(&analyzer, 1, 0, black_box(&text));
            black_box(index);
        });
    });
}

fn bench_term_lookup(c: &mut Criterion) {
    let analyzer = Analyzer::new();
    let mut index = MemIndex::new();
    let text = "database rust engine ".repeat(100_000);

    index.add_document(&analyzer, 1, 0, &text);

    c.bench_function("term_lookup", |b| {
        b.iter(|| {
            black_box(index.lookup("database", 0));
        });
    });
}

fn bench_realistic_document(c: &mut Criterion) {
    let analyzer = Analyzer::new();
    let text = include_str!("fixtures/wiki.txt");

    corpus_stats("wiki", text, &analyzer);

    c.bench_function("index_realistic_document", |b| {
        b.iter(|| {
            let mut index = MemIndex::new();
            index.add_document(&analyzer, 1, 0, black_box(text));
            black_box(index);
        });
    });
}

fn generate_unique_terms(count: usize) -> String {
    (0..count)
        .map(|i| format!("term{}", i))
        .collect::<Vec<_>>()
        .join(" ")
}

fn bench_index_unique_terms(c: &mut Criterion) {
    let analyzer = Analyzer::new();
    let text = generate_unique_terms(100_000);

    c.bench_function("index_unique_terms_100k", |b| {
        b.iter(|| {
            let mut index = MemIndex::new();
            index.add_document(&analyzer, 1, 0, black_box(&text));
            black_box(index);
        });
    });
}

fn corpus_stats(name: &str, text: &str, analyzer: &Analyzer) {
    let tokens = analyzer.analyze(text);
    let unique = tokens
        .iter()
        .map(|t| &t.text)
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    println!(
        "{name}: bytes={}, tokens={}, unique_terms={}",
        text.len(),
        tokens.len(),
        unique
    );
}

fn bench_lookup_unique_terms_100k(c: &mut Criterion) {
    let analyzer = Analyzer::new();
    let mut index = MemIndex::new();
    let text = generate_unique_terms(100_000);

    index.add_document(&analyzer, 1, 0, &text);

    c.bench_function("lookup_unique_terms_100k_last", |b| {
        b.iter(|| {
            black_box(index.lookup("term99999", 0));
        });
    });
}

fn bench_prefix_lookup_unique_terms_100k(c: &mut Criterion) {
    let analyzer = Analyzer::new();
    let mut index = MemIndex::new();
    let text = generate_unique_terms(100_000);

    index.add_document(&analyzer, 1, 0, &text);

    c.bench_function("prefix_lookup_unique_terms_100k_term9", |b| {
        b.iter(|| {
            black_box(index.lookup_prefix("term9", 0));
        });
    });
}

fn bench_wildcard_lookup_unique_terms_100k(c: &mut Criterion) {
    let analyzer = Analyzer::new();
    let mut index = MemIndex::new();
    let text = generate_unique_terms(100_000);
    let pattern = core_index::wildcard::WildcardPattern::parse("term9*");

    index.add_document(&analyzer, 1, 0, &text);

    c.bench_function("wildcard_lookup_unique_terms_100k_term9_star", |b| {
        b.iter(|| {
            black_box(index.lookup_wildcard(&pattern, 0));
        });
    });
}

fn bench_prefix_lookup_unique_terms_100k_distinct_docs(c: &mut Criterion) {
    let analyzer = Analyzer::new();
    let mut index = MemIndex::new();

    for i in 0..100_000u64 {
        let text = format!("term{i}");
        index.add_document(&analyzer, i + 1, 0, &text);
    }

    c.bench_function("prefix_lookup_100k_distinct_docs_term9", |b| {
        b.iter(|| {
            black_box(index.lookup_prefix("term9", 0));
        });
    });
}

fn bench_snapshot_lookup_many_segments(c: &mut Criterion) {
    use core_index::{lsm::IndexSnapshot, posting::DeleteSet};

    let analyzer = Analyzer::new();
    let mut segments = Vec::new();

    for seg_id in 0..20u64 {
        let mut mem = MemIndex::new();

        for i in 0..5_000u64 {
            let doc_id = seg_id * 5_000 + i + 1;
            let text = format!("term{i} database");
            mem.add_document(&analyzer, doc_id, 0, &text);
        }

        segments.push(Arc::new(mem.freeze()) as Arc<dyn SearchIndex + Send + Sync>);
    }

    let snapshot = IndexSnapshot::new(MemIndex::new(), segments, DeleteSet::new());

    c.bench_function("snapshot_lookup_20_segments_database", |b| {
        b.iter(|| {
            black_box(snapshot.lookup("database", 0));
        });
    });
}

fn bench_disk_term_lookup(c: &mut Criterion) {
    use core_index::{
        disk::{reader::DiskSegment, writer::write_segment},
        search::SearchIndex,
    };

    let analyzer = Analyzer::new();
    let mut index = MemIndex::new();
    let text = generate_unique_terms(100_000);

    index.add_document(&analyzer, 1, 0, &text);
    let segment = index.freeze();

    let path = std::env::temp_dir().join(format!("corelamo-bench-disk-{}.idx", std::process::id()));

    write_segment(&path, &segment).unwrap();
    let disk = DiskSegment::open(&path).unwrap();

    c.bench_function("disk_term_lookup_100k_last", |b| {
        b.iter(|| {
            black_box(disk.lookup("term99999", 0));
        });
    });

    std::fs::remove_file(path).unwrap();
}

fn bench_disk_prefix_lookup_unique_terms_100k(c: &mut Criterion) {
    use core_index::{
        disk::{reader::DiskSegment, writer::write_segment},
        search::SearchIndex,
    };

    let analyzer = Analyzer::new();
    let mut index = MemIndex::new();
    let text = generate_unique_terms(100_000);

    index.add_document(&analyzer, 1, 0, &text);
    let segment = index.freeze();

    let path = std::env::temp_dir().join(format!(
        "corelamo-bench-disk-prefix-{}.idx",
        std::process::id()
    ));

    write_segment(&path, &segment).unwrap();
    let disk = DiskSegment::open(&path).unwrap();

    c.bench_function("disk_prefix_lookup_unique_terms_100k_term9", |b| {
        b.iter(|| {
            black_box(disk.lookup_prefix("term9", 0));
        });
    });

    std::fs::remove_file(path).unwrap();
}

fn bench_disk_write_segment_100k_unique_terms(c: &mut Criterion) {
    use core_index::disk::writer::write_segment;

    let analyzer = Analyzer::new();
    let mut index = MemIndex::new();
    let text = generate_unique_terms(100_000);

    index.add_document(&analyzer, 1, 0, &text);
    let segment = index.freeze();

    c.bench_function("disk_write_segment_100k_unique_terms", |b| {
        b.iter(|| {
            let path = std::env::temp_dir().join(format!(
                "corelamo-bench-write-{}-{}.idx",
                std::process::id(),
                uuid_like()
            ));

            write_segment(&path, black_box(&segment)).unwrap();
            black_box(std::fs::metadata(&path).unwrap().len());

            std::fs::remove_file(path).unwrap();
        });
    });
}

fn bench_disk_open_segment_100k_unique_terms(c: &mut Criterion) {
    use core_index::disk::{reader::DiskSegment, writer::write_segment};

    let analyzer = Analyzer::new();
    let mut index = MemIndex::new();
    let text = generate_unique_terms(100_000);

    index.add_document(&analyzer, 1, 0, &text);
    let segment = index.freeze();

    let path = std::env::temp_dir().join(format!("corelamo-bench-open-{}.idx", std::process::id()));

    write_segment(&path, &segment).unwrap();

    c.bench_function("disk_open_segment_100k_unique_terms", |b| {
        b.iter(|| {
            let disk = DiskSegment::open(black_box(&path)).unwrap();
            black_box(disk);
        });
    });

    std::fs::remove_file(path).unwrap();
}

fn uuid_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn bench_disk_encode_segment_100k_unique_terms(c: &mut Criterion) {
    use core_index::disk::writer::write_segment_to;
    use std::io::Cursor;

    let analyzer = Analyzer::new();
    let mut index = MemIndex::new();
    let text = generate_unique_terms(100_000);

    index.add_document(&analyzer, 1, 0, &text);
    let segment = index.freeze();

    c.bench_function("disk_encode_segment_100k_unique_terms", |b| {
        b.iter(|| {
            let mut out = Cursor::new(Vec::new());
            write_segment_to(&mut out, black_box(&segment)).unwrap();
            black_box(out.into_inner());
        });
    });
}

fn bench_disk_write_overwrite_segment_100k_unique_terms(c: &mut Criterion) {
    use core_index::disk::writer::write_segment;

    let analyzer = Analyzer::new();
    let mut index = MemIndex::new();
    let text = generate_unique_terms(100_000);

    index.add_document(&analyzer, 1, 0, &text);
    let segment = index.freeze();

    let path = std::env::temp_dir().join(format!(
        "corelamo-bench-overwrite-{}.idx",
        std::process::id()
    ));

    c.bench_function("disk_write_overwrite_segment_100k_unique_terms", |b| {
        b.iter(|| {
            write_segment(&path, black_box(&segment)).unwrap();
        });
    });

    println!("segment bytes={}", std::fs::metadata(&path).unwrap().len());

    std::fs::remove_file(path).ok();
}

fn build_persistent_lsm_many_segments(
    segment_count: u64,
    docs_per_segment: u64,
) -> (core_index::lsm::LsmIndex, std::path::PathBuf) {
    use core_index::lsm::LsmIndex;

    let analyzer = Analyzer::new();

    let root = std::env::temp_dir().join(format!(
        "corelamo-bench-compact-{}-{}",
        std::process::id(),
        uuid_like()
    ));

    std::fs::remove_dir_all(&root).ok();

    let mut lsm = LsmIndex::persistent(&root, 1_000_000).unwrap();

    for seg_id in 0..segment_count {
        for i in 0..docs_per_segment {
            let doc_id = seg_id * docs_per_segment + i + 1;
            let text = format!(
                "document {doc_id} rust database search engine term{}",
                i % 100
            );
            lsm.add_document(&analyzer, doc_id, 0, &text).unwrap();
        }

        lsm.flush().unwrap();
    }

    (lsm, root)
}

fn bench_persistent_lookup_before_compaction(c: &mut Criterion) {
    use core_index::search::SearchIndex;

    let (lsm, root) = build_persistent_lsm_many_segments(20, 500);

    c.bench_function(
        "persistent_lookup_20_segments_database_before_compaction",
        |b| {
            b.iter(|| {
                black_box(lsm.lookup("database", 0));
            });
        },
    );

    std::fs::remove_dir_all(root).ok();
}

fn bench_persistent_compact_20_segments(c: &mut Criterion) {
    c.bench_function("persistent_compact_20_segments_10k_docs", |b| {
        b.iter(|| {
            let (mut lsm, root) = build_persistent_lsm_many_segments(20, 500);

            lsm.compact_all().unwrap();

            black_box(lsm.segment_count());

            std::fs::remove_dir_all(root).ok();
        });
    });
}

fn bench_persistent_lookup_after_compaction(c: &mut Criterion) {
    use core_index::search::SearchIndex;

    let (mut lsm, root) = build_persistent_lsm_many_segments(20, 500);

    lsm.compact_all().unwrap();

    c.bench_function(
        "persistent_lookup_1_segment_database_after_compaction",
        |b| {
            b.iter(|| {
                black_box(lsm.lookup("database", 0));
            });
        },
    );

    std::fs::remove_dir_all(root).ok();
}

fn build_persistent_lsm_rare_term_in_one_segment(
    segment_count: u64,
    docs_per_segment: u64,
) -> (core_index::lsm::LsmIndex, std::path::PathBuf) {
    use core_index::lsm::LsmIndex;

    let analyzer = Analyzer::new();

    let root = std::env::temp_dir().join(format!(
        "corelamo-bench-rare-compact-{}-{}",
        std::process::id(),
        uuid_like()
    ));

    std::fs::remove_dir_all(&root).ok();

    let mut lsm = LsmIndex::persistent(&root, 1_000_000).unwrap();

    for seg_id in 0..segment_count {
        for i in 0..docs_per_segment {
            let doc_id = seg_id * docs_per_segment + i + 1;

            let text = if seg_id == segment_count - 1 {
                format!("document {doc_id} rareterm database search")
            } else {
                format!("document {doc_id} commonterm database search")
            };

            lsm.add_document(&analyzer, doc_id, 0, &text).unwrap();
        }

        lsm.flush().unwrap();
    }

    (lsm, root)
}

fn bench_persistent_rare_lookup_before_compaction(c: &mut Criterion) {
    use core_index::search::SearchIndex;

    let (lsm, root) = build_persistent_lsm_rare_term_in_one_segment(20, 500);

    c.bench_function(
        "persistent_lookup_20_segments_rareterm_before_compaction",
        |b| {
            b.iter(|| {
                black_box(lsm.lookup("rareterm", 0));
            });
        },
    );

    std::fs::remove_dir_all(root).ok();
}

fn bench_persistent_rare_lookup_after_compaction(c: &mut Criterion) {
    use core_index::search::SearchIndex;

    let (mut lsm, root) = build_persistent_lsm_rare_term_in_one_segment(20, 500);

    lsm.compact_all().unwrap();

    c.bench_function(
        "persistent_lookup_1_segment_rareterm_after_compaction",
        |b| {
            b.iter(|| {
                black_box(lsm.lookup("rareterm", 0));
            });
        },
    );

    std::fs::remove_dir_all(root).ok();
}

fn bench_persistent_reopen_before_compaction(c: &mut Criterion) {
    let (_lsm, root) = build_persistent_lsm_many_segments(20, 500);

    c.bench_function("persistent_reopen_20_segments_before_compaction", |b| {
        b.iter(|| {
            let reopened =
                core_index::lsm::LsmIndex::persistent(black_box(&root), 1_000_000).unwrap();

            black_box(reopened.segment_count());
        });
    });

    std::fs::remove_dir_all(root).ok();
}

fn bench_persistent_reopen_after_compaction(c: &mut Criterion) {
    let (mut lsm, root) = build_persistent_lsm_many_segments(20, 500);

    lsm.compact_all().unwrap();
    drop(lsm);

    c.bench_function("persistent_reopen_1_segment_after_compaction", |b| {
        b.iter(|| {
            let reopened =
                core_index::lsm::LsmIndex::persistent(black_box(&root), 1_000_000).unwrap();

            black_box(reopened.segment_count());
        });
    });

    std::fs::remove_dir_all(root).ok();
}

criterion_group!(
    benches,
    bench_disk_term_lookup,
    bench_disk_prefix_lookup_unique_terms_100k,
    bench_disk_write_segment_100k_unique_terms,
    bench_disk_open_segment_100k_unique_terms,
    bench_disk_encode_segment_100k_unique_terms,
    bench_disk_write_overwrite_segment_100k_unique_terms,
    bench_persistent_lookup_before_compaction,
    bench_persistent_compact_20_segments,
    bench_persistent_lookup_after_compaction,
    bench_persistent_rare_lookup_before_compaction,
    bench_persistent_rare_lookup_after_compaction,
    bench_persistent_reopen_before_compaction,
    bench_persistent_reopen_after_compaction,
);
criterion_main!(benches);
