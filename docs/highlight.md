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

`"$auto"` composes with explicit fields (option "a"):

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
