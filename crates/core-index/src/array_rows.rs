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

#[derive(Debug, Clone, PartialEq, Eq)]
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

    pub fn parent_of(&self, array_row_id: ArrayRowId) -> Option<ArrayRowId> {
        let idx = array_row_id.checked_sub(self.base)?;
        let parent = *self.parent_row.get(idx as usize)?;
        (parent != NO_PARENT_ROW).then_some(parent)
    }

    pub fn push_row(&mut self, array_row_id: ArrayRowId, doc: DocId, parent: Option<ArrayRowId>) {
        if self.row_to_doc.is_empty() {
            self.base = array_row_id;
        }
        debug_assert_eq!(
            array_row_id,
            self.base + self.row_to_doc.len() as ArrayRowId,
            "array rows must be pushed in allocation order"
        );
        self.row_to_doc.push(doc);
        self.parent_row.push(parent.unwrap_or(NO_PARENT_ROW));
    }

    pub fn merge_from(&mut self, other: &ArrayRowIndex) {
        if other.is_empty() {
            return;
        }
        if self.is_empty() {
            self.base = other.base;
        }
        debug_assert_eq!(other.base, self.base + self.len() as ArrayRowId);
        self.row_to_doc.extend_from_slice(&other.row_to_doc);
        self.parent_row.extend_from_slice(&other.parent_row);
    }

    pub fn row_to_doc(&self) -> &[DocId] {
        &self.row_to_doc
    }
    pub fn parent_rows(&self) -> &[ArrayRowId] {
        &self.parent_row
    }

    pub fn doc_of(&self, array_row_id: ArrayRowId) -> Option<DocId> {
        let idx = array_row_id.checked_sub(self.base)?;
        self.row_to_doc.get(idx as usize).copied()
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
