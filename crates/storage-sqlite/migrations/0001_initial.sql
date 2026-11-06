CREATE TABLE character (
 singleton INTEGER PRIMARY KEY CHECK(singleton = 1), id TEXT NOT NULL, name TEXT NOT NULL,
 persona TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0, active_provider TEXT NOT NULL
);
CREATE TABLE sources (
 id TEXT PRIMARY KEY, document TEXT NOT NULL, audience TEXT NOT NULL,
 created_at INTEGER NOT NULL, suppressed INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX sources_recent ON sources(audience, created_at DESC);
CREATE TABLE memories (
 id TEXT PRIMARY KEY, family TEXT NOT NULL, document TEXT NOT NULL, status TEXT NOT NULL,
 audience TEXT NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER
);
CREATE INDEX memories_live ON memories(status, audience, created_at DESC);
CREATE TABLE memory_sources (
 memory_id TEXT NOT NULL REFERENCES memories(id), source_id TEXT NOT NULL REFERENCES sources(id),
 PRIMARY KEY(memory_id, source_id)
);
CREATE INDEX memory_source_lineage ON memory_sources(source_id);
CREATE TABLE grants (
 extension TEXT NOT NULL, capability TEXT NOT NULL, action TEXT NOT NULL, target TEXT NOT NULL,
 allowed INTEGER NOT NULL, PRIMARY KEY(extension, capability, action, target)
);
CREATE TABLE operations (id TEXT PRIMARY KEY, document TEXT NOT NULL, status TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE activities (id TEXT PRIMARY KEY, kind TEXT NOT NULL, document TEXT NOT NULL, audience TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE INDEX activity_counts ON activities(kind, created_at);
CREATE TABLE generations (id TEXT PRIMARY KEY, turn_id TEXT NOT NULL, audience TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL DEFAULT(unixepoch()));
CREATE TABLE segments (generation_id TEXT NOT NULL REFERENCES generations(id) ON DELETE CASCADE, segment INTEGER NOT NULL, text TEXT NOT NULL, played INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(generation_id, segment));
CREATE TABLE resources (id TEXT PRIMARY KEY, mime TEXT NOT NULL, audience TEXT NOT NULL, sha256 TEXT NOT NULL, data BLOB NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE devices (id TEXT PRIMARY KEY, name TEXT NOT NULL, audience TEXT NOT NULL, admin INTEGER NOT NULL, token_hash TEXT NOT NULL UNIQUE, revoked INTEGER NOT NULL DEFAULT 0);
CREATE TABLE checkpoints (id TEXT PRIMARY KEY, kind TEXT NOT NULL, revision INTEGER NOT NULL, document TEXT NOT NULL, created_at INTEGER NOT NULL);
