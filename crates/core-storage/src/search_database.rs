use std::{
    collections::{BTreeMap, HashSet},
    io,
};

use crate::document_store::{DocumentStore, StoredDocument};
use core_index::{
    analyzer::analyzer::Analyzer,
    document::{policy::IndexKind, IndexPolicy, IndexedDocument},
    lsm::{
        index_worker::{
            build_segments_parallel, IndexCommand, IndexWorker, ReindexStatus, ReindexingStats,
        },
        make_batches,
        snapshot::SharedIndexSnapshot,
        LsmIndex,
    },
};

use core_protocol::{
    errors::{DocFailure, FailReason},
    format::Format,
};
use core_query::{planner::QueryPlan, Query, QueryExecutor, SearchHit};
use indexmap::IndexMap;
use tokio::sync::watch;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMode {
    StoreOnly,
    StoreAndIndex,
}

pub struct SearchDatabase<S: DocumentStore> {
    store: S,
    index_worker: IndexWorker,
    snapshot: SharedIndexSnapshot,
    analyzer: Analyzer,
    policy: IndexPolicy,
    next_internal_id: u64,
}

pub struct InsertReport {
    pub inserted: u32,
    pub failures: Vec<DocFailure>,
}

pub struct ReplaceReport {
    pub replaced: u32,
    pub failures: Vec<DocFailure>,
}

pub struct DeleteReport {
    pub deleted: u32,
    pub failures: Vec<DocFailure>,
}

//start stop database (already stopped/started)
pub enum DatabasePowerButtonOutcome {
    Changed,
    Nochange,
}

#[derive(Debug)]
pub struct DocumentInput {
    pub external_id: String,
    pub fields: BTreeMap<String, String>, //Don't know if this can be HashMap instead

    pub source: Vec<u8>,
    pub format: Format, // Stores the format of the document JSON/XML
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchDocumentHit {
    pub external_id: String,
    pub internal_id: u64,
    pub score: f32,
    pub fields: BTreeMap<String, String>,
}

pub struct SearchDocumentResults {
    pub total_hits: usize,
    pub hits: Vec<SearchDocumentHit>,
}

impl<S: DocumentStore> SearchDatabase<S> {
    pub fn new(store: S, index: LsmIndex, analyzer: Analyzer) -> Self {
        Self::with_policy(store, index, analyzer, IndexPolicy::default_document())
    }
    pub fn index_worker(&self) -> &IndexWorker {
        &self.index_worker
    }

    pub fn with_policy(store: S, index: LsmIndex, analyzer: Analyzer, policy: IndexPolicy) -> Self {
        let next_internal_id = store.max_internal_id() + 1;
        let snapshot = SharedIndexSnapshot::empty();
        let index_worker = IndexWorker::start(index, analyzer.clone(), snapshot.clone());

        Self {
            store,
            index_worker,
            snapshot,
            analyzer,
            policy,
            next_internal_id,
        }
    }

    fn allocate_internal_id(&mut self) -> u64 {
        let id = self.next_internal_id;
        self.next_internal_id += 1;
        id
    }

    pub fn document_count(&self) -> usize {
        self.store.document_count()
    }
    pub fn for_each_document(
        &self,
        f: &mut dyn FnMut(&StoredDocument) -> io::Result<()>,
    ) -> io::Result<()> {
        self.store.for_each_document(f)
    }

    pub fn put_document(&mut self, input: DocumentInput, mode: IndexMode) -> io::Result<()> {
        let doc = StoredDocument {
            external_id: input.external_id,
            internal_id: self.allocate_internal_id(),
            source: input.source,
            fields: input.fields,
            format: input.format,
        };
        self.store.put(doc.clone())?;

        if mode == IndexMode::StoreAndIndex {
            let indexed = stored_document_to_indexed(&doc, &self.policy);
            self.index_worker.add_indexed_document_wait(indexed)?;
        }

        Ok(())
    }

    pub fn put_documents_parallel(
        &mut self,
        inputs: Vec<DocumentInput>,
        batch_size: usize,
    ) -> io::Result<InsertReport> {
        use std::time::Instant;
        let total_started = Instant::now();
        let started = Instant::now();

        let mut stored_documents = Vec::with_capacity(inputs.len());
        let mut indexed_documents = Vec::with_capacity(inputs.len());

        let mut failures = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        //INFO: bik porno ar Id/AutoID un kaa logika strada
        //FIX: izdomat to logiku lidz galam piem kas notiek ja ir auto 1 2 3 un tad kads ieliiek
        //id:7 yk?
        for (index, input) in inputs.into_iter().enumerate() {
            let (internal_id, external_id) = if input.external_id.is_empty() {
                let internal_id = self.allocate_internal_id();
                (internal_id, internal_id.to_string())
            } else {
                let external_id = input.external_id;
                if self.store.get(&external_id)?.is_some() || !seen.insert(external_id.clone()) {
                    failures.push(DocFailure::with_id(
                        index,
                        external_id,
                        FailReason::DuplicatePrimaryId,
                    ));
                    continue;
                }
                (self.allocate_internal_id(), external_id)
            };

            let doc = StoredDocument {
                external_id,
                internal_id,
                source: input.source,
                fields: input.fields,
                format: input.format,
            };

            indexed_documents.push(stored_document_to_indexed(&doc, &self.policy));
            stored_documents.push(doc);
        }

        let inserted = stored_documents.len() as u32;
        // tracing::info!(time=?started.elapsed(), "Document conversion time");
        let started = Instant::now();
        self.store.put_batch(stored_documents)?;
        // tracing::info!(time=?started.elapsed(),"Document store batch write time");

        let started = Instant::now();
        let batches = make_batches(indexed_documents, batch_size);
        // tracing::info!(time=?started.elapsed(),batches=%batches.len(), "Batch splitting" );
        let doc_counts: Vec<u64> = batches.iter().map(|batch| batch.len() as u64).collect();
        let started = Instant::now();
        let segments = build_segments_parallel(self.analyzer.clone(), batches);
        /*tracing::info!(
            time=?started.elapsed(),
            segments=%segments.len(),
            "Parallel segment build",
        );
        */
        let started = Instant::now();
        for (segment, doc_count) in segments.into_iter().zip(doc_counts) {
            self.index_worker.add_segment_wait(segment, doc_count)?;
        }
        // tracing::info!(time=?started.elapsed(), "Segment publish/write");

        // tracing::info!(total_time=?total_started.elapsed(),"TOTAL Parallel document putting" );

        Ok(InsertReport { inserted, failures })
    }

    pub fn put_document_store_only_return_indexed(
        &mut self,
        input: DocumentInput,
    ) -> io::Result<IndexedDocument> {
        let doc = StoredDocument {
            external_id: input.external_id,
            internal_id: self.allocate_internal_id(),
            source: input.source,
            fields: input.fields,
            format: input.format,
        };

        self.store.put(doc.clone())?;

        Ok(stored_document_to_indexed(&doc, &self.policy))
    }

    // Update creates a new internal version, the old internal_id is tombstoned, while the
    // external_id points to the latest version
    // INFO: changed the name cuz upsert is more precise here
    pub fn upsert_document(&mut self, input: DocumentInput, mode: IndexMode) -> io::Result<()> {
        if let Some(old_doc) = self.store.get(&input.external_id)? {
            self.index_worker
                .delete_document_wait(old_doc.internal_id)?;
        }

        let doc = StoredDocument {
            external_id: input.external_id,
            internal_id: self.allocate_internal_id(),
            source: input.source,
            fields: input.fields,
            format: input.format,
        };
        self.store.put(doc.clone())?;

        if mode == IndexMode::StoreAndIndex {
            let indexed = stored_document_to_indexed(&doc, &self.policy);
            self.index_worker.add_indexed_document_wait(indexed)?;
        }

        Ok(())
    }

    pub fn delete_document(&mut self, external_id: &str) -> io::Result<()> {
        if let Some(old_doc) = self.store.get(external_id)? {
            self.index_worker
                .delete_document_wait(old_doc.internal_id)?;
        }

        self.store.delete(external_id)
    }

    pub fn get_document(&mut self, external_id: &str) -> io::Result<Option<StoredDocument>> {
        self.store.get(external_id)
    }

    pub fn search(&self, query: &Query, xpath: u32) -> Vec<SearchHit> {
        let snapshot = self.snapshot.get();
        let executor = QueryExecutor::new(&snapshot, &self.analyzer);
        executor.search(query, xpath)
    }

    // WARN: MIght not be used
    pub fn search_documents(
        &mut self,
        query: &Query,
        xpath: u32,
    ) -> io::Result<Vec<StoredDocument>> {
        let hits = self.search(query, xpath);

        let mut docs = Vec::new();

        for hit in hits {
            if let Some(doc) = self.store.get_by_internal_id(hit.doc_id)? {
                docs.push(doc);
            }
        }

        Ok(docs)
    }

    pub fn search_document_hits(
        &mut self,
        query: &Query,
        xpath: u32,
    ) -> io::Result<Vec<SearchDocumentHit>> {
        let hits = self.search(query, xpath);

        let mut results = Vec::new();

        for hit in hits {
            if let Some(doc) = self.store.get_by_internal_id(hit.doc_id)? {
                results.push(SearchDocumentHit {
                    external_id: doc.external_id.clone(),
                    internal_id: doc.internal_id,
                    score: hit.score,
                    fields: visible_fields(&doc, &self.policy, None),
                });
            }
        }

        Ok(results)
    }

    pub fn search_document_hits_top_k(
        &mut self,
        query: &Query,
        xpath: u32,
        k: usize,
    ) -> io::Result<Vec<SearchDocumentHit>> {
        let snapshot = self.snapshot.get();
        let executor = QueryExecutor::new(&snapshot, &self.analyzer);
        let hits = executor.search_top_k(query, xpath, k);

        self.resolve_document_hits(hits, None)
    }

    pub fn search_document_hits_all_fields_top_k(
        &mut self,
        query: &Query,
        k: usize,
    ) -> io::Result<Vec<SearchDocumentHit>> {
        let snapshot = self.snapshot.get();
        let executor = QueryExecutor::new(&snapshot, &self.analyzer);

        let xpaths: Vec<_> = self.policy.searchable_xpaths().collect();
        let hits = executor.search_all_xpaths_top_k(query, xpaths, k);

        self.resolve_document_hits(hits, None)
    }

    pub fn search_document_results_all_fields_top_k(
        &mut self,
        query: &Query,
        k: usize,
    ) -> io::Result<SearchDocumentResults> {
        let snapshot = self.snapshot.get();
        let executor = QueryExecutor::new(&snapshot, &self.analyzer);

        let xpaths: Vec<_> = self.policy.searchable_xpaths().collect();
        let all_hits = executor.search_all_xpaths(query, xpaths);

        let total_hits = all_hits.len();

        let mut top_hits = all_hits;
        top_hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });
        top_hits.truncate(k);

        let hits = self.resolve_document_hits(top_hits, None)?;

        Ok(SearchDocumentResults { total_hits, hits })
    }

    pub fn search_document_hits_plan(
        &mut self,
        plan: &QueryPlan,
        return_fields: Option<&IndexMap<String, bool>>,
        offset: usize,
        limit: usize,
    ) -> io::Result<Vec<SearchDocumentHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let requested_hits = offset.checked_add(limit).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "offset and limit exceed the supported range",
            )
        })?;

        let snapshot = self.snapshot.get();
        let executor = QueryExecutor::new(&snapshot, &self.analyzer);

        let xpaths: Vec<_> = self.policy.searchable_xpaths().collect();

        // the executor might not rank enough results to coover the skipped portion and the
        // requested page, so we must ensure it does

        let mut hits = executor.search_plan_all_xpaths_top_k(plan, xpaths, requested_hits);

        if offset >= hits.len() {
            return Ok(Vec::new());
        }

        println!(
            "offset={}, limit={}, requested={}, returned={}",
            offset,
            limit,
            requested_hits,
            hits.len(),
        );

        for (i, hit) in hits.iter().take(15).enumerate() {
            println!("{i}: doc={} score={}", hit.doc_id, hit.score);
        }
        // I don't want to allocate another Vec and also lets not touch skipped docs
        hits.drain(..offset);
        hits.truncate(limit);

        self.resolve_document_hits(hits, return_fields)
    }

    fn resolve_document_hits(
        &mut self,
        hits: Vec<SearchHit>,
        return_fields: Option<&IndexMap<String, bool>>,
    ) -> io::Result<Vec<SearchDocumentHit>> {
        let mut results = Vec::new();

        for hit in hits {
            if let Some(doc) = self.store.get_by_internal_id(hit.doc_id)? {
                results.push(SearchDocumentHit {
                    external_id: doc.external_id.clone(),
                    internal_id: doc.internal_id,
                    score: hit.score,
                    fields: visible_fields(&doc, &self.policy, return_fields),
                });
            }
        }

        Ok(results)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.index_worker.flush_wait()
    }

    // pub fn compact_all(&mut self) -> io::Result<()> {
    //     self.index.compact_all()
    // }
    //
    // pub fn segment_count(&self) -> usize {
    //     self.index.segment_count()
    // }
    //
    pub fn shutdown(self) -> io::Result<LsmIndex> {
        self.index_worker.shutdown()
    }

    pub fn index_sender(&self) -> std::sync::mpsc::Sender<IndexCommand> {
        self.index_worker.sender()
    }

    pub fn analyze_query_term(&self, term: &str) -> Option<String> {
        self.analyzer
            .analyze(term)
            .first()
            .map(|token| token.text.clone())
    }

    pub fn segment_count(&self) -> io::Result<usize> {
        self.index_worker.segment_count()
    }

    pub fn set_policy(&mut self, policy: IndexPolicy) -> io::Result<()> {
        policy.validate()?;
        self.policy = policy;
        Ok(())
    }

    pub fn policy(&self) -> &IndexPolicy {
        &self.policy
    }

    pub fn reindex(&mut self) -> io::Result<()> {
        let _docs = self.store.all_documents()?; // jauzliek _ ?

        todo!("needs LsmINdex reset/clear before rebuilding")
    }

    pub fn build_indexed_batches_from_store(
        &self,
        batch_size: usize,
    ) -> io::Result<Vec<Vec<IndexedDocument>>> {
        let mut batches = Vec::new();
        let mut current = Vec::with_capacity(batch_size);

        self.store.for_each_document(
            &mut (|doc| {
                current.push(stored_document_to_indexed(doc, &self.policy));

                if current.len() == batch_size {
                    batches.push(std::mem::take(&mut current));
                    current = Vec::with_capacity(batch_size);
                }

                Ok(())
            }),
        )?;

        if !current.is_empty() {
            batches.push(current);
        }

        Ok(batches)
    }

    pub fn shutdown_into_store(self) -> io::Result<S> {
        let _inex = self.index_worker.shutdown()?;
        Ok(self.store)
    }

    pub fn reindex_existing_documents(
        &mut self,
        batch_size: usize,
        progress: &watch::Sender<ReindexingStats>,
    ) -> io::Result<()> {
        //reindexing stats
        let total = self.store.document_count() as u64;

        let _ = progress.send(ReindexingStats {
            status: ReindexStatus::Reindexing,
            progress: 0,
            eta_seconds: None,
        });
        let mut indexed_documents = Vec::with_capacity(self.store.document_count());
        self.store.for_each_document(
            &mut (|doc| {
                indexed_documents.push(stored_document_to_indexed(doc, &self.policy));
                Ok(())
            }),
        )?;

        let batches = make_batches(indexed_documents, batch_size);

        let doc_counts: Vec<u64> = batches.iter().map(|batch| batch.len() as u64).collect();
        let segments = build_segments_parallel(self.analyzer.clone(), batches);
        //reindexing stats
        let started_at = std::time::Instant::now();
        let mut calibrated_rate: Option<f64> = None;
        let mut done: u64 = 0;

        for (segment, doc_count) in segments.into_iter().zip(doc_counts) {
            self.index_worker.add_segment_wait(segment, doc_count)?;
            done += doc_count;

            if calibrated_rate.is_none()
                && (done >= 1000 || started_at.elapsed() >= std::time::Duration::from_secs(5))
            {
                let elapsed = started_at.elapsed().as_secs_f64();
                if elapsed > 0.0 {
                    calibrated_rate = Some((done as f64) / elapsed);
                }
            }

            let pct = if total > 0 {
                (((done as f64) / (total as f64)) * 100.0) as u8
            } else {
                0
            };
            let eta_seconds = calibrated_rate.map(|rate| {
                let remaining = total.saturating_sub(done) as f64;
                (remaining / rate).max(0.0) as u64
            });

            let _ = progress.send(ReindexingStats {
                status: ReindexStatus::Reindexing,
                progress: pct,
                eta_seconds,
            });
        }

        self.flush()?;

        let _ = progress.send(ReindexingStats {
            status: ReindexStatus::Complete,
            progress: 100,
            eta_seconds: None,
        });
        Ok(())
    }

    //reindex partaisisiana
    // core-storage, on SearchDatabase
    pub fn reindex_documents_after(
        &mut self,
        watermark_internal_id: u64,
        batch_size: usize,
    ) -> io::Result<()> {
        let mut indexed_documents = Vec::new();

        self.store.for_each_document(
            &mut (|doc| {
                if doc.internal_id > watermark_internal_id {
                    indexed_documents.push(stored_document_to_indexed(doc, &self.policy));
                }
                Ok(())
            }),
        )?;

        if indexed_documents.is_empty() {
            return Ok(());
        }

        let batches = make_batches(indexed_documents, batch_size);
        let doc_counts: Vec<u64> = batches.iter().map(|batch| batch.len() as u64).collect();
        let segments = build_segments_parallel(self.analyzer.clone(), batches);

        for (segment, doc_count) in segments.into_iter().zip(doc_counts) {
            self.index_worker.add_segment_wait(segment, doc_count)?;
        }

        self.flush()
    }
}

fn stored_document_to_indexed(doc: &StoredDocument, policy: &IndexPolicy) -> IndexedDocument {
    let mut indexed = IndexedDocument::new(doc.internal_id);

    for field in policy.indexed_fields() {
        //TODO: we now have IndexKind::Id/IdAutoIncrement it should be searchable imo
        if field.index != IndexKind::Text {
            continue;
        }

        let Some(text) = doc.fields.get(&field.name) else {
            continue;
        };

        indexed = indexed.with_part(field.xpath, text, field.weight);
    }

    indexed
}

fn visible_fields(
    doc: &StoredDocument,
    policy: &IndexPolicy,
    resolved: Option<&IndexMap<String, bool>>,
) -> BTreeMap<String, String> {
    doc.fields
        .iter()
        .filter(|(path, _)| should_include(path, policy, resolved))
        .map(|(path, value)| (path.clone(), value.clone()))
        .collect()
}

fn should_include(
    path: &str,
    policy: &IndexPolicy,
    resolved: Option<&IndexMap<String, bool>>,
) -> bool {
    if let Some(rf) = resolved {
        let mut candidate = path;
        loop {
            if let Some(&include) = rf.get(candidate) {
                return include;
            }
            match candidate.rfind('/') {
                Some(idx) => {
                    candidate = &candidate[..idx];
                }
                None => {
                    break;
                }
            }
        }
    }

    let top_level = path.split('/').next().unwrap_or(path);
    policy
        .fields
        .iter()
        .find(|f| f.name == top_level)
        .map(|f| f.list)
        .unwrap_or(false)
}
