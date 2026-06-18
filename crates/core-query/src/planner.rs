use crate::ast::Query;

#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub root: Query,
}

pub struct QueryPlanner;

impl QueryPlanner {
    pub fn plan(query: Query) -> QueryPlan {
        QueryPlan { root: query }
    }
}
