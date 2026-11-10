use std::collections::HashMap;
use std::sync::Arc;

use core_index::{
    analyzer::{Analyzer, analyzer::AnalyzerSet},
    document::{
        IndexPolicy,
        policy::{FieldKind, FieldPolicy},
    },
    numeric_values::{NumericBound, NumericRange, NumericValue, parse_float, parse_integer},
    types::XPathId,
};
use core_protocol::{command_reponse_definitions::Fuzziness, errors::CorelamoError};
use indexmap::IndexMap;
use simd_json::{OwnedValue, StaticNode, base::TypedValue};

use crate::{
    Query,
    query_string_parser::{analyze_query, parse_json_query},
    synonyms::{Synonyms, expand_synonyms},
};

//for each query we construct analyzer + compiled query + the fields that are searched for with this
//analyzer
#[derive(Clone)]
pub struct QueryGroup {
    pub analyzer: Analyzer,
    pub query: Option<Query>,
    pub ctxs: Vec<FieldCtx>,
}

#[derive(Debug, Clone, Copy)]
pub struct FieldCtx {
    pub xpath: XPathId,
    pub exact_xpath: Option<XPathId>,
    pub row_keyed: bool,
}

//hujna prieks arrays nezinu vai vispar vel stradaa lol
#[derive(Debug, Clone)]
pub struct SameElementBinding {
    pub depth: u32, //inside how many arrays is this in
    pub clauses: Vec<ClauseBinding>,
}

#[derive(Debug, Clone)]
pub struct ClauseBinding {
    pub ctx: FieldCtx,
    pub depth: u32,
    pub nested: Option<Box<SameElementBinding>>,
}

#[derive(Debug, Clone)]
pub struct FieldQuery {
    pub ctx: FieldCtx,
    pub query: Query,
    pub row_keyed: bool,
    pub depth: u32,
    pub same_element: Option<SameElementBinding>, //Some when query is Query::SameElement
}

fn field_ctx(field: &FieldPolicy, policy: &IndexPolicy) -> FieldCtx {
    FieldCtx {
        xpath: field.xpath(policy),
        exact_xpath: field.exact_xpath(policy),
        row_keyed: field.row_keyed,
    }
}

pub fn field_analyzer<'a>(field: &FieldPolicy, analyzers: &'a AnalyzerSet) -> &'a Analyzer {
    field
        .analyzer
        .as_deref()
        .and_then(|name| analyzers.get(name))
        .unwrap_or_else(|| analyzers.default_analyzer())
}

pub fn compile_query(
    raw: &OwnedValue,
    search_fields: Option<&[String]>,
    analyzers: &AnalyzerSet,
    policy: &IndexPolicy,
    synonyms: Option<&Synonyms>,
) -> Result<Arc<Vec<QueryGroup>>, CorelamoError> {
    if is_blank_query(raw) {
        let fields: Vec<&FieldPolicy> = match search_fields {
            Some(names) => resolve_search_fields(names, false, policy)?,
            None => policy.searchable_fields(),
        };
        let ctxs: Vec<FieldCtx> = fields.iter().map(|f| field_ctx(f, policy)).collect();
        return Ok(Arc::new(vec![QueryGroup {
            analyzer: analyzers.default_analyzer().clone(),
            query: Some(Query::MatchAll),
            ctxs,
        }]));
    }

    reject_filter_only_operators(raw)?;

    let raw_query = parse_json_query(raw)?;
    let needs_exact = tree_has_exact(&raw_query);

    let fields: Vec<&FieldPolicy> = match search_fields {
        Some(names) => resolve_search_fields(names, needs_exact, policy)?,
        None => policy.searchable_fields(),
    };

    // group fields by analyzer name; analyze once per analyzer, then reuse
    let mut by_name: HashMap<Option<String>, (Analyzer, Option<Query>, Vec<FieldCtx>)> =
        HashMap::new();
    for field in fields {
        let key = field.analyzer.clone();
        let entry = by_name.entry(key).or_insert_with(|| {
            let analyzer = field_analyzer(field, analyzers).clone();
            let expanded = match synonyms {
                Some(dictionary) => expand_synonyms(raw_query.clone(), dictionary, &analyzer),
                None => raw_query.clone(),
            };
            let query = analyze_query(expanded, &analyzer);
            (analyzer, query, Vec::new())
        });
        entry.2.push(field_ctx(field, policy));
    }

    Ok(Arc::new(
        by_name
            .into_iter()
            .map(|(_, (analyzer, query, ctxs))| QueryGroup {
                analyzer,
                query,
                ctxs,
            })
            .collect(),
    ))
}

pub fn is_blank_query(v: &OwnedValue) -> bool {
    match v {
        OwnedValue::String(s) => s.trim().is_empty(),
        OwnedValue::Object(obj) => obj.is_empty(),
        OwnedValue::Static(StaticNode::Null) => true,
        _ => false,
    }
}

//determining which xpaths to look into either from search_fields or searchable ones
fn resolve_search_fields<'a>(
    names: &[String],
    needs_exact: bool,
    policy: &'a IndexPolicy,
) -> Result<Vec<&'a FieldPolicy>, CorelamoError> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let field = policy
            .field_by_path(name)
            .ok_or_else(|| CorelamoError::PathNotIndexed(name.clone()))?;

        let mut leaves: Vec<&FieldPolicy> = Vec::new();
        field.collect_leaves(&mut leaves);

        for leaf in leaves {
            if !leaf.searchable() && !needs_exact {
                return Err(CorelamoError::InvalidData(format!(
                    "field '{name}' is not searchable ..."
                )));
            }
            if needs_exact && leaf.exact_xpath(policy).is_none() {
                return Err(CorelamoError::InvalidData(format!(
                    "field '{name}' has no exact index ..."
                )));
            }
            out.push(leaf);
        }
    }
    Ok(out)
}

fn reject_filter_only_operators(v: &OwnedValue) -> Result<(), CorelamoError> {
    match v {
        OwnedValue::Object(obj) => {
            if obj.contains_key("same_element") {
                return Err(CorelamoError::InvalidData(
                    "'same_element' is only valid as a filter, not as the main query".into(),
                ));
            }
            if obj.contains_key("range") {
                return Err(CorelamoError::InvalidData(
                    "'range' is only valid as a filter, not as the main query".into(),
                ));
            }
            for (_, child) in obj.iter() {
                reject_filter_only_operators(child)?;
            }
        }
        OwnedValue::Array(items) => {
            for item in items.iter() {
                reject_filter_only_operators(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn tree_has_exact(q: &Query) -> bool {
    match q {
        Query::Exact(_) => true,
        Query::And(ps) | Query::Or(ps) | Query::Wand(ps) => ps.iter().any(tree_has_exact),
        Query::SameElement(ps) => ps.iter().any(tree_has_exact),
        //the dictionary's exact forms are optional: only what was typed counts
        Query::Synonym(forms) => forms
            .first()
            .is_some_and(|typed| tree_has_exact(&typed.query)),
        _ => false,
    }
}

pub fn compile_filters(
    filters: &IndexMap<String, OwnedValue>,
    analyzers: &AnalyzerSet,
    policy: &IndexPolicy,
) -> Result<HashMap<String, FieldQuery>, CorelamoError> {
    let mut out = HashMap::with_capacity(filters.len());

    for (name, raw) in filters {
        let field = policy
            .field_by_path(name)
            .ok_or_else(|| CorelamoError::PathNotIndexed(name.clone()))?;

        //same_element is a container spec — handle it before the blank check
        let is_same_element =
            matches!(raw, OwnedValue::Object(o) if o.contains_key("same_element"));

        let (query, same_element) = if is_same_element {
            let OwnedValue::Object(obj) = raw else {
                unreachable!()
            };
            let inner = obj.get("same_element").unwrap();
            let (q, b) = resolve_same_element(field, inner, analyzers, policy)?
                .ok_or_else(|| CorelamoError::InvalidData(format!("blank filter '{name}'")))?;
            (q, Some(b))
        } else {
            match build_node(raw, field, analyzers)? {
                Some(q) => (q, None),
                None => continue,
            }
        };

        if !is_same_element && tree_has_exact(&query) && field.exact_xpath(policy).is_none() {
            return Err(CorelamoError::InvalidData(format!(
                "field '{}' has no exact index (add 'exact = true' to its policy)",
                field.name
            )));
        }

        out.insert(
            name.clone(),
            FieldQuery {
                ctx: if is_same_element {
                    FieldCtx {
                        xpath: 0,
                        exact_xpath: None,
                        row_keyed: false,
                    }
                } else {
                    field_ctx(field, policy)
                },
                query,
                row_keyed: field.row_keyed,
                depth: field.depth,
                same_element,
            },
        );
    }

    Ok(out)
}

//single core: OwnedValue + field context -> resolved Query (non-same_element)
//single core: OwnedValue + field context -> resolved Query (non-same_element)
fn build_node(
    v: &OwnedValue,
    field: &FieldPolicy,
    analyzers: &AnalyzerSet,
) -> Result<Option<Query>, CorelamoError> {
    if let OwnedValue::Object(obj) = v
        && let Some(inner) = obj.get("range")
    {
        return resolve_numeric_filter(field, inner);
    }

    //          sis bisk porno bet simd sitaada strukturs
    if let OwnedValue::Static(StaticNode::Bool(b)) = v {
        if field.kind != FieldKind::Bool {
            return Err(CorelamoError::InvalidData(format!(
                "field '{}' is {} — expected a bool value (true/false)",
                field.name,
                field.kind.label()
            )));
        }
        return Ok(Some(Query::Bool(*b)));
    }

    // a numeric field takes a bare number or numeric string as a range:
    // {"year": 2000}, {"year": "2000"}, {"year": ">=1990"} — no {"range": ...} wrapper
    if field.kind.is_numeric() {
        return resolve_numeric_filter(field, v);
    }

    let node = parse_json_query(v)?;
    Ok(analyze_query(node, field_analyzer(field, analyzers)))
}

fn resolve_numeric_filter(
    field: &FieldPolicy,
    v: &OwnedValue,
) -> Result<Option<Query>, CorelamoError> {
    let parse: fn(&str) -> Option<NumericValue> = match field.kind {
        FieldKind::Integer => parse_integer,
        FieldKind::Float => parse_float,
        _ => {
            return Err(CorelamoError::InvalidData(format!(
                "field '{}' is {} — numeric filters only apply to numeric fields",
                field.name,
                field.kind.label()
            )));
        }
    };

    // a bare number or a numeric string
    let text: String = match v {
        OwnedValue::String(s) => s.clone(),
        OwnedValue::Static(StaticNode::I64(n)) => n.to_string(),
        OwnedValue::Static(StaticNode::U64(n)) => n.to_string(),
        OwnedValue::Static(StaticNode::F64(n)) => n.to_string(),
        other => {
            return Err(CorelamoError::InvalidData(format!(
                "field '{}' is {} — filter with a number or a range string like '30..40' (got {})",
                field.name,
                field.kind.label(),
                other.value_type()
            )));
        }
    };

    let r = parse_numeric_range(&text, parse).map_err(|e| {
        CorelamoError::InvalidData(format!("invalid filter on '{}': {e}", field.name))
    })?;
    Ok(Some(Query::Range(r)))
}

fn resolve_same_element(
    field: &FieldPolicy,
    inner: &OwnedValue,
    analyzers: &AnalyzerSet,
    policy: &IndexPolicy,
) -> Result<Option<(Query, SameElementBinding)>, CorelamoError> {
    if field.kind != FieldKind::Struct {
        return Err(CorelamoError::InvalidData(format!(
            "same_element requires an array field ('{}' is {})",
            field.name,
            field.kind.label()
        )));
    }

    let OwnedValue::Object(map) = inner else {
        return Err(CorelamoError::InvalidData(
            "'same_element' must be an object mapping subfield -> clause".into(),
        ));
    };

    let mut children = Vec::with_capacity(map.len());
    let mut clauses = Vec::with_capacity(map.len());

    for (sub_name, sub_v) in map.iter() {
        let full = format!("{}/{}", field.full_path, sub_name);
        let sub_field = policy
            .field_by_path(&full)
            .ok_or_else(|| CorelamoError::PathNotIndexed(full.clone()))?;

        //nested array-in-array
        if let OwnedValue::Object(o) = sub_v
            && o.contains_key("same_element")
        {
            let (sub_query, sub_binding) =
                resolve_same_element(sub_field, o.get("same_element").unwrap(), analyzers, policy)?
                    .ok_or_else(|| {
                        CorelamoError::InvalidData(format!("blank clause '{sub_name}'"))
                    })?;

            clauses.push(ClauseBinding {
                ctx: FieldCtx {
                    xpath: 0,
                    exact_xpath: None,
                    row_keyed: false,
                },
                depth: sub_field.depth,
                nested: Some(Box::new(sub_binding)),
            });
            children.push(sub_query);
            continue;
        }

        let sub = build_node(sub_v, sub_field, analyzers)?
            .ok_or_else(|| CorelamoError::InvalidData(format!("blank clause '{sub_name}'")))?;

        if tree_has_exact(&sub) && sub_field.exact_xpath(policy).is_none() {
            return Err(CorelamoError::InvalidData(format!(
                "subfield '{full}' has no exact index (add 'exact = true' to its policy)"
            )));
        }

        clauses.push(ClauseBinding {
            ctx: field_ctx(sub_field, policy),
            depth: sub_field.depth,
            nested: None,
        });
        children.push(sub);
    }

    Ok(Some((
        Query::SameElement(children),
        SameElementBinding {
            depth: field.depth,
            clauses,
        },
    )))
}

pub fn parse_fuzziness(raw: Option<&str>) -> Result<Fuzziness, CorelamoError> {
    let Some(raw) = raw else {
        return Ok(Fuzziness::Auto);
    };

    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Ok(Fuzziness::Auto),
        "0" => Ok(Fuzziness::Zero),
        "1" => Ok(Fuzziness::One),
        "2" => Ok(Fuzziness::Two),
        other => Err(CorelamoError::InvalidData(format!(
            "fuzziness must be \"auto\", 0, 1 or 2, got '{other}'"
        ))),
    }
}

pub fn resolve_suggest_xpaths(
    names: &[String],
    policy: &IndexPolicy,
) -> Result<Vec<XPathId>, CorelamoError> {
    let mut out: Vec<XPathId> = Vec::with_capacity(names.len());

    for name in names {
        let field = policy
            .fields
            .iter()
            .find(|f| &f.name == name)
            .ok_or_else(|| CorelamoError::PathNotIndexed(name.clone()))?;

        if !field.searchable() {
            return Err(CorelamoError::InvalidData(format!(
                "field '{name}' is not searchable (add 'searchable = true' to its policy)"
            )));
        }

        let xpath = field.xpath(policy);

        if !out.contains(&xpath) {
            out.push(xpath);
        }
    }

    Ok(out)
}

//vibemaxxing funciton: parsing the query for the foken numbres
pub fn parse_numeric_range(
    raw: &str,
    parse: fn(&str) -> Option<NumericValue>,
) -> Result<NumericRange, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty numeric filter".to_string());
    }

    if let Some(idx) = s.find("..") {
        let left = s[..idx].trim();
        let right = s[idx + 2..].trim();
        if has_op_prefix(left) || has_op_prefix(right) {
            return Err(format!(
                "comparison operators can't be combined with '..' (use '30..40', or '>40'): '{s}'"
            ));
        }
        if left.is_empty() || right.is_empty() {
            return Err(format!(
                "'..' requires both bounds, e.g. '30..40' (use '>=30' or '<=40' for one-sided ranges): '{s}'"
            ));
        }
        let lo = NumericBound {
            value: parse(left).ok_or_else(|| format!("invalid lower bound '{left}'"))?,
            inclusive: true,
        };
        let hi = NumericBound {
            value: parse(right).ok_or_else(|| format!("invalid upper bound '{right}'"))?,
            inclusive: true,
        };
        if lo.value > hi.value {
            return Err(format!(
                "empty range: lower '{left}' is greater than upper '{right}'"
            ));
        }
        return Ok(NumericRange {
            lo: Some(lo),
            hi: Some(hi),
        });
    }

    let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = s.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = s.strip_prefix("==") {
        ("==", r)
    } else if let Some(r) = s.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = s.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = s.strip_prefix('=') {
        ("=", r)
    } else {
        ("", s)
    };

    let rest = rest.trim();
    if rest.is_empty() {
        return Err(format!("missing number after '{op}'"));
    }
    let value = parse(rest).ok_or_else(|| format!("invalid number '{rest}'"))?;

    let range = match op {
        "=" | "==" | "" => {
            let bound = NumericBound {
                value,
                inclusive: true,
            };
            NumericRange {
                lo: Some(bound),
                hi: Some(bound),
            }
        }
        ">=" => NumericRange {
            lo: Some(NumericBound {
                value,
                inclusive: true,
            }),
            hi: None,
        },
        ">" => NumericRange {
            lo: Some(NumericBound {
                value,
                inclusive: false,
            }),
            hi: None,
        },
        "<=" => NumericRange {
            lo: None,
            hi: Some(NumericBound {
                value,
                inclusive: true,
            }),
        },
        "<" => NumericRange {
            lo: None,
            hi: Some(NumericBound {
                value,
                inclusive: false,
            }),
        },
        _ => return Err(format!("unknown operator in '{s}'")),
    };
    Ok(range)
}

fn has_op_prefix(t: &str) -> bool {
    t.starts_with('=') || t.starts_with('>') || t.starts_with('<')
}
