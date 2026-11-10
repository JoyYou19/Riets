# Corelamo Search — Query & Filter Format

Endpoint: `POST /api/databases/<db>/search`

```json
{
  "query": "...",          // optional — see "Match-all" below
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
| `"word"` | single word; auto-detects wildcard/prefix |
| `"word1 word2"` | multi-word → `WAND` (weakAnd): any word may match, docs matching more words rank higher |
| `{"term": "word"}` | term (single word only — multi-word is rejected) |
| `{"exact": "word words"}` | exact whole value (multi-word OK, case-insensitive) |
| `{"fuzzy": "word"}` | fuzzy, default options |
| `{"fuzzy": {"value": "word words", "fuzziness": "auto", "prefix_length": 2, "max_expansions": 50}}` | fuzzy with options |
| `{"phrase": "new york"}` / `{"phrase": ["new", "york"]}` | phrase (adjacent, in order) |

### Wildcard / prefix (auto-detected)

| input | result |
|---|---|
| `"g*"` | prefix `g` |
| `{"term": "g*"}` | prefix `g` |
| `{"term": "*verse"}` | wildcard (ends in "verse") |
| `{"term": "m?ry"}` | wildcard (single char) |
| `{"term": "coo[pr]er"}` | wildcard (char class) |

Detection runs per word, so a bare multi-word string classifies each word: `"g* verse"` → `WAND(prefix g, term verse)`.

### Combinators

`AND` / `OR` / `WAND` accept an array or a space-separated string:

```json
{ "query": { "AND": ["drama", "romance"] } }
{ "query": { "AND": "drama romance war" } }
{ "query": { "OR": ["war", {"exact": "Drama"}] } }
{ "query": { "WAND": "comedy romance drama" } }
```

- `AND` — all must match.
- `OR` — any matches (additive scoring).
- `WAND` — weakAnd (Vespa's default): any word may match, scored by which/how many; also the default for a bare multi-word string.

Words inside an array or string are split on whitespace (`"into *verse"` → `into` + prefix `verse`).

### Match-all (empty query)

`query` is optional. Any of these means "match all documents":

```json
{ }                    // no query key at all
{ "query": "" }        // empty string
{ "query": {} }        // empty object
{ "query": null }      // null
```

- nothing else → returns the first `docs` (default 10) by doc-id, score 0.
- `+ filters` → filter-only.
- `+ sort` → sort-only.

There is no `"match_all"` keyword anymore — `"match_all"` is just an ordinary word to search for.

## 2. filters

Filter values use the same leaf operators as query, plus `range`, `same_element`, and `bool`.
Unlike the main query, a **bare multi-word filter string is an `AND`** (every word must match).

```json
// text (term/exact/fuzzy/phrase)
{ "filters": { "genres": "Drama" } }                 // term
{ "filters": { "genres": "Drama Comedy" } }          // AND of the two words
{ "filters": { "genres": { "exact": "Drama" } } }
{ "filters": { "title": { "fuzzy": "braev" } } }

// bool (true / false)
{ "filters": { "featured": true } }
{ "filters": { "featured": false } }

// numeric — bare number, numeric string, or range string (no wrapper needed)
{ "filters": { "year": 2000 } }
{ "filters": { "year": "2000" } }                    // exact value
{ "filters": { "year": ">=1990" } }
{ "filters": { "year": "1990..1995" } }
{ "filters": { "year": { "range": "1929..1931" } } } // explicit alias

// same_element (same array element)
{ "filters": { "cast": { "same_element": { "name": { "exact": "Gary" }, "surname": { "exact": "Cooper" } } } } }
{ "filters": { "random_nums": { "same_element": { "num1": { "range": "30..70" }, "num2": ">=50" } } } }
```

### Numeric filters

`Integer` and `Float` fields accept a bare number, a numeric string, or a range string:

```json
{ "filters": { "year": 2000 } }      // JSON number → exact
{ "filters": { "year": "2000" } }    // exact
{ "filters": { "year": ">2000" } }   // strictly greater
{ "filters": { "price": 9.99 } }     // float
```

Range string syntax: `a..b` (inclusive), `>=a`, `>a`, `<=a`, `<a`, `=a`, `a` (exact).

A numeric string that isn't a valid number/range (`"year": "hello"`) is a `400`.

### Bool

A bare `true` / `false` matches docs where the field equals that value.

A doc where the bool field is **absent** matches **neither** `true` **nor** `false` — missing ≠ false.

### same_element

Numeric clauses inside `same_element` also accept bare numbers / numeric strings:

```json
{ "filters": { "grid": { "same_element": { "is_hot": true, "value": 5 } } } }
{ "filters": { "grid": { "same_element": { "is_hot": true, "value": ">=5" } } } }
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
- `ratio`: 0–100. **Blend only** — ratios must sum to ≤ 100 (the rest is relevance). A single blend field may omit `ratio` (defaults to 40).
- **Strict** ignores `ratio` entirely; fields are applied in declaration order as lexicographic keys, then relevance, then `doc_id` as tiebreakers.

A `sort` alone (blank query, no filters) is valid — it returns the top `docs` ordered by the sort key.

Supported field kinds: numeric (`Integer`/`Float`) and `Bool`.

Bool sort: `true` = 1.0, `false` = 0.0, missing sorts last (strict mode; in blend mode this currently holds only for `desc`).

## 5. return_fields

```json
{ "return_fields": { "title": true, "year": true, "cast": true } }
```

`false` hides a field; `true` includes it. Omit to return the whole document.
