use std::{collections::BTreeMap, io};

use crate::document_store::{DocumentStore, StoredDocument};
use core_index::{
    analyzer::analyzer::Analyzer,
    document::{IndexPolicy, IndexedDocument, policy::IndexKind},
    lsm::{
        LsmIndex,
        index_worker::{IndexCommand, IndexWorker, build_segments_parallel},
        make_batches,
        snapshot::SharedIndexSnapshot,
    },
};
use core_query::{planner::QueryPlan, Query, QueryExecutor, SearchHit};

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

#[derive(Debug)]
pub struct DocumentInput {
    pub external_id: String,
    pub fields: BTreeMap<String, String>, //Don't know if this can be HashMap instead
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

    pub fn put_document(&mut self, input: DocumentInput, mode: IndexMode) -> io::Result<()> {
        let doc = StoredDocument {
            external_id: input.external_id,
            internal_id: self.allocate_internal_id(),
            fields: input.fields,
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
    ) -> io::Result<()> {
        use std::time::Instant;

        let total_started = Instant::now();

        let started = Instant::now();

        let mut stored_documents = Vec::with_capacity(inputs.len());
        let mut indexed_documents = Vec::with_capacity(inputs.len());

        for input in inputs {
            let doc = StoredDocument {
                external_id: input.external_id,
                internal_id: self.allocate_internal_id(),
                fields: input.fields,
            };

            indexed_documents.push(stored_document_to_indexed(&doc, &self.policy));
            stored_documents.push(doc);
        }

        println!("document conversion took {:?}", started.elapsed());

        let started = Instant::now();

        self.store.put_batch(stored_documents)?;

        println!("document store batch write took {:?}", started.elapsed());

        let started = Instant::now();

        let batches = make_batches(indexed_documents, batch_size);

        println!(
            "batch splitting took {:?}, batches={}",
            started.elapsed(),
            batches.len()
        );

        let started = Instant::now();

        let segments = build_segments_parallel(self.analyzer.clone(), batches);

        println!(
            "parallel segment build took {:?}, segments={}",
            started.elapsed(),
            segments.len()
        );

        let started = Instant::now();

        for segment in segments {
            self.index_worker.add_segment_wait(segment)?;
        }

        println!("segment publish/write took {:?}", started.elapsed());

        println!(
            "TOTAL put_documents_parallel took {:?}",
            total_started.elapsed()
        );

        Ok(())
    }

    pub fn put_document_store_only_return_indexed(
        &mut self,
        input: DocumentInput,
    ) -> io::Result<IndexedDocument> {
        let doc = StoredDocument {
            external_id: input.external_id,
            internal_id: self.allocate_internal_id(),
            fields: input.fields,
        };

        self.store.put(doc.clone())?;

        Ok(stored_document_to_indexed(&doc, &self.policy))
    }

    // Update creates a new internal version, the old internal_id is tombstoned, while the
    // external_id points to the latest version
    pub fn update_document(&mut self, input: DocumentInput, mode: IndexMode) -> io::Result<()> {
        if let Some(old_doc) = self.store.get(&input.external_id)? {
            self.index_worker
                .delete_document_wait(old_doc.internal_id)?;
        }

        let doc = StoredDocument {
            external_id: input.external_id,
            internal_id: self.allocate_internal_id(),
            fields: input.fields,
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
                    fields: visible_fields(&doc, &self.policy),
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

        self.resolve_document_hits(hits)
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

        self.resolve_document_hits(hits)
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

        let hits = self.resolve_document_hits(top_hits)?;

        Ok(SearchDocumentResults { total_hits, hits })
    }

    pub fn search_document_hits_plan_top_k(
        &mut self,
        plan: &QueryPlan,
        k: usize,
    ) -> io::Result<Vec<SearchDocumentHit>> {
        let snapshot = self.snapshot.get();
        let executor = QueryExecutor::new(&snapshot, &self.analyzer);

        let xpaths: Vec<_> = self.policy.searchable_xpaths().collect();
        let hits = executor.search_plan_all_xpaths_top_k(plan, xpaths, k);

        self.resolve_document_hits(hits)
    }

    fn resolve_document_hits(
        &mut self,
        hits: Vec<SearchHit>,
    ) -> io::Result<Vec<SearchDocumentHit>> {
        let mut results = Vec::new();

        for hit in hits {
            if let Some(doc) = self.store.get_by_internal_id(hit.doc_id)? {
                results.push(SearchDocumentHit {
                    external_id: doc.external_id.clone(),
                    internal_id: doc.internal_id,
                    score: hit.score,
                    fields: visible_fields(&doc, &self.policy),
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
        let docs = self.store.all_documents()?;

        todo!("needs LsmINdex reset/clear before rebuilding")
    }

    pub fn build_indexed_batches_from_store(
        &self,
        batch_size: usize,
    ) -> io::Result<Vec<Vec<IndexedDocument>>> {
        let mut batches = Vec::new();
        let mut current = Vec::with_capacity(batch_size);

        self.store.for_each_document(&mut |doc| {
            current.push(stored_document_to_indexed(doc, &self.policy));

            if current.len() == batch_size {
                batches.push(std::mem::take(&mut current));
                current = Vec::with_capacity(batch_size);
            }

            Ok(())
        })?;

        if !current.is_empty() {
            batches.push(current);
        }

        Ok(batches)
    }

    pub fn shutdown_into_store(self) -> io::Result<S> {
        let _inex = self.index_worker.shutdown()?;
        Ok(self.store)
    }

    pub fn reindex_existing_documents(&mut self, batch_size: usize) -> io::Result<()> {
        let mut indexed_documents = Vec::with_capacity(self.store.document_count());

        self.store.for_each_document(&mut |doc| {
            indexed_documents.push(stored_document_to_indexed(doc, &self.policy));
            Ok(())
        })?;

        let batches = make_batches(indexed_documents, batch_size);
        let segments = build_segments_parallel(self.analyzer.clone(), batches);

        for segment in segments {
            self.index_worker.add_segment_wait(segment)?;
        }

        self.flush()
    }
}

fn stored_document_to_indexed(doc: &StoredDocument, policy: &IndexPolicy) -> IndexedDocument {
    let mut indexed = IndexedDocument::new(doc.internal_id);

    for field in policy.indexed_fields() {
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

fn visible_fields(doc: &StoredDocument, policy: &IndexPolicy) -> BTreeMap<String, String> {
    policy
        .fields
        .iter()
        .filter(|field| field.stored)
        .filter_map(|field| {
            doc.fields
                .get(&field.name)
                .map(|value| (field.name.clone(), value.clone()))
        })
        .collect()
}
