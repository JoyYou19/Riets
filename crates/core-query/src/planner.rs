use crate::Query;

#[derive(Debug, Clone)]
pub struct QuerySignal {
    // Query to execute
    pub query: Query,
    // How much this contributes
    pub boost: f32,
    // If true documents must have it
    pub required: bool,
    // boost never filters
    pub rerank_only: bool,
}

#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub retrieval: Query,
    pub signals: Vec<QuerySignal>,
}

pub struct QueryPlanner;

impl QueryPlanner {
    pub fn plan(query: Query) -> QueryPlan {
        let mut signals = Vec::new();
        Self::collect_signals(&query, &mut signals);

        QueryPlan {
            retrieval: query,
            signals,
        }
    }

    fn collect_signals(query: &Query, out: &mut Vec<QuerySignal>) {
        match query {
            Query::Term(term) => {
                out.push(QuerySignal {
                    query: Query::Term(term.clone()),
                    boost: 0.15,
                    required: false,
                    rerank_only: true,
                });
            }

            Query::Prefix(prefix) => {
                out.push(QuerySignal {
                    query: Query::Prefix(prefix.clone()),
                    boost: 0.10,
                    required: false,
                    rerank_only: true,
                });
            }

            Query::Wildcard(pattern) => {
                out.push(QuerySignal {
                    query: Query::Wildcard(pattern.clone()),
                    boost: 0.05,
                    required: false,
                    rerank_only: true,
                });
            }

            Query::Phrase(terms) => {
                if terms.len() >= 2 {
                    out.push(QuerySignal {
                        query: Query::Phrase(terms.clone()),
                        boost: 4.0,
                        required: false,
                        rerank_only: true,
                    });
                }

                for term in terms {
                    Self::collect_signals(&Query::Term(term.clone()), out);
                }
            }

            Query::And(parts) => {
                // out.push(QuerySignal {
                //     query: query.clone(),
                //     boost: 1.0,
                //     required: true,
                //     rerank_only: false,
                // });

                let terms: Vec<String> = parts
                    .iter()
                    .filter_map(|part| match part {
                        Query::Term(term) => Some(term.clone()),
                        _ => None,
                    })
                    .collect();

                if terms.len() >= 2 {
                    out.push(QuerySignal {
                        query: Query::Phrase(terms.clone()),
                        boost: 5.0,
                        required: false,
                        rerank_only: true,
                    });

                    for window in terms.windows(2) {
                        out.push(QuerySignal {
                            query: Query::Phrase(window.to_vec()),
                            boost: 2.0,
                            required: false,
                            rerank_only: true,
                        });
                    }
                }

                for part in parts {
                    Self::collect_signals(part, out);
                }
            }

            Query::Or(parts) => {
                out.push(QuerySignal {
                    query: query.clone(),
                    boost: 0.75,
                    required: false,
                    rerank_only: false,
                });

                for part in parts {
                    Self::collect_signals(part, out);
                }
            }
        }
    }
}
