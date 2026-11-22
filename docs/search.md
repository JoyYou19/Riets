# Search

Endpoint: `POST /api/databases/<db>/search`

```json
{
  "query": "...",          // optional — see "Match-all"
  "filters": { ... },      // optional
  "search_fields": [...],  // optional
  "docs": 10,              // optional (default 10)
  "offset": 0,             // optional (default 0)
  "return_fields": {...},  // optional
  "sort": { ... },         // optional
  "highlight": { ... },    // optional — see "Highlighting"
  "wildcard_enabled": true // optional (default true)
}
```

### Quick start (curl)

```bash
# simple text search
curl -s -X POST "http://localhost:6006/api/databases/movies/search" \
  -d '{"query": "spiderman tom holand"}'

# exact + filter + sort + highlight
curl -s -X POST "http://localhost:6006/api/databases/movies/search" \
  -d '{"query": {"exact": "Drama"}, "filters": {"year": ">=2000"},
       "sort": {"year": {"order": "desc"}},
       "highlight": {"fields": {"title": {}}}, "docs": 3}'
```

---

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

### Negation (`NOT`)

`{"NOT": <clause>}` excludes documents that match the clause. A bare string inside
`NOT` is an **`OR`** of its words: `{"NOT": "jaguar car"}` excludes docs containing
*either* word, same as `{"NOT": {"OR": "jaguar car"}}`.

```json
{ "query": { "NOT": "documentary" } }                       // everything except documentary
{ "query": { "AND": ["spiderman", { "NOT": "animation" }] } }
{ "query": { "NOT": { "OR": "war western" } } }             // neither war nor western
{ "query": { "NOT": { "AND": "comedy romance" } } }         // not both (one alone is fine)
{ "query": { "NOT": { "phrase": "new york" } } }            // exclude the phrase
{ "query": { "NOT": { "exact": "drama" } } }                // exclude the exact value
```

- `NOT` is a **hard exclusion** — it never adds to the score, it only removes documents.
- A pure-negative query (only `NOT`, nothing positive) matches everything else.
- `NOT NOT x` normalizes to `x`.
- `NOT` can wrap any clause: term, exact, phrase, fuzzy, `AND`/`OR`/`WAND` — and, in
  filters, ranges, bools and `same_element`.

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
- `+ sort` → sort-only (see §4).

There is no `"match_all"` keyword — `"match_all"` is just an ordinary word to search for.

---

## 2. filters

Filter values use the same leaf operators as query, plus `range`, `same_element`, `bool`, and `NOT`.
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

### Negation (`NOT`)

`{"field": {"NOT": <clause>}}` excludes docs that match the clause, for every field
kind — text, bool, numeric, date, and `same_element`.

```json
{ "filters": { "genres": { "NOT": { "exact": "Drama" } } } }
{ "filters": { "year": { "NOT": ">2000" } } }
{ "filters": { "is_cool": { "NOT": true } } }
{ "filters": { "release_date": { "NOT": ">=2024-01-01" } } }
{ "filters": { "cast": { "NOT": { "same_element": { "name": { "exact": "Tobey" }, "surname": { "exact": "Maguire" } } } } } }
```

`NOT` filters are AND-ed with the other filters — a doc must clear all of them.

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

### Date filters

`date` fields behave like numeric fields: a bare date string, or a range string
with the same operators. Values parse with the field's `$format` (and its default
timezone, if any); an explicit timezone on the value overrides it.

```json
{ "filters": { "release_date": "2024-01-15" } }
{ "filters": { "release_date": "2024-01-01..2024-12-31" } }
{ "filters": { "release_date": ">=2024-01-15" } }
{ "filters": { "event_time": ">=2024-07-04 14:30:00+03:00" } }  // explicit tz overrides default
{ "filters": { "created_at": ">=1705276800000" } }               // epoch-millis
```

An invalid date is a `400`, same as a bad numeric filter.

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

---

## 3. search_fields

Lists which fields the main query runs against (per-field, "any element" for arrays).

```json
{ "query": "gary", "search_fields": ["cast/name"] }
{ "query": "robinson", "search_fields": ["cast/name", "cast/surname"] }
```

- Absent → default = all `searchable = true`, non-array fields (+ automatic same-element array bonus).
- Present → only the listed fields; the automatic array bonus is disabled.

Array subfields (`cast/name`, `random_nums/num1`) match "any element" (not necessarily the same person).

---

## 4. sort

Sort fields go directly under `sort` (flattened):

```json
{ "sort": { "mode": "blend", "year": { "order": "asc", "ratio": 40 } } }
```

or inside an optional `fields` wrapper — use this when a document field is literally named `mode`:

```json
{ "sort": { "mode": "strict", "fields": { "mode": { "order": "asc" }, "year": { "order": "asc" } } } }
```

Per field:

- `mode`: `"blend"` (default) | `"strict"`.
- `order`: `"asc"` | `"desc"` (default `desc`).
- `ratio`: 0–100. **Blend only** — ratios must sum to ≤ 100 (the rest is relevance). A single blend field may omit `ratio` (defaults to 40).
- `modifier`: `"none"` (default) | `"log1p"` | `"sqrt"` — compresses the value before sorting (for skewed power-law fields like views or pagerank).

```json
{ "sort": { "views": { "order": "desc", "modifier": "log1p" } } }
```

- **Strict** ignores `ratio`; fields are applied in declaration order, then relevance, then `doc_id` as tiebreakers.
- **Blend** normalizes each field to `[0,1]` using the corpus min/max, weights by `ratio`, and fills the rest with relevance.

A `sort` alone (blank query, no filters) is valid — it returns the top `docs` ordered by the sort key. Supported field kinds: numeric (`Integer`/`Float`), `date`, and `Bool`.

Bool sort: `true` = 1.0, `false` = 0.0, missing sorts last (strict mode; in blend mode this currently holds only for `desc`).

---

## 5. return_fields

```json
{ "return_fields": { "title": true, "year": true, "cast": true } }
```

`false` hides a field; `true` includes it. Omit to return the whole document.

---

# Highlighting

Highlighting wraps the terms a document matched (including **synonyms, fuzzy, and phrase matches**) in HTML tags, and returns relevance-ranked fragments per field.

## Request syntax

```json
{
  "query": "spiderman tom holand",
  "highlight": {
    "fields": {
      "text": { "number_of_fragments": 3, "sentence": true }
    }
  },
  "docs": 3
}
```

`highlight.fields` is a map. Keys are one of:

| Key | Meaning |
|---|---|
| `"text"`, `"title"`, … | an explicit field (by its full path) |
| `"*"` | defaults applied to **every** searched text field |
| `"$auto"` | auto-select the remaining fields and order them by relevance |

## Field options

| Option | Type | Default | Description |
|---|---|---|---|
| `fragment_size` | integer | `150` | Max bytes per fragment (word-aligned). |
| `number_of_fragments` | integer | `3` | Max fragments **per field** (`0` = none). |
| `pre_tags` | string[] | `["<em>"]` | Opening tag(s) around a match. |
| `post_tags` | string[] | `["</em>"]` | Closing tag(s) around a match. |
| `sentence` | boolean | `false` | `true` = sentence-aligned fragments; `false` = fixed-size windows. |

All options are optional (defaults apply when omitted).

## Field selection

### Explicit fields

```json
"fields": { "title": { "number_of_fragments": 1 }, "text": {} }
```

Highlights only the named fields, in the order given.

### `"*"` wildcard

```json
"fields": { "*": { "number_of_fragments": 2, "sentence": true } }
```

Highlights **every searched text field**, with these options.

### Per-field override over `"*"`

```json
"fields": {
  "*":    { "number_of_fragments": 2, "sentence": true },
  "lead": { "number_of_fragments": 5 }
}
```

`lead` uses its own options; every other searched text field uses `"*"`. Explicit fields are always emitted first, in request order.

### `"$auto"` — relevance-ordered auto-selection

```json
"fields": { "$auto": { "number_of_fragments": 3, "sentence": true } }
```

You don't list fields; the system highlights **every searched text field that matched**, ordering the fields **per document by that document's best match weight** (rarest/strongest match first).

`"$auto"` composes with explicit fields:

```json
"fields": {
  "title": { "number_of_fragments": 1 },
  "$auto": { "number_of_fragments": 3 }
}
```

`title` is always highlighted first, then the remaining fields are auto-selected in relevance order.

### Precedence

1. An explicit field name wins over `"*"`.
2. If both `"*"` and `"$auto"` are present, `"$auto"` wins for the unlisted fields.
3. Explicit fields are never duplicated by `"*"`/`"$auto"`.

## Output shape

Highlights are returned as a **per-field object** (not a flat array):

```json
"highlights": {
  "text":  ["<em>Holand</em> is a village …"],
  "title": ["<em>Holand</em>, Vega"]
}
```

Keys are the field's full path. Within each field, fragments are ordered **best-first** by relevance.

## Relevance ordering

- **Within a field:** each fragment is scored by the **summed idf** of the matched terms it contains, times a density bonus for tightly-packed matches. Fragments are returned best-first.
- **Across fields (`$auto`):** fields are ordered by their **best match weight** (the highest-idf term that matched in that field), per document.

So a rare-term match ranks above a common-term match, and the "top weight" matches surface first.

## `sentence` mode vs fragment mode

- **`sentence: false`** (default): the text is cut into fixed, **non-overlapping** `fragment_size` windows, snapped to word boundaries. Nearby matches collapse into one window instead of producing overlapping fragments.
- **`sentence: true`**: fragments align to **sentence boundaries**. A sentence shorter than `fragment_size` is returned whole; a longer one is cut to `fragment_size` at a word boundary, anchored at the start of the sentence.

## Validation & errors

A highlight field must be a **searched, text** field, otherwise the request fails:

| Case | Error |
|---|---|
| Unknown field | `PathNotIndexed` |
| Bool / numeric / date / struct field | `cannot highlight 'X': only text fields support highlighting` |
| Text field not searched by this query | `cannot highlight 'X': it is not a searched field` |

`"*"` and `"$auto"` only ever expand over searched text fields, so they can't select a non-highlight-able field.

## Examples

Highlight one field, sentence-aligned:

```json
"highlight": { "fields": { "text": { "number_of_fragments": 2, "sentence": true } } }
```

Highlight everything, fixed windows, custom tags:

```json
"highlight": { "fields": { "*": { "number_of_fragments": 2, "pre_tags": ["<b>"], "post_tags": ["</b>"] } } }
```

Auto-select and order by relevance:

```json
"highlight": { "fields": { "$auto": { "sentence": true } } }
```

## Notes & limitations

- Sentence segmentation uses the Unicode **UAX #29** algorithm (language-neutral). It has no abbreviation dictionary, so `"Dr. Ernest"` may still split after `"Dr."`.
- Decimals (`3.14`), ellipses (`...`), and acronyms (`U.S.`, `U.S.A.`) do **not** split.
- `number_of_fragments: 0` disables highlighting for that field.
