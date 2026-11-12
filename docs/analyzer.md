# Analyzers in the policy

Text analysis is configured **per field**. You define named **analyzers** in one
top-level block, then point each field at one (or fall back to a default).

---

## 1. The two top-level blocks

```json
{
  "analyzers": { ... },   // named analysis recipes (optional)
  "document":  { ... }    // the document schema (fields)
}
```

`analyzers` may be omitted entirely; then every field uses the built-in default.

---

## 2. Defining an analyzer

An analyzer is one object with these optional keys (every key has a default):

```json
"name": {
  "tokenizer":         "word_delimiter",   // see §3
  "stemming":          "english",          // language, or omit/null = no stemming
  "lowercase":         true,               // default true
  "ascii_folding":     false,              // default false (Jānis -> Janis)
  "stopwords":         ["a", "the", "and"],// array of words, or omit = none
  "min_length":        1,                  // drop tokens shorter than this
  "max_length":        40,                 // drop tokens longer than this
  "preserve_original": true,               // keep the raw token too
  "literal_symbols":    ["_"],             // chars kept inside words
  "char_filters":       [ {"html_strip": {}} ]
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

- `["a", "the", "of"]` → an explicit list.
- omit → no stopword removal.

---

## 3. Tokenizers

| name | behavior | example |
|---|---|---|
| `word_delimiter` | split on non-alphanumerics, keep original + parts + joined form | `"Spider-Man"` → `spider-man`, `spider`, `man`, `spiderman` |
| `keyword` | the whole value is **one** token | `Spider-Man` → `spider-man` |
| `whitespace` | split only on spaces | `Spider-Man` (unchanged) |
| `standard` | split on non-alphanumerics, no rejoin | `spider`, `man` |
| `url` | keep whole URL + host + domain + parts | `https://www.Example.com/path` → whole, `Example.com`, `https`, `www`, `Example`, `com`, `path` |
| `code` | word-delimiter splitting **plus** camelCase / snake_case / digit-letter splitting | `getHTTPResponse` → `getHTTPResponse`, `get`, `HTTP`, `Response` |
| `filepath` | split on `/`, `\`, `.` (Unix + Windows) | `src/core/shard_db.rs` → `src`, `core`, `shard_db`, `rs` |

> `code` is not a separate tokenizer — it is `word_delimiter` with the two
> code-splitting switches (`camelCase` and `foo123`) turned on. So `code`
> already does "word delimiters + code".

(`keyword` is what powers the `$exact` copy — see §6.)

---

## 4. Applying an analyzer to a field

In `document`, add `"$analyzer": "name"` to a text field:

```json
"document": {
  "id":    { "$type": "id_auto" },
  "title": { "$type": "text", "$analyzer": "title" },
  "body":  { "$type": "text" },                  // no $analyzer -> $default
  "url":   { "$type": "text", "$analyzer": "url" },
  "code":  { "$type": "text", "$analyzer": "code" }
}
```

### Resolution order

For a text field, the analyzer used is:

1. `"$analyzer": "name"` — the named analyzer, if present.
2. otherwise `analyzers.$default`, if defined.
3. otherwise the **built-in default** (English full-text).

So you don't have to write an `analyzers` block at all — omit it and every
field uses English full-text.

---

## 5. `$default` and the built-in default

`$default` is a normal analyzer in the `analyzers` block, used as the fallback
for fields that don't specify `$analyzer`:

```json
"analyzers": {
  "$default": { "stemming": "english" },
  "title":    { "stemming": null },          // no stemming
  "url":      { "tokenizer": "url", "ascii_folding": true }
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

(no stopwords, no ascii folding).

> ⚠️ Two different "no stemming" defaults, don't confuse them:
> - a **named** analyzer that omits `stemming` → **no stemming**,
> - the **built-in** default (no `$default`) → **English stemming**.
> If you want a fallback without stemming, define `"$default": {}` explicitly.

---

## 5b. Defining an analyzer — every variant

All the ways to specify analysis, from least to most config:

```jsonc
// A. Nothing at all — omit `analyzers`.
//    Every text field uses the built-in default (English full-text).
{ "document": { "title": { "$type": "text" } } }

// B. Only a fallback.
{ "analyzers": { "$default": { "stemming": "english" } }, "document": { ... } }

// C. Fallback + named analyzers (the common case).
{ "analyzers": { "$default": { }, "url": { "tokenizer": "url" } }, "document": { ... } }

// D. Named only, no $default.
//    Fields with $analyzer use their named analyzer; the rest use the built-in default.
{ "analyzers": { "url": { "tokenizer": "url" } }, "document": { ... } }

// E. Minimal named analyzer — every key defaulted.
//    "title": {}  ==  word_delimiter, lowercase, no stemming, no stopwords.
{ "analyzers": { "title": {} }, "document": { "title": { "$type": "text", "$analyzer": "title" } } }

// F. One-key analyzer.
{ "analyzers": { "url": { "tokenizer": "url" } }, "document": { ... } }

// G. Explicitly no stemming (equivalent to omitting it).
{ "analyzers": { "title": { "stemming": null } }, "document": { ... } }

// H. A field without $analyzer vs with — mixing in one document.
{ "document": {
    "body":  { "$type": "text" },                       // -> $default / built-in
    "title": { "$type": "text", "$analyzer": "title" }  // -> named
} }
```

---

## 6. `$exact`

`"$exact": true` on a text field additionally indexes the **whole value,
lowercased, as one keyword term** (plus whole-value name terms for name-like
fields). This is what makes exact / case-insensitive whole-value lookups work,
independent of the field's main analyzer:

```json
"title": { "$type": "text", "$analyzer": "title", "$exact": true }
```

- `title` is analyzed with `title` (for word search),
- **and** its whole lowercased value is indexed as one term (for exact match).

---

## 7. Char filters (pre-tokenization)

`char_filters` run on the raw string **before** tokenizing:

- `{ "html_strip": {} }` — remove HTML tags (`<b>x</b>` → `x`).
- `{ "markdown_strip": {} }` — strip markdown syntax and links.
- `{ "pattern_replace": { "pattern": "\\?utm_.*", "replacement": "" } }` — regex replace.

Order matters; they run in the order listed.

---

## 8. Changing an analyzer

Changing an analyzer (or the schema) affects how documents are indexed. When
you `set-policy` with a changed analyzer or field schema, the API returns a
warning:

> policy changed; reindex required to apply to existing documents

New and updated documents use the new policy immediately; existing documents
keep their old terms until you run a **reindex**.

---

## 9. Complete `policy.json` example

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
    "tag":         { "$type": "text", "$analyzer": "exact_tag" },
    "url":         { "$type": "text", "$analyzer": "url",  "$exact": true },
    "rating":      { "$type": "float" },
    "year":        { "$type": "integer" },
    "published":   { "$type": "date" },
    "is_public":   { "$type": "bool" },
    "source_code": { "$type": "text", "$analyzer": "code" },
    "repo_path":   { "$type": "text", "$analyzer": "filepath" },
    "readme":      { "$type": "text", "$analyzer": "html" }
  }
}
