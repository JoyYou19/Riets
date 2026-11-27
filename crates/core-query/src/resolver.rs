use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use core_index::datetimes::parse_date;
use core_index::document::policy::{ExactMode, FieldPolicy};
use core_index::{
    analyzer::{Analyzer, text_analyzer::AnalyzerSet},
    document::{IndexPolicy, policy::FieldKind},
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
    pub negatives: Vec<Query>,
}

#[derive(Debug, Clone, Copy)]
pub struct FieldCtx {
    pub xpath: XPathId,
    pub exact_xpath: Option<XPathId>,
    pub row_keyed: bool,
    pub searchable: bool,
    pub exact: Option<ExactMode>,
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
    let exact_xpath = field.exact_xpath(policy);
    FieldCtx {
        xpath: field.xpath(policy),
        exact_xpath,
        row_keyed: field.row_keyed,
        searchable: field.searchable(),
        exact: exact_xpath.map(|_| field.exact.unwrap_or(ExactMode::CaseInsensitive)),
    }
}

#[must_use]
pub fn field_analyzer<'a>(field: &FieldPolicy, analyzers: &'a AnalyzerSet) -> &'a Analyzer {
    field
        .analyzer
        .as_deref()
        .and_then(|name| analyzers.get(name))
        .unwrap_or_else(|| analyzers.default_analyzer())
}
type ByName = BTreeMap<Option<String>, (Analyzer, Option<Query>, Vec<Query>, Vec<FieldCtx>)>;

#[allow(
    clippy::rc_buffer,
    reason = "callers keep the compiled groups as Arc<Vec<_>>"
)]
pub fn compile_query(
    raw: &OwnedValue,
    search_fields: Option<&[String]>,
    analyzers: &AnalyzerSet,
    policy: &IndexPolicy,
    synonyms: Option<&Synonyms>,
    wildcards_enabled: bool,
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
            negatives: Vec::new(),
        }]));
    }

    reject_filter_only_operators(raw)?;

    let raw_query = parse_json_query(raw, Query::And, wildcards_enabled)?;
    let needs_exact = tree_has_exact(&raw_query);

    let fields: Vec<&FieldPolicy> = match search_fields {
        Some(names) => resolve_search_fields(names, needs_exact, policy)?,
        None => default_search_fields(policy, needs_exact),
    };

    if needs_exact
        && !fields
            .iter()
            .any(|field| field.exact_xpath(policy).is_some())
    {
        return Err(CorelamoError::InvalidData(
            "the query contains an exact match, but no selected field has an exact index".into(),
        ));
    }

    // group fields by analyzer name; analyze once per analyzer, then reuse
    let mut by_name: ByName = BTreeMap::new();
    for field in fields {
        let key = field.analyzer.clone();
        let entry = by_name.entry(key).or_insert_with(|| {
            let analyzer = field_analyzer(field, analyzers).clone();
            let expanded = match synonyms {
                Some(dictionary) => expand_synonyms(raw_query.clone(), dictionary, &analyzer),
                None => raw_query.clone(),
            };
            let (query, negatives) = match analyze_query(expanded, &analyzer) {
                Some(q) => {
                    let (positive, negatives) = split_not(q);
                    (Some(positive), negatives)
                }
                None => (None, Vec::new()),
            };
            (analyzer, query, negatives, Vec::new())
        });
        entry.3.push(field_ctx(field, policy));
    }

    Ok(Arc::new(
        by_name
            .into_values()
            .map(|(analyzer, query, negatives, ctxs)| QueryGroup {
                analyzer,
                query,
                ctxs,
                negatives,
            })
            .collect(),
    ))
}
#[must_use]
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
            out.push(leaf);
        }
    }
    Ok(out)
}

fn default_search_fields<'a>(policy: &'a IndexPolicy, needs_exact: bool) -> Vec<&'a FieldPolicy> {
    let mut fields = policy.searchable_fields();
    if needs_exact {
        for leaf in policy.indexed_fields() {
            if leaf.exact_xpath(policy).is_some()
                && !fields.iter().any(|known| known.full_path == leaf.full_path)
            {
                fields.push(leaf);
            }
        }
    }
    fields
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
            for child in obj.values() {
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

//ok so NOTs are un-rankable shits so we split the whole query into the query + all the NOT: {....}
//so that we can consider them as pure filters
fn split_not(query: Query) -> (Query, Vec<Query>) {
    let mut negatives = Vec::new();
    let positive = strip_not_in_query(query, &mut negatives);
    (positive, negatives)
}

fn strip_not_in_query(query: Query, negatives: &mut Vec<Query>) -> Query {
    match query {
        Query::Not(inner) => {
            negatives.push(Query::Not(inner));
            Query::MatchAll
        }
        Query::And(parts) => collapse_not(parts, Query::And, negatives),
        Query::Or(parts) => collapse_not(parts, Query::Or, negatives),
        Query::Wand(parts) => collapse_not(parts, Query::Wand, negatives),
        leaf => leaf,
    }
}

fn collapse_not(
    parts: Vec<Query>,
    make: fn(Vec<Query>) -> Query,
    negatives: &mut Vec<Query>,
) -> Query {
    let mut kept: Vec<Query> = parts
        .into_iter()
        .map(|p| strip_not_in_query(p, negatives))
        .filter(|q| !matches!(q, Query::MatchAll))
        .collect();
    if kept.len() > 1 {
        return make(kept);
    }
    kept.pop().unwrap_or(Query::MatchAll)
}

fn tree_has_exact(q: &Query) -> bool {
    match q {
        Query::Exact(_) | Query::ExactField(_) => true,
        Query::And(ps) | Query::Or(ps) | Query::Wand(ps) | Query::SameElement(ps) => {
            ps.iter().any(tree_has_exact)
        }
        Query::Not(inner) => tree_has_exact(inner),
        //the dictionary's exact forms are optional: only what was typed counts
        Query::Synonym(forms) => forms
            .first()
            .is_some_and(|typed| tree_has_exact(&typed.query)),
        _ => false,
    }
}

pub fn resolve_filters(
    filters: &IndexMap<String, OwnedValue>,
    analyzers: &AnalyzerSet,
    policy: &IndexPolicy,
    wildcards_enabled: bool,
) -> Result<HashMap<String, FieldQuery>, CorelamoError> {
    let mut out = HashMap::with_capacity(filters.len());

    for (name, raw) in filters {
        let field = policy
            .field_by_path(name)
            .ok_or_else(|| CorelamoError::PathNotIndexed(name.clone()))?;

        //same_element is a container spec — handle it before the blank check
        let (negate, inner_raw) = if let OwnedValue::Object(o) = raw
            && let Some(inner) = o.get("NOT")
        {
            (true, inner)
        } else {
            (false, raw)
        };
        let same_element_inner = match inner_raw {
            OwnedValue::Object(o) => o.get("same_element"),
            _ => None,
        };
        let is_same_element = same_element_inner.is_some();

        let (query, same_element) = if let Some(inner) = same_element_inner {
            let (q, b) = resolve_same_element(field, inner, analyzers, policy, wildcards_enabled)?
                .ok_or_else(|| CorelamoError::InvalidData(format!("blank filter '{name}'")))?;
            let q = if negate { Query::Not(Box::new(q)) } else { q };
            (q, Some(b))
        } else {
            let Some(q) = build_node(raw, field, analyzers, wildcards_enabled)? else {
                continue;
            };
            (q, None)
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
                        exact: None,
                        searchable: false,
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
fn build_node(
    v: &OwnedValue,
    field: &FieldPolicy,
    analyzers: &AnalyzerSet,
    wildcards_enabled: bool,
) -> Result<Option<Query>, CorelamoError> {
    //{"field": {"NOT": inner}} — negate the inner clause
    if let OwnedValue::Object(obj) = v
        && let Some(inner) = obj.get("NOT")
        && !matches!(
            field.kind,
            FieldKind::Text | FieldKind::Id | FieldKind::IdAuto
        )
    {
        let inner_query = build_node(inner, field, analyzers, wildcards_enabled)?
            .ok_or_else(|| CorelamoError::InvalidData("blank 'NOT' clause".into()))?;
        return Ok(Some(match inner_query {
            Query::Not(x) => *x, // NOT NOT x == x
            q => Query::Not(Box::new(q)),
        }));
    }

    if let OwnedValue::Object(obj) = v
        && let Some(inner) = obj.get("range")
    {
        if field.kind == FieldKind::DateTime {
            return resolve_date_filter(field, inner);
        }
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

    if field.kind == FieldKind::Bool {
        return match v {
            OwnedValue::String(s) => match s.trim() {
                "true" => Ok(Some(Query::Bool(true))),
                "false" => Ok(Some(Query::Bool(false))),
                _ => Err(CorelamoError::InvalidData(format!(
                    "field '{}' is bool — expected true/false (got \"{s}\")",
                    field.name
                ))),
            },
            other => Err(CorelamoError::InvalidData(format!(
                "field '{}' is bool — expected true/false (got {})",
                field.name,
                other.value_type()
            ))),
        };
    }

    // a numeric field takes a bare number or numeric string as a range:
    // {"year": 2000}, {"year": "2000"}, {"year": ">=1990"} — no {"range": ...} wrapper
    if field.kind.is_numeric() {
        return resolve_numeric_filter(field, v);
    }

    if field.kind == FieldKind::DateTime {
        return resolve_date_filter(field, v);
    }

    let node = parse_json_query(v, Query::And, wildcards_enabled)?;
    Ok(analyze_query(node, field_analyzer(field, analyzers)))
}

fn resolve_date_filter(
    field: &FieldPolicy,
    v: &OwnedValue,
) -> Result<Option<Query>, CorelamoError> {
    let format = field.date_format.as_deref().unwrap_or("YYYY-MM-DD");

    // a bare date string, an epoch-millis number, or a range string
    let text: String = match v {
        OwnedValue::String(s) => s.clone(),
        OwnedValue::Static(StaticNode::I64(n)) => n.to_string(),
        OwnedValue::Static(StaticNode::U64(n)) => n.to_string(),
        other => {
            return Err(CorelamoError::InvalidData(format!(
                "field '{}' is date — filter with a date string or a range like '2024-01-01..2024-12-31' (got {})",
                field.name,
                other.value_type()
            )));
        }
    };

    let parse = |s: &str| -> Result<NumericValue, &'static str> {
        parse_date(s, format)
            .map(NumericValue::Int)
            .map_err(|_invalid| "invalid date")
    };
    let r = parse_numeric_range(&text, parse).map_err(|e| {
        CorelamoError::InvalidData(format!("invalid filter on '{}': {e}", field.name))
    })?;
    Ok(Some(Query::Range(r)))
}

fn resolve_numeric_filter(
    field: &FieldPolicy,
    v: &OwnedValue,
) -> Result<Option<Query>, CorelamoError> {
    let parse: fn(&str) -> Result<NumericValue, &'static str> = match field.kind {
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
    wildcards_enabled: bool,
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
            && let Some(nested) = o.get("same_element")
        {
            let (sub_query, sub_binding) =
                resolve_same_element(sub_field, nested, analyzers, policy, wildcards_enabled)?
                    .ok_or_else(|| {
                        CorelamoError::InvalidData(format!("blank clause '{sub_name}'"))
                    })?;

            clauses.push(ClauseBinding {
                ctx: FieldCtx {
                    xpath: 0,
                    exact_xpath: None,
                    row_keyed: false,
                    searchable: false,
                    exact: None,
                },
                depth: sub_field.depth,
                nested: Some(Box::new(sub_binding)),
            });
            children.push(sub_query);
            continue;
        }

        let sub = build_node(sub_v, sub_field, analyzers, wildcards_enabled)?
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

//vibemaxxing funciton: parsing the query for the foken numbres splitted 2 helpers below
pub fn parse_numeric_range(
    raw: &str,
    parse: impl Fn(&str) -> Result<NumericValue, &'static str>,
) -> Result<NumericRange, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty numeric filter".to_owned());
    }

    if let Some((left, right)) = s.split_once("..") {
        return parse_between(s, left.trim(), right.trim(), &parse);
    }

    let (op, rest) = split_operator(s);
    let rest = rest.trim();
    if rest.is_empty() {
        return Err(format!("missing number after '{op}'"));
    }
    let value = parse(rest).map_err(|e| format!("invalid value '{rest}': {e}"))?;

    let bound = |inclusive: bool| Some(NumericBound { value, inclusive });
    match op {
        "=" | "==" | "" => Ok(NumericRange {
            lo: bound(true),
            hi: bound(true),
        }),
        ">=" => Ok(NumericRange {
            lo: bound(true),
            hi: None,
        }),
        ">" => Ok(NumericRange {
            lo: bound(false),
            hi: None,
        }),
        "<=" => Ok(NumericRange {
            lo: None,
            hi: bound(true),
        }),
        "<" => Ok(NumericRange {
            lo: None,
            hi: bound(false),
        }),
        _ => Err(format!("unknown operator in '{s}'")),
    }
}

fn parse_between(
    s: &str,
    left: &str,
    right: &str,
    parse: &impl Fn(&str) -> Result<NumericValue, &'static str>,
) -> Result<NumericRange, String> {
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
        value: parse(left).map_err(|e| format!("invalid lower bound '{left}': {e}"))?,
        inclusive: true,
    };
    let hi = NumericBound {
        value: parse(right).map_err(|e| format!("invalid upper bound '{right}': {e}"))?,
        inclusive: true,
    };
    if lo.value > hi.value {
        return Err(format!(
            "empty range: lower '{left}' is greater than upper '{right}'"
        ));
    }
    Ok(NumericRange {
        lo: Some(lo),
        hi: Some(hi),
    })
}

//longest operators first, so ">=" is not read as ">"
fn split_operator(s: &str) -> (&str, &str) {
    for op in [">=", "<=", "==", ">", "<", "="] {
        if let Some(rest) = s.strip_prefix(op) {
            return (op, rest);
        }
    }
    ("", s)
}

fn has_op_prefix(t: &str) -> bool {
    t.starts_with('=') || t.starts_with('>') || t.starts_with('<')
}
