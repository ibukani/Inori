//! Durable storage. SQLite is only opened by the server, never over a shared drive.
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use inori_domain::*;
use inori_runtime::{ModuleState, Repository, Resource};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, Sqlite, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow},
};
use std::{path::Path, str::FromStr};
use uuid::Uuid;

#[derive(Clone)]
pub struct SqliteRepository {
    pool: SqlitePool,
}
pub fn schema_version() -> i64 {
    sqlx::migrate!("./migrations")
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0)
}
fn audience(a: Audience) -> &'static str {
    match a {
        Audience::Private => "private",
        Audience::Public => "public",
    }
}
fn decode_audience(s: &str) -> Audience {
    if s == "public" {
        Audience::Public
    } else {
        Audience::Private
    }
}
fn status(s: OperationStatus) -> &'static str {
    match s {
        OperationStatus::Running => "running",
        OperationStatus::Succeeded => "succeeded",
        OperationStatus::Failed => "failed",
        OperationStatus::Unknown => "unknown",
    }
}
fn character_row(row: SqliteRow) -> Character {
    Character {
        id: row.get("id"),
        name: row.get("name"),
        persona: row.get("persona"),
        revision: row.get::<i64, _>("revision") as u64,
        active_provider: row.get("active_provider"),
    }
}
fn source_row(row: SqliteRow) -> Result<SourceRecord> {
    let mut source: SourceRecord = serde_json::from_str(row.get("document"))?;
    source.suppressed = row.get::<i64, _>("suppressed") != 0;
    Ok(source)
}
fn memory_row(row: SqliteRow) -> Result<MemoryRecord> {
    let mut memory: MemoryRecord = serde_json::from_str(row.get("document"))?;
    memory.status = match row.get::<&str, _>("status") {
        "active" => MemoryStatus::Active,
        "superseded" => MemoryStatus::Superseded,
        _ => MemoryStatus::Deleted,
    };
    Ok(memory)
}
fn operation_row(row: SqliteRow) -> Result<Operation> {
    let mut operation: Operation = serde_json::from_str(row.get("document"))?;
    operation.status = match row.get::<&str, _>("status") {
        "running" => OperationStatus::Running,
        "succeeded" => OperationStatus::Succeeded,
        "failed" => OperationStatus::Failed,
        _ => OperationStatus::Unknown,
    };
    Ok(operation)
}

impl SqliteRepository {
    pub async fn open(path: &Path, initial: Character) -> Result<Self> {
        Self::open_impl(path, initial, false).await
    }
    /// The caller must hold server.lock for the entire serving lifetime.
    pub async fn open_locked(path: &Path, initial: Character) -> Result<Self> {
        Self::open_impl(path, initial, true).await
    }
    async fn open_impl(path: &Path, initial: Character, server_locked: bool) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let _migration_lock;
        if path.exists() && std::fs::metadata(path)?.len() > 0 {
            let pool = Self::read_pool(path).await?;
            let version: Option<i64> =
                sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
                    .fetch_one(&pool)
                    .await
                    .context("existing database is not an Inori database")?;
            let version = version.unwrap_or(0);
            if version > schema_version() {
                pool.close().await;
                bail!("database contains an unknown future migration");
            }
            if version < schema_version() {
                _migration_lock = if !server_locked {
                    let file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(path.parent().unwrap_or(Path::new(".")).join("server.lock"))?;
                    file.try_lock()
                        .context("stop the server before migrating the database")?;
                    Some(file)
                } else {
                    None
                };
                let directory = path.parent().unwrap_or(Path::new(".")).join("backups");
                tokio::fs::create_dir_all(&directory).await?;
                let destination = directory.join(format!(
                    "pre-schema-{}-{}.sqlite",
                    schema_version(),
                    Uuid::new_v4()
                ));
                sqlx::query("VACUUM INTO ?")
                    .bind(destination.to_string_lossy().as_ref())
                    .execute(&pool)
                    .await
                    .context("pre-migration backup failed; database not migrated")?;
            }
            pool.close().await;
        }
        let options = SqliteConnectOptions::from_str("sqlite:")?
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(3));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        sqlx::query("INSERT OR IGNORE INTO character(singleton,id,name,persona,revision,active_provider) VALUES(1,?,?,?,?,?)")
            .bind(initial.id).bind(initial.name).bind(initial.persona).bind(initial.revision as i64).bind(initial.active_provider).execute(&pool).await?;
        Ok(Self { pool })
    }
    pub async fn close(&self) {
        self.pool.close().await;
    }
    async fn read_pool(path: &Path) -> Result<SqlitePool> {
        if !path.is_file() {
            bail!("database does not exist");
        }
        Ok(SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::from_str("sqlite:")?
                    .filename(path)
                    .read_only(true)
                    .create_if_missing(false)
                    .busy_timeout(std::time::Duration::from_secs(3)),
            )
            .await?)
    }
    /// No migrations, directory creation or initialization; also works with old schemas.
    pub async fn backup_file(source: &Path, destination: &Path) -> Result<()> {
        if destination.exists() {
            bail!("backup destination already exists");
        }
        let pool = Self::read_pool(source).await?;
        let result = sqlx::query("VACUUM INTO ?")
            .bind(destination.to_string_lossy().as_ref())
            .execute(&pool)
            .await;
        pool.close().await;
        result?;
        Ok(())
    }
    pub async fn peek_character(path: &Path) -> Result<Option<Character>> {
        if !path.exists() {
            return Ok(None);
        }
        let pool = Self::read_pool(path).await?;
        let result = sqlx::query("SELECT * FROM character WHERE singleton=1")
            .fetch_optional(&pool)
            .await;
        pool.close().await;
        Ok(result?.map(character_row))
    }
    async fn bump(tx: &mut Transaction<'_, Sqlite>, expected: u64) -> Result<u64> {
        if expected > i64::MAX as u64 {
            bail!(DomainError::StaleRevision);
        }
        let result = sqlx::query(
            "UPDATE character SET revision=revision+1 WHERE singleton=1 AND revision=?",
        )
        .bind(expected as i64)
        .execute(&mut **tx)
        .await?;
        if result.rows_affected() != 1 {
            bail!(DomainError::StaleRevision);
        }
        Ok(expected + 1)
    }
    async fn insert_source(tx: &mut Transaction<'_, Sqlite>, source: &SourceRecord) -> Result<()> {
        validate_text(&source.text, MAX_TEXT_BYTES)?;
        sqlx::query(
            "INSERT INTO sources(id,document,audience,created_at,suppressed) VALUES(?,?,?,?,?)",
        )
        .bind(&source.id)
        .bind(serde_json::to_string(source)?)
        .bind(audience(source.audience))
        .bind(source.created_at)
        .bind(source.suppressed)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
    async fn insert_memory(
        tx: &mut Transaction<'_, Sqlite>,
        memory: &MemoryRecord,
        family: &str,
    ) -> Result<()> {
        sqlx::query("INSERT INTO memories(id,family,document,status,audience,created_at,expires_at) VALUES(?,?,?,'active',?,?,?)")
            .bind(&memory.id).bind(family).bind(serde_json::to_string(memory)?).bind(audience(memory.audience)).bind(memory.created_at).bind(memory.expires_at).execute(&mut **tx).await?;
        for id in &memory.source_ids {
            sqlx::query("INSERT INTO memory_sources(memory_id,source_id) VALUES(?,?)")
                .bind(&memory.id)
                .bind(id)
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }
    async fn suppress_sources(tx: &mut Transaction<'_, Sqlite>, ids: &[String]) -> Result<()> {
        for id in ids {
            sqlx::query("UPDATE sources SET suppressed=1 WHERE id=?")
                .bind(id)
                .execute(&mut **tx)
                .await?;
            sqlx::query("UPDATE memories SET status='deleted' WHERE id IN (SELECT memory_id FROM memory_sources WHERE source_id=?) AND status='active'")
                .bind(id).execute(&mut **tx).await?;
        }
        Ok(())
    }
    async fn replace_memory(
        &self,
        id: &str,
        text: &str,
        revision: u64,
        now: i64,
        restore_version: Option<&str>,
    ) -> Result<MemoryRecord> {
        validate_text(text, MAX_MEMORY_BYTES)?;
        let mut tx = self.pool.begin().await?;
        let revision = Self::bump(&mut tx, revision).await?;
        let row = sqlx::query("SELECT * FROM memories WHERE id=? AND status='active'")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .context("active memory not found")?;
        let family: String = row.get("family");
        let old = memory_row(row)?;
        if let Some(version) = restore_version {
            let record = sqlx::query("SELECT family,status FROM memories WHERE id=?")
                .bind(version)
                .fetch_optional(&mut *tx)
                .await?
                .context("history not found")?;
            if record.get::<String, _>("family") != family
                || record.get::<&str, _>("status") == "deleted"
            {
                bail!("history belongs to another memory or was deleted");
            }
        }
        sqlx::query("UPDATE memories SET status='superseded' WHERE family=? AND status='active'")
            .bind(&family)
            .execute(&mut *tx)
            .await?;
        Self::suppress_sources(&mut tx, &old.source_ids).await?;
        let source = SourceRecord {
            id: Uuid::new_v4().to_string(),
            actor: Actor::User,
            text: text.into(),
            created_at: now,
            audience: old.audience,
            turn_id: None,
            model: None,
            suppressed: false,
        };
        Self::insert_source(&mut tx, &source).await?;
        let memory = MemoryRecord {
            id: Uuid::new_v4().to_string(),
            text: text.into(),
            kind: MemoryKind::Confirmed,
            status: MemoryStatus::Active,
            audience: old.audience,
            source_ids: vec![source.id],
            created_at: now,
            expires_at: None,
            model: None,
            processor_version: "owner-edit/1".into(),
            supersedes: Some(old.id),
            revision,
        };
        Self::insert_memory(&mut tx, &memory, &family).await?;
        tx.commit().await?;
        Ok(memory)
    }
}

#[async_trait]
impl Repository for SqliteRepository {
    async fn character(&self) -> Result<Character> {
        Ok(character_row(
            sqlx::query("SELECT * FROM character WHERE singleton=1")
                .fetch_one(&self.pool)
                .await?,
        ))
    }
    async fn source(&self, id: &str) -> Result<Option<SourceRecord>> {
        sqlx::query("SELECT * FROM sources WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(source_row)
            .transpose()
    }
    async fn sources(&self, a: Audience, limit: usize) -> Result<Vec<SourceRecord>> {
        let rows = sqlx::query("SELECT * FROM sources WHERE suppressed=0 AND (audience='public' OR ?='private') ORDER BY created_at DESC,rowid DESC LIMIT ?")
            .bind(audience(a)).bind(limit.min(200) as i64).fetch_all(&self.pool).await?;
        let mut result = rows
            .into_iter()
            .map(source_row)
            .collect::<Result<Vec<_>>>()?;
        result.reverse();
        Ok(result)
    }
    async fn append_source(&self, record: SourceRecord) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE character SET revision=revision+1 WHERE singleton=1")
            .execute(&mut *tx)
            .await?;
        Self::insert_source(&mut tx, &record).await?;
        tx.commit().await?;
        Ok(())
    }
    async fn pending_memory_sources(&self, limit: usize) -> Result<Vec<SourceRecord>> {
        sqlx::query("SELECT * FROM sources s WHERE suppressed=0 AND json_extract(document,'$.actor')='user' AND json_extract(document,'$.turn_id') IS NOT NULL AND NOT EXISTS(SELECT 1 FROM extraction_receipts r WHERE r.source_id=s.id) ORDER BY created_at,rowid LIMIT ?")
            .bind(limit.min(32) as i64).fetch_all(&self.pool).await?.into_iter().map(source_row).collect()
    }
    async fn memories(
        &self,
        a: Audience,
        history: bool,
        now: i64,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>> {
        let rows = sqlx::query("SELECT * FROM memories m WHERE (m.audience='public' OR ?='private') AND ((?=1 AND m.status!='deleted') OR (m.status='active' AND (expires_at IS NULL OR expires_at>?) AND NOT EXISTS(SELECT 1 FROM memory_sources ms JOIN sources s ON s.id=ms.source_id WHERE ms.memory_id=m.id AND s.suppressed=1))) ORDER BY CASE WHEN json_extract(document,'$.kind')='confirmed' THEN 0 ELSE 1 END, created_at DESC, rowid DESC LIMIT ?")
            .bind(audience(a)).bind(history).bind(now).bind(limit.min(1000) as i64).fetch_all(&self.pool).await?;
        rows.into_iter().map(memory_row).collect()
    }
    async fn propose(&self, p: MemoryProposal, owner: bool, now: i64) -> Result<MemoryRecord> {
        let mut tx = self.pool.begin().await?;
        let next = Self::bump(&mut tx, p.expected_revision).await?;
        let mut sources = Vec::new();
        for id in &p.source_ids {
            if let Some(row) = sqlx::query("SELECT * FROM sources WHERE id=?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
            {
                sources.push(source_row(row)?);
            }
        }
        validate_proposal(&p, &sources, p.expected_revision, now, owner)?;
        let memory = MemoryRecord {
            id: Uuid::new_v4().to_string(),
            text: p.text,
            kind: p.kind,
            status: MemoryStatus::Active,
            audience: p.audience,
            source_ids: p.source_ids,
            created_at: now,
            expires_at: p.expires_at,
            model: p.model,
            processor_version: p.processor_version,
            supersedes: None,
            revision: next,
        };
        Self::insert_memory(&mut tx, &memory, &memory.id).await?;
        tx.commit().await?;
        Ok(memory)
    }
    async fn assert_memory(
        &self,
        text: &str,
        a: Audience,
        revision: u64,
        now: i64,
    ) -> Result<MemoryRecord> {
        validate_text(text, MAX_MEMORY_BYTES)?;
        let mut tx = self.pool.begin().await?;
        let next = Self::bump(&mut tx, revision).await?;
        let source = SourceRecord {
            id: Uuid::new_v4().to_string(),
            actor: Actor::User,
            text: text.into(),
            created_at: now,
            audience: a,
            turn_id: None,
            model: None,
            suppressed: false,
        };
        Self::insert_source(&mut tx, &source).await?;
        let memory = MemoryRecord {
            id: Uuid::new_v4().to_string(),
            text: text.into(),
            kind: MemoryKind::Confirmed,
            status: MemoryStatus::Active,
            audience: a,
            source_ids: vec![source.id],
            created_at: now,
            expires_at: None,
            model: None,
            processor_version: "owner-assertion/1".into(),
            supersedes: None,
            revision: next,
        };
        Self::insert_memory(&mut tx, &memory, &memory.id).await?;
        tx.commit().await?;
        Ok(memory)
    }
    async fn correct_memory(
        &self,
        id: &str,
        text: &str,
        revision: u64,
        now: i64,
    ) -> Result<MemoryRecord> {
        self.replace_memory(id, text, revision, now, None).await
    }
    async fn restore_memory(
        &self,
        id: &str,
        version: &str,
        revision: u64,
        now: i64,
    ) -> Result<MemoryRecord> {
        let row = sqlx::query("SELECT * FROM memories WHERE id=?")
            .bind(version)
            .fetch_optional(&self.pool)
            .await?
            .context("history not found")?;
        let old = memory_row(row)?;
        self.replace_memory(id, &old.text, revision, now, Some(version))
            .await
    }
    async fn delete_memory(&self, id: &str, revision: u64, _now: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        Self::bump(&mut tx, revision).await?;
        let family: String = sqlx::query_scalar("SELECT family FROM memories WHERE id=?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .context("memory not found")?;
        let ids: Vec<String> = sqlx::query_scalar("SELECT DISTINCT source_id FROM memory_sources WHERE memory_id IN (SELECT id FROM memories WHERE family=?)").bind(&family).fetch_all(&mut *tx).await?;
        Self::suppress_sources(&mut tx, &ids).await?;
        sqlx::query("UPDATE memories SET status='deleted' WHERE family=?")
            .bind(family)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    async fn update_persona(
        &self,
        name: &str,
        persona: &str,
        revision: u64,
        now: i64,
    ) -> Result<Character> {
        validate_text(name, 128)?;
        validate_text(persona, 8192)?;
        let before = self.character().await?;
        let mut tx = self.pool.begin().await?;
        Self::bump(&mut tx, revision).await?;
        sqlx::query("INSERT INTO checkpoints(id,kind,revision,document,created_at) VALUES(?,'persona',?,?,?)").bind(Uuid::new_v4().to_string()).bind(before.revision as i64).bind(serde_json::to_string(&before)?).bind(now).execute(&mut *tx).await?;
        sqlx::query("UPDATE character SET name=?,persona=? WHERE singleton=1")
            .bind(name)
            .bind(persona)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.character().await
    }
    async fn switch_provider(&self, provider: &str, revision: u64, now: i64) -> Result<Character> {
        validate_text(provider, 128)?;
        let before = self.character().await?;
        let memories = self.memories(Audience::Private, false, now, 1000).await?;
        let mut tx = self.pool.begin().await?;
        Self::bump(&mut tx, revision).await?;
        sqlx::query("INSERT INTO checkpoints(id,kind,revision,document,created_at) VALUES(?,'provider',?,?,?)")
            .bind(Uuid::new_v4().to_string()).bind(before.revision as i64).bind(json!({"character":before,"memories":memories}).to_string()).bind(now).execute(&mut *tx).await?;
        sqlx::query("UPDATE character SET active_provider=? WHERE singleton=1")
            .bind(provider)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.character().await
    }
    async fn grants(&self) -> Result<Vec<Grant>> {
        Ok(sqlx::query("SELECT * FROM grants")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| Grant {
                extension: r.get("extension"),
                capability: r.get("capability"),
                action: r.get("action"),
                target: r.get("target"),
                allowed: r.get::<i64, _>("allowed") != 0,
            })
            .collect())
    }
    async fn set_grant(&self, grant: Grant) -> Result<()> {
        sqlx::query("INSERT INTO grants(extension,capability,action,target,allowed) VALUES(?,?,?,?,?) ON CONFLICT(extension,capability,action,target) DO UPDATE SET allowed=excluded.allowed")
            .bind(grant.extension).bind(grant.capability).bind(grant.action).bind(grant.target).bind(grant.allowed).execute(&self.pool).await?;
        Ok(())
    }
    async fn begin_operation(&self, operation: Operation) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        // Acquire the SQLite writer before checking permission and reserving the ID.
        sqlx::query("UPDATE character SET revision=revision WHERE singleton=1")
            .execute(&mut *tx)
            .await?;
        let allowed: Option<i64> = sqlx::query_scalar("SELECT allowed FROM grants WHERE extension=? AND capability=? AND action=? AND target=?")
            .bind(&operation.extension).bind(&operation.capability).bind(&operation.action).bind(&operation.target).fetch_optional(&mut *tx).await?;
        if allowed != Some(1) {
            bail!(DomainError::PermissionDenied);
        }
        let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM operations WHERE id=?")
            .bind(&operation.id)
            .fetch_one(&mut *tx)
            .await?;
        if exists != 0 {
            bail!(DomainError::DuplicateOperation);
        }
        sqlx::query("INSERT INTO operations(id,document,status,created_at) VALUES(?,?,?,?)")
            .bind(&operation.id)
            .bind(serde_json::to_string(&operation)?)
            .bind(status(operation.status))
            .bind(operation.created_at)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    async fn finish_operation(
        &self,
        id: &str,
        state: OperationStatus,
        result: Value,
    ) -> Result<()> {
        let row = sqlx::query("SELECT * FROM operations WHERE id=? AND status='running'")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .context("running operation not found")?;
        let mut op = operation_row(row)?;
        op.status = state;
        op.result = Some(result);
        sqlx::query("UPDATE operations SET status=?,document=? WHERE id=? AND status='running'")
            .bind(status(state))
            .bind(serde_json::to_string(&op)?)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn operations(&self, limit: usize) -> Result<Vec<Operation>> {
        sqlx::query("SELECT * FROM operations ORDER BY created_at DESC,rowid DESC LIMIT ?")
            .bind(limit.min(100) as i64)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(operation_row)
            .collect()
    }
    async fn recover_operations(&self) -> Result<u64> {
        Ok(
            sqlx::query("UPDATE operations SET status='unknown' WHERE status='running'")
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
    async fn add_activity(&self, activity: Activity) -> Result<()> {
        validate_text(&activity.text, MAX_TEXT_BYTES)?;
        let mut tx = self.pool.begin().await?;
        if let Some(source) = activity.kind.strip_prefix("extract:") {
            sqlx::query(
                "INSERT OR IGNORE INTO extraction_receipts(source_id,processed_at) VALUES(?,?)",
            )
            .bind(source)
            .bind(activity.created_at)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "INSERT INTO activities(id,kind,document,audience,created_at) VALUES(?,?,?,?,?)",
        )
        .bind(&activity.id)
        .bind(&activity.kind)
        .bind(serde_json::to_string(&activity)?)
        .bind(audience(activity.audience))
        .bind(activity.created_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    async fn activities(&self, a: Audience, limit: usize) -> Result<Vec<Activity>> {
        sqlx::query_scalar::<_,String>("SELECT document FROM activities WHERE audience='public' OR ?='private' ORDER BY created_at DESC,rowid DESC LIMIT ?").bind(audience(a)).bind(limit.min(100) as i64).fetch_all(&self.pool).await?.into_iter().map(|s| serde_json::from_str(&s).map_err(Into::into)).collect()
    }
    async fn count_activities(&self, kind: &str, since: i64) -> Result<u32> {
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM activities WHERE kind=? AND created_at>=?")
                .bind(kind)
                .bind(since)
                .fetch_one(&self.pool)
                .await?;
        Ok(n as u32)
    }
    async fn new_generation(&self, id: &str, turn: &str, a: Audience) -> Result<()> {
        sqlx::query("INSERT INTO generations(id,turn_id,audience) VALUES(?,?,?)")
            .bind(id)
            .bind(turn)
            .bind(audience(a))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn last_activity_at(&self, kind: &str) -> Result<Option<i64>> {
        Ok(
            sqlx::query_scalar("SELECT max(created_at) FROM activities WHERE kind=?")
                .bind(kind)
                .fetch_one(&self.pool)
                .await?,
        )
    }
    async fn cancel_generation(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE generations SET active=0 WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn cancel_all_generations(&self) -> Result<()> {
        sqlx::query("UPDATE generations SET active=0")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn generation_active(&self, id: &str) -> Result<bool> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT active FROM generations WHERE id=?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                == Some(1),
        )
    }
    async fn add_segment(&self, generation: &str, segment: u64, text: &str) -> Result<()> {
        validate_text(text, MAX_TEXT_BYTES)?;
        sqlx::query("INSERT INTO segments(generation_id,segment,text) SELECT id,?,? FROM generations WHERE id=? AND active=1").bind(segment as i64).bind(text).bind(generation).execute(&self.pool).await?;
        Ok(())
    }
    async fn acknowledge_segment(&self, generation: &str, segment: u64, now: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query("UPDATE segments SET played=1 WHERE generation_id=? AND segment=? AND played=0 AND EXISTS(SELECT 1 FROM generations WHERE id=? AND active=1)")
            .bind(generation).bind(segment as i64).bind(generation).execute(&mut *tx).await?;
        if updated.rows_affected() == 0 {
            return Ok(false);
        }
        let row = sqlx::query("SELECT s.text,g.turn_id,g.audience FROM segments s JOIN generations g ON s.generation_id=g.id WHERE s.generation_id=? AND s.segment=?").bind(generation).bind(segment as i64).fetch_one(&mut *tx).await?;
        let source = SourceRecord {
            id: format!("speech:{generation}:{segment}"),
            actor: Actor::Assistant,
            text: row.get("text"),
            created_at: now,
            audience: decode_audience(row.get("audience")),
            turn_id: Some(row.get("turn_id")),
            model: None,
            suppressed: false,
        };
        Self::insert_source(&mut tx, &source).await?;
        sqlx::query("UPDATE character SET revision=revision+1 WHERE singleton=1")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }
    async fn put_resource(
        &self,
        mime: &str,
        a: Audience,
        bytes: &[u8],
        now: i64,
    ) -> Result<Resource> {
        if bytes.is_empty()
            || bytes.len() > 16 * 1024 * 1024
            || !matches!(mime, "audio/wav" | "image/png" | "application/octet-stream")
        {
            bail!("invalid resource or media limit exceeded");
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE character SET revision=revision WHERE singleton=1")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM resources WHERE created_at<?")
            .bind(now - 3600)
            .execute(&mut *tx)
            .await?;
        let used: i64 = sqlx::query_scalar("SELECT coalesce(sum(length(data)),0) FROM resources")
            .fetch_one(&mut *tx)
            .await?;
        if used + bytes.len() as i64 > 128 * 1024 * 1024 {
            bail!("resource quota exceeded");
        }
        let resource = Resource {
            id: Uuid::new_v4().to_string(),
            mime: mime.into(),
            bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
            audience: a,
        };
        sqlx::query(
            "INSERT INTO resources(id,mime,audience,sha256,data,created_at) VALUES(?,?,?,?,?,?)",
        )
        .bind(&resource.id)
        .bind(mime)
        .bind(audience(a))
        .bind(&resource.sha256)
        .bind(bytes)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(resource)
    }
    async fn resource(&self, id: &str) -> Result<Option<(Resource, Vec<u8>)>> {
        let row = sqlx::query("SELECT * FROM resources WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| {
            let data: Vec<u8> = r.get("data");
            (
                Resource {
                    id: r.get("id"),
                    mime: r.get("mime"),
                    bytes: data.len() as u64,
                    sha256: r.get("sha256"),
                    audience: decode_audience(r.get("audience")),
                },
                data,
            )
        }))
    }
    async fn housekeeping(&self, now: i64) -> Result<()> {
        sqlx::query("DELETE FROM resources WHERE created_at<?")
            .bind(now - 3600)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM generations WHERE created_at<?")
            .bind(now - 86400)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM activities WHERE id IN (SELECT id FROM activities ORDER BY created_at DESC,rowid DESC LIMIT -1 OFFSET 10000)").execute(&self.pool).await?;
        Ok(())
    }
    async fn register_device(&self, device: Device, token_hash: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO devices(id,name,audience,admin,token_hash,revoked) VALUES(?,?,?,?,?,?)",
        )
        .bind(device.id)
        .bind(device.name)
        .bind(audience(device.audience))
        .bind(device.admin)
        .bind(token_hash)
        .bind(device.revoked)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    async fn device_by_hash(&self, hash: &str) -> Result<Option<Device>> {
        Ok(
            sqlx::query("SELECT * FROM devices WHERE token_hash=? AND revoked=0")
                .bind(hash)
                .fetch_optional(&self.pool)
                .await?
                .map(|r| Device {
                    id: r.get("id"),
                    name: r.get("name"),
                    audience: decode_audience(r.get("audience")),
                    admin: r.get::<i64, _>("admin") != 0,
                    revoked: r.get::<i64, _>("revoked") != 0,
                }),
        )
    }
    async fn devices(&self) -> Result<Vec<Device>> {
        Ok(sqlx::query("SELECT * FROM devices")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| Device {
                id: r.get("id"),
                name: r.get("name"),
                audience: decode_audience(r.get("audience")),
                admin: r.get::<i64, _>("admin") != 0,
                revoked: r.get::<i64, _>("revoked") != 0,
            })
            .collect())
    }
    async fn revoke_device(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE devices SET revoked=1 WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn backup(&self, path: &Path) -> Result<()> {
        if path.exists() {
            bail!("backup destination already exists");
        }
        let path = path
            .to_str()
            .ok_or_else(|| anyhow!("backup path is not UTF-8"))?;
        sqlx::query("VACUUM INTO ?")
            .bind(path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    async fn ensure_module_state(&self, id: &str, version: u64) -> Result<ModuleState> {
        validate_text(id, 128)?;
        if version == 0 || version > i64::MAX as u64 {
            bail!("unsupported module state version");
        }
        sqlx::query("INSERT OR IGNORE INTO module_states(module_id,state_schema_version,revision,data) VALUES(?,?,0,'{}')").bind(id).bind(version as i64).execute(&self.pool).await?;
        let state = self
            .module_state(id)
            .await?
            .context("module state missing")?;
        if state.state_schema_version != version {
            bail!("module state schema is incompatible: {id}");
        }
        Ok(state)
    }
    async fn module_state(&self, id: &str) -> Result<Option<ModuleState>> {
        let row = sqlx::query("SELECT * FROM module_states WHERE module_id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| {
            Ok(ModuleState {
                module_id: r.get("module_id"),
                state_schema_version: r.get::<i64, _>("state_schema_version") as u64,
                revision: r.get::<i64, _>("revision") as u64,
                data: serde_json::from_str(r.get("data"))?,
            })
        })
        .transpose()
    }
    async fn save_module_state(
        &self,
        id: &str,
        version: u64,
        revision: u64,
        data: Value,
    ) -> Result<ModuleState> {
        if revision >= i64::MAX as u64 || version > i64::MAX as u64 {
            bail!(DomainError::StaleRevision);
        }
        let document = serde_json::to_string(&data)?;
        if document.len() > 65_536 {
            bail!("module state exceeds byte limit");
        }
        let result=sqlx::query("UPDATE module_states SET data=?,revision=revision+1 WHERE module_id=? AND state_schema_version=? AND revision=?").bind(document).bind(id).bind(version as i64).bind(revision as i64).execute(&self.pool).await?;
        if result.rows_affected() != 1 {
            bail!(DomainError::StaleRevision);
        }
        Ok(ModuleState {
            module_id: id.into(),
            state_schema_version: version,
            revision: revision + 1,
            data,
        })
    }
    async fn record_composition(&self, fingerprint: &str, config: Value, now: i64) -> Result<()> {
        let document = serde_json::to_string(&config)?;
        if document.len() > 262_144 {
            bail!("composition record exceeds byte limit");
        }
        sqlx::query("INSERT INTO composition_history(fingerprint,document,created_at) SELECT ?,?,? WHERE NOT EXISTS(SELECT 1 FROM (SELECT fingerprint FROM composition_history ORDER BY id DESC LIMIT 1) WHERE fingerprint=?)").bind(fingerprint).bind(document).bind(now).bind(fingerprint).execute(&self.pool).await?;
        Ok(())
    }
    async fn composition_history(&self, limit: usize) -> Result<Vec<Value>> {
        sqlx::query("SELECT fingerprint,document,created_at FROM composition_history ORDER BY id DESC LIMIT ?").bind(limit.min(100)as i64).fetch_all(&self.pool).await?.into_iter().map(|r|Ok(json!({"fingerprint":r.get::<String,_>("fingerprint"),"config":serde_json::from_str::<Value>(r.get("document"))?,"created_at":r.get::<i64,_>("created_at")}))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn an_older_binary_refuses_a_database_with_an_unknown_future_migration() {
        let (temp, repo) = db().await;
        repo.assert_memory("保持する情報", Audience::Private, 0, 1)
            .await
            .unwrap();
        sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES(999,'future',1,x'00',0)").execute(&repo.pool).await.unwrap();
        assert!(
            SqliteRepository::open(&temp.path().join("test.sqlite"), Character::default())
                .await
                .is_err()
        );
        assert_eq!(
            repo.memories(Audience::Private, false, 2, 10)
                .await
                .unwrap()[0]
                .text,
            "保持する情報"
        );
    }
    #[tokio::test]
    async fn extraction_receipt_survives_activity_log_pruning() {
        let (_temp, repo) = db().await;
        repo.append_source(SourceRecord {
            id: "raw".into(),
            actor: Actor::User,
            text: "架空の発言".into(),
            created_at: 1,
            audience: Audience::Private,
            turn_id: Some("turn".into()),
            model: None,
            suppressed: false,
        })
        .await
        .unwrap();
        assert_eq!(repo.pending_memory_sources(1).await.unwrap().len(), 1);
        repo.add_activity(Activity {
            id: "extract:raw".into(),
            kind: "extract:raw".into(),
            text: "処理済み".into(),
            created_at: 2,
            audience: Audience::Private,
        })
        .await
        .unwrap();
        sqlx::query("DELETE FROM activities")
            .execute(&repo.pool)
            .await
            .unwrap();
        assert!(repo.pending_memory_sources(1).await.unwrap().is_empty());
    }
    async fn db() -> (tempfile::TempDir, SqliteRepository) {
        let temp = tempfile::tempdir().unwrap();
        let repo = SqliteRepository::open(&temp.path().join("test.sqlite"), Character::default())
            .await
            .unwrap();
        (temp, repo)
    }
    #[tokio::test]
    async fn correction_is_transactional_and_deleted_evidence_stays_suppressed_after_restart() {
        let (temp, repo) = db().await;
        let m = repo
            .assert_memory("紅茶が好き", Audience::Private, 0, 1)
            .await
            .unwrap();
        let current = repo
            .correct_memory(&m.id, "珈琲が好き", 1, 2)
            .await
            .unwrap();
        let p = MemoryProposal {
            text: "紅茶が好き".into(),
            kind: MemoryKind::Inference,
            audience: Audience::Private,
            source_ids: m.source_ids.clone(),
            expires_at: None,
            model: Some("other-model".into()),
            processor_version: "v2".into(),
            expected_revision: 1,
        };
        assert!(
            repo.propose(p.clone(), false, 3)
                .await
                .unwrap_err()
                .downcast_ref::<DomainError>()
                == Some(&DomainError::StaleRevision)
        );
        let mut p = p;
        p.expected_revision = 2;
        assert!(repo.propose(p, false, 3).await.is_err());
        repo.delete_memory(&current.id, 2, 3).await.unwrap();
        repo.close().await;
        let repo = SqliteRepository::open(&temp.path().join("test.sqlite"), Character::default())
            .await
            .unwrap();
        assert!(
            repo.memories(Audience::Private, false, 4, 100)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            repo.source(&m.source_ids[0])
                .await
                .unwrap()
                .unwrap()
                .suppressed
        );
        assert_eq!(repo.character().await.unwrap().revision, 3);
    }
    #[tokio::test]
    async fn private_memory_is_not_in_public_view_and_switch_keeps_confirmed_facts() {
        let (_temp, repo) = db().await;
        repo.assert_memory("private-secret", Audience::Private, 0, 1)
            .await
            .unwrap();
        repo.assert_memory("公開してよい趣味", Audience::Public, 1, 2)
            .await
            .unwrap();
        repo.switch_provider("larger-model", 2, 3).await.unwrap();
        let public = repo
            .memories(Audience::Public, false, 4, 100)
            .await
            .unwrap();
        assert_eq!(public.len(), 1);
        assert!(!public[0].text.contains("secret"));
        assert_eq!(
            repo.memories(Audience::Private, false, 4, 100)
                .await
                .unwrap()
                .len(),
            2
        );
    }
    #[tokio::test]
    async fn unknown_operations_are_not_retried_and_permissions_are_checked() {
        let (_temp, repo) = db().await;
        let op = Operation {
            id: "once".into(),
            extension: "example".into(),
            capability: "echo".into(),
            action: "invoke".into(),
            target: "server".into(),
            status: OperationStatus::Running,
            result: None,
            created_at: 1,
        };
        assert!(repo.begin_operation(op.clone()).await.is_err());
        repo.set_grant(Grant {
            extension: "example".into(),
            capability: "echo".into(),
            action: "invoke".into(),
            target: "server".into(),
            allowed: true,
        })
        .await
        .unwrap();
        repo.begin_operation(op.clone()).await.unwrap();
        assert_eq!(repo.recover_operations().await.unwrap(), 1);
        assert!(repo.begin_operation(op).await.is_err());
        assert_eq!(
            repo.operations(10).await.unwrap()[0].status,
            OperationStatus::Unknown
        );
    }
    #[tokio::test]
    async fn cancelled_and_duplicate_playback_ack_cannot_create_unspoken_history() {
        let (_temp, repo) = db().await;
        repo.new_generation("g", "t", Audience::Private)
            .await
            .unwrap();
        repo.add_segment("g", 0, "まだ話していない").await.unwrap();
        repo.cancel_generation("g").await.unwrap();
        assert!(!repo.acknowledge_segment("g", 0, 1).await.unwrap());
        assert!(
            repo.sources(Audience::Private, 10)
                .await
                .unwrap()
                .is_empty()
        );
        repo.new_generation("h", "u", Audience::Private)
            .await
            .unwrap();
        repo.add_segment("h", 0, "話した部分").await.unwrap();
        assert!(repo.acknowledge_segment("h", 0, 2).await.unwrap());
        assert!(!repo.acknowledge_segment("h", 0, 2).await.unwrap());
        assert_eq!(repo.sources(Audience::Private, 10).await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn backup_restores_state_and_correction_history_can_be_explicitly_restored() {
        let (temp, repo) = db().await;
        let a = repo
            .assert_memory("初めの好み", Audience::Private, 0, 1)
            .await
            .unwrap();
        let b = repo
            .correct_memory(&a.id, "新しい好み", 1, 2)
            .await
            .unwrap();
        let restored = repo.restore_memory(&b.id, &a.id, 2, 3).await.unwrap();
        assert_eq!(restored.text, "初めの好み");
        let path = temp.path().join("backup.sqlite");
        repo.backup(&path).await.unwrap();
        let copy = SqliteRepository::open(&path, Character::default())
            .await
            .unwrap();
        assert_eq!(copy.character().await.unwrap().revision, 3);
        assert_eq!(
            copy.memories(Audience::Private, false, 4, 100)
                .await
                .unwrap()[0]
                .text,
            "初めの好み"
        );
    }
}
