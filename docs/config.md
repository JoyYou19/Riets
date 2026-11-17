# Database config — `config.toml`

Each database has a `config.toml` in its directory (`<data-root>/databases/<db>/config.toml`). It is read by `DatabaseOptions` and editable via `POST /api/databases/{db}/set-config` (TOML body).

## Full default example

```toml
enable_background_compaction = true
dead_file_treshold = 0.5
bootable = true
sync_mode = "Manual"
skip_unreplayable_wal_records = false

[runtime]
flush_threshold = 134217728      # 128 MiB
indexing_batch_size = 100000

[runtime.compaction]
max_segments_per_compaction = 16
compact_when_segments_at_least = 2
size_ratio = 2
min_width = 4
max_segments = 10

[runtime.compaction.idle_merge_after]
secs = 10
nanos = 0

[compaction_interval]
secs = 30
nanos = 0

[incremental_backup_interval]
secs = 3600
nanos = 0

[full_backup_interval]
secs = 86400
nanos = 0

[backup_lifetime]
secs = 604800
nanos = 0

[embed]
max_docs_per_round = 10000
embed_batch = 1024
wait_for_load = true

[embed.round_time_budget]
secs = 120
nanos = 0

[search.cascade]
name = 1
exact = 2
phrase = 3
fuzzy = 4
all = 5
all_but_one = 6
any = 7
```

## Keys

| Key | Type | Default | Meaning |
|---|---|---|---|
| `enable_background_compaction` | bool | `true` | Run segment compaction in the background. |
| `dead_file_treshold` | float | `0.5` | Fraction of a segment that may be dead before it is compacted. |
| `bootable` | bool | `true` | Whether the database is allowed to boot. |
| `sync_mode` | string | `"Manual"` | WAL durability. `"SyncEach"` = fsync every write; `"Manual"` = no forced fsync. |
| `skip_unreplayable_wal_records` | bool | `false` | Skip (instead of fail on) WAL records that can't be replayed. |

### `[runtime]` — index runtime

| Key | Type | Default | Meaning |
|---|---|---|---|
| `flush_threshold` | int | `134217728` (128 MiB) | In-memory buffer size before a flush. |
| `indexing_batch_size` | int | `100000` | Documents per segment. |

### `[runtime.compaction]` — compaction policy

| Key | Type | Default | Meaning |
|---|---|---|---|
| `max_segments_per_compaction` | int | `16` | Cap on segments merged in one compaction. |
| `compact_when_segments_at_least` | int | `2` | Never merge below this many segments. |
| `size_ratio` | int | `2` | A tiered run only contains segments whose sizes are within this factor. |
| `min_width` | int | `4` | Fewest similar-sized adjacent segments worth merging. |
| `max_segments` | int | `10` | Above this many segments, merge the cheapest adjacent run even if sizes differ. |
| `idle_merge_after` | duration | `10s` | After this long idle, merge similar-sized neighbours even when fewer than `min_width`. |

### `[embed]` — embedding work settings

| Key | Type | Default | Meaning |
|---|---|---|---|
| `max_docs_per_round` | int | `10000` | Upper bound on documents embedded in one round. |
| `round_time_budget` | duration | `120s` | A round starts no new batch after this long. |
| `embed_batch` | int | `1024` | Documents loaded and handed to the model per call. |
| `wait_for_load` | bool | `true` | Wait for a bulk load to finish before embedding. Off: embed while loading. |

### `[search.cascade]` — ranking cascade

Maps a rank (`1` = runs first) to each tier. A tier left out is **off**. Empty (or absent) = the default order below.

Tier keys (snake_case): `name`, `exact`, `phrase`, `fuzzy`, `all`, `all_but_one`, `any`.

Default order:

```
name → exact → phrase → fuzzy → all → all_but_one → any
```

- `name` and `exact` are **hard tiers** — they fill the top of the page first, in order.
- `phrase`, `fuzzy`, `all`, `all_but_one`, `any` are **relevance sources** — they all feed one relevance pool, positioned where the **first** of them appears in your order.

#### Example — exact first, then just fuzzy + any

```toml
[search.cascade]
exact = 1
fuzzy = 2
any = 3
```

This disables `name`, `phrase`, `all`, `all_but_one`; runs `exact` (hard) first, then the relevance pool built from `fuzzy` + `any`.

> Durations serialize as a TOML table `{ secs = N, nanos = 0 }` (see the example). When writing by hand, `secs` is seconds and `nanos` is an offset.
