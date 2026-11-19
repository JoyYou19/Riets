use std::io;

use crate::types::{ARRAY_ROW_FLAG, ArrayRowId, DocId};
pub const NO_PARENT_ROW: ArrayRowId = ArrayRowId(u64::MAX);

#[derive(Debug, Clone, Default)]
pub struct ArrayRowAllocator {
    next: ArrayRowId,
}

impl ArrayRowAllocator {
    pub fn new() -> Self {
        Self {
            next: ArrayRowId(ARRAY_ROW_FLAG),
        }
    }

    pub fn starting_at(next: ArrayRowId) -> Self {
        Self { next }
    }

    pub fn alloc(&mut self) -> ArrayRowId {
        let id = self.next;
        self.next = ArrayRowId(
            id.raw()
                .checked_add(1)
                .expect("array row id space exhausted"),
        );
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
    //in which [i] of the parent was this element in
    element_index: Vec<u32>,
}

impl ArrayRowIndex {
    pub fn new(base: ArrayRowId) -> Self {
        Self {
            base,
            row_to_doc: Vec::new(),
            element_index: Vec::new(),
            parent_row: Vec::new(),
        }
    }

    pub fn parent_of(&self, array_row_id: ArrayRowId) -> Option<ArrayRowId> {
        let idx = array_row_id.raw().checked_sub(self.base.raw())?;
        let parent = *self.parent_row.get(idx as usize)?;
        (parent != NO_PARENT_ROW).then_some(parent)
    }

    pub fn push_row(
        &mut self,
        array_row_id: ArrayRowId,
        doc: DocId,
        parent: Option<ArrayRowId>,
        element_index: u32,
    ) {
        if self.row_to_doc.is_empty() {
            self.base = array_row_id;
        }
        debug_assert_eq!(
            array_row_id.raw(),
            self.base.raw() + self.row_to_doc.len() as u64,
            "array rows must be pushed in allocation order"
        );
        self.row_to_doc.push(doc);
        self.parent_row.push(parent.unwrap_or(NO_PARENT_ROW));
        self.element_index.push(element_index);
    }

    pub fn element_index_of(&self, array_row_id: ArrayRowId) -> Option<u32> {
        let idx = array_row_id.raw().checked_sub(self.base.raw())? as usize;
        self.element_index.get(idx).copied()
    }

    pub fn doc_of(&self, array_row_id: ArrayRowId) -> Option<DocId> {
        let idx = array_row_id.raw().checked_sub(self.base.raw())? as usize;
        self.row_to_doc.get(idx).copied()
    }

    pub fn max_array_row(&self) -> Option<ArrayRowId> {
        if self.is_empty() {
            None
        } else {
            Some(ArrayRowId(
                self.base.raw() + self.row_to_doc.len() as u64 - 1,
            ))
        }
    }

    pub fn merge_from(&mut self, other: &ArrayRowIndex) -> io::Result<()> {
        if other.is_empty() {
            return Ok(());
        }
        if self.is_empty() {
            self.base = other.base;
        } else if other.base.raw() != self.base.raw() + self.len() as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "array row index gap: cannot merge base {} (len {}) with base {} — reindex required",
                    self.base.raw(),
                    self.len(),
                    other.base.raw()
                ),
            ));
        }
        self.row_to_doc.extend_from_slice(&other.row_to_doc);
        self.element_index.extend_from_slice(&other.element_index);
        self.parent_row.extend_from_slice(&other.parent_row);
        Ok(())
    }

    pub fn element_indices(&self) -> &[u32] {
        &self.element_index
    }

    pub fn row_to_doc(&self) -> &[DocId] {
        &self.row_to_doc
    }
    pub fn parent_rows(&self) -> &[ArrayRowId] {
        &self.parent_row
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
}

impl Default for ArrayRowIndex {
    fn default() -> Self {
        Self::new(ArrayRowId(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_from_rejects_gap() {
        let mut a = ArrayRowIndex::default();
        a.push_row(ArrayRowId(0), 0, None, 0);
        a.push_row(ArrayRowId(1), 1, None, 0);

        let mut b = ArrayRowIndex::default();
        b.push_row(ArrayRowId(10), 2, None, 0); // gap after 1

        assert!(a.merge_from(&b).is_err());
    }

    #[test]
    fn merge_from_accepts_contiguous() {
        let mut a = ArrayRowIndex::default();
        a.push_row(ArrayRowId(0), 0, None, 0);
        let mut b = ArrayRowIndex::default();
        b.push_row(ArrayRowId(1), 1, None, 0);

        assert!(a.merge_from(&b).is_ok());
        assert_eq!(a.doc_of(ArrayRowId(1)), Some(1));
    }
}
