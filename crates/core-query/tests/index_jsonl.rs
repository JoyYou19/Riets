use core_index::{
    analyzer::analyzer::Analyzer,
    document::{IndexedDocument, WeightInterval},
    lsm::LsmIndex,
};
use core_query::{Query, QueryExecutor};
use core_testkit::corpus::load_jsonl;

#[test]
fn indexes_and_searches_jsonl_fixture() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../core-testkit/fixtures/tiny.jsonl"
    );

    let docs = load_jsonl(fixture).unwrap();

    let analyzer = Analyzer::new();
    let root = std::env::temp_dir().join(format!("corelamo-jsonl-test-{}", std::process::id()));

    let mut index = LsmIndex::persistent(&root, 10).unwrap();

    for doc in docs {
        let indexed = IndexedDocument::new(doc.id)
            .with_part(1, &doc.title, WeightInterval::TITLE)
            .with_part(2, &doc.body, WeightInterval::TEXT);

        index.add_indexed_document(&analyzer, &indexed).unwrap();
    }

    index.flush().unwrap();

    let executor = QueryExecutor::new(&index, &analyzer);

    let hits = executor.search(&Query::Term("database".into()), 2);

    assert!(!hits.is_empty());

    std::fs::remove_dir_all(root).ok();
}

#[test]
#[ignore]
fn indexes_and_searches_generated_10k_jsonl_fixture() {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../core-testkit/fixtures/generated_10k.jsonl"
    );

    let docs = load_jsonl(fixture).unwrap();

    let analyzer = Analyzer::new();
    let root = std::env::temp_dir().join(format!("corelamo-jsonl-10k-test-{}", std::process::id()));

    let mut index = LsmIndex::persistent(&root, 5_000).unwrap();

    for doc in docs {
        let indexed = IndexedDocument::new(doc.id)
            .with_part(1, &doc.title, WeightInterval::TITLE)
            .with_part(2, &doc.body, WeightInterval::TEXT);

        index.add_indexed_document(&analyzer, &indexed).unwrap();
    }

    index.flush().unwrap();

    let executor = QueryExecutor::new(&index, &analyzer);

    let hits = executor.search(&Query::Term("database".into()), 2);

    println!("hits={}", hits.len());
    println!("segments={}", index.segment_count());

    assert!(!hits.is_empty());

    std::fs::remove_dir_all(root).ok();
}
