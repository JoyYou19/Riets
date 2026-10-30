use std::{ collections::VecDeque, io, panic::{self, AssertUnwindSafe}, sync::{Arc, mpsc} };

use crate::{
    document_projections::{ id_path_to_strip, project_document },
    document_store::{ DocumentStore, StoredDocument },
    json_indexing_helper::index_document,
    json_parse::{ ParsedNode, parse_source_into_node },
};
use core_index::{
    analyzer::analyzer::Analyzer,
    array_rows::ArrayRowAllocator,
    document::{ AnalyzedDocument, IndexPolicy, IndexedDocument },
    lsm::{
        LsmIndex,
        index_worker::{
            IndexCommand,
            IndexWorker,
            IndexingStats,
            PendingAck,
            ReindexProgress,

        },
        snapshot::SharedIndexSnapshot,
    },
    types::{ DocId, LocalDocId, MAX_LOCAL_DOC_ID, ShardId, local_of, make_doc_id, shard_of },
};

use ahash::{ HashSet, HashSetExt };

use bincode::{ Decode, Encode };
use core_protocol::{
    command_reponse_definitions::LookupResponse,
    document_out::DocumentOut,
    errors::{ DocFailure, FailReason },
    format::Format,
};
use core_timing::timed;
use indexmap::IndexMap;
use serde::{ Deserialize, Serialize };
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMode {
    StoreOnly,
    StoreAndIndex,
}

/// A flush started with `SearchDatabase::start_flush`: the index batches sent before it,
/// then the index flush itself, in the order the index worker finishes them.
#[must_use = "a flush ticket must be completed or waited on to know the flush succeeded"]
pub struct FlushTicket {
    acks: VecDeque<PendingAck>,
}

impl FlushTicket {
    /// Non-blocking. `None` while the worker is still busy; `Some(Ok(()))` once every batch
    /// and the flush are on disk; `Some(Err(_))` with the first failure.
    pub fn try_complete(&mut self) -> Option<io::Result<()>> {
        while let Some(front) = self.acks.front_mut() {
            match front.try_take()? {
                Ok(()) => {
                    self.acks.pop_front();
                }
                Err(e) => {
                    return Some(Err(e));
                }
            }
        }
        Some(Ok(()))
    }

    /// Waits until everything is on disk, or returns the first failure.
    pub fn wait(self) -> io::Result<()> {
        for ack in self.acks {
            ack.wait()?;
        }
        Ok(())
    }
}
pub struct IndexPipeline<'a, S: DocumentStore> {
    // Database that we import into
    db: &'a mut SearchDatabase<S>,
    // Number of docs per ImmutableSegment
    batch_size: usize,
    // Number of completed batches before building segments in
    // parallel
    //window_size: usize,
    // The documents that are waiting to be written to document store
    current_store_batch: Vec<StoredDocument>,
    // The current batch waiting to be written to the index segment
    current_batch: Vec<IndexedDocument>,
    //completed batches that are waiting to be indexed together
    // pending_batches: Vec<Vec<IndexedDocument>>,
    inserted: u32,
    failures: Vec<DocFailure>,
    seen: HashSet<String>,
}
const MIN_INDEX_BATCH: usize = 4096;
///nezinu mosk vairak mosk mazak jauztaisa
pub struct SearchDatabase<S: DocumentStore> {
    store: S,
    index_worker: IndexWorker,
    analyzer: Analyzer,
    policy: IndexPolicy,

    shard_id: ShardId,
    next_local_id: LocalDocId,
    array_row_allocator: ArrayRowAllocator,
    index_in_flight: VecDeque<PendingAck>,
}

pub struct InsertReport {
    pub inserted: u32,
    pub failures: Vec<DocFailure>,
}

pub struct ReplaceReport {
    pub replaced: u32,
    pub failures: Vec<DocFailure>,
}

pub struct WordStats {
    pub word: String,
    //total occurances for a word
    pub occurrences: u64,
    //number of different docs that a word appeared in
    pub documents: u64,
}

pub struct DeleteReport {
    pub deleted: u32,
    pub failures: Vec<DocFailure>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Encode, Decode)]
pub struct DocumentInput {
    pub external_id: String,
    pub parsed: ParsedNode,
    pub source: Vec<u8>,
    pub format: Format, // Stores the format of the document JSON/XML
}

#[derive(Debug, Clone)]
pub struct SearchDocumentHit {
    pub external_id: String,
    pub internal_id: DocId,
    pub score: f32,
    pub doc: DocumentOut,
    pub highlights: Vec<String>,
}

impl<S: DocumentStore> SearchDatabase<S> {
    #[timed(database_lifecycle)]
    pub fn new(store: S, index: LsmIndex, analyzer: Analyzer) -> io::Result<Self> {
        Self::with_policy(store, index, analyzer, IndexPolicy::default_document())
    }

    pub fn store(&self) -> &S {
        &self.store
    }
    pub fn mut_store(&mut self) -> &mut S {
        &mut self.store
    }
    pub fn index_worker(&self) -> &IndexWorker {
        &self.index_worker
    }

    pub fn shard_id(&self) -> ShardId {
        self.shard_id
    }
    /// Collects the results of batches the worker has finished, oldest first, without waiting.
    /// An indexing error from an earlier batch is returned here.
    fn error_return(&mut self) -> io::Result<()> {
        while let Some(pending) = self.index_in_flight.front_mut() {
            match pending.try_take() {
                None => {
                    break;
                }
                Some(result) => {
                    self.index_in_flight.pop_front();
                    result?;
                }
            }
        }
        Ok(())
    }
    /// True when no batch is in flight. Collects the finished batch's result,
    /// so an indexing error surfaces here.
    fn index_idle(&mut self) -> io::Result<bool> {
        self.error_return()?;
        Ok(self.index_in_flight.is_empty())
    }
    /// Waits for the previous batch; returns its indexing error, if any.
    #[timed(flushing)]
    fn wait_index_in_flight(&mut self) -> io::Result<()> {
        while let Some(pending) = self.index_in_flight.pop_front() {
            pending.wait()?;
        }
        Ok(())
    }

    // WARN: This is old, must be changed, backwards compatibility here is 0
    #[timed(database_lifecycle)]
    pub fn with_policy(
        store: S,
        index: LsmIndex,
        analyzer: Analyzer,
        policy: IndexPolicy
    ) -> io::Result<Self> {
        Self::with_shard_policy(store, index, analyzer, policy, ShardId(0))
    }

    /* Each shard must receive a distinct shardId
    The shard allocates only local sequences and packs them into globally unique document IDs.*/
    #[timed(database_lifecycle)]
    pub fn with_shard_policy(
        store: S,
        index: LsmIndex,
        analyzer: Analyzer,
        policy: IndexPolicy,
        shard_id: ShardId
    ) -> io::Result<Self> {
        let next_local_id = next_local_id_for_shard(&store, &index,shard_id)?;
        let next_array_row = index.max_array_row().saturating_add(1);

        let snapshot = SharedIndexSnapshot::empty();
        let index_worker = IndexWorker::start(index, analyzer.clone(), snapshot.clone());

        Ok(Self {
            store,
            index_worker,
            analyzer,
            policy,
            shard_id,
            next_local_id,
            array_row_allocator: ArrayRowAllocator::starting_at(next_array_row),
            index_in_flight: VecDeque::new(),
        })
    }

    #[timed(database_lifecycle)]
    pub fn with_shard_policy_and_snapshot(
        store: S,
        index: LsmIndex,
        analyzer: Analyzer,
        policy: IndexPolicy,
        shard_id: ShardId,
        shared_snapshot: SharedIndexSnapshot
    ) -> io::Result<Self> {
        let next_local_id = next_local_id_for_shard(&store,&index, shard_id)?;
        let next_array_row = index.max_array_row().saturating_add(1);

        let index_worker = IndexWorker::start(index, analyzer.clone(), shared_snapshot.clone());

        Ok(Self {
            store,
            index_worker,
            analyzer,
            policy,
            shard_id,
            next_local_id,
            array_row_allocator: ArrayRowAllocator::starting_at(next_array_row),
            index_in_flight: VecDeque::new(),
        })
    }

    /// Allocates the next globally unique ID owned by this shard.
    fn allocate_internal_id(&mut self) -> io::Result<DocId> {
        if self.next_local_id > MAX_LOCAL_DOC_ID {
            return Err(
                io::Error::other(format!("document ID space exhausted for shard {}", self.shard_id))
            );
        }

        let local_id = self.next_local_id;
        self.next_local_id = self.next_local_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("local document ID overflow"))?;

        Ok(make_doc_id(self.shard_id, local_id))
    }

    pub fn document_count(&self) -> usize {
        self.store.document_count()
    }
    pub fn for_each_document(
        &self,
        f: &mut dyn FnMut(&StoredDocument) -> io::Result<()>
    ) -> io::Result<()> {
        self.store.for_each_document(f)
    }

    #[timed(inserting)]
    pub fn put_document(&mut self, input: DocumentInput, mode: IndexMode) -> io::Result<()> {
        let internal_id = self.allocate_internal_id()?;
        if mode == IndexMode::StoreAndIndex {
            let indexed = index_document(
                internal_id,
                &input.parsed,
                &self.policy,
                &mut self.array_row_allocator
            );
            self.index_worker.add_indexed_document_wait(indexed.analyze(&self.analyzer))?;
        }
        let doc = StoredDocument {
            external_id: input.external_id,
            internal_id,
            source: Arc::from(input.source),
            format: input.format,
        };
        self.store.put(doc)?;
        self.store.sync()?;
        Ok(())
    }

    pub fn begin_import(
        &mut self,
        batch_size: usize
        // window_size: usize,
    ) -> io::Result<IndexPipeline<'_, S>> {
        if batch_size == 0 {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "batch_size must be greater than zero")
            );
        }

        // if window_size == 0 {
        //     return Err(io::Error::new(
        //         io::ErrorKind::InvalidInput,
        //         "window_size must be greater than zero",
        //     ));
        // }

        Ok(IndexPipeline {
            db: self,
            batch_size,
            //window_size,
            current_store_batch: Vec::with_capacity(batch_size),
            current_batch: Vec::with_capacity(batch_size),
            // pending_batches: Vec::with_capacity(window_size),
            inserted: 0,
            failures: Vec::new(),
            seen: HashSet::new(),
        })
    }

    #[timed(inserting)]
    pub fn put_documents_parallel(
        &mut self,
        inputs: Vec<DocumentInput>,
        batch_size: usize
        // window_size: usize,
    ) -> io::Result<InsertReport> {
        let total_bytes: u64 = inputs
            .iter()
            .map(|d| d.source.len() as u64)
            .sum();
        core_timing::add_bytes("inserting", "put_documents_parallel", file!(), total_bytes);
        let mut pipeline = self.begin_import(batch_size)?;
        pipeline.seen.reserve(inputs.len());
        for (input_index, input) in inputs.into_iter().enumerate() {
            pipeline.push(input, input_index)?;
        }

        pipeline.finish()
    }

    // Update creates a new internal version, the old internal_id is tombstoned, while the
    // external_id points to the latest version
    // INFO: changed the name cuz upsert is more precise here
    #[timed(modifying_documents)]
    pub fn upsert_document(&mut self, input: DocumentInput, mode: IndexMode) -> io::Result<()> {
        let internal_id = self.allocate_internal_id()?;
        let external_id = if input.external_id.is_empty() {
            internal_id.to_string()
        } else {
            input.external_id
        };

        if let Some(old_doc) = self.store.get(&external_id)? {
            self.index_worker.delete_document_wait(old_doc.internal_id)?;
        }

        if mode == IndexMode::StoreAndIndex {
            let indexed = index_document(
                internal_id,
                &input.parsed,
                &self.policy,
                &mut self.array_row_allocator
            );
            self.index_worker.add_indexed_document_wait(indexed.analyze(&self.analyzer))?;
        }

        let doc = StoredDocument {
            external_id,
            internal_id,
            source: Arc::from(input.source),
            format: input.format,
        };
        self.store.put(doc)?;
        Ok(())
    }

    #[timed(modifying_documents)]
    pub fn delete_document(&mut self, external_id: &str) -> io::Result<()> {
        if let Some(old_doc) = self.store.get(external_id)? {
            self.index_worker.delete_document_wait(old_doc.internal_id)?;
        }

        self.store.delete(external_id)
    }

    pub fn get_document(&self, external_id: &str) -> io::Result<Option<StoredDocument>> {
        self.store.get(external_id)
    }

    //lookup-retrieves+filters document based on request
    #[timed(search)]
    pub fn lookup_documents(
        &self,
        ids: &[String],
        return_fields: Option<&IndexMap<String, bool>>
    ) -> io::Result<LookupResponse> {
        let mut found = Vec::new();
        let mut not_found = Vec::new();
        for id in ids {
            match self.get_document(id)? {
                Some(doc) =>
                    found.push((
                        doc.external_id.clone(),
                        project_document(&doc, &self.policy, return_fields)?,
                    )),
                None => not_found.push(id.clone()),
            }
        }
        let id_field = id_path_to_strip(
            self.policy.id_field().map(|f| f.name.as_str()),
            return_fields
        ).map(String::from);
        Ok(LookupResponse::new(found, not_found, id_field))
    }

    #[timed(flushing)]
    pub fn start_flush(&mut self) -> io::Result<FlushTicket> {
        // Store first: a durable index segment must never reference undurable sources.
        self.store.sync()?;
        let mut acks = std::mem::take(&mut self.index_in_flight);
        match self.index_worker.flush_deferred() {
            Ok(flush) => acks.push_back(flush),
            Err(e) => {
                self.index_in_flight = acks;
                return Err(e);
            }
        }
        Ok(FlushTicket { acks })
    }

    /// Flushes the store and the index and waits until both are on disk.
    pub fn flush(&mut self) -> io::Result<()> {
        self.start_flush()?.wait()
    }

    pub fn index_stats_cached(&self) -> IndexingStats {
        self.index_worker.cached_stats()
    }
    #[timed(database_lifecycle)]
    pub fn shutdown(mut self) -> io::Result<LsmIndex> {
        self.wait_index_in_flight()?;
        self.store.sync()?;
        self.index_worker.shutdown()
    }

    pub fn index_sender(&self) -> mpsc::Sender<IndexCommand> {
        self.index_worker.sender()
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

      #[timed(reindex)]
    pub fn reindex_existing_documents(
        &mut self,
        batch_size: usize,
        progress: &ReindexProgress
    ) -> io::Result<()> {
        let store = &self.store;
        let analyzer = &self.analyzer;
        let policy = &self.policy;
        let worker = &self.index_worker;
        let allocator = &mut self.array_row_allocator;
        let in_flight = &mut self.index_in_flight;

        let mut current: Vec<IndexedDocument> = Vec::with_capacity(batch_size);
        store.for_each_document(
            &mut (|doc| {
                if progress.is_cancelled() {
                    return Err(io::Error::other("reindex cancelled"));
                }
                let node = match parse_source_into_node(&doc.source, policy) {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("[reindex] skipping document {}: {e:?}", doc.external_id);
                        return Ok(());
                    }
                };
                current.push(index_document(doc.internal_id, &node, policy, allocator));
                if current.len() >= batch_size {
                    let batch = std::mem::replace(&mut current, Vec::with_capacity(batch_size));
                    send_reindex_batch(worker, analyzer, batch, in_flight, progress)?;
                }
                Ok(())
            })
        )?;
        if !current.is_empty() {
            send_reindex_batch(worker, analyzer, current, in_flight, progress)?;
        }
        self.wait_index_in_flight()?;
        self.flush()
    }

    pub fn index_stats(&self) -> io::Result<IndexingStats> {
        self.index_worker.get_stats()
    }
}

impl<'a, S: DocumentStore> IndexPipeline<'a, S> {
    #[timed(inserting)]
    pub fn push(&mut self, input: DocumentInput, input_index: usize) -> io::Result<()> {
        let external_id = input.external_id;

        if self.external_id_exists(&external_id)? {
            self.failures.push(
                DocFailure::with_id(input_index, external_id, FailReason::DuplicatePrimaryId)
            );
            return Ok(());
        }
        let internal_id = self.db.allocate_internal_id()?;
        self.seen.insert(external_id.clone());
        let indexed = index_document(
            internal_id,
            &input.parsed,
            &self.db.policy,
            &mut self.db.array_row_allocator
        );
        let stored = StoredDocument {
            external_id,
            internal_id,
            source: Arc::from(input.source),
            format: input.format,
        };
        self.current_store_batch.push(stored);
        self.current_batch.push(indexed);
        self.inserted += 1;

        let len = self.current_batch.len();
        let at_cap = len >= self.batch_size;
        if at_cap || (len >= MIN_INDEX_BATCH && self.db.index_idle()?) {
            self.flush_batch()?;
        }
        Ok(())
    }

    // Checks both persisted documents and IDs
    #[timed(inserting)]
    fn external_id_exists(&self, external_id: &str) -> io::Result<bool> {
        if self.seen.contains(external_id) {
            return Ok(true);
        }

        self.db.store.contains(external_id)
    }

    #[timed(inserting)]
    pub fn finish(mut self) -> io::Result<InsertReport> {
        self.flush_batch()?;
        // self.flush_window()?;

        //self.db.flush()?;
        Ok(InsertReport {
            inserted: self.inserted,
            failures: self.failures,
        })
    }


    #[timed(inserting)]
    fn flush_batch(&mut self) -> io::Result<()> {
        if self.current_batch.is_empty() {
            return Ok(());
        }
        let batch_bytes: u64 = self.current_store_batch
            .iter()
            .map(|d| d.source.len() as u64)
            .sum();
        core_timing::add_bytes("inserting", "flush_batch", file!(), batch_bytes);

        let stored = std::mem::take(&mut self.current_store_batch);
        self.db.store.put_batch(stored)?;
        self.db.error_return()?;

        let indexed = std::mem::take(&mut self.current_batch);
        let analyzer = self.db.analyzer.clone();
        let (analyzed, documents) = mpsc::channel();
        rayon::spawn(move || {
            // A panic in rayon::spawn would abort the process. Caught, it drops the sender
            // instead, and the index worker reports this batch as failed.
            if let Ok(docs) =
                panic::catch_unwind(AssertUnwindSafe(|| analyze_batch(&analyzer, &indexed)))
            {
                let _ = analyzed.send(docs);
            }
        });
        let pending = self.db.index_worker.add_indexed_documents_deferred(documents)?;
        self.db.index_in_flight.push_back(pending);

        self.current_store_batch = Vec::with_capacity(self.batch_size);
        self.current_batch = Vec::with_capacity(self.batch_size);
        Ok(())
    }
    
}


    /// Tokenizes a batch, in parallel on rayon's pool for larger batches. The output keeps the
    /// input order, so it lines up with the store batch.
    #[timed(indexing_documents)]
    fn analyze_batch(analyzer: &Analyzer, docs: &[IndexedDocument]) -> Vec<AnalyzedDocument> {
        use rayon::prelude::*;
        docs.par_iter()
            .map(|doc| doc.analyze(analyzer))
            .collect()
    }
#[timed(reindex)]
fn send_reindex_batch(
    worker: &IndexWorker,
    analyzer: &Analyzer,
    batch: Vec<IndexedDocument>,
    in_flight: &mut VecDeque<PendingAck>,
    progress: &ReindexProgress
) -> io::Result<()> {
    while in_flight.len() >= 2 {
        in_flight.pop_front().expect("len checked").wait()?;
    }
    let count = batch.len() as u64;
    let analyzer = analyzer.clone();
    let (analyzed, documents) = mpsc::channel();
    rayon::spawn(move || {
        if let Ok(docs) = panic::catch_unwind(AssertUnwindSafe(|| analyze_batch(&analyzer, &batch))) {
            let _ = analyzed.send(docs);
        }
    });
    in_flight.push_back(worker.add_indexed_documents_deferred(documents)?);
    progress.add(count);
    Ok(())
}

#[timed(database_lifecycle)]
fn next_local_id_for_shard<S: DocumentStore>(
    store: &S,
    index:&LsmIndex,
    shard_id: ShardId
) -> io::Result<LocalDocId> {
     let max_global_id = store.max_internal_id().max(index.highest_known_doc_id());

    if max_global_id == 0 {
        return Ok(1);
    }

    let stored_shard = shard_of(max_global_id);

    if stored_shard != shard_id {
        return Err(
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "document store belongs to shard {stored_shard}, \
                 but was opened as shard {shard_id}"
                )
            )
        );
    }

    let max_local_id = local_of(max_global_id);

    max_local_id
        .checked_add(1)
        .ok_or_else(|| io::Error::other(format!("document ID space is full for shard {shard_id}")))
}
