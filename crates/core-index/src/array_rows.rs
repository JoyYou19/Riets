use crate::types::{ArrayRowId, DocId};

pub const NO_PARENT_ROW: ArrayRowId = ArrayRowId::MAX;

//INFO: one rowid per array element in a database

#[derive(Debug, Clone, Default)]
pub struct ArrayRowAllocator {
    next: ArrayRowId,
}

impl ArrayRowAllocator {
    pub fn new() -> Self {
        Self { next: 0 }
    }

    //Resume from a recovered high-water mark, e.g
    pub fn starting_at(next: ArrayRowId) -> Self {
        Self { next }
    }

    pub fn alloc(&mut self) -> ArrayRowId {
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .expect("array row id space exhausted");
        id
    }

    pub fn next(&self) -> ArrayRowId {
        self.next
    }
}

#[derive(Debug, Clone)]
pub struct ArrayRowIndex {
    base: ArrayRowId,
    //array row -> its root document's packed DocId.
    row_to_doc: Vec<DocId>,
    //rowid -> parent id
    parent_row: Vec<ArrayRowId>,
}

impl ArrayRowIndex {
    pub fn new(base: ArrayRowId) -> Self {
        Self {
            base,
            row_to_doc: Vec::new(),
            parent_row: Vec::new(),
        }
    }

    pub fn push_row(&mut self, array_row_id: ArrayRowId, doc: DocId, parent: Option<ArrayRowId>) {
        debug_assert_eq!(
            array_row_id,
            self.base + self.row_to_doc.len() as ArrayRowId,
            "array rows must be pushed in allocation order"
        );
        self.row_to_doc.push(doc);
        self.parent_row.push(parent.unwrap_or(NO_PARENT_ROW));
    }

    pub fn doc_of(&self, array_row_id: ArrayRowId) -> Option<DocId> {
        let idx = array_row_id.checked_sub(self.base)?;
        self.row_to_doc.get(idx as usize).copied()
    }

    pub fn parent_of(&self, array_row_id: ArrayRowId) -> Option<ArrayRowId> {
        let idx = array_row_id.checked_sub(self.base)?;
        let parent = *self.parent_row.get(idx as usize)?;
        (parent != NO_PARENT_ROW).then_some(parent)
    }

    pub fn base(&self) -> ArrayRowId {
        self.base
    }

    pub fn len(&self) -> usize {
        self.row_to_doc.len()
    }

    pub fn is_empty(&self) -> bool {
        self.row_to_doc.is_empty()
    }

    pub fn max_array_row(&self) -> Option<ArrayRowId> {
        if self.is_empty() {
            None
        } else {
            Some(self.base + self.row_to_doc.len() as ArrayRowId - 1)
        }
    }
}

impl Default for ArrayRowIndex {
    fn default() -> Self {
        Self::new(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_is_monotonic_and_unique() {
        let mut alloc = ArrayRowAllocator::starting_at(10);
        assert_eq!(alloc.alloc(), 10);
        assert_eq!(alloc.alloc(), 11);
        assert_eq!(alloc.alloc(), 12);
        assert_eq!(alloc.next(), 13);
    }

    #[test]
    fn array_row_index_maps_rows_to_docs_and_parents() {
        // segment whose array rows start at 100
        let mut idx = ArrayRowIndex::new(100);

        // top-level array row (parent = the root document)
        idx.push_row(100, /*doc*/ 7, /*parent*/ None);
        // nested array row (parent = array row 100)
        idx.push_row(101, 7, Some(100));

        assert_eq!(idx.doc_of(100), Some(7));
        assert_eq!(idx.doc_of(101), Some(7));
        assert_eq!(idx.parent_of(100), None);
        assert_eq!(idx.parent_of(101), Some(100));

        // out-of-range lookups return None, don't panic
        assert_eq!(idx.doc_of(99), None);
        assert_eq!(idx.doc_of(102), None);

        assert_eq!(idx.max_array_row(), Some(101));
    }
}
