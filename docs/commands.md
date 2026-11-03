

REST OF THE COMMANDS
Corelamo HTTP Commands

All routes live under /api. Everything except /api/login is a protected route: it goes through the auth middleware (unless the server runs with auth=false) and then a per-handler permission check.
Conventions

    {db_name}: name of the target database, taken from the URL path.

    "Requires running": the handler returns DatabaseNotRunning if any shard is stopped.

    Unknown fields: most request structs use deny_unknown_fields. A typo returns an error such as Unknown field 'x'. Did you mean 'y'?.

    Empty bodies: handlers that need a body reject an empty one with request body is empty.

    Batch responses: insert/replace/upsert/partial-replace/delete return HTTP 200 with a per-document breakdown of what succeeded and failed. Failure indexes are mapped back to the position in the client's input array.

Authentication
POST /api/login (public)

Exchanges credentials for a token.

{ "username": "admin", "password": "secret" }

Response: { "token": "..." }. Bad credentials return Unauthorized.
Document operations
POST /api/databases/{db_name}/insert

Inserts new documents. Requires running. Permission: Insert.

    Body: documents parsed by doc_parser::parse_documents against the database's policy. Parsing runs on a blocking thread.

    Documents that fail to parse, or that conflict with an existing document, are reported as failures. Conflicts use status 409.

    Response title: inserted N into '<db>', M failed.

POST /api/databases/{db_name}/replace

Replaces whole documents. Requires running. Permission: Replace.

    Same body format as insert.

    A document that does not exist is a failure with status 404.

POST /api/databases/{db_name}/upsert

Inserts if the document is missing, replaces if it exists. Requires running. Permission: Upsert.

    Same body format as insert.

POST /api/databases/{db_name}/partial-replace

Patches fields on existing documents instead of replacing them. Requires running. Permission: Replace.

[
  { "id": "doc1", "patch": { "year": 1999 } },
  { "id": "doc2", "patch": { "title": "New title" } }
]

    The body must be a non-empty array. Each item has exactly id and patch.

    Each document is fetched first, the patch is merged on a blocking thread, and the result is written with replace.

    If nothing could be patched, the response lists the failures with title partially replaced 0 document(s) ....

    Fetches are sequential (see the WARN in the code), so keep batches small.

DELETE /api/databases/{db_name}/delete

Deletes documents by id. Requires running. Permission: Delete.

["doc1", "doc2", "doc3"]

    Ids that don't exist are failures with status 404.

POST /api/databases/{db_name}/retrieve

Fetches the full stored documents by id. Requires running. Permission: Retrieve.

["doc1", "doc2"]

Response:

{
  "documents": [ { "id": "doc1", "data": { } } ],
  "not_found": ["doc2"],
  "skipped_ids": []
}

skipped_ids holds documents that could not be converted to the request format.
POST /api/databases/{db_name}/lookup

Like retrieve, but goes through the index layer and supports field projection. Requires running. Permission: Lookup.

{
  "ids": ["doc1", "doc2"],
  "return_fields": { "title": true, "year": true }
}

    ids is required. return_fields is optional.

    Response: { "documents": [ { "id", "data" } ], "not_found": [] }.

    When no id field needs stripping, the response is serialized straight from the raw bytes (fast path).

Search helpers (not search itself)
POST /api/databases/{db_name}/did-you-mean

Spelling suggestions per word. Requires running. Permission: DidYouMean.

{
  "value": "teh matrx",
  "search_fields": ["title"],
  "did_you_mean_count": 3,
  "fuzziness": "auto"
}

Field
	

Required
	

Notes

value
	

yes
	

The text to check

search_fields
	

no
	

Restrict suggestions to these fields

did_you_mean_count
	

no
	

Suggestions per word (default 3)

fuzziness
	

no
	

"0", "1", "2" or "auto" (a string)

Response:

{
  "original": "teh matrx",
  "corrected": "the matrix",
  "suggestions": [ { "word": "teh", "suggestions": ["the"] } ]
}

POST /api/databases/{db_name}/info-words

Term statistics. Requires running. Permission: Search.

{ "words": ["matrix", "neo"] }

Response: { "words": [ { "word", "occurrences", "documents" } ] }.
GET /api/databases/{db_name}/all-fields

Lists every field path (xpath) known to the database and its detected kind. Permission: AllFields.

Response: { "fields": { "title": "Text", "year": "Number" } }. The kind strings are the Debug output of the kind enum.
Database lifecycle
POST /api/databases/{db_name}/create-database

Creates a database. Permission: CreateDatabase.

{ "shard_count": 4 }

    The body is optional. The default is 4 shards and the minimum is 1 (shard_count is a u16).

    The name is validated with validate_db_name. An existing name returns AlreadyExists.

    Creation runs on a blocking thread. The new manager is registered and its backup scheduler is started.

    Returns 201 Created.

DELETE /api/databases/{db_name}/delete-database

Shuts the database down and deletes it from disk. Permission: DeleteDatabase.

    If other requests still hold a reference, it returns Conflict ("in use, try again") and re-registers the database.

    If the disk removal fails after the in-memory removal, the error says so explicitly.

DELETE /api/databases/{db_name}/clear-database

Wipes all data and the index but keeps the database. Permission: ClearDB.
POST /api/databases/{db_name}/rename-database

Permission: RenameDatabase.

{ "name": "new_name" }

    The database must be stopped first (stop-database), otherwise Conflict.

    The new name is validated, must differ from the current one, and must not already exist.

    Flow: shut down, rename the directory, reload the database, register it under the new name.

    If it renames on disk but cannot reopen, the error says the data is safe and will load on the next server restart.

POST /api/databases/{db_name}/start-database

Starts all shards. Permission: StartDB.
POST /api/databases/{db_name}/stop-database

Stops all shards. The database stays registered, but data endpoints return DatabaseNotRunning. Permission: StopDB.
POST /api/databases/{db_name}/restart-database

Stop then start. Permission: RestartDB.
GET /api/list-databases

Permission: ListDatabases.

Response: { "databases": [ { "name": "movies", "running": true } ] }.
GET /api/databases/{db_name}/status

Live statistics from atomics only, so it never blocks behind an fsync or reindex commit. Requires running. Permission: Status.

Response sections:

    background_compaction_enabled, shard_count

    metrics: search, indexing and reindex request/error counters, plus average times

    indexed: documents, deleted, segments

    reindexing: status, progress, documents_indexed, eta_seconds

    backup: status, progress_percent, mb_done, mb_total, eta_seconds, restoring

GET /api/databases/{db_name}/disk-usage

Computes disk usage on a blocking thread. Permission: Status.
Reindexing
POST /api/databases/{db_name}/reindex

Starts a reindex. Requires running. Permission: Reindex.

    The preparation runs synchronously on a blocking thread, and the actual reindexing continues in the background.

    Poll /status and read the reindexing section for progress.

POST /api/databases/{db_name}/reindex/abort

Aborts a running reindex. Permission: Reindex. It is idempotent: if the status is already Cancelled, it just says so.
Policy and configuration
GET /api/databases/{db_name}/get-policy

Returns the database's schema/indexing policy as TOML (application/toml). Permission: GetPolicy.
POST /api/databases/{db_name}/set-policy

Body is TOML. It is parsed first, then applied to all shards. Permission: SetPolicy.
GET /api/databases/{db_name}/get-config

Returns DatabaseOptions as pretty TOML. Permission: GetConfig.
POST /api/databases/{db_name}/set-config

Body is TOML DatabaseOptions. It is parsed before the database is looked up, then applied to all shards. Permission: SetConfig.
Logs
GET /api/databases/{db_name}/get-logs

Returns the database logs as text/plain. Permission: GetLogs.

    The body is optional. To pick a day, send { "date": "YYYY-MM-DD" }. Without a body you get the default log.

DELETE /api/databases/{db_name}/clear-logs

Deletes the database's logs. Permission: ClearLogs.
Backups
POST /api/databases/{db_name}/backup

Starts a full backup in the background and returns immediately. Permission: BackupFull.

    try_start_backup() rejects the call if another backup is already running.

    Track progress through the backup section of /status.

POST /api/databases/{db_name}/backup/incremental

Starts an incremental backup in the background. Permission: BackupIncremental.

    Requires an existing full backup, otherwise InvalidData.

    Failures are only logged, since the request has already returned.

GET /api/databases/{db_name}/list-backups

Permission: ListBackups.

Response: { "backups": [ ... ] }.
POST /api/databases/{db_name}/restore-backup/{backup_id}

Restores a backup. Permission: Restore.

    Unlike backup, this is synchronous: the request stays open until the restore finishes.

    It returns NotFound if the id doesn't exist, and rejects the call if a restore or backup is already running.

    Failures come back as FailedToEx.

DELETE /api/databases/{db_name}/delete-backup/{backup_id}

Deletes a backup. Permission: Delete. Returns NotFound if the id doesn't exist.
User management

Users are stored in the internal users database. All handlers check a permission first, then the auth service enforces its own rules (for example, who may change whom).

Route
	

Body
	

Permission

POST /api/users
	

{ "username", "password", "roles": ["..."] }
	

CreateUser

DELETE /api/users/{username}
	

none
	

DeleteUser

GET /api/users/list-users
	

none
	

ListUsers

POST /api/users/{username}/password
	

{ "password" }
	

UpdatePwd

POST /api/users/{username}/roles
	

{ "roles": ["..."] }
	

UpdateRole

list-users returns { "users": [ { "username", "roles" } ] }.
Diagnostics
POST /api/timings

Returns the timing report from core_timing as text/plain. Permission: Status.

{ "categories": ["search", "inserting"], "file": "handlers.rs" }

    The body is optional. Without one you get the unfiltered report.

    Timing categories used in the code: search, retrieve_opps, inserting, modifying_documents, database_lifecycle, reindex, backup, restore, disk_usage, command_parsing.

Things I noticed while reading the code

    average_search_us in /status is computed with as_millis(), so the value is in milliseconds despite the key name.

    delete-backup is guarded by Permission::Delete (the document-delete permission). If you want backups handled separately, a dedicated permission would be cleaner.

    reindex/abort, get-policy, get-config and list-backups don't check all_running(), which is probably intended since they are admin operations.

    delete_document_handler wraps errors from manager.delete as "delete task panicked", even though the error is a normal Err rather than a panic. The same pattern appears in replace, upsert and set-config. The message is misleading and could pass the original error through.

    CorsLayer::permissive() is flagged by your own TODO. It's worth tightening before any real deployment.

    Login takes the auth write lock, which blocks permission checks (read lock) while a password hash is being verified, if hashing happens inside auth.login.


