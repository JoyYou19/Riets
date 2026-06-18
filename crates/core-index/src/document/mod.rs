pub mod document;
pub mod policy;

pub use document::{DocumentPart, IndexedDocument};
pub use policy::WeightInterval;
pub use policy::{FieldPolicy, IndexPolicy};
