# Policy (document schema)

The **policy** is the document schema: which fields exist, their type, how each
is indexed, and which analyzer analyzes it. It lives in `policy.json` (loaded by
`IndexPolicy`).

---

## 1. The two top-level blocks

```json
{
  "analyzers": { ... },   // optional — named analyzers (see ANALYZERS doc)
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
> so `$exact: true` is only meaningful on `text` (see §4).

---

## 3. Defining a field — the three forms

A field may be written as a **string**, an **object**, or a **one-element array**
(a repeated field).

```jsonc
// 1. Shorthand string — type only, everything else defaulted.
"title": "text",

// 2. Object — type + options.
"title": { "$type": "text", "$analyzer": "title", "$exact": true },

// 3. Array — a repeated field (array of values). The array form is what makes
//    it repeated; there is no $repeated key. Exactly one element.
"tags": [ "text" ],                        // repeated strings
"nums": [ { "$type": "integer" } ]         // repeated integers
```

A **struct** is an object whose non-`$` keys are its sub-fields (no `$type`
needed, but `"$type": "struct"` is also accepted):

```jsonc
"address": {                     // struct
  "street": "text",
  "zip":    "integer"
}
```

---

## 4. Field options (`$…`)

On an object field you may set:

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
  (case-insensitive exact match), independent of the field's analyzer. `id`,
  `id_auto`, `int`, `float`, `date`, `bool` are already exact.
- `$analyzer` only applies to `text`; it names an entry from the `analyzers`
  block (resolution order: `$analyzer` → `analyzers.$default` → built-in default).

> There is **no `$repeated` key** — a repeated field is expressed with the array
> form in §3, never by a flag.

---

## 5. Every field type, with an example

```jsonc
"document": {

  // — text —
  "title":   "text",                                          // full-text, default analyzer
  "body":    { "$type": "text", "$analyzer": "title" },       // named analyzer
  "url":     { "$type": "text", "$analyzer": "url", "$exact": true },

  // — integer —
  "year":    "integer",
  "count":   "int",                                            // "int" == "integer"

  // — float —
  "rating":  "float",

  // — boolean —
  "visible": "bool",

  // — date —
  "published": "date",

  // — id / id_auto —
  "slug":    "id",        // explicit id, searchable + exact
  "doc_id":  "id_auto",   // engine-assigned, not searchable

  // — repeated strings (array of text) —
  "tags":    [ "text" ],

  // — repeated integers —
  "years":   [ "integer" ],

  // — repeated structs (array of objects) —
  "items":   [ { "name": "text", "qty": "integer", "price": "float" } ],

  // — struct (single nested object) —
  "address": { "street": "text", "zip": "integer" },

  // — nested struct inside a repeated struct —
  "orders":  [ { "sku": "text", "ship_to": { "city": "text", "zip": "integer" } } ],

  // — not indexed —
  "debug":   "none"
}
```

---

## 6. Every variation

```jsonc
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
{ "document": {
    "address": { "street": "text", "zip": "integer", "geo": { "lat": "float", "lon": "float" } }
} }

// F. Non-text fields (all exact by default, not full-text).
{ "document": {
    "year": "integer", "score": "float", "published": "date", "visible": "bool",
    "id": "id", "doc_id": "id_auto"
} }

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

## 7. Full `policy.json` example

```json
{
  "analyzers": {
    "$default":  { "stemming": "english" },
    "title":     { "stemming": null },
    "name":      { "ascii_folding": true },
    "url":       { "tokenizer": "url", "ascii_folding": true },
    "code":      { "tokenizer": "code", "lowercase": false },
    "filepath":  { "tokenizer": "filepath" }
  },
  "document": {
    "id":        { "$type": "id_auto" },

    "title":     { "$type": "text", "$analyzer": "title", "$exact": true },
    "author":    { "$type": "text", "$analyzer": "name",  "$exact": true },
    "body":      { "$type": "text" },

    "tags":      [ "text" ],

    "url":         { "$type": "text", "$analyzer": "url",  "$exact": true },
    "source_code": { "$type": "text", "$analyzer": "code" },
    "repo_path":   { "$type": "text", "$analyzer": "filepath" },

    "year":      "integer",
    "rating":    "float",
    "published": "date",
    "is_public": "bool",

    "embedding": { "$type": "text", "$vector": true },

    "items":     [ { "sku": "text", "qty": "integer", "price": "float" } ],

    "address": {
      "street":  "text",
      "city":    "text",
      "zip":     "integer",
      "geo":     { "lat": "float", "lon": "float" }
    }
  }
}
```
