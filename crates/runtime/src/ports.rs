use anyhow::Result;
use async_trait::async_trait;
use inori_domain::*;
pub use inori_module_api::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub id: String,
    pub mime: String,
    pub bytes: u64,
    pub sha256: String,
    pub audience: Audience,
}

#[async_trait]
pub trait Repository: Send + Sync {
    async fn character(&self) -> Result<Character>;
    async fn source(&self, id: &str) -> Result<Option<SourceRecord>>;
    async fn sources(&self, audience: Audience, limit: usize) -> Result<Vec<SourceRecord>>;
    async fn pending_memory_sources(&self, limit: usize) -> Result<Vec<SourceRecord>>;
    async fn append_source(&self, record: SourceRecord) -> Result<()>;
    async fn memories(
        &self,
        audience: Audience,
        history: bool,
        now: i64,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>>;
    async fn propose(
        &self,
        proposal: MemoryProposal,
        owner: bool,
        now: i64,
    ) -> Result<MemoryRecord>;
    async fn assert_memory(
        &self,
        text: &str,
        audience: Audience,
        revision: u64,
        now: i64,
    ) -> Result<MemoryRecord>;
    async fn correct_memory(
        &self,
        id: &str,
        text: &str,
        revision: u64,
        now: i64,
    ) -> Result<MemoryRecord>;
    async fn restore_memory(
        &self,
        id: &str,
        version: &str,
        revision: u64,
        now: i64,
    ) -> Result<MemoryRecord>;
    async fn delete_memory(&self, id: &str, revision: u64, now: i64) -> Result<()>;
    async fn update_persona(
        &self,
        name: &str,
        persona: &str,
        revision: u64,
        now: i64,
    ) -> Result<Character>;
    async fn switch_provider(&self, provider: &str, revision: u64, now: i64) -> Result<Character>;
    async fn grants(&self) -> Result<Vec<Grant>>;
    async fn set_grant(&self, grant: Grant) -> Result<()>;
    async fn begin_operation(&self, operation: Operation) -> Result<()>;
    async fn finish_operation(
        &self,
        id: &str,
        status: OperationStatus,
        result: Value,
    ) -> Result<()>;
    async fn operations(&self, limit: usize) -> Result<Vec<Operation>>;
    async fn recover_operations(&self) -> Result<u64>;
    async fn add_activity(&self, activity: Activity) -> Result<()>;
    async fn activities(&self, audience: Audience, limit: usize) -> Result<Vec<Activity>>;
    async fn count_activities(&self, kind: &str, since: i64) -> Result<u32>;
    async fn last_activity_at(&self, kind: &str) -> Result<Option<i64>>;
    async fn new_generation(&self, id: &str, turn: &str, audience: Audience) -> Result<()>;
    async fn cancel_generation(&self, id: &str) -> Result<()>;
    async fn cancel_all_generations(&self) -> Result<()>;
    async fn generation_active(&self, id: &str) -> Result<bool>;
    async fn add_segment(&self, generation: &str, segment: u64, text: &str) -> Result<()>;
    async fn acknowledge_segment(&self, generation: &str, segment: u64, now: i64) -> Result<bool>;
    async fn put_resource(
        &self,
        mime: &str,
        audience: Audience,
        bytes: &[u8],
        now: i64,
    ) -> Result<Resource>;
    async fn resource(&self, id: &str) -> Result<Option<(Resource, Vec<u8>)>>;
    async fn housekeeping(&self, now: i64) -> Result<()>;
    async fn register_device(&self, device: Device, token_hash: &str) -> Result<()>;
    async fn device_by_hash(&self, token_hash: &str) -> Result<Option<Device>>;
    async fn devices(&self) -> Result<Vec<Device>>;
    async fn revoke_device(&self, id: &str) -> Result<()>;
    async fn backup(&self, destination: &std::path::Path) -> Result<()>;
    async fn ensure_module_state(&self, id: &str, version: u64) -> Result<ModuleState>;
    async fn module_state(&self, id: &str) -> Result<Option<ModuleState>>;
    async fn save_module_state(
        &self,
        id: &str,
        version: u64,
        revision: u64,
        data: Value,
    ) -> Result<ModuleState>;
    async fn record_composition(&self, fingerprint: &str, config: Value, now: i64) -> Result<()>;
    async fn composition_history(&self, limit: usize) -> Result<Vec<Value>>;
}

pub trait Clock: Send + Sync {
    fn now(&self) -> i64;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
    }
}
