# Corelamo HTTP Commands

All routes live under `/api`. Everything except `/api/login` is a protected route: it goes through the auth middleware (unless the server runs with `auth=false`) and then a per-handler permission check.

## Conventions

- `{db_name}`: name of the target database, taken from the URL path.
- **"Requires running"**: the handler returns `DatabaseNotRunning` if any shard is stopped.
- **Unknown fields**: most request structs use `deny_unknown_fields`. A typo returns an error such as `Unknown field 'x'. Did you mean 'y'?`.
- **Empty bodies**: handlers that need a body reject an empty one with `request body is empty`.
- **Batch responses**: insert/replace/upsert/partial-replace/delete return HTTP 200 with a per-document breakdown of what succeeded and failed. Failure indexes are mapped back to the position in the client's input array.

### Response envelope

Most command endpoints wrap their result in a fixed JSON envelope; the payload is under `data`:

```json
{
  "status": 200,
  "title": "Login successful",
  "instance": "/api/login",
  "request_id": "9f4c2c1e-…",
  "time_taken": "1.2ms",
  "data": { }
}
```

Some endpoints return raw bodies instead (no envelope): `get-policy` (JSON), `get-config` and `dictionary` (TOML), `get-logs` (text), `metrics`, `timings`.

Errors use a separate shape:

```json
{
  "type": "http://corelamo.com/errors/unauthorized",
  "title": "Unauthorized",
  "status": 401,
  "detail": "missing X-Corelamo-Key header",
  "instance": "/api/…",
  "request_id": "…",
  "time_taken_ms": 0
}
```

---

## Running the server

```bash
corelamo-runtime --root-path /var/lib/corelamo --port 6006 --auth true
```

| flag | default | meaning |
|---|---|---|
| `--root-path <path>` | `/var/lib/corelamo` | root for all databases and config; `corelamo` is appended unless the path already ends with it |
| `--name <name>` | `corelamo` | instance name |
| `--host <host>` | `0.0.0.0` | bind address |
| `--port <port>` | `6006` | bind port |
| `--format <format>` | `json` | default response format |
| `--auth <true\|false>` | `true` | `false` disables authentication (every request runs as an anonymous admin) |
| `--max_payload_size <MB>` | `1024` | request body limit |
| `--max_request_timeout <s>` | `240` | request timeout |
| `--overwrite_config <true\|false>` | `false` | `true`: CLI wins over the config file and is persisted; `false`: config file wins over CLI |
| `--embedding-model <id>` | `bge-m3` | embedding model |

Settings live in `<root-path>/CorelamoSettings.toml`, created with defaults on first start. Precedence: **config file > CLI > built-in default** by default; **CLI > config > default** with `--overwrite_config true` (the CLI values are written back to the file).

`-h` / `--help` prints the same list.

---

## Authentication

### First start: the superuser

On the first start with an empty user database the server creates the superuser:

| | |
|---|---|
| username | `corelamo` |
| role | `admin` (every permission) |
| password | generated once, printed to **stderr** |

```
  user:     corelamo
  password: <generated>
```

If stdin is a terminal, it also asks `Set your own password now? [y/N]:`; `y` replaces the generated password (minimum 8 characters). With no terminal (systemd, Docker, CI) it skips the prompt and the printed password stays valid.

The password cannot be recovered. Change it later as a logged-in admin with `POST /api/users/corelamo/password`.

### `POST /api/login` (public, no token needed)

Body: `{"username": "...", "password": "..."}`.

```bash
curl -s -X POST "http://localhost:6006/api/login" \
  -H "Content-Type: application/json" \
  -d '{"username": "corelamo", "password": "<generated>"}'
```

Success (HTTP 200; the token is at **`data.token`**):

```json
{
  "status": 200,
  "title": "Login successful",
  "instance": "/api/login",
  "request_id": "…",
  "time_taken": "…",
  "data": { "token": "64 hex characters" }
}
```

Failure: HTTP 401 `Unauthorized`, `detail: "invalid username or password"` — the same answer for an unknown user and a wrong password.

### Using the token

Every protected request carries the token in the **`X-Corelamo-Key`** header (there is no `Authorization: Bearer`):

```bash
curl -s "http://localhost:6006/api/databases/movies/status" \
  -H "X-Corelamo-Key: <token>"
```

- Missing header → HTTP 401 `Unauthorized`, `detail: "missing X-Corelamo-Key header"`, plus `WWW-Authenticate: Corelamo-Key`.
- Unknown or expired token → HTTP 401 `Unauthorized`.
- A valid token that lacks the route's permission → HTTP 403 `Permission denied`, `detail: "user '<username>' lacks permission '<Permission>'"`.

### Token lifetime

- 32 random bytes from the OS RNG, hex-encoded (64 characters).
- Valid for **24 hours** from login.
- Stored **hashed and in memory only**: a server restart invalidates every token (clients must log in again).
- Every login issues a new independent token; logging out revokes only that one.

### `POST /api/logout`

Revokes the token sent in `X-Corelamo-Key`.

```bash
curl -s -X POST "http://localhost:6006/api/logout" \
  -H "X-Corelamo-Key: <token>"
```

Revoking an unknown token is not an error.

### Auth disabled (`--auth false`)

No login and no header: every request runs as `anonymous` with the `admin` role. Intended for local development — do not expose such a server.

### Permissions

Each handler also checks a permission on the principal; the per-route tables below list which. `admin` has every permission, other roles a subset (see `core-auth/src/bootstrap.rs`).

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

Returns the database's schema/indexing policy as **JSON** (`application/json`, raw — no envelope). Permission: `GetPolicy`.

```bash
curl -s "http://localhost:6006/api/databases/movies/get-policy"
```

### `POST /api/databases/{db_name}/set-policy`

Body is a **JSON** `IndexPolicy` (the same shape `get-policy` returns). It is parsed first, then applied to all shards. Permission: `SetPolicy`.

```bash
curl -s -X POST "http://localhost:6006/api/databases/movies/set-policy" \
  -H "Content-Type: application/json" \
  --data-binary '{
    "analyzers": { "title": { "stemming": "english" } },
    "document": {
      "title": { "$type": "text", "$weight": { "min": 65, "max": 90 } },
      "year": { "$type": "int" }
    }
  }'
```

- Field names must not contain `/`: model nesting as a struct (`"rank": { "pagerank": … }`), not `"rank/pagerank"`.
- Unknown field settings (`$…`) are rejected.
- See `policy.md` for the whole format and `search.md` for how the fields are queried.

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
