# Dictionary (synonyms)

Two TOML files:

1. **`CorelamoDictionary.toml`** — the server-wide *default* dictionary, next to `CorelamoSettings.toml`. Short forms, formulas and Roman-numeral rules. Written from a built-in copy on first start; changes take effect when the server starts.
2. **`<data-root>/databases/<db>/dictionary.toml`** — each database's own synonyms and one-way entries, plus a flag for whether to also use the default.

---

## Per-database `dictionary.toml`

```toml
default = true

[synonyms]
# Forms that mean the same: any of them finds the others.
groups = [
  ["car", "automobile"],
]

[one_way]
# The first form also searches the others; the others do not search it.
groups = [
  ["king", "monarch"],
]
```

- `default` (default `true`): whether the database also searches with the **default** dictionary (`CorelamoDictionary.toml`).
- `[synonyms]`: **bidirectional** — any form in a group matches the others.
- `[one_way]`: **directional** — the first form expands to the rest, but not vice versa.

---

## Case and the `~` prefix

A form is looked up by its lowercased, stemmed analyzer form.

- A form **with a capital** (`CaP`, `US`, `WW2`) is **case-sensitive**: it only matches when typed exactly like that.
- A `~` prefix makes a form **case-insensitive**: it matches any case.

```toml
[synonyms]
groups = [
  ["car", "automobile"],        # lowercase: case-insensitive by nature
  ["US", "united states"],      # "US" only matches typed as "US"
  ["~NaCl", "salt"],            # ~NaCl matches any case
]
```

---

## Matching rules

- Synonyms are indexed **per field analyzer**: a form is matched on the analyzer's output (lowercased, stemmed, folded), so it follows the field's stemming.
- Case-sensitive forms search the case-preserving **exact** index, not the analyzed index.
- A group applies to every searched text field (and to `$exact` fields for case-sensitive forms).

---

## The default dictionary (`CorelamoDictionary.toml`)

The default has five sections (all optional):

```toml
[roman]
# Uppercase Roman numerals also search the number, and numbers also search
# the numeral:  II <-> 2,  VIII <-> 8,  MMXXVI <-> 2026.
enabled = true
min_value = 2        # 1 / I excluded: "I" is almost always the pronoun
max_value = 3999
min_len = 2          # single letters (V, X, C, …) excluded
arabic_to_roman = true   # also expand numbers into numerals (2 -> II)
denylist = ["MIX", "DIX", "LIV", "CIV", "DIV", "DC", "CD", "MD", "MC", "CC", "CL", "CV", "XL", "LI", "DI", "MI", "MM"]

[acronyms]
groups = [
  ["AI", "artificial intelligence"],
  ["CPU", "central processing unit"],
  ["SQL", "structured query language"],
]

[formulas]
groups = [
  ["~H2O", "water"],
  ["~NaCl", "salt"],
]

[synonyms]
groups = [
  ["car", "automobile"],
]

[one_way]
groups = [
  ["usa", "united states of america"],
]
```

- `[roman]` — Roman-numeral ↔ number expansion (`II` ↔ `2`). Only **capitals** trigger it, so lowercase `ii` stays a word. `denylist` lists numerals that are usually words/acronyms and are never expanded.
- `[acronyms]` — `AI` → `artificial intelligence` (case-sensitive short form).
- `[formulas]` — chemical/short-form codes, usually with `~` for case-insensitivity (`~H2O`).
- `[synonyms]` / `[one_way]` — same semantics as the per-database file.

The built-in default is large (tens of thousands of formulas/acronyms); a database opts in via `default = true`.

---

## HTTP

```bash
# read
curl -s -X GET "http://localhost:6006/api/databases/movies/dictionary"

# replace (TOML body)
curl -s -X POST "http://localhost:6006/api/databases/movies/set-dictionary" \
  -d 'default = true

[synonyms]
groups = [
  ["car", "automobile"],
]

[one_way]
groups = [
  ["king", "monarch"],
]'

# reload from disk
curl -s -X POST "http://localhost:6006/api/databases/movies/reload-dictionary"
```

- `GET /api/databases/{db}/dictionary` — read the database's `dictionary.toml`.
- `POST /api/databases/{db}/set-dictionary` — replace it (TOML body).
- `POST /api/databases/{db}/reload-dictionary` — reload from disk.
