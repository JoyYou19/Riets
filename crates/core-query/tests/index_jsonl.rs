use core_index::{
    analyzer::analyzer::Analyzer,
    document::{IndexedDocument, WeightInterval},
    lsm::LsmIndex,
    search::{SearchIndex, SearchStats},
};
use core_query::{Query, QueryExecutor};

fn test_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "corelamo-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::remove_dir_all(&root).ok();
    root
}

#[test]
fn analyzer_does_not_break_database_term() {
    let analyzer = Analyzer::new();

    let once = analyzer.analyze("database");
    assert!(!once.is_empty());

    let token_once = once[0].text.clone();

    let twice = analyzer.analyze(&token_once);
    assert!(!twice.is_empty());

    println!("database -> {:?} -> {:?}", token_once, twice[0].text);

    assert_eq!(
        token_once, twice[0].text,
        "query term is being analyzed differently the second time"
    );
}

#[test]
fn raw_lookup_finds_database_before_and_after_flush() {
    let analyzer = Analyzer::new();
    let root = test_root("raw-lookup-database");

    let mut index = LsmIndex::persistent(&root, 10).unwrap();

    let indexed = IndexedDocument::new(4)
        .with_part(1, "Memory Tables Document 4", WeightInterval::TITLE)
        .with_part(
            2,
            "Throughput rust payload database writer storage posting field worker system",
            WeightInterval::TEXT,
        );

    index.add_indexed_document(&analyzer, &indexed).unwrap();

    let token = analyzer.analyze("database")[0].text.clone();

    assert!(
        !index.lookup(&token, 2).is_empty(),
        "database should be found in mem index before flush"
    );

    index.flush().unwrap();

    assert!(
        !index.lookup(&token, 2).is_empty(),
        "database should be found after flush"
    );

    assert_eq!(index.doc_len(4, 1), Some(4));
    assert_eq!(index.doc_len(4, 2), Some(10));

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn raw_lookup_finds_database_after_reopen_from_disk() {
    let analyzer = Analyzer::new();
    let root = test_root("disk-lookup-database");

    {
        let mut index = LsmIndex::persistent(&root, 10).unwrap();

        let indexed = IndexedDocument::new(4)
            .with_part(1, "Memory Tables Document 4", WeightInterval::TITLE)
            .with_part(
                2,
                "Throughput rust payload database writer storage posting field worker system",
                WeightInterval::TEXT,
            );

        index.add_indexed_document(&analyzer, &indexed).unwrap();
        index.flush().unwrap();
    }

    let index = LsmIndex::persistent(&root, 10).unwrap();
    let token = analyzer.analyze("database")[0].text.clone();

    assert!(
        !index.lookup(&token, 2).is_empty(),
        "database should be found after reopening disk segment"
    );

    assert_eq!(index.doc_len(4, 2), Some(10));
    assert_eq!(index.doc_count(2), 1);
    assert_eq!(index.total_doc_len(2), 10);

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn executor_search_finds_database() {
    let analyzer = Analyzer::new();
    let root = test_root("executor-search-database");

    let mut index = LsmIndex::persistent(&root, 10).unwrap();

    let indexed = IndexedDocument::new(4)
        .with_part(1, "Memory Tables Document 4", WeightInterval::TITLE)
        .with_part(
            2,
            "Throughput rust payload database writer storage posting field worker system",
            WeightInterval::TEXT,
        );

    index.add_indexed_document(&analyzer, &indexed).unwrap();
    index.flush().unwrap();

    let executor = QueryExecutor::new(&index, &analyzer);
    let hits = executor.search(&Query::Term("database".into()), 2);

    assert!(!hits.is_empty());
    assert_eq!(hits[0].doc_id, 4);
    assert!(hits[0].score > 0.0);

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn double_analyzed_query_can_explain_missing_database() {
    let analyzer = Analyzer::new();

    let raw = "database";
    let once = analyzer.analyze(raw)[0].text.clone();
    let twice = analyzer.analyze(&once)[0].text.clone();

    println!("raw={raw:?}, once={once:?}, twice={twice:?}");

    // If this fails, CorelamoDatabase::build_query must stop pre-analyzing terms.
    assert_eq!(once, twice);
}

#[test]
fn analyzer_is_idempotent_for_common_terms() {
    let analyzer = Analyzer::new();

    for word in [
        "database",
        "storage",
        "running",
        "reader",
        "engine",
        "rust",
        "distributed",
        "posting",
    ] {
        let once = analyzer.analyze(word)[0].text.clone();
        let twice = analyzer.analyze(&once)[0].text.clone();

        assert_eq!(once, twice, "{word} was not idempotent: {once} -> {twice}");
    }
}
