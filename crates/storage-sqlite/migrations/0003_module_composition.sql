CREATE TABLE module_states (
 module_id TEXT PRIMARY KEY, state_schema_version INTEGER NOT NULL,
 revision INTEGER NOT NULL DEFAULT 0, data TEXT NOT NULL
);
CREATE TABLE composition_history (
 id INTEGER PRIMARY KEY AUTOINCREMENT, fingerprint TEXT NOT NULL,
 document TEXT NOT NULL, created_at INTEGER NOT NULL
);
