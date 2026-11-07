use anyhow::{Context, Result, bail};
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Path, Query, Request, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use futures_util::{SinkExt, StreamExt};
use inori_domain::*;
pub use inori_extension_host::{RegisteredExtension, load_extensions};
use inori_protocol as wire;
use inori_runtime::{Service, domain_audience};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
mod bootstrap;
pub use bootstrap::{
    build_providers, build_service, build_service_with_credentials, instantiate, legacy_service,
    module_catalog, resolve_config, selected_extensions, validate_character,
};
pub struct App {
    pub service: Arc<Service>,
    pub extensions: BTreeMap<String, RegisteredExtension>,
    pub data_dir: PathBuf,
    pub shutdown: CancellationToken,
    pub internet_configured: bool,
    pub internet_access: tokio::sync::watch::Sender<bool>,
}
pub fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
#[derive(Debug)]
pub struct ApiError(pub anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Some(error) = self.0.downcast_ref::<inori_module_api::ModuleError>() {
            let code = match error {
                inori_module_api::ModuleError::Busy(_) => "module_busy",
                _ => "module_unavailable",
            };
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"code":code,"error":error.to_string()})),
            )
                .into_response();
        }
        let status = match self.0.downcast_ref::<DomainError>() {
            Some(DomainError::StaleRevision | DomainError::DuplicateOperation) => {
                StatusCode::CONFLICT
            }
            Some(DomainError::PermissionDenied | DomainError::PrivateEvidence) => {
                StatusCode::FORBIDDEN
            }
            _ => StatusCode::BAD_REQUEST,
        };
        (status, Json(json!({"error":self.0.to_string()}))).into_response()
    }
}
type ApiResult<T> = std::result::Result<Json<T>, ApiError>;
fn admin(d: &Device) -> Result<()> {
    if !d.admin {
        bail!(DomainError::PermissionDenied);
    }
    Ok(())
}
fn audience(d: &Device, a: Audience) -> Result<()> {
    if !d.audience.permits(a) {
        bail!(DomainError::PermissionDenied);
    }
    Ok(())
}

pub fn router(app: Arc<App>) -> Router {
    router_with(app, true, true)
}

pub fn internet_router(app: Arc<App>) -> Router {
    router_with(app, false, false)
}

fn router_with(app: Arc<App>, local_management: bool, health: bool) -> Router {
    let protected = Router::new()
        .route("/v1/snapshot", get(snapshot))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/modules", get(modules))
        .route("/v1/character", put(persona))
        .route("/v1/providers", get(providers))
        .route("/v1/providers/active", put(switch_provider))
        .route("/v1/chat", post(chat))
        .route("/v1/generations/{id}/cancel", post(cancel))
        .route("/v1/playback", post(playback))
        .route("/v1/events", get(events))
        .route("/v1/presence", put(presence))
        .route("/v1/memories", get(memories).post(create_memory))
        .route("/v1/memories/review", post(review))
        .route(
            "/v1/memories/{id}",
            put(correct_memory).delete(delete_memory),
        )
        .route("/v1/memories/{id}/restore", post(restore_memory))
        .route("/v1/grants", get(grants).put(set_grant))
        .route("/v1/extensions", get(extensions))
        .route("/v1/operations", post(invoke))
        .route("/v1/resources", post(upload))
        .route("/v1/resources/{id}", get(resource))
        .route("/v1/resources/{id}/transcribe", post(transcribe))
        .route("/v1/devices", get(devices).post(create_device))
        .route("/v1/devices/{id}", delete(revoke))
        .route("/v1/backups", post(backup));
    let protected = if local_management {
        protected.route(
            "/v1/network/access",
            get(network_access).put(set_network_access),
        )
    } else {
        protected
    }
    .route_layer(middleware::from_fn_with_state(app.clone(), authenticate));
    let router = Router::new().merge(protected);
    let router = if health {
        router.route(
            "/healthz",
            get(|| async { Json(json!({"status":"ok","protocol":"1.0"})) }),
        )
    } else {
        router
    };
    router
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .layer(tower::limit::ConcurrencyLimitLayer::new(8))
        .with_state(app)
}

async fn network_access(
    State(app): State<Arc<App>>,
    Extension(device): Extension<Device>,
) -> ApiResult<wire::NetworkAccessState> {
    admin(&device)?;
    Ok(Json(wire::NetworkAccessState {
        internet_enabled: *app.internet_access.borrow(),
    }))
}

async fn set_network_access(
    State(app): State<Arc<App>>,
    Extension(device): Extension<Device>,
    Json(request): Json<wire::NetworkAccessUpdate>,
) -> ApiResult<wire::NetworkAccessState> {
    admin(&device)?;
    if request.internet_enabled && !app.internet_configured {
        return Err(anyhow::anyhow!(
            "インターネットURLが未設定です。setupで公開URLを設定してください"
        )
        .into());
    }
    let state = wire::NetworkAccessState {
        internet_enabled: request.internet_enabled,
    };
    let bytes = toml::to_string(&state)?;
    inori_app_support::write_private(
        &app.data_dir.join("network-access.toml"),
        bytes.as_bytes(),
        true,
    )?;
    app.internet_access.send_replace(request.internet_enabled);
    Ok(Json(state))
}
async fn authenticate(State(app): State<Arc<App>>, mut r: Request, next: Next) -> Response {
    let token = r
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(token) = token.filter(|v| v.len() >= 16 && v.len() <= 256) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match app.service.repo.device_by_hash(&token_hash(token)).await {
        Ok(Some(d)) => {
            r.extensions_mut().insert(d);
            next.run(r).await
        }
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}
async fn capabilities(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
) -> ApiResult<wire::ServerCapabilities> {
    let mut capabilities = a.service.capabilities(d.audience);
    capabilities.actions &= d.admin;
    Ok(Json(capabilities))
}
async fn modules(State(a): State<Arc<App>>, Extension(d): Extension<Device>) -> ApiResult<Value> {
    admin(&d)?;
    Ok(Json(a.service.module_report()))
}
async fn snapshot(State(a): State<Arc<App>>, Extension(d): Extension<Device>) -> ApiResult<Value> {
    Ok(Json(serde_json::to_value(
        a.service.snapshot(d.audience).await?,
    )?))
}
async fn providers(State(a): State<Arc<App>>, Extension(d): Extension<Device>) -> ApiResult<Value> {
    admin(&d)?;
    Ok(Json(
        json!({"providers":a.service.provider_profiles(),"voice":a.service.voice_settings()}),
    ))
}
async fn persona(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Json(r): Json<wire::PersonaUpdate>,
) -> ApiResult<Character> {
    admin(&d)?;
    Ok(Json(
        a.service
            .repo
            .update_persona(
                &r.name,
                &r.persona,
                r.expected_revision,
                a.service.clock.now(),
            )
            .await?,
    ))
}
async fn switch_provider(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Json(r): Json<wire::ProviderSwitch>,
) -> ApiResult<Character> {
    admin(&d)?;
    Ok(Json(
        a.service
            .switch_provider(&r.provider, r.expected_revision)
            .await?,
    ))
}
async fn chat(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Json(r): Json<wire::ChatRequest>,
) -> ApiResult<wire::ChatAccepted> {
    audience(&d, domain_audience(r.audience))?;
    a.service.require_channel_port("dialogue")?;
    if r.speak {
        a.service.require_channel_port("synthesis")?;
    }
    let r = a.service.modules.channel.as_ref().unwrap().normalize(r)?;
    Ok(Json(a.service.start_chat(r, &d.id).await?))
}
async fn cancel(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
) -> ApiResult<Value> {
    a.service.interrupt(&id, &d.id, d.admin).await?;
    Ok(Json(
        json!({"cancelled":true,"external_effects_reversed":false}),
    ))
}
async fn playback(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Json(r): Json<wire::PlaybackAck>,
) -> ApiResult<Value> {
    admin(&d)?;
    Ok(Json(
        json!({"recorded":a.service.repo.acknowledge_segment(&r.generation_id,r.segment_id,a.service.clock.now()).await?}),
    ))
}
async fn presence(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Json(r): Json<wire::PresenceUpdate>,
) -> ApiResult<Value> {
    admin(&d)?;
    a.service.set_presence(r.present);
    Ok(Json(json!({"ok":true})))
}
#[derive(Default, Deserialize)]
struct MemoryQuery {
    #[serde(default)]
    history: bool,
}
async fn memories(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Query(q): Query<MemoryQuery>,
) -> ApiResult<Value> {
    if q.history {
        admin(&d)?;
    }
    Ok(Json(
        json!({"memories":a.service.repo.memories(d.audience,q.history,a.service.clock.now(),200).await?}),
    ))
}
async fn create_memory(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Json(r): Json<wire::MemoryCreate>,
) -> ApiResult<MemoryRecord> {
    admin(&d)?;
    Ok(Json(
        a.service
            .repo
            .assert_memory(
                &r.text,
                domain_audience(r.audience),
                r.expected_revision,
                a.service.clock.now(),
            )
            .await?,
    ))
}
async fn correct_memory(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
    Json(r): Json<wire::MemoryEdit>,
) -> ApiResult<MemoryRecord> {
    admin(&d)?;
    Ok(Json(
        a.service
            .repo
            .correct_memory(&id, &r.text, r.expected_revision, a.service.clock.now())
            .await?,
    ))
}
async fn restore_memory(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
    Json(r): Json<wire::MemoryRestore>,
) -> ApiResult<MemoryRecord> {
    admin(&d)?;
    Ok(Json(
        a.service
            .repo
            .restore_memory(
                &id,
                &r.version_id,
                r.expected_revision,
                a.service.clock.now(),
            )
            .await?,
    ))
}
async fn delete_memory(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
    Json(r): Json<wire::RevisionRequest>,
) -> ApiResult<Value> {
    admin(&d)?;
    a.service
        .repo
        .delete_memory(&id, r.expected_revision, a.service.clock.now())
        .await?;
    Ok(Json(json!({"deleted":true})))
}
async fn review(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Json(r): Json<wire::MemoryReviewRequest>,
) -> ApiResult<Value> {
    admin(&d)?;
    Ok(Json(
        a.service
            .review_memory(&r.provider, domain_audience(r.audience))
            .await?,
    ))
}
async fn grants(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
) -> ApiResult<Vec<Grant>> {
    admin(&d)?;
    Ok(Json(a.service.repo.grants().await?))
}
async fn set_grant(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Json(r): Json<wire::PermissionUpdate>,
) -> ApiResult<Value> {
    admin(&d)?;
    a.service
        .repo
        .set_grant(Grant {
            extension: r.extension,
            capability: r.capability,
            action: r.action,
            target: r.target,
            allowed: r.allowed,
        })
        .await?;
    Ok(Json(json!({"ok":true})))
}
async fn extensions(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
) -> ApiResult<Value> {
    admin(&d)?;
    let mut list = Vec::new();
    for e in a.extensions.values() {
        list.push(e.manifest.clone());
    }
    Ok(Json(json!({"extensions":list})))
}
async fn invoke(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Json(r): Json<wire::OperationRequest>,
) -> ApiResult<Value> {
    admin(&d)?;
    check_permission(
        &a.service.repo.grants().await?,
        &r.extension,
        &r.capability,
        &r.action,
        &r.target,
    )?;
    a.service.require_channel_port("actions")?;
    Ok(Json(a.service.execute_operation(r).await?))
}

#[derive(Deserialize)]
struct ResourceQuery {
    #[serde(default)]
    audience: Audience,
}
async fn upload(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Query(q): Query<ResourceQuery>,
    h: HeaderMap,
    bytes: Bytes,
) -> ApiResult<inori_runtime::Resource> {
    audience(&d, q.audience)?;
    a.service.require_standard()?;
    let mime = h
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");
    Ok(Json(
        a.service
            .repo
            .put_resource(mime, q.audience, &bytes, a.service.clock.now())
            .await?,
    ))
}
async fn resource(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    let (r, bytes) = a
        .service
        .repo
        .resource(&id)
        .await?
        .context("resource not found")?;
    audience(&d, r.audience)?;
    Ok((
        [
            (header::CONTENT_TYPE, r.mime),
            (header::CACHE_CONTROL, "no-store".into()),
        ],
        bytes,
    )
        .into_response())
}
async fn transcribe(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
) -> ApiResult<Value> {
    let (resource, _) = a
        .service
        .repo
        .resource(&id)
        .await?
        .context("resource not found")?;
    audience(&d, resource.audience)?;
    a.service.require_channel_port("recognition")?;
    Ok(Json(
        json!({"text":a.service.transcribe(&id,d.audience).await?}),
    ))
}
async fn devices(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
) -> ApiResult<Vec<Device>> {
    admin(&d)?;
    Ok(Json(a.service.repo.devices().await?))
}
pub async fn issue_device(
    repo: &dyn inori_runtime::Repository,
    request: wire::CreateDeviceRequest,
) -> Result<wire::CreateDeviceResponse> {
    if request.name.trim().is_empty()
        || request.name.chars().count() > 128
        || request.name.chars().any(char::is_control)
        || request.admin && matches!(request.audience, wire::Audience::Public)
    {
        bail!("invalid device name or public administrator");
    }
    let device_id = Uuid::new_v4().to_string();
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    repo.register_device(
        Device {
            id: device_id.clone(),
            name: request.name,
            audience: domain_audience(request.audience),
            admin: request.admin,
            revoked: false,
        },
        &token_hash(&token),
    )
    .await?;
    Ok(wire::CreateDeviceResponse { device_id, token })
}
async fn create_device(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Json(r): Json<wire::CreateDeviceRequest>,
) -> ApiResult<wire::CreateDeviceResponse> {
    admin(&d)?;
    Ok(Json(issue_device(a.service.repo.as_ref(), r).await?))
}
async fn revoke(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
) -> ApiResult<Value> {
    admin(&d)?;
    a.service.repo.revoke_device(&id).await?;
    Ok(Json(json!({"revoked":true})))
}
async fn backup(State(a): State<Arc<App>>, Extension(d): Extension<Device>) -> ApiResult<Value> {
    admin(&d)?;
    let directory = a.data_dir.join("backups");
    tokio::fs::create_dir_all(&directory).await?;
    let name = format!("{}.sqlite", Uuid::new_v4());
    a.service.repo.backup(&directory.join(&name)).await?;
    Ok(Json(
        json!({"backup":name,"schema":inori_storage_sqlite::schema_version(),"app_version":env!("CARGO_PKG_VERSION")}),
    ))
}
#[derive(Deserialize)]
struct EventQuery {
    session_id: Option<String>,
}
async fn events(
    State(a): State<Arc<App>>,
    Extension(d): Extension<Device>,
    h: HeaderMap,
    Query(q): Query<EventQuery>,
    upgrade: WebSocketUpgrade,
) -> std::result::Result<Response, ApiError> {
    let hash = token_hash(
        h.get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .context("missing token")?,
    );
    Ok(upgrade
        .max_message_size(65_536)
        .max_frame_size(65_536)
        .on_upgrade(move |socket| event_socket(socket, a, d, hash, q.session_id)))
}
async fn event_socket(
    socket: WebSocket,
    a: Arc<App>,
    d: Device,
    hash: String,
    mut session: Option<String>,
) {
    let (mut send, mut receive) = socket.split();
    let mut events = a.service.subscribe();
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut negotiated: Option<wire::ServerCapabilities> = None;
    loop {
        let mut event = tokio::select! {
            _=a.shutdown.cancelled()=>break,
            _=heartbeat.tick()=>{if a.service.repo.device_by_hash(&hash).await.ok().flatten().is_none(){break;}continue;},
            message=receive.next()=>{
                match message{
                    Some(Ok(Message::Text(text)))=>{
                        let is_hello=serde_json::from_str::<Value>(&text).ok().is_some_and(|v|v["kind"]=="client.hello");
                        if is_hello{
                            let Ok(hello)=serde_json::from_str::<wire::ClientHello>(&text)else{break};
                            if hello.protocol_major!=1 || hello.session_id.is_empty() || hello.session_id.len()>128 || session.as_ref().is_some_and(|s|*s!=hello.session_id){break;}
                            if a.service.repo.device_by_hash(&hash).await.ok().flatten().is_none(){break;}
                            let Ok(mut capabilities)=a.service.negotiate(&hello,d.audience)else{break};
                            capabilities.actions &= d.admin;
                            session=Some(hello.session_id.clone());
                            let event=wire::EventEnvelope{protocol_version:wire::PROTOCOL_VERSION.into(),id:Uuid::new_v4().to_string(),kind:"server.hello".into(),session_id:Some(hello.session_id),generation_id:None,audience:match d.audience{Audience::Private=>wire::Audience::Private,Audience::Public=>wire::Audience::Public},payload:serde_json::to_value(&capabilities).unwrap_or(Value::Null)};
                            let Ok(text)=serde_json::to_string(&event)else{break};
                            if !matches!(tokio::time::timeout(std::time::Duration::from_secs(2),send.send(Message::Text(text.into()))).await,Ok(Ok(()))){break;}
                            negotiated=Some(capabilities);
                        }
                        continue;
                    },Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break,_=>continue,
                }
            },
            event=events.recv()=>match event{Ok(event)=>event,Err(_)=>break}
        };
        if !d.audience.permits(domain_audience(event.audience)) {
            continue;
        }
        if event.session_id.is_some() && session.is_some() && event.session_id != session {
            continue;
        }
        if a.service
            .repo
            .device_by_hash(&hash)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            break;
        }
        if let Some(channel) = &a.service.modules.channel {
            let Some(output) = channel.outgoing(event, negotiated.as_ref()) else {
                continue;
            };
            event = output;
        }
        let Ok(text) = serde_json::to_string(&event) else {
            break;
        };
        if !matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                send.send(Message::Text(text.into()))
            )
            .await,
            Ok(Ok(()))
        ) {
            break;
        }
    }
    let _ = send.close().await;
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub character: Character,
    pub providers: Vec<inori_runtime::ProviderProfile>,
    pub voice: inori_runtime::VoiceSettings,
    pub autonomy: AutonomyPolicy,
    pub extensions: Vec<PathBuf>,
    pub composition: Option<wire::CompositionConfig>,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            character: Character::default(),
            providers: vec![inori_runtime::ProviderProfile::default()],
            voice: Default::default(),
            autonomy: Default::default(),
            extensions: Vec::new(),
            composition: None,
        }
    }
}
