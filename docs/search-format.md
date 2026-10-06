# Corelamo Search — Query & Filter Format

Endpoint: `POST /api/databases/<db>/search`

```json
{
  "query": "...",          // main query (see below)
  "filters": { ... },      // optional
  "search_fields": [...],  // optional
  "docs": 10,              // optional (default 10)
  "offset": 0,             // optional (default 0)
  "return_fields": {...},  // optional
  "sort": { ... }          // optional
}
```

## 1. Main query

### Leaf operators

| JSON | meaning |
|---|---|
| `"word"` | term (single word; auto-detects wildcard/prefix) |
| `{"term": "word"}` | term (single word) |
| `{"exact": "word words"}` | exact (multi-word OK, case-sensitive, phrase-matched) |
| `{"fuzzy": "word"}` | fuzzy, default options |
| `{"fuzzy": {"value": "word words", "fuzziness": "auto", "prefix_length": 2, "max_expansions": 50}}` | fuzzy with options |
| `{"phrase": ["new", "york"]}` | phrase |
| `"match_all"` | match all documents |

### Wildcard / prefix (auto-detected inside term)

| input | result |
|---|---|
| `{"term": "g*"}` | prefix `g` |
| `{"term": "*verse"}` | wildcard (ends in "verse") |
| `{"term": "m?ry"}` | wildcard (single char) |
| `{"term": "coo[pr]er"}` | wildcard (char class) |

### Combinators

`AND` / `OR` / `WAND` accept an array or a space-separated string:

```json
{ "query": { "AND": ["drama", "romance"] } }
{ "query": { "AND": "drama romance war" } }
{ "query": { "OR": ["war", {"exact": "Drama"}] } }
{ "query": { "WAND": "comedy romance drama" } }
```

- `AND` — all must match (conjunctive).
- `OR` / `WAND` — any matches (disjunctive, additive scoring). `WAND` is the "relevance" alias.

Multi-word strings inside an array are split into words (`"into *verse"` → `into` AND `*verse`).

### Empty query

```json
{ "query": "" }
{ "query": {} }
```

Blank query = filter-only. Errors `400` if `filters` is also empty. (Use `"match_all"` to return everything.)

## 2. filters

Filter values use the same leaf operators as query, plus `range`, `same_element`, and `bool`.

```json
// leaf (term/exact/fuzzy/phrase)
{ "filters": { "genres": "Drama" } }
{ "filters": { "genres": { "exact": "Drama" } } }
{ "filters": { "title": { "fuzzy": "braev" } } }

// bool (true / false)
{ "filters": { "featured": true } }
{ "filters": { "featured": false } }

// numeric range
{ "filters": { "year": { "range": "1929..1931" } } }
{ "filters": { "year": { "range": ">=1930" } } }
{ "filters": { "year": { "range": "1982" } } }   // exact value

// same_element (same array element)
{ "filters": { "cast": { "same_element": { "name": { "exact": "Gary" }, "surname": { "exact": "Cooper" } } } } }
{ "filters": { "random_nums": { "same_element": { "num1": { "range": "30..70" }, "num2": { "range": "50..100" } } } } }
```

### Range syntax

`a..b` (inclusive), `>=a`, `>a`, `<=a`, `<a`, `=a`, `a` (exact).

### Bool

A bare `true` / `false` matches docs where the field equals that value.

A doc where the bool field is **absent** matches **neither** `true` **nor** `false` — missing ≠ false.

### Numeric values in same_element

Numeric clauses inside `same_element` use the `{"range": ...}` form. A bare number is rejected:

```json
// WRONG — 400 "query node must be a string or object, found u64"
{ "filters": { "grid": { "same_element": { "is_hot": true, "value": 5 } } } }

// RIGHT
{ "filters": { "grid": { "same_element": { "is_hot": true, "value": { "range": "5" } } } } }
```

Multiple filters are AND-ed together.

## 3. search_fields

Lists which fields the main query runs against (per-field, "any element" for arrays).

```json
{ "query": "gary", "search_fields": ["cast/name"] }
{ "query": "robinson", "search_fields": ["cast/name", "cast/surname"] }
```

- Absent → default = all `searchable = true`, non-array fields (+ automatic same-element array bonus).
- Present → only the listed fields; the automatic array bonus is disabled.

Array subfields (`cast/name`, `random_nums/num1`) match "any element" (not necessarily the same person).

## 4. sort

Sort fields go directly under `sort` (flattened):

```json
{ "sort": { "mode": "blend", "year": { "order": "asc", "ratio": 40 } } }
```

or inside an optional `fields` wrapper — use this when a document field is literally named `mode`:

```json
{ "sort": { "mode": "strict", "fields": { "mode": { "order": "asc" }, "year": { "order": "asc" } } } }
```

- `mode`: `"blend"` (default) | `"strict"`.
- `order`: `"asc"` | `"desc"` (default `desc`).
- `ratio`: 0–100. **Blend only** — ratios must sum to ≤ 100 (the rest is relevance). A single blend field may omit `ratio` (defaults to 100).
- **Strict** ignores `ratio` entirely; fields are applied in declaration order as lexicographic keys, then relevance, then `doc_id` as tiebreakers.

Supported field kinds: numeric (`Integer`/`Float`) and `Bool`.

Bool sort: `true` = 1.0, `false` = 0.0, missing sorts last (in both `asc` and `desc`).

## 5. return_fields

```json
{ "return_fields": { "title": true, "year": true, "cast": true } }
```

`false` hides a field; `true` includes it. Omit to return the whole document.
