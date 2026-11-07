//! Typed, IO-neutral boundaries for built-in server modules. No database or GUI access.
use anyhow::Result;
use async_trait::async_trait;
use inori_domain::{Audience, Character, MemoryRecord, SourceRecord};
pub use inori_protocol::{
    CapabilityBinding, CapabilityRequirement, CapabilitySpec, ClientHello, CompositionConfig,
    ModuleDescriptor, ModuleState, ModuleStatus, ServerCapabilities,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error("module {module} is unavailable: {reason}")]
    Unavailable { module: String, reason: String },
    #[error("module {0} is busy")]
    Busy(String),
}
pub fn unavailable(module: &str, reason: &str) -> anyhow::Error {
    ModuleError::Unavailable {
        module: module.into(),
        reason: reason.into(),
    }
    .into()
}

#[async_trait]
pub trait ModuleHost: Send + Sync {
    async fn save_state(&self, expected_revision: u64, data: Value) -> Result<ModuleState>;
}
pub struct ModuleContext {
    pub state: ModuleState,
    pub host: Arc<dyn ModuleHost>,
    pub cancel: CancellationToken,
}
#[async_trait]
pub trait Module: Send + Sync {
    fn id(&self) -> &str;
    async fn start(&self, _context: ModuleContext) -> Result<()> {
        Ok(())
    }
    async fn stop(&self) -> Result<Option<Value>> {
        Ok(None)
    }
}
#[macro_export]
macro_rules! module {
    ($name:ty,$id:literal) => {
        #[async_trait::async_trait]
        impl $crate::Module for $name {
            fn id(&self) -> &str {
                $id
            }
        }
    };
}
pub fn descriptor(
    id: &str,
    capability: &str,
    requires: &[(&str, &str, bool)],
    settings_schema: Value,
) -> ModuleDescriptor {
    ModuleDescriptor {
        id: id.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        execution: "builtin".into(),
        provides: vec![CapabilitySpec {
            id: capability.into(),
            major: 1,
        }],
        requires: requires
            .iter()
            .map(|(port, capability, optional)| CapabilityRequirement {
                port: (*port).into(),
                capability: (*capability).into(),
                major: 1,
                optional: *optional,
            })
            .collect(),
        settings_schema,
        permissions: vec![],
        max_concurrency: 1,
        state_schema_version: 1,
    }
}
pub fn no_settings() -> Value {
    json!({"type":"object","properties":{},"additionalProperties":false})
}

#[derive(Clone)]
pub struct Awareness {
    pub utc: String,
    pub present: bool,
}
pub trait AwarenessProvider: Module {
    fn observe(&self, now: i64, present: bool) -> Result<Awareness>;
}
pub struct DialogueInput {
    pub character: Character,
    pub memories: Vec<MemoryRecord>,
    pub sources: Vec<SourceRecord>,
    pub audience: Audience,
    pub mode: inori_protocol::DialogueMode,
    /// Identity of the authoritative current user input. History is oldest first;
    /// matching text alone cannot distinguish repeated user messages.
    pub request_source_id: String,
    pub request: String,
    pub profile: ProviderProfile,
    pub awareness: Option<Awareness>,
}
pub trait DialoguePolicy: Module {
    fn prepare(&self, input: DialogueInput) -> Result<GenerationContext>;
}
#[derive(Clone)]
pub struct MemoryCandidate {
    pub text: String,
    pub expires_at: Option<i64>,
}
pub trait MemoryProcessor: Module {
    fn extraction(&self, source: &SourceRecord) -> GenerationContext;
    fn interpret(&self, text: &str, max: usize) -> Result<Vec<MemoryCandidate>>;
    fn review(
        &self,
        audience: Audience,
        evidence: Value,
        max_tokens: u32,
    ) -> Result<GenerationContext>;
}
pub trait AutonomyPlanner: Module {
    fn prepare(&self, character: &Character, awareness: Awareness) -> GenerationContext;
}

#[async_trait]
pub trait SpeechRecognizer: Module {
    fn remote(&self) -> bool;
    async fn transcribe(&self, wav: Vec<u8>, cancel: CancellationToken) -> Result<String>;
}
#[async_trait]
pub trait SpeechSynthesizer: Module {
    fn remote(&self) -> bool;
    async fn synthesize(&self, text: &str, cancel: CancellationToken) -> Result<Vec<u8>>;
}
#[async_trait]
pub trait ActionCall: Send {
    async fn invoke(self: Box<Self>, cancel: CancellationToken) -> Result<Value>;
}
pub trait ActionExecutor: Module {
    fn prepare(
        &self,
        extension: &str,
        capability: &str,
        arguments: Value,
    ) -> Result<Box<dyn ActionCall>>;
}
pub trait ChannelAdapter: Module {
    fn normalize(
        &self,
        request: inori_protocol::ChatRequest,
    ) -> Result<inori_protocol::ChatRequest>;
    fn negotiate(
        &self,
        hello: &ClientHello,
        available: ServerCapabilities,
    ) -> Result<ServerCapabilities>;
    fn outgoing(
        &self,
        event: inori_protocol::EventEnvelope,
        negotiated: Option<&ServerCapabilities>,
    ) -> Option<inori_protocol::EventEnvelope>;
}
#[async_trait]
pub trait PermissionGuard: Send + Sync {
    async fn check(
        &self,
        extension: &str,
        capability: &str,
        action: &str,
        target: &str,
    ) -> Result<()>;
}
pub struct Modules {
    pub lifecycle: Vec<Arc<dyn Module>>,
    pub awareness: Option<Arc<dyn AwarenessProvider>>,
    pub dialogue: Option<Arc<dyn DialoguePolicy>>,
    pub memory: Option<Arc<dyn MemoryProcessor>>,
    pub autonomy: Option<Arc<dyn AutonomyPlanner>>,
    pub recognizer: Option<Arc<dyn SpeechRecognizer>>,
    pub synthesizer: Option<Arc<dyn SpeechSynthesizer>>,
    pub actions: Option<Arc<dyn ActionExecutor>>,
    pub channel: Option<Arc<dyn ChannelAdapter>>,
}
impl Modules {
    pub fn empty() -> Self {
        Self {
            lifecycle: vec![],
            awareness: None,
            dialogue: None,
            memory: None,
            autonomy: None,
            recognizer: None,
            synthesizer: None,
            actions: None,
            channel: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub id: String,
    #[serde(default = "mock_kind")]
    pub kind: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub extension: Option<String>,
    #[serde(default)]
    pub capability: Option<String>,
    #[serde(default)]
    pub thinking: Option<bool>,
    #[serde(default)]
    pub cloud: bool,
    #[serde(default)]
    pub allow_private: bool,
    #[serde(default = "context_limit")]
    pub max_context_chars: usize,
    #[serde(default = "output_limit")]
    pub max_output_tokens: u32,
    #[serde(default = "request_timeout")]
    pub timeout_seconds: u64,
}
fn mock_kind() -> String {
    "mock".into()
}
fn context_limit() -> usize {
    12_000
}
fn output_limit() -> u32 {
    384
}
fn request_timeout() -> u64 {
    60
}
impl Default for ProviderProfile {
    fn default() -> Self {
        Self {
            id: "mock".into(),
            kind: mock_kind(),
            base_url: String::new(),
            model: "deterministic-demo".into(),
            api_key_env: None,
            extension: None,
            capability: None,
            thinking: None,
            cloud: false,
            allow_private: true,
            max_context_chars: context_limit(),
            max_output_tokens: output_limit(),
            timeout_seconds: request_timeout(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GenerationContext {
    pub audience: Audience,
    pub system: String,
    pub messages: Vec<(String, String)>,
    pub max_output_tokens: u32,
    pub purpose: String,
}

#[async_trait]
pub trait InferenceProvider: Send + Sync {
    fn profile(&self) -> &ProviderProfile;
    async fn generate(
        &self,
        context: GenerationContext,
        output: mpsc::Sender<String>,
        cancel: CancellationToken,
    ) -> Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VoiceSettings {
    pub whisper_url: Option<String>,
    pub voicevox_url: Option<String>,
    #[serde(default)]
    pub speaker: u32,
    #[serde(default)]
    pub cancellable_synthesis: bool,
    #[serde(default)]
    pub allow_private_remote: bool,
}
