# Policy (document schema + analyzers)

The **policy** is the document schema: which fields exist, their type, how each
is indexed, and which analyzer analyzes it. It lives in each database's
`policy.json` (loaded by `IndexPolicy`). Edit via
`POST /api/databases/{db}/set-policy` (TOML body).

---

## 1. The two top-level blocks

```json
{
  "analyzers": { ... },   // optional — named analyzers
  "document":  { ... }    // required — the field schema
}
```

- `analyzers` may be omitted (every field then uses the built-in default analyzer).
- `document` **must** be present — it maps field name → field definition.

---

## 2. Field types (`$type`)

| `$type` | kind | searchable default | exact default | notes |
| --- | --- | --- | --- | --- |
| `text` | full-text | `true` | `false` | takes an analyzer |
| `id` | explicit string id | `true` | `true` | whole value is exact-matchable |
| `id_auto` | auto-generated id | `false` | `true` | engine assigns it |
| `int` / `integer` | integer | `false` | `true` | sort/filter, not full-text |
| `float` | float | `false` | `true` | sort/filter |
| `date` | date | `false` | `true` | sort/filter |
| `bool` | boolean | `false` | `true` | filter |
| `struct` | nested object | `false` | `false` | sub-fields below it |
| `none` | not indexed | `false` | `false` | stored but not searchable |

> `id` / `id_auto` / `int` / `float` / `date` / `bool` are **exact by default**,
> so `$exact: true` is only meaningful on `text`.

---

## 3. Defining a field — the three forms

A field may be written as a **string**, an **object**, or a **one-element array**
(a repeated field).

```json
{
  "document": {
    "title": "text",                                       // shorthand string
    "body":  { "$type": "text", "$analyzer": "title" },    // object
    "tags":  [ "text" ],                                   // repeated strings
    "nums":  [ { "$type": "integer" } ]                    // repeated integers
  }
}
```

A **struct** is an object whose non-`$` keys are its sub-fields (no `$type`
needed, but `"$type": "struct"` is also accepted):

```json
{
  "document": {
    "address": {
      "street": "text",
      "zip":    "integer"
    }
  }
}
```

---

## 4. Field options (`$…`)

| key | type | default | meaning |
| --- | --- | --- | --- |
| `$type` | string | (required for leaf) | field type, §2 |
| `$searchable` | bool | per-type (§2) | include in full-text search |
| `$analyzer` | string | *(field default)* | named analyzer (text fields) |
| `$list` | bool | `true` | list this field in search/lookup results by default |
| `$exact` | bool | per-type (§2) | also index the whole value as one term |
| `$vector` | bool | `false` | vector field for semantic search |
| `$weight` | object | per-type | scoring interval, `{ "min": 1, "max": 75 }` |

- `$weight` is a scoring interval `{ "min": N, "max": M }`. Text fields default
  to `{ "min": 1, "max": 75 }`; other fields to `{ "min": 0, "max": 100 }`.
- `$exact` on a `text` field adds a keyword index of the whole lowercased value
  (case-insensitive exact match), independent of the field's analyzer.
- `$analyzer` only applies to `text`; it names an entry from the `analyzers`
  block (resolution order: `$analyzer` → `analyzers.$default` → built-in default).

> There is **no `$repeated` key** — a repeated field is expressed with the array
> form in §3, never by a flag.

---

## 5. Every field type, with a full example

```json
{
  "document": {
    "title":     "text",
    "body":      { "$type": "text", "$analyzer": "title" },
    "url":       { "$type": "text", "$analyzer": "url", "$exact": true },

    "year":      "integer",
    "count":     "int",
    "rating":    "float",
    "visible":   "bool",
    "published": "date",

    "slug":      "id",
    "doc_id":    "id_auto",

    "tags":      [ "text" ],
    "years":     [ "integer" ],
    "items":     [ { "name": "text", "qty": "integer", "price": "float" } ],

    "address":   { "street": "text", "zip": "integer" },
    "orders":    [ { "sku": "text", "ship_to": { "city": "text", "zip": "integer" } } ],

    "debug":     "none"
  }
}
```

---

## 6. Every variation

```json
// A. Minimal — one shorthand field.
{ "document": { "title": "text" } }

// B. Field + analyzer + exact.
{ "analyzers": { "title": { "stemming": null } },
  "document": { "title": { "$type": "text", "$analyzer": "title", "$exact": true } } }

// C. Repeated field (array shorthand).
{ "document": { "tags": [ "text" ] } }

// D. Repeated struct (array of one object).
{ "document": { "items": [ { "sku": "text", "qty": "integer" } ] } }

// E. Struct (nested object) — no $type needed.
{ "document": { "address": { "street": "text", "zip": "integer",
    "geo": { "lat": "float", "lon": "float" } } } }

// F. Non-text fields (all exact by default, not full-text).
{ "document": { "year": "integer", "score": "float", "published": "date",
    "visible": "bool", "id": "id", "doc_id": "id_auto" } }

// G. Hide a field from search.
{ "document": { "secret_note": { "$type": "text", "$searchable": false } } }

// H. Vector field (semantic search).
{ "document": { "embedding": { "$type": "text", "$vector": true } } }

// I. Hide a field from results by default.
{ "document": { "internal_id": { "$type": "integer", "$list": false } } }

// J. Not indexed at all.
{ "document": { "debug": "none" } }
```

---

# Analyzers

Text analysis is configured **per field**. You define named **analyzers** in one
top-level block, then point each field at one (or fall back to a default).

## 7. Defining an analyzer

An analyzer is one object with these optional keys (every key has a default):

```json
{
  "analyzers": {
    "name": {
      "tokenizer":         "word_delimiter",
      "stemming":          "english",
      "lowercase":         true,
      "ascii_folding":     false,
      "stopwords":         ["a", "the", "and"],
      "min_length":        1,
      "max_length":        40,
      "preserve_original": true,
      "literal_symbols":   ["_"],
      "char_filters":      [ { "html_strip": {} } ]
    }
  }
}
```

### Defaults (when a key is omitted)

| key | default | meaning |
|---|---|---|
| `tokenizer` | `word_delimiter` | |
| `stemming` | *(none)* | no stemming |
| `lowercase` | `true` | |
| `ascii_folding` | `false` | |
| `stopwords` | *(none)* | no stopword removal |
| `min_length` | `1` | tokens < 1 chars dropped |
| `max_length` | `40` | tokens > 40 chars dropped |
| `preserve_original` | `true` | |
| `literal_symbols` | `["_"]` | underscore stays part of the word |
| `char_filters` | `[]` | |

### `stemming` languages

`arabic, danish, dutch, english, finnish, french, german, greek, hungarian,
italian, norwegian, portuguese, romanian, russian, spanish, swedish, tamil,
turkish`

Omit `stemming` **or** set `stemming: null` for **no stemming** — e.g. a
`title` analyzer that should not collapse "running" → "run".

### `stopwords`

```json
{ "analyzers": { "en": { "stopwords": ["a", "the", "of"] } } }
```

- `["a", "the", "of"]` → an explicit list.
- omit → no stopword removal.

---

## 8. Tokenizers

| name | behavior | `"Spider-Man"` → |
|---|---|---|
| `word_delimiter` | split on non-alphanumerics, keep original + parts + joined | `spider-man`, `spider`, `man`, `spiderman` |
| `keyword` | the whole value is **one** token | `spider-man` |
| `whitespace` | split only on spaces | `Spider-Man` |
| `standard` | split on non-alphanumerics, no rejoin | `spider`, `man` |
| `url` | keep whole URL + host + domain + parts | whole, `Example.com`, `https`, `www`, `Example`, `com`, `path` |
| `code` | word-delimiter + camelCase / snake_case / digit-letter splitting | `getHTTPResponse` → `getHTTPResponse`, `get`, `HTTP`, `Response` |
| `filepath` | split on `/`, `\`, `.` (Unix + Windows) | `src/core/shard_db.rs` → `src`, `core`, `shard_db`, `rs` |

> `code` is not a separate tokenizer — it is `word_delimiter` with the two
> code-splitting switches (`camelCase` and `foo123`) turned on.

Full example — one analyzer per tokenizer:

```json
{
  "analyzers": {
    "word_delimiter": { "tokenizer": "word_delimiter" },
    "keyword":        { "tokenizer": "keyword", "lowercase": true },
    "whitespace":     { "tokenizer": "whitespace" },
    "standard":       { "tokenizer": "standard" },
    "url":            { "tokenizer": "url", "ascii_folding": true },
    "code":           { "tokenizer": "code", "lowercase": false },
    "filepath":       { "tokenizer": "filepath" }
  }
}
```

---

## 9. Applying an analyzer to a field

In `document`, add `"$analyzer": "name"` to a text field:

```json
{
  "analyzers": { "title": { "stemming": null }, "url": { "tokenizer": "url" } },
  "document": {
    "id":    { "$type": "id_auto" },
    "title": { "$type": "text", "$analyzer": "title" },
    "body":  { "$type": "text" },
    "url":   { "$type": "text", "$analyzer": "url" }
  }
}
```

### Resolution order

For a text field, the analyzer used is:

1. `"$analyzer": "name"` — the named analyzer, if present.
2. otherwise `analyzers.$default`, if defined.
3. otherwise the **built-in default** (English full-text).

---

## 10. `$default` and the built-in default

`$default` is a normal analyzer in the `analyzers` block, used as the fallback
for fields that don't specify `$analyzer`:

```json
{
  "analyzers": {
    "$default": { "stemming": "english" },
    "title":    { "stemming": null },
    "url":      { "tokenizer": "url", "ascii_folding": true }
  }
}
```

If `$default` is absent, the engine falls back to a **built-in default**,
equivalent to:

```json
{
  "tokenizer": "word_delimiter",
  "lowercase": true,
  "min_length": 1,
  "max_length": 40,
  "preserve_original": true,
  "stemming": "english"
}
```

> ⚠️ Two different "no stemming" defaults, don't confuse them:
> - a **named** analyzer that omits `stemming` → **no stemming**,
> - the **built-in** default (no `$default`) → **English stemming**.
> If you want a fallback without stemming, define `"$default": {}` explicitly.

---

## 11. `$exact`

`"$exact": true` on a text field additionally indexes the **whole value,
lowercased, as one keyword term** (plus whole-value name terms for name-like
fields). This is what makes exact / case-insensitive whole-value lookups work,
independent of the field's main analyzer:

```json
{ "document": { "title": { "$type": "text", "$analyzer": "title", "$exact": true } } }
```

- `title` is analyzed with `title` (for word search),
- **and** its whole lowercased value is indexed as one term (for exact match).

---

## 12. Char filters (pre-tokenization)

`char_filters` run on the raw string **before** tokenizing. Order matters; they
run in the order listed.

```json
{
  "analyzers": {
    "html":      { "char_filters": [ { "html_strip": {} } ], "stemming": "english" },
    "markdown":  { "char_filters": [ { "markdown_strip": {} } ] },
    "clean_url": { "char_filters": [ { "pattern_replace": { "pattern": "\\?utm_.*", "replacement": "" } } ] }
  }
}
```

- `{ "html_strip": {} }` — remove HTML tags (`<b>x</b>` → `x`).
- `{ "markdown_strip": {} }` — strip markdown syntax and links.
- `{ "pattern_replace": { "pattern": "..", "replacement": ".." } }` — regex replace.

---

## 13. Changing an analyzer

Changing an analyzer (or the schema) affects how documents are indexed. When
you `set-policy` with a changed analyzer or field schema, the API returns a
warning:

> policy changed; reindex required to apply to existing documents

New and updated documents use the new policy immediately; existing documents
keep their old terms until you run a **reindex**.

---

## 14. Full `policy.json` example

```json
{
  "analyzers": {
    "$default":  { "stemming": "english" },
    "title":     { "stemming": null },
    "name":      { "ascii_folding": true },
    "url":       { "tokenizer": "url", "ascii_folding": true },
    "code":      { "tokenizer": "code", "lowercase": false },
    "filepath":  { "tokenizer": "filepath" },
    "html":      { "char_filters": [ { "html_strip": {} } ], "stemming": "english" },
    "exact_tag": { "tokenizer": "keyword", "lowercase": true }
  },
  "document": {
    "id":          { "$type": "id_auto" },

    "title":       { "$type": "text", "$analyzer": "title", "$exact": true },
    "author":      { "$type": "text", "$analyzer": "name",  "$exact": true },
    "body":        { "$type": "text" },
    "url":         { "$type": "text", "$analyzer": "url",   "$exact": true },
    "source_code": { "$type": "text", "$analyzer": "code" },
    "repo_path":   { "$type": "text", "$analyzer": "filepath" },
    "readme":      { "$type": "text", "$analyzer": "html" },
    "tag":         { "$type": "text", "$analyzer": "exact_tag" },

    "year":        "integer",
    "rating":      "float",
    "published":   "date",
    "is_public":   "bool",

    "tags":        [ "text" ],
    "items":       [ { "sku": "text", "qty": "integer", "price": "float" } ],

    "address": {
      "street":  "text",
      "city":    "text",
      "zip":     "integer",
      "geo":     { "lat": "float", "lon": "float" }
    },

    "embedding":   { "$type": "text", "$vector": true },
    "internal_id": { "$type": "integer", "$list": false },
    "debug":       "none"
  }
}
```
