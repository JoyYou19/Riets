use core_index::types::{ArrayRowId, Position, XPathId};

//term of a phrase that matched withing one docs one field
//INFO: fast-path -> highlight reads the offests and reads the documents bytes to return the snippet
//      slow-path -> like <title>Batman</title>-> "Batman" bytes are inprecise so highlight would
//      re-analyze the field and only then return the snippet
#[derive(Debug, Clone)]
pub struct MatchedTerm {
    pub xpath: XPathId,
    pub positions: Vec<Position>,

    //for nested arrays
    pub row_id: Option<ArrayRowId>,

    //in which of [0, 1, 2] appeared in for highlighting
    //[2, 0] = cast[2].aliases[0].
    pub row_path: Vec<u32>,
    pub offsets: Vec<(u32, u32)>,
    //relevance of a matched term
    pub weight: f32,
}
