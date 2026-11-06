//! Pure character rules. No database, network, GUI, or model SDK dependencies.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

pub const MAX_TEXT_BYTES: usize = 16_384;
pub const MAX_MEMORY_BYTES: usize = 2_048;
pub const MAX_SOURCES: usize = 16;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("text must be non-empty and within the byte limit")]
    InvalidText,
    #[error("the state changed; reload before applying this change")]
    StaleRevision,
    #[error("a live original source is required")]
    InvalidSource,
    #[error("private evidence cannot be published by an AI")]
    PrivateEvidence,
    #[error("an AI may propose inferences or summaries, not confirmed facts")]
    UntrustedPromotion,
    #[error("the permission does not allow this capability, action, and target")]
    PermissionDenied,
    #[error("this operation already exists; inspect its recorded result")]
    DuplicateOperation,
    #[error("the expiry is already in the past")]
    Expired,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Audience {
    #[default]
    Private,
    Public,
}
impl Audience {
    pub fn permits(self, record: Self) -> bool {
        self == Self::Private || record == Self::Public
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Character {
    pub id: String,
    pub name: String,
    pub persona: String,
    pub revision: u64,
    pub active_provider: String,
}
impl Default for Character {
    fn default() -> Self {
        Self { id: "inori".into(), name: "Inori".into(),
            persona: "好奇心があり、親しみやすいキャラクター。相手を理解し、自分の興味も持つ。分からないことは断定せず、実行していない出来事を体験談にしない。".into(),
            revision: 0, active_provider: "mock".into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceRecord {
    pub id: String,
    pub actor: Actor,
    pub text: String,
    pub created_at: i64,
    pub audience: Audience,
    pub turn_id: Option<String>,
    pub model: Option<String>,
    pub suppressed: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Confirmed,
    Inference,
    Summary,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Active,
    Superseded,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryRecord {
    pub id: String,
    pub text: String,
    pub kind: MemoryKind,
    pub status: MemoryStatus,
    pub audience: Audience,
    pub source_ids: Vec<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub model: Option<String>,
    pub processor_version: String,
    pub supersedes: Option<String>,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryProposal {
    pub text: String,
    pub kind: MemoryKind,
    pub audience: Audience,
    pub source_ids: Vec<String>,
    pub expires_at: Option<i64>,
    pub model: Option<String>,
    pub processor_version: String,
    pub expected_revision: u64,
}

pub fn validate_text(text: &str, max: usize) -> Result<(), DomainError> {
    if text.trim().is_empty() || text.len() > max {
        Err(DomainError::InvalidText)
    } else {
        Ok(())
    }
}

/// Caller supplies original records, never recursively generated "evidence".
pub fn validate_proposal(
    p: &MemoryProposal,
    sources: &[SourceRecord],
    current_revision: u64,
    now: i64,
    owner_assertion: bool,
) -> Result<(), DomainError> {
    validate_text(&p.text, MAX_MEMORY_BYTES)?;
    if p.expected_revision != current_revision {
        return Err(DomainError::StaleRevision);
    }
    if !owner_assertion && p.kind == MemoryKind::Confirmed {
        return Err(DomainError::UntrustedPromotion);
    }
    if p.expires_at.is_some_and(|expiry| expiry <= now) {
        return Err(DomainError::Expired);
    }
    let ids: BTreeSet<_> = p.source_ids.iter().collect();
    if ids.is_empty()
        || ids.len() != p.source_ids.len()
        || ids.len() > MAX_SOURCES
        || sources.len() != ids.len()
    {
        return Err(DomainError::InvalidSource);
    }
    for source in sources {
        if !ids.contains(&source.id) || source.suppressed || source.text.is_empty() {
            return Err(DomainError::InvalidSource);
        }
        if !p.audience.permits(source.audience) {
            return Err(DomainError::PrivateEvidence);
        }
    }
    if !owner_assertion
        && p.kind == MemoryKind::Inference
        && !sources
            .iter()
            .any(|s| matches!(s.actor, Actor::User | Actor::Tool))
    {
        return Err(DomainError::InvalidSource);
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Grant {
    pub extension: String,
    pub capability: String,
    pub action: String,
    pub target: String,
    pub allowed: bool,
}
pub fn check_permission(
    grants: &[Grant],
    extension: &str,
    capability: &str,
    action: &str,
    target: &str,
) -> Result<(), DomainError> {
    if grants.iter().any(|g| {
        g.allowed
            && g.extension == extension
            && g.capability == capability
            && g.action == action
            && g.target == target
    }) {
        Ok(())
    } else {
        Err(DomainError::PermissionDenied)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Running,
    Succeeded,
    Failed,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub extension: String,
    pub capability: String,
    pub action: String,
    pub target: String,
    pub status: OperationStatus,
    pub result: Option<serde_json::Value>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activity {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub created_at: i64,
    pub audience: Audience,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub character: Character,
    pub memories: Vec<MemoryRecord>,
    pub sources: Vec<SourceRecord>,
    pub activities: Vec<Activity>,
    pub operations: Vec<Operation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub audience: Audience,
    pub admin: bool,
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomyPolicy {
    pub interval_seconds: u64,
    pub max_actions: usize,
    pub enabled: bool,
    pub cloud_enabled: bool,
    pub daily_inference_limit: u32,
}
impl Default for AutonomyPolicy {
    fn default() -> Self {
        Self {
            interval_seconds: 1800,
            max_actions: 3,
            enabled: true,
            cloud_enabled: false,
            daily_inference_limit: 24,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> SourceRecord {
        SourceRecord {
            id: "s".into(),
            actor: Actor::User,
            text: "今日は疲れた".into(),
            created_at: 1,
            audience: Audience::Private,
            turn_id: None,
            model: None,
            suppressed: false,
        }
    }
    fn proposal() -> MemoryProposal {
        MemoryProposal {
            text: "今日は疲れている可能性がある".into(),
            kind: MemoryKind::Inference,
            audience: Audience::Private,
            source_ids: vec!["s".into()],
            expires_at: Some(100),
            model: Some("small".into()),
            processor_version: "1".into(),
            expected_revision: 4,
        }
    }
    #[test]
    fn stale_background_and_self_promotion_are_rejected() {
        assert_eq!(
            validate_proposal(&proposal(), &[source()], 5, 2, false),
            Err(DomainError::StaleRevision)
        );
        let mut p = proposal();
        p.kind = MemoryKind::Confirmed;
        assert_eq!(
            validate_proposal(&p, &[source()], 4, 2, false),
            Err(DomainError::UntrustedPromotion)
        );
    }
    #[test]
    fn private_evidence_and_suppressed_sources_cannot_be_reused() {
        let mut p = proposal();
        p.audience = Audience::Public;
        assert_eq!(
            validate_proposal(&p, &[source()], 4, 2, false),
            Err(DomainError::PrivateEvidence)
        );
        let mut s = source();
        s.suppressed = true;
        assert_eq!(
            validate_proposal(&proposal(), &[s], 4, 2, false),
            Err(DomainError::InvalidSource)
        );
    }
    #[test]
    fn assistant_repetition_does_not_become_evidence_for_facts() {
        let mut s = source();
        s.actor = Actor::Assistant;
        assert_eq!(
            validate_proposal(&proposal(), &[s], 4, 2, false),
            Err(DomainError::InvalidSource)
        );
    }
    #[test]
    fn permissions_are_exact_and_default_deny() {
        let grants = vec![Grant {
            extension: "example".into(),
            capability: "test.echo".into(),
            action: "invoke".into(),
            target: "server".into(),
            allowed: true,
        }];
        assert!(check_permission(&grants, "example", "test.echo", "invoke", "server").is_ok());
        assert!(check_permission(&grants, "example", "test.echo", "invoke", "desktop").is_err());
    }
}
