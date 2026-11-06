use anyhow::Result;
use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use inori_domain::*;
use inori_module_api::*;
use inori_runtime::{Clock, Repository, Service, SystemClock};
use inori_server::{
    App, ServerConfig, build_service, instantiate, module_catalog, resolve_config, router,
    token_hash,
};
use inori_storage_sqlite::SqliteRepository;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const OWNER: &str = "synthetic-composition-owner";
const PUBLIC: &str = "synthetic-composition-public";
fn config(ids: &[&str]) -> ServerConfig {
    let mut config = ServerConfig::default();
    let catalog = module_catalog();
    let mut graph = inori_composition::legacy(&catalog, false, false);
    graph.enabled.retain(|id| ids.contains(&id.as_str()));
    graph
        .bindings
        .retain(|b| ids.contains(&b.module.as_str()) && ids.contains(&b.provider.as_str()));
    config.composition = Some(graph);
    config
}
async fn repository() -> (tempfile::TempDir, Arc<SqliteRepository>) {
    let dir = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        SqliteRepository::open(&dir.path().join("inori.sqlite"), Character::default())
            .await
            .unwrap(),
    );
    for (id, token, audience, admin) in [
        ("owner", OWNER, Audience::Private, true),
        ("public", PUBLIC, Audience::Public, false),
    ] {
        repo.register_device(
            Device {
                id: id.into(),
                name: id.into(),
                audience,
                admin,
                revoked: false,
            },
            &token_hash(token),
        )
        .await
        .unwrap();
    }
    (dir, repo)
}
fn app(service: Arc<Service>) -> Arc<App> {
    Arc::new(App {
        service,
        extensions: BTreeMap::new(),
        data_dir: std::env::temp_dir(),
        shutdown: CancellationToken::new(),
    })
}
async fn request(
    routes: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = routes
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
fn chat() -> Value {
    json!({"text":"こんにちは","audience":"private","mode":"auto","session_id":"test","speak":false})
}

#[tokio::test]
async fn empty_composition_keeps_management_and_rejects_features_before_side_effects() {
    let (_dir, repo) = repository().await;
    let mut config = config(&[]);
    // Disabled modules must not even inspect missing executables or invalid URLs.
    config.extensions.push("not-installed/manifest.json".into());
    config.providers[0].kind = "not-installed".into();
    config.voice.whisper_url = Some("invalid URL".into());
    let service = build_service(
        &config,
        repo.clone(),
        Arc::new(SystemClock),
        &BTreeMap::new(),
    )
    .await
    .unwrap();
    let routes = router(app(service.clone()));
    assert_eq!(
        request(&routes, "GET", "/v1/capabilities", "", json!({}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, caps) = request(&routes, "GET", "/v1/capabilities", OWNER, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(caps["text_chat"], false);
    assert_eq!(
        request(&routes, "GET", "/v1/modules", PUBLIC, json!({}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&routes, "GET", "/v1/modules", OWNER, json!({}))
            .await
            .0,
        StatusCode::OK
    );
    let (status, error) = request(&routes, "POST", "/v1/chat", OWNER, chat()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error["code"], "module_unavailable");
    assert!(
        repo.sources(Audience::Private, 20)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        request(&routes, "POST", "/v1/chat", PUBLIC, chat()).await.0,
        StatusCode::FORBIDDEN
    );
    let operation = json!({"id":"not-run","extension":"demo","capability":"write","action":"invoke","target":"server","arguments":{}});
    assert_eq!(
        request(&routes, "POST", "/v1/operations", OWNER, operation.clone())
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    repo.set_grant(Grant {
        extension: "demo".into(),
        capability: "write".into(),
        action: "invoke".into(),
        target: "server".into(),
        allowed: true,
    })
    .await
    .unwrap();
    assert_eq!(
        request(&routes, "POST", "/v1/operations", OWNER, operation)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(repo.operations(10).await.unwrap().is_empty());
    assert_eq!(
        request(
            &routes,
            "POST",
            "/v1/memories",
            OWNER,
            json!({"text":"本人の確定情報","audience":"private","expected_revision":0})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(repo.character().await.unwrap().revision, 1);
    assert_eq!(repo.composition_history(10).await.unwrap().len(), 1);
    service.stop().await;
}

struct VirtualClock(AtomicI64);
impl Clock for VirtualClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
#[tokio::test]
async fn no_dialogue_runs_memory_and_autonomy_with_one_shared_budget_and_maintenance() {
    let (_dir, repo) = repository().await;
    let mut config = config(&[
        "perception.basic",
        "memory.standard",
        "autonomy.standard",
        "inference.providers",
    ]);
    config.autonomy.enabled = true;
    config.autonomy.interval_seconds = 60;
    config.autonomy.daily_inference_limit = 2;
    let clock = Arc::new(VirtualClock(AtomicI64::new(100_000)));
    repo.append_source(SourceRecord {
        id: "synthetic-source".into(),
        actor: Actor::User,
        text: "猫が好き".into(),
        created_at: 100_000,
        audience: Audience::Private,
        turn_id: Some("synthetic-turn".into()),
        model: None,
        suppressed: false,
    })
    .await
    .unwrap();
    let service = build_service(&config, repo.clone(), clock.clone(), &BTreeMap::new())
        .await
        .unwrap();
    assert!(!service.capabilities(Audience::Private).text_chat);
    service.start_workers();
    tokio::time::timeout(std::time::Duration::from_secs(7), async {
        loop {
            if repo
                .activities(Audience::Private, 30)
                .await
                .unwrap()
                .iter()
                .any(|a| a.kind == "autonomy")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !repo
            .memories(Audience::Private, false, 100_000, 20)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(repo.count_activities("inference", 86_400).await.unwrap(), 2);
    let expired = repo
        .put_resource("audio/wav", Audience::Private, b"synthetic", 1)
        .await
        .unwrap();
    // Even with an exhausted AI budget, expiry maintenance keeps running.
    clock.0.store(100_200, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            if repo.resource(&expired.id).await.unwrap().is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(repo.count_activities("inference", 86_400).await.unwrap(), 2);
    service.stop().await;
}

#[tokio::test]
async fn disabling_reenabling_and_restart_preserve_personality_memories_activity_and_module_state()
{
    let (dir, repo) = repository().await;
    repo.assert_memory("確認済み", Audience::Private, 0, 1)
        .await
        .unwrap();
    repo.add_activity(Activity {
        id: "plan".into(),
        kind: "autonomy".into(),
        text: "予定".into(),
        created_at: 1,
        audience: Audience::Private,
    })
    .await
    .unwrap();
    let config = config(&["perception.basic"]);
    let one = build_service(
        &config,
        repo.clone(),
        Arc::new(SystemClock),
        &BTreeMap::new(),
    )
    .await
    .unwrap();
    let state = repo
        .module_state("perception.basic")
        .await
        .unwrap()
        .unwrap();
    repo.save_module_state(
        "perception.basic",
        1,
        state.revision,
        json!({"retained":true}),
    )
    .await
    .unwrap();
    assert!(
        repo.save_module_state("perception.basic", 1, state.revision, json!({}))
            .await
            .is_err()
    );
    one.stop().await;
    drop(one);
    let off = build_service(
        &self::config(&[]),
        repo.clone(),
        Arc::new(SystemClock),
        &BTreeMap::new(),
    )
    .await
    .unwrap();
    off.stop().await;
    drop(off);
    let revision = repo.character().await.unwrap().revision;
    repo.close().await;
    drop(repo);
    let repo = Arc::new(
        SqliteRepository::open(&dir.path().join("inori.sqlite"), Character::default())
            .await
            .unwrap(),
    );
    let on = build_service(
        &config,
        repo.clone(),
        Arc::new(SystemClock),
        &BTreeMap::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        repo.module_state("perception.basic")
            .await
            .unwrap()
            .unwrap()
            .data["retained"],
        true
    );
    assert_eq!(repo.character().await.unwrap().revision, revision);
    assert_eq!(
        repo.memories(Audience::Private, false, 1, 10)
            .await
            .unwrap()[0]
            .text,
        "確認済み"
    );
    assert_eq!(
        repo.activities(Audience::Private, 10).await.unwrap()[0].id,
        "plan"
    );
    assert_eq!(repo.composition_history(10).await.unwrap().len(), 3);
    assert!(
        repo.ensure_module_state("perception.basic", 2)
            .await
            .is_err()
    );
    on.stop().await;
}

struct AlternateDialogue;
inori_module_api::module!(AlternateDialogue, "dialogue.alternate");
impl DialoguePolicy for AlternateDialogue {
    fn prepare(&self, input: DialogueInput) -> Result<GenerationContext> {
        Ok(GenerationContext {
            audience: input.audience,
            system: "alternate".into(),
            messages: vec![("user".into(), "交換された方針".into())],
            max_output_tokens: 32,
            purpose: "dialogue".into(),
        })
    }
}
#[tokio::test]
async fn different_implementation_connects_to_same_capability_without_consumer_changes() {
    let (_dir, repo) = repository().await;
    let cfg = config(&[
        "dialogue.standard",
        "inference.providers",
        "channel.standard",
    ]);
    let mut catalog = module_catalog();
    catalog
        .iter_mut()
        .find(|m| m.id == "dialogue.standard")
        .unwrap()
        .id = "dialogue.alternate".into();
    let mut definition = cfg.composition.clone().unwrap();
    for id in &mut definition.enabled {
        if id == "dialogue.standard" {
            *id = "dialogue.alternate".into();
        }
    }
    for b in &mut definition.bindings {
        if b.module == "dialogue.standard" {
            b.module = "dialogue.alternate".into();
        }
        if b.provider == "dialogue.standard" {
            b.provider = "dialogue.alternate".into();
        }
    }
    let graph = inori_composition::resolve(definition, catalog).unwrap();
    let ordinary = resolve_config(&cfg).unwrap();
    let mut modules = instantiate(&cfg, &ordinary, &BTreeMap::new()).unwrap();
    modules.lifecycle.retain(|m| m.id() != "dialogue.standard");
    let alternate = Arc::new(AlternateDialogue);
    modules.dialogue = Some(alternate.clone());
    modules.lifecycle.push(alternate);
    let providers =
        inori_server::build_providers(cfg.providers.clone(), &BTreeMap::new(), repo.clone())
            .unwrap();
    let service = Service::new(
        repo.clone(),
        Arc::new(SystemClock),
        providers,
        cfg.autonomy,
        cfg.voice,
        modules,
        graph,
    )
    .await
    .unwrap();
    let mut events = service.subscribe();
    let routes = router(app(service.clone()));
    assert_eq!(
        request(&routes, "POST", "/v1/chat", OWNER, chat()).await.0,
        StatusCode::OK
    );
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.kind == "turn.completed" {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        repo.sources(Audience::Private, 20)
            .await
            .unwrap()
            .iter()
            .any(|s| s.actor == Actor::Assistant && s.text.contains("交換された方針"))
    );
    service.stop().await;
}

struct LifecycleProbe {
    id: &'static str,
    fail: bool,
    events: Arc<Mutex<Vec<String>>>,
    host: Arc<Mutex<Option<Arc<dyn ModuleHost>>>>,
}
#[async_trait]
impl Module for LifecycleProbe {
    fn id(&self) -> &str {
        self.id
    }
    async fn start(&self, c: ModuleContext) -> Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(format!("start:{}", self.id));
        *self.host.lock().unwrap() = Some(c.host);
        if self.fail {
            anyhow::bail!("synthetic preparation failure");
        }
        Ok(())
    }
    async fn stop(&self) -> Result<Option<Value>> {
        self.events
            .lock()
            .unwrap()
            .push(format!("stop:{}", self.id));
        Ok(Some(json!({"checkpoint":true})))
    }
}
#[tokio::test]
async fn lifecycle_orders_preparation_and_reverse_shutdown_and_rejects_late_state_writes() {
    let (_dir, repo) = repository().await;
    for fail in [false, true] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let host = Arc::new(Mutex::new(None));
        let catalog = vec![
            descriptor("a", "a", &[], no_settings()),
            descriptor("b", "b", &[("a", "a", false)], no_settings()),
        ];
        let graph =
            inori_composition::resolve(inori_composition::legacy(&catalog, false, false), catalog)
                .unwrap();
        let mut modules = Modules::empty();
        for id in ["b", "a"] {
            modules.lifecycle.push(Arc::new(LifecycleProbe {
                id,
                fail: fail && id == "b",
                events: events.clone(),
                host: host.clone(),
            }));
        }
        let result = Service::new(
            repo.clone(),
            Arc::new(SystemClock),
            vec![],
            Default::default(),
            Default::default(),
            modules,
            graph,
        )
        .await;
        if fail {
            assert!(result.is_err());
        } else {
            let service = result.unwrap();
            let h = host.lock().unwrap().clone().unwrap();
            let state = repo.module_state("b").await.unwrap().unwrap();
            h.save_state(state.revision, json!({"valid":true}))
                .await
                .unwrap();
            service.stop().await;
            assert!(
                h.save_state(state.revision + 1, json!({"late":true}))
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            *events.lock().unwrap(),
            vec!["start:a", "start:b", "stop:b", "stop:a"]
        );
    }
}

#[test]
fn validate_config_is_static_and_has_no_data_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("not-created");
    let cfg = dir.path().join("server.toml");
    let mut config = config(&[]);
    config.extensions.push("absent/manifest.json".into());
    std::fs::write(&cfg, toml::to_string(&config).unwrap()).unwrap();
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_inori-server"))
            .arg("--data-dir")
            .arg(&data)
            .arg("--config")
            .arg(&cfg)
            .arg("validate-config")
            .output()
            .unwrap()
    };
    let output = run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!data.exists());
    config
        .composition
        .as_mut()
        .unwrap()
        .enabled
        .push("unknown".into());
    std::fs::write(&cfg, toml::to_string(&config).unwrap()).unwrap();
    assert!(!run().status.success());
    assert!(!data.exists());
}

#[tokio::test]
async fn hello_negotiates_io_without_granting_permissions_and_legacy_clients_still_receive_events()
{
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{Message, client::IntoClientRequest},
    };
    let (_dir, repo) = repository().await;
    let cfg = config(&[
        "dialogue.standard",
        "inference.providers",
        "channel.standard",
    ]);
    let service = build_service(&cfg, repo.clone(), Arc::new(SystemClock), &BTreeMap::new())
        .await
        .unwrap();
    let app = app(service.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(axum::serve(listener, router(app)).into_future());
    let connect = |token: &str, session: &str| {
        let mut request = format!("ws://{address}/v1/events?session_id={session}")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        request
    };
    let (mut modern, _) = connect_async(connect(PUBLIC, "public-session"))
        .await
        .unwrap();
    modern.send(Message::Text(json!({"kind":"client.hello","protocol_major":1,"session_id":"public-session","text":true,"audio_input":true,"audio_output":true,"presentation":true,"audio_mime_types":["audio/wav"]}).to_string().into())).await.unwrap();
    let hello = tokio::time::timeout(std::time::Duration::from_secs(2), modern.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let hello: Value = serde_json::from_str(hello.to_text().unwrap()).unwrap();
    assert_eq!(hello["kind"], "server.hello");
    assert_eq!(hello["payload"]["text_chat"], true);
    assert_eq!(hello["payload"]["speech"], false);
    assert!(repo.grants().await.unwrap().is_empty());
    let routes = router(self::app(service.clone()));
    assert_eq!(
        request(&routes, "POST", "/v1/chat", PUBLIC, chat()).await.0,
        StatusCode::FORBIDDEN
    );
    service.emit(
        "private.test",
        None,
        None,
        Audience::Private,
        json!({"secret":"private"}),
    );
    service.emit(
        "public.test",
        Some("other-session"),
        None,
        Audience::Public,
        json!({}),
    );
    service.emit(
        "public.test",
        Some("public-session"),
        None,
        Audience::Public,
        json!({"marker":"allowed"}),
    );
    let next = tokio::time::timeout(std::time::Duration::from_secs(2), modern.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(next.to_text().unwrap().contains("allowed"));
    assert!(!next.to_text().unwrap().contains("secret"));
    let (mut legacy, _) = connect_async(connect(OWNER, "legacy-session"))
        .await
        .unwrap();
    service.emit(
        "legacy.test",
        Some("legacy-session"),
        None,
        Audience::Private,
        json!({"marker":"legacy"}),
    );
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), legacy.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(event.to_text().unwrap().contains("legacy.test"));
    // A hello claiming a different session is closed, rather than changing ownership.
    modern.send(Message::Text(json!({"kind":"client.hello","protocol_major":1,"session_id":"another-owner","text":true,"audio_input":true,"audio_output":true,"presentation":true,"audio_mime_types":["audio/wav"]}).to_string().into())).await.unwrap();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(2), modern.next())
        .await
        .unwrap();
    assert!(closed.is_none() || matches!(closed, Some(Ok(Message::Close(_)))));
    legacy.close(None).await.unwrap();
    service.stop().await;
    server.abort();
}

struct BlockingProvider {
    profile: ProviderProfile,
    started: std::sync::atomic::AtomicUsize,
    release: tokio::sync::Semaphore,
}
#[async_trait]
impl InferenceProvider for BlockingProvider {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }
    async fn generate(
        &self,
        _: GenerationContext,
        out: tokio::sync::mpsc::Sender<String>,
        cancel: CancellationToken,
    ) -> Result<()> {
        self.started.fetch_add(1, Ordering::SeqCst);
        tokio::select! {_=cancel.cancelled()=>return Ok(()),permit=self.release.acquire()=>permit?.forget()};
        out.send("fixture".into()).await?;
        Ok(())
    }
}
#[tokio::test]
async fn inference_is_shared_bounded_and_cancellable_and_cannot_cross_session_ownership() {
    let (_dir, repo) = repository().await;
    let cfg = config(&[
        "dialogue.standard",
        "memory.standard",
        "inference.providers",
        "channel.standard",
    ]);
    let mut graph = resolve_config(&cfg).unwrap();
    graph
        .catalog
        .iter_mut()
        .find(|m| m.id == "inference.providers")
        .unwrap()
        .max_concurrency = 1;
    let modules = instantiate(&cfg, &graph, &BTreeMap::new()).unwrap();
    let provider = Arc::new(BlockingProvider {
        profile: Default::default(),
        started: std::sync::atomic::AtomicUsize::new(0),
        release: tokio::sync::Semaphore::new(0),
    });
    let service = Service::new(
        repo.clone(),
        Arc::new(SystemClock),
        vec![provider.clone()],
        cfg.autonomy,
        cfg.voice,
        modules,
        graph,
    )
    .await
    .unwrap();
    let review_service = service.clone();
    let review = tokio::spawn(async move {
        review_service
            .review_memory("mock", Audience::Private)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while provider.started.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let busy = service
        .review_memory("mock", Audience::Private)
        .await
        .unwrap_err();
    assert!(matches!(
        busy.downcast_ref::<ModuleError>(),
        Some(ModuleError::Busy(_))
    ));
    let mut events = service.subscribe();
    let req: inori_protocol::ChatRequest = serde_json::from_value(chat()).unwrap();
    let queued = service.start_chat(req.clone(), "owner").await.unwrap();
    assert!(
        service
            .interrupt(&queued.generation_id, "different-device", false)
            .await
            .is_err()
    );
    service
        .interrupt(&queued.generation_id, "owner", false)
        .await
        .unwrap();
    provider.release.add_permits(1);
    review.await.unwrap().unwrap();
    assert_eq!(provider.started.load(Ordering::SeqCst), 1);
    assert!(!repo.generation_active(&queued.generation_id).await.unwrap());
    let next = service.start_chat(req, "owner").await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while provider.started.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    service
        .interrupt(&next.generation_id, "owner", false)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let e = events.recv().await.unwrap();
            if e.generation_id.as_deref() == Some(&next.generation_id) && e.kind == "turn.cancelled"
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        repo.sources(Audience::Private, 20)
            .await
            .unwrap()
            .iter()
            .all(|s| s.actor != Actor::Assistant)
    );
    service.stop().await;
}
