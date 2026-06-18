use std::{path::PathBuf, sync::Arc};

use crate::{disk::reader::DiskSegment, search::SearchIndex, segment::ImmutableSegment};

// What segment exists?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentHandle {
    Memory(Arc<ImmutableSegment>),
    Disk(PathBuf),
}

impl SegmentHandle {
    pub fn open_search_index(&self) -> std::io::Result<Arc<dyn SearchIndex + Send + Sync>> {
        match self {
            SegmentHandle::Memory(segment) => {
                Ok(segment.clone() as Arc<dyn SearchIndex + Send + Sync>)
            }

            SegmentHandle::Disk(path) => {
                let segment = DiskSegment::open(path)?;
                Ok(Arc::new(segment) as Arc<dyn SearchIndex + Send + Sync>)
            }
        }
    }
}
