use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use inori_domain::*;
use inori_inference_providers::HttpProvider;
use inori_runtime::{
    Clock, GenerationContext, InferenceProvider, ProviderProfile, Repository, SystemClock,
};
use inori_server::{App, router, token_hash};
use inori_storage_sqlite::SqliteRepository;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const OWNER: &str = "test-owner-token-placeholder";
const PUBLIC: &str = "test-public-token-placeholder";
async fn setup() -> (tempfile::TempDir, Router, Arc<App>) {
    let temp = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        SqliteRepository::open(&temp.path().join("db.sqlite"), Character::default())
            .await
            .unwrap(),
    );
    repo.register_device(
        Device {
            id: "owner".into(),
            name: "test".into(),
            audience: Audience::Private,
            admin: true,
            revoked: false,
        },
        &token_hash(OWNER),
    )
    .await
    .unwrap();
    repo.register_device(
        Device {
            id: "public".into(),
            name: "public".into(),
            audience: Audience::Public,
            admin: false,
            revoked: false,
        },
        &token_hash(PUBLIC),
    )
    .await
    .unwrap();
    let service = inori_server::legacy_service(
        repo,
        Arc::new(SystemClock),
        vec![Arc::new(HttpProvider::new(Default::default()).unwrap())],
        Default::default(),
        Default::default(),
    )
    .await
    .unwrap();
    let app = Arc::new(App {
        service,
        extensions: BTreeMap::new(),
        data_dir: temp.path().into(),
        shutdown: CancellationToken::new(),
    });
    (temp, router(app.clone()), app)
}
fn request(method: &str, path: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
async fn value(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap()
}

#[tokio::test]
async fn device_issuance_is_admin_only_and_returns_a_distinct_revocable_credential() {
    let (_temp, routes, app) = setup().await;
    let body = json!({"name":"synthetic client", "audience":"private", "admin":false});
    let denied = routes
        .clone()
        .oneshot(request("POST", "/v1/devices", PUBLIC, body.clone()))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let response = routes
        .clone()
        .oneshot(request("POST", "/v1/devices", OWNER, body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let issued: inori_protocol::CreateDeviceResponse =
        serde_json::from_value(value(response).await).unwrap();
    assert!(!format!("{issued:?}").contains(&issued.token));
    let device = app
        .service
        .repo
        .device_by_hash(&token_hash(&issued.token))
        .await
        .unwrap()
        .unwrap();
    assert!(!device.admin);
    let denied = routes
        .clone()
        .oneshot(request(
            "POST",
            "/v1/devices",
            &issued.token,
            json!({"name":"escalation", "audience":"private", "admin":true}),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    app.service
        .repo
        .revoke_device(&issued.device_id)
        .await
        .unwrap();
    let denied = routes
        .oneshot(request(
            "GET",
            "/v1/capabilities",
            &issued.token,
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn snapshot_matches_the_canonical_schema_and_generated_client_type() {
    let (_temp, routes, app) = setup().await;
    app.service
        .repo
        .assert_memory("架空の検証データ", Audience::Private, 0, 1)
        .await
        .unwrap();
    let body = value(
        routes
            .oneshot(request("GET", "/v1/snapshot", OWNER, Value::Null))
            .await
            .unwrap(),
    )
    .await;
    let mut schema: Value =
        serde_json::from_str(include_str!("../../../contracts/v1.json")).unwrap();
    schema["$ref"] = json!("#/$defs/Snapshot");
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(
        validator.is_valid(&body),
        "{:?}",
        validator
            .iter_errors(&body)
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    );
    let wire: inori_protocol::Snapshot = serde_json::from_value(body).unwrap();
    assert_eq!(wire.memories.len(), 1);
}

#[tokio::test]
async fn authentication_revisions_public_scope_and_revocation() {
    let (_temp, routes, app) = setup().await;
    assert_eq!(
        routes
            .clone()
            .oneshot(request(
                "GET",
                "/v1/snapshot",
                "wrong-token-placeholder",
                Value::Null
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        routes
            .clone()
            .oneshot(request(
                "POST",
                "/v1/memories",
                OWNER,
                json!({"text":"private-secret","audience":"private","expected_revision":0})
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        routes
            .clone()
            .oneshot(request(
                "POST",
                "/v1/memories",
                OWNER,
                json!({"text":"stale","audience":"private","expected_revision":0})
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    app.service
        .repo
        .update_persona("Inori", "private-persona-secret", 1, 10)
        .await
        .unwrap();
    let body = value(
        routes
            .clone()
            .oneshot(request("GET", "/v1/snapshot", PUBLIC, Value::Null))
            .await
            .unwrap(),
    )
    .await
    .to_string();
    assert!(!body.contains("private-secret"));
    assert!(!body.contains("private-persona-secret"));
    assert_eq!(routes.clone().oneshot(request("PUT","/v1/grants",PUBLIC,json!({"extension":"x","capability":"x","action":"invoke","target":"server","allowed":true}))).await.unwrap().status(),StatusCode::FORBIDDEN);
    app.service.repo.revoke_device("public").await.unwrap();
    assert_eq!(
        routes
            .oneshot(request("GET", "/v1/snapshot", PUBLIC, Value::Null))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn memory_lifecycle_and_model_switch_preserve_owner_assertions() {
    let (_temp, routes, app) = setup().await;
    let created = value(
        routes
            .clone()
            .oneshot(request(
                "POST",
                "/v1/memories",
                OWNER,
                json!({"text":"紅茶が好き","audience":"private","expected_revision":0}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let old = created["id"].as_str().unwrap();
    let edited = value(
        routes
            .clone()
            .oneshot(request(
                "PUT",
                &format!("/v1/memories/{old}"),
                OWNER,
                json!({"text":"珈琲が好き","expected_revision":1}),
            ))
            .await
            .unwrap(),
    )
    .await;
    let current = edited["id"].as_str().unwrap();
    assert_eq!(edited["kind"], "confirmed");
    assert_eq!(
        routes
            .clone()
            .oneshot(request(
                "PUT",
                "/v1/providers/active",
                OWNER,
                json!({"provider":"mock","expected_revision":2})
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.service
            .repo
            .memories(Audience::Private, false, 100, 10)
            .await
            .unwrap()[0]
            .text,
        "珈琲が好き"
    );
    assert_eq!(
        routes
            .clone()
            .oneshot(request(
                "DELETE",
                &format!("/v1/memories/{current}"),
                OWNER,
                json!({"expected_revision":3})
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert!(
        app.service
            .repo
            .memories(Audience::Private, false, 100, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        app.service
            .repo
            .source(created["source_ids"][0].as_str().unwrap())
            .await
            .unwrap()
            .unwrap()
            .suppressed
    );
}
#[tokio::test]
async fn mock_chat_streams_and_is_durable() {
    let (_temp, routes, app) = setup().await;
    let mut events = app.service.subscribe();
    let response=routes.clone().oneshot(request("POST","/v1/chat",OWNER,json!({"text":"こんにちは","audience":"private","mode":"auto","session_id":"session","speak":false}))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let accepted = value(response).await;
    let mut deltas = 0;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.kind == "text.delta" {
                deltas += 1;
            }
            if event.kind == "turn.error" {
                panic!("{}", event.payload);
            }
            if event.kind == "turn.completed" {
                assert_eq!(
                    event.generation_id.as_deref(),
                    accepted["generation_id"].as_str()
                );
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(deltas > 1);
    let records = app
        .service
        .repo
        .sources(Audience::Private, 10)
        .await
        .unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].actor, Actor::User);
    assert_eq!(records[1].actor, Actor::Assistant);
    let snapshot = routes
        .oneshot(request("GET", "/v1/snapshot", OWNER, Value::Null))
        .await
        .unwrap();
    assert_eq!(snapshot.status(), StatusCode::OK);
    let snapshot = value(snapshot).await;
    assert_eq!(snapshot["sources"][0]["actor"], "user");
    assert_eq!(snapshot["sources"][1]["actor"], "assistant");
    app.service.stop().await;
}

struct FakeClock(AtomicI64);
impl Clock for FakeClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}
#[tokio::test]
async fn autonomy_respects_presence_cooldown_and_daily_budget_without_real_waits() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        SqliteRepository::open(&temp.path().join("db.sqlite"), Character::default())
            .await
            .unwrap(),
    );
    let clock = Arc::new(FakeClock(AtomicI64::new(1_700_000_000)));
    let provider = Arc::new(HttpProvider::new(Default::default()).unwrap());
    let policy = AutonomyPolicy {
        daily_inference_limit: 2,
        ..Default::default()
    };
    let service = inori_server::legacy_service(
        repo,
        clock.clone(),
        vec![provider.clone()],
        policy,
        Default::default(),
    )
    .await
    .unwrap();
    service.set_presence(true);
    assert!(!service.autonomy_tick(provider.clone()).await.unwrap());
    service.set_presence(false);
    assert!(service.autonomy_tick(provider.clone()).await.unwrap());
    assert!(!service.autonomy_tick(provider.clone()).await.unwrap());
    clock.0.fetch_add(1800, Ordering::Relaxed);
    assert!(service.autonomy_tick(provider.clone()).await.unwrap());
    clock.0.fetch_add(1800, Ordering::Relaxed);
    assert!(!service.autonomy_tick(provider).await.unwrap());
    service.stop().await;
}

struct CaptureProvider {
    profile: ProviderProfile,
    seen: Arc<Mutex<Vec<GenerationContext>>>,
}
#[async_trait::async_trait]
impl InferenceProvider for CaptureProvider {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }
    async fn generate(
        &self,
        context: GenerationContext,
        out: mpsc::Sender<String>,
        _: CancellationToken,
    ) -> anyhow::Result<()> {
        self.seen.lock().unwrap().push(context);
        out.send("公開の返答です。".into()).await?;
        Ok(())
    }
}
#[tokio::test]
async fn public_model_context_does_not_receive_private_persona_or_memory() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        SqliteRepository::open(&temp.path().join("db.sqlite"), Character::default())
            .await
            .unwrap(),
    );
    repo.update_persona("Inori", "private-persona-secret", 0, 1)
        .await
        .unwrap();
    repo.assert_memory("private-memory-secret", Audience::Private, 1, 2)
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(CaptureProvider {
        profile: Default::default(),
        seen: seen.clone(),
    });
    let service = inori_server::legacy_service(
        repo,
        Arc::new(SystemClock),
        vec![provider],
        Default::default(),
        Default::default(),
    )
    .await
    .unwrap();
    let mut events = service.subscribe();
    service
        .start_chat(
            inori_protocol::ChatRequest {
                text: "こんにちは".into(),
                audience: inori_protocol::Audience::Public,
                mode: inori_protocol::DialogueMode::Auto,
                session_id: "public".into(),
                speak: false,
            },
            "public",
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while events.recv().await.unwrap().kind != "turn.completed" {}
    })
    .await
    .unwrap();
    {
        let contexts = seen.lock().unwrap();
        assert!(!contexts[0].system.contains("secret"));
        assert!(
            contexts[0]
                .messages
                .iter()
                .all(|(_, s)| !s.contains("secret"))
        );
    }
    service.stop().await;
}
