# Corelamo HTTP Commands

All routes live under `/api`. Everything except `/api/login` is a protected route: it goes through the auth middleware (unless the server runs with `auth=false`) and then a per-handler permission check.

## Conventions

- `{db_name}`: name of the target database, taken from the URL path.
- **"Requires running"**: the handler returns `DatabaseNotRunning` if any shard is stopped.
- **Unknown fields**: most request structs use `deny_unknown_fields`. A typo returns an error such as `Unknown field 'x'. Did you mean 'y'?`.
- **Empty bodies**: handlers that need a body reject an empty one with `request body is empty`.
- **Batch responses**: insert/replace/upsert/partial-replace/delete return HTTP 200 with a per-document breakdown of what succeeded and failed. Failure indexes are mapped back to the position in the client's input array.

---

## Authentication

### `POST /api/login` (public)

Exchanges credentials for a token.

```bash
curl -s -X POST "http://localhost:6006/api/login" \
  -d '{"username": "admin", "password": "secret"}'
```

Response: `{ "token": "..." }`. Bad credentials return `Unauthorized`.

### `POST /api/logout`

Invalidates the current session token.

```bash
curl -s -X POST "http://localhost:6006/api/logout"
```

---

## Document operations

### `POST /api/databases/{db_name}/insert`

Inserts new documents. Requires running. Permission: `Insert`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/insert" \
  -d '[{"id": "1", "title": "Dune", "year": 2021}]'
```

- Documents that fail to parse, or that conflict with an existing document, are reported as failures. Conflicts use status 409.
- Response title: `inserted N into '<db>', M failed`.

### `POST /api/databases/{db_name}/replace`

Replaces whole documents. Requires running. Permission: `Replace`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/replace" \
  -d '[{"id": "1", "title": "Dune: Part Two", "year": 2024}]'
```

- Same body format as insert. A document that does not exist is a failure with status 404.

### `POST /api/databases/{db_name}/upsert`

Inserts if the document is missing, replaces if it exists. Requires running. Permission: `Upsert`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/upsert" \
  -d '[{"id": "1", "title": "Dune", "year": 2021}]'
```

- Same body format as insert.

### `POST /api/databases/{db_name}/partial-replace`

Patches fields on existing documents instead of replacing them. Requires running. Permission: `Replace`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/partial-replace" \
  -d '[{"id": "doc1", "patch": {"year": 1999}}]'
```

- The body must be a non-empty array. Each item has exactly `id` and `patch`.
- Each document is fetched first, the patch is merged on a blocking thread, and the result is written with `replace`.
- Fetches are sequential, so keep batches small.

### `DELETE /api/databases/{db_name}/delete`

Deletes documents by id. Requires running. Permission: `Delete`.

```bash
curl -s -X DELETE "http://localhost:6006/api/databases/movies/delete" \
  -d '["doc1", "doc2", "doc3"]'
```

- Ids that don't exist are failures with status 404.

### `POST /api/databases/{db_name}/retrieve`

Fetches the full stored documents by id. Requires running. Permission: `Retrieve`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/retrieve" \
  -d '["doc1", "doc2"]'
```

Response:

```json
{
  "documents": [ { "id": "doc1", "data": { } } ],
  "not_found": ["doc2"],
  "skipped_ids": []
}
```

`skipped_ids` holds documents that could not be converted to the request format.

### `POST /api/databases/{db_name}/lookup`

Like retrieve, but goes through the index layer and supports field projection. Requires running. Permission: `Lookup`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/lookup" \
  -d '{"ids": ["doc1", "doc2"], "return_fields": {"title": true, "year": true}}'
```

- `ids` is required. `return_fields` is optional.
- Response: `{ "documents": [ { "id", "data" } ], "not_found": [] }`.

---

## Search helpers (not search itself)

### `POST /api/databases/{db_name}/did-you-mean`

Spelling suggestions per word. Requires running. Permission: `DidYouMean`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/did-you-mean" \
  -d '{"value": "teh matrx", "search_fields": ["title"], "did_you_mean_count": 3, "fuzziness": "auto"}'
```

Response:

```json
{
  "original": "teh matrx",
  "corrected": "the matrix",
  "suggestions": [ { "word": "teh", "suggestions": ["the"] } ]
}
```

| Field | Required | Notes |
|---|---|---|
| `value` | yes | The text to check |
| `search_fields` | no | Restrict suggestions to these fields |
| `did_you_mean_count` | no | Suggestions per word (default 3) |
| `fuzziness` | no | `"0"`, `"1"`, `"2"` or `"auto"` (a string) |

### `POST /api/databases/{db_name}/info-words`

Term statistics. Requires running. Permission: `Search`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/info-words" \
  -d '{"words": ["matrix", "neo"]}'
```

Response: `{ "words": [ { "word", "occurrences", "documents" } ] }`.

### `GET /api/databases/{db_name}/all-fields`

Lists every field path (xpath) seen in the stored documents and its kind. Permission: `AllFields`.

```bash
curl -s "http://localhost:6006/api/databases/movies/all-fields"
```

Response: `{ "fields": { "title": "text", "year": "int", "is_cool": "none" } }`. Fields declared in the policy report their kind; fields present in documents but not declared report `none`.

### `POST /api/databases/{db_name}/analyze`

Analyzes a document's text fields with the database's policy and analyzers — a testing aid. Permission: `AnalyzeDoc`.

```bash
# full: token objects
curl -s -X POST "http://localhost:6006/api/databases/movies/analyze" \
  -d '{"options": {"full": true}, "docs": {"title": "The Amazing Spider-Man 2", "year": 2014}}'

# compact: joined analyzed text
curl -s -X POST "http://localhost:6006/api/databases/movies/analyze" \
  -d '{"options": {"full": false}, "docs": {"extract": "Spider-Man is a movie"}}'
```

- `options.full` (default `true`): `true` returns each text field as `{ "value", "filtered", "tokens": [{ "text", "position", "position_length", "start", "end" }] }`; `false` returns each text field as just the joined analyzed token text.
- `docs` is a single object or an array of documents.
- Non-text fields pass through unchanged; a type mismatch (e.g. `"year": "eight"` for an `int` field) returns `400 invalid value for field 'year': expected int, got 'eight'`.

---

## Dictionary (synonyms)

### `GET /api/databases/{db_name}/dictionary`

Returns the database's `dictionary.toml` as `application/toml`. Permission: `GetConfig`.

```bash
curl -s "http://localhost:6006/api/databases/movies/dictionary"
```

### `POST /api/databases/{db_name}/set-dictionary`

Body is TOML. It is parsed first (a TOML error needs no database), then applied. Permission: `SetConfig`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/set-dictionary" \
  -d 'default = true

[synonyms]
groups = [ ["car", "automobile"] ]

[one_way]
groups = [ ["king", "monarch"] ]'
```

### `POST /api/databases/{db_name}/reload-dictionary`

Reloads the dictionary from disk. Permission: `SetConfig`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/reload-dictionary"
```

See `dictionary.md` for the file format.

---

## Database lifecycle

### `POST /api/databases/{db_name}/create-database`

Creates a database. Permission: `CreateDatabase`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/create-database" \
  -d '{"shard_count": 4}'
```

- The body is optional. The default is 4 shards and the minimum is 1 (`shard_count` is a u16).
- Returns `201 Created`.

### `DELETE /api/databases/{db_name}/delete-database`

Shuts the database down and deletes it from disk. Permission: `DeleteDatabase`.

```bash
curl -s -X DELETE "http://localhost:6006/api/databases/movies/delete-database"
```

- If other requests still hold a reference, it returns `Conflict` ("in use, try again").

### `DELETE /api/databases/{db_name}/clear-database`

Wipes all data and the index but keeps the database. Permission: `ClearDB`.

```bash
curl -s -X DELETE "http://localhost:6006/api/databases/movies/clear-database"
```

### `POST /api/databases/{db_name}/rename-database`

Permission: `RenameDatabase`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/rename-database" \
  -d '{"name": "films"}'
```

- The database must be stopped first (`stop-database`), otherwise `Conflict`.

### `POST /api/databases/{db_name}/start-database`

Starts all shards. Permission: `StartDB`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/start-database"
```

### `POST /api/databases/{db_name}/stop-database`

Stops all shards. The database stays registered, but data endpoints return `DatabaseNotRunning`. Permission: `StopDB`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/stop-database"
```

### `POST /api/databases/{db_name}/restart-database`

Stop then start. Permission: `RestartDB`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/restart-database"
```

### `GET /api/list-databases`

Permission: `ListDatabases`.

```bash
curl -s "http://localhost:6006/api/list-databases"
```

Response: `{ "databases": [ { "name": "movies", "running": true } ] }`.

### `GET /api/databases/{db_name}/status`

Live statistics from atomics only, so it never blocks behind an fsync or reindex commit. Requires running. Permission: `Status`.

```bash
curl -s "http://localhost:6006/api/databases/movies/status"
```

Response sections: `background_compaction_enabled`, `shard_count`, `metrics`, `indexed`, `reindexing`, `backup`.

### `GET /api/databases/{db_name}/disk-usage`

Computes disk usage on a blocking thread. Permission: `Status`.

```bash
curl -s "http://localhost:6006/api/databases/movies/disk-usage"
```

---

## Reindexing

### `POST /api/databases/{db_name}/reindex`

Starts a reindex. Requires running. Permission: `Reindex`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/reindex"
```

- Poll `/status` and read the `reindexing` section for progress.

### `POST /api/databases/{db_name}/reindex/abort`

Aborts a running reindex. Permission: `Reindex`. Idempotent.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/reindex/abort"
```

---

## Policy and configuration

### `GET /api/databases/{db_name}/get-policy`

Returns the database's schema/indexing policy as TOML (`application/toml`). Permission: `GetPolicy`.

```bash
curl -s "http://localhost:6006/api/databases/movies/get-policy"
```

### `POST /api/databases/{db_name}/set-policy`

Body is TOML. It is parsed first, then applied to all shards. Permission: `SetPolicy`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/set-policy" \
  -H "Content-Type: application/toml" \
  --data-binary 'analyzers = { title = { stemming = "english" } }

[document]
title = "text"
year = "integer"'
```

### `GET /api/databases/{db_name}/get-config`

Returns `DatabaseOptions` as pretty TOML. Permission: `GetConfig`.

```bash
curl -s "http://localhost:6006/api/databases/movies/get-config"
```

### `POST /api/databases/{db_name}/set-config`

Body is TOML `DatabaseOptions`. It is parsed before the database is looked up, then applied to all shards. Permission: `SetConfig`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/set-config" \
  --data-binary 'enable_background_compaction = true

[search.cascade]
name = 1
exact = 2
fuzzy = 3
any = 4'
```

See `config.md` for the config keys.

---

## Logs

### `GET /api/databases/{db_name}/get-logs`

Returns the database logs as `text/plain`. Permission: `GetLogs`.

```bash
curl -s "http://localhost:6006/api/databases/movies/get-logs"
curl -s -X POST "http://localhost:6006/api/databases/movies/get-logs" \
  -d '{"date": "2026-11-16"}'
```

### `DELETE /api/databases/{db_name}/clear-logs`

Deletes the database's logs. Permission: `ClearLogs`.

```bash
curl -s -X DELETE "http://localhost:6006/api/databases/movies/clear-logs"
```

---

## Backups

### `POST /api/databases/{db_name}/backup`

Starts a full backup in the background and returns immediately. Permission: `BackupFull`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/backup"
```

### `POST /api/databases/{db_name}/backup/incremental`

Starts an incremental backup in the background. Permission: `BackupIncremental`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/backup/incremental"
```

- Requires an existing full backup, otherwise `InvalidData`.

### `GET /api/databases/{db_name}/list-backups`

Permission: `ListBackups`.

```bash
curl -s "http://localhost:6006/api/databases/movies/list-backups"
```

### `POST /api/databases/{db_name}/restore-backup/{backup_id}`

Restores a backup. Permission: `Restore`. Synchronous — the request stays open until the restore finishes.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/restore-backup/full_2026-11-16_12-00-00"
```

### `DELETE /api/databases/{db_name}/delete-backup/{backup_id}`

Deletes a backup. Permission: `Delete`. Returns `NotFound` if the id doesn't exist.

```bash
curl -s -X DELETE "http://localhost:6006/api/databases/movies/delete-backup/full_2026-11-16_12-00-00"
```

---

## User management

Users are stored in the internal users database. All handlers check a permission first, then the auth service enforces its own rules (for example, who may change whom).

| Route | Body | Permission | Example |
|---|---|---|---|
| `POST /api/users` | `{ "username", "password", "roles": ["..."] }` | `CreateUser` | `curl -s -X POST "…/api/users" -d '{"username":"alice","password":"pw","roles":["admin"]}'` |
| `DELETE /api/users/{username}` | none | `DeleteUser` | `curl -s -X DELETE "…/api/users/alice"` |
| `GET /api/users/list-users` | none | `ListUsers` | `curl -s "…/api/users/list-users"` |
| `POST /api/users/{username}/password` | `{ "password" }` | `UpdatePwd` | `curl -s -X POST "…/api/users/alice/password" -d '{"password":"new"}'` |
| `POST /api/users/{username}/roles` | `{ "roles": ["..."] }` | `UpdateRole` | `curl -s -X POST "…/api/users/alice/roles" -d '{"roles":["admin","editor"]}'` |

`list-users` returns `{ "users": [ { "username", "roles" } ] }`.

---

## Diagnostics

### `POST /api/timings`

Returns the timing report from `core_timing` as `text/plain`. Permission: `Status`.

```bash
curl -s -X POST "http://localhost:6006/api/timings" \
  -d '{"categories": ["search", "inserting"], "file": "handlers.rs"}'
```

- The body is optional. Timing categories: `search`, `retrieve_opps`, `inserting`, `modifying_documents`, `database_lifecycle`, `reindex`, `backup`, `restore`, `disk_usage`, `command_parsing`.

### `GET /api/metrics`

Prometheus metrics endpoint.

```bash
curl -s "http://localhost:6006/api/metrics"
```

### `GET /api/list-models`

Lists the available embedding models. Permission: `ListModels`.

```bash
curl -s "http://localhost:6006/api/list-models"
```
