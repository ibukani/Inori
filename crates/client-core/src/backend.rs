use crate::{
    ApiClient, ClientConfig,
    audio::{PlaybackController, VoiceCapture, VoiceCaptureEvent},
};
use anyhow::{Context, Result, bail};
use inori_domain::Snapshot;
use inori_protocol::{
    Audience, ChatAccepted, ChatRequest, DialogueMode, EventEnvelope, ServerCapabilities,
};
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug)]
pub enum UiEvent {
    Connected,
    Disconnected,
    Availability(ServerCapabilities),
    Snapshot(Snapshot),
    Event(EventEnvelope),
    Error(String),
    Providers(Value),
    Capabilities { extensions: Value, grants: Value },
    Review(Value),
    Transcript(String),
    Microphone(bool),
    Listening(bool),
    SpeechOutput(bool),
    Speaking(bool),
}
#[derive(Debug)]
pub enum ClientCommand {
    Chat {
        text: String,
        mode: DialogueMode,
        speak: bool,
    },
    Interrupt,
    Refresh,
    SavePersona {
        name: String,
        persona: String,
        revision: u64,
    },
    AddMemory {
        text: String,
        revision: u64,
    },
    EditMemory {
        id: String,
        text: String,
        revision: u64,
    },
    DeleteMemory {
        id: String,
        revision: u64,
    },
    RestoreMemory {
        id: String,
        version: String,
        revision: u64,
    },
    SwitchProvider {
        id: String,
        revision: u64,
    },
    Review {
        provider: String,
    },
    SetGrant {
        extension: String,
        capability: String,
        action: String,
        target: String,
        allowed: bool,
    },
    Invoke {
        extension: String,
        capability: String,
        arguments: Value,
    },
    Presence {
        present: bool,
    },
    SetMicrophone(bool),
    SetSpeechOutput(bool),
}
pub struct BackendControl {
    pub commands: mpsc::Sender<ClientCommand>,
    pub playback: PlaybackController,
    shutdown: CancellationToken,
}
impl BackendControl {
    pub fn send(&self, command: ClientCommand) -> Result<()> {
        self.commands
            .try_send(command)
            .context("client command queue is full or offline")
    }
    pub fn interrupt(&self) -> Result<()> {
        self.playback.stop();
        self.send(ClientCommand::Interrupt)
    }
}
impl Drop for BackendControl {
    fn drop(&mut self) {
        self.playback.stop();
        self.shutdown.cancel();
    }
}

pub fn spawn_backend(config: ClientConfig) -> (BackendControl, mpsc::Receiver<UiEvent>) {
    let (commands, rx) = mpsc::channel(32);
    let (events, output) = mpsc::channel(128);
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    let playback = PlaybackController::default();
    let audio = playback.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build();
        match runtime {
            Ok(runtime) => runtime.block_on(async move {
                if let Err(error) = run(config, rx, events.clone(), stop, audio).await {
                    let _ = events.send(UiEvent::Error(error.to_string())).await;
                }
            }),
            Err(error) => {
                let _ = events.blocking_send(UiEvent::Error(error.to_string()));
            }
        }
    });
    (
        BackendControl {
            commands,
            playback,
            shutdown,
        },
        output,
    )
}
async fn refresh(client: &ApiClient, ui: &mpsc::Sender<UiEvent>) -> Result<()> {
    ui.send(UiEvent::Snapshot(client.snapshot().await?)).await?;
    Ok(())
}
async fn capabilities(client: &ApiClient, ui: &mpsc::Sender<UiEvent>) -> Result<()> {
    let extensions = client
        .call::<Value>(Method::GET, "/v1/extensions", None)
        .await?;
    let grants = client
        .call::<Value>(Method::GET, "/v1/grants", None)
        .await?;
    ui.send(UiEvent::Capabilities { extensions, grants })
        .await?;
    Ok(())
}

async fn run(
    config: ClientConfig,
    mut commands: mpsc::Receiver<ClientCommand>,
    ui: mpsc::Sender<UiEvent>,
    stop: CancellationToken,
    playback: PlaybackController,
) -> Result<()> {
    let client = Arc::new(ApiClient::new(&config)?);
    client.prefer_lan_if_available().await;
    let session = Uuid::new_v4().to_string();
    let active = Arc::new(Mutex::new(None::<String>));
    let (stream_tx, mut stream_rx) = mpsc::channel(64);
    let (audio_tx, mut audio_rx) = mpsc::channel::<(String, u64, String, u64)>(2);
    let (voice_tx, mut voice_rx) = mpsc::channel::<VoiceCaptureEvent>(8);
    let speech_enabled = Arc::new(AtomicBool::new(false));
    let reconnect_client = client.clone();
    let reconnect_stop = stop.clone();
    let reconnect_ui = ui.clone();
    let reconnect_session = session.clone();
    let reconnect_playback = playback.clone();
    tokio::spawn(async move {
        loop {
            if reconnect_stop.is_cancelled() {
                break;
            }
            let result = reconnect_client
                .listen(
                    &reconnect_session,
                    stream_tx.clone(),
                    reconnect_stop.child_token(),
                )
                .await;
            if reconnect_stop.is_cancelled() {
                break;
            }
            reconnect_playback.stop();
            let _ = stream_tx
                .send(EventEnvelope {
                    protocol_version: "1.0".into(),
                    id: "disconnected".into(),
                    kind: "client.disconnected".into(),
                    session_id: None,
                    generation_id: None,
                    audience: Audience::Private,
                    payload: json!({}),
                })
                .await;
            if let Err(error) = result {
                let _ = reconnect_ui
                    .send(UiEvent::Error(format!("接続が切れました: {error}")))
                    .await;
            }
            tokio::select! {_=reconnect_stop.cancelled()=>break,_=tokio::time::sleep(std::time::Duration::from_secs(2))=>{}}
        }
    });
    let audio_client = client.clone();
    let audio_ui = ui.clone();
    let audio_controller = playback.clone();
    let audio_stop = stop.clone();
    let audio_speech_enabled = speech_enabled.clone();
    tokio::spawn(async move {
        while let Some((generation, segment, resource, epoch)) =
            tokio::select! {_=audio_stop.cancelled()=>None,item=audio_rx.recv()=>item}
        {
            if epoch != audio_controller.epoch() || !audio_speech_enabled.load(Ordering::Acquire) {
                continue;
            }
            let result = async {
                let bytes = audio_client.bytes(&resource).await?;
                if epoch != audio_controller.epoch()
                    || !audio_speech_enabled.load(Ordering::Acquire)
                {
                    return Ok(());
                }
                let _ = audio_ui.send(UiEvent::Speaking(true)).await;
                let p = audio_controller.clone();
                let played = tokio::task::spawn_blocking(move || p.play(bytes, epoch)).await??;
                if played {
                    audio_client
                        .call(
                            Method::POST,
                            "/v1/playback",
                            Some(&json!({"generation_id":generation,"segment_id":segment})),
                        )
                        .await?;
                    refresh(&audio_client, &audio_ui).await?;
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            let _ = audio_ui.send(UiEvent::Speaking(false)).await;
            if let Err(error) = result {
                let _ = audio_ui
                    .send(UiEvent::Error(format!("音声再生: {error}")))
                    .await;
            }
        }
    });
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    let slots = Arc::new(Semaphore::new(4));
    let mut capture: Option<VoiceCapture> = None;
    let mut microphone_epoch = 0_u64;
    let mut available: Option<ServerCapabilities> = None;
    let mut present = true;
    loop {
        tokio::select! {
            _=stop.cancelled()=>break,
            _=heartbeat.tick()=>{let c=client.clone();tokio::spawn(async move{let _=c.call(Method::PUT,"/v1/presence",Some(&json!({"present":present}))).await;});},
            event=stream_rx.recv()=>{
                let Some(event)=event else{break};
                let current_generation=event.generation_id==*active.lock().await;
                match event.kind.as_str(){
                    "client.connected"=>{
                        ui.send(UiEvent::Connected).await?;
                        match tokio::time::timeout(std::time::Duration::from_secs(3),client.availability()).await {
                            Ok(Ok(value))=>{
                                let (microphone_off, speech_off)=reconcile_voice_capabilities(
                                    &value, &mut capture, &mut microphone_epoch, &playback, &speech_enabled,
                                );
                                if microphone_off { ui.send(UiEvent::Microphone(false)).await?; }
                                if speech_off { ui.send(UiEvent::SpeechOutput(false)).await?; }
                                ui.send(UiEvent::Availability(value.clone())).await?;
                                available=Some(value);
                            },
                            _=>{
                                available=None;
                                microphone_epoch+=1;
                                drop(capture.take());
                                playback.stop();
                                speech_enabled.store(false,Ordering::Release);
                                ui.send(UiEvent::Microphone(false)).await?;
                                ui.send(UiEvent::SpeechOutput(false)).await?;
                                ui.send(UiEvent::Error("サーバーの利用可能な機能を確認できませんでした。".into())).await?;
                            }
                        };
                        let c=client.clone();let u=ui.clone();tokio::spawn(async move{let _=refresh(&c,&u).await;});
                    },
                    "server.hello"=>{
                        let value:ServerCapabilities=serde_json::from_value(event.payload.clone())?;
                        let (microphone_off, speech_off)=reconcile_voice_capabilities(
                            &value, &mut capture, &mut microphone_epoch, &playback, &speech_enabled,
                        );
                        if microphone_off { ui.send(UiEvent::Microphone(false)).await?; }
                        if speech_off { ui.send(UiEvent::SpeechOutput(false)).await?; }
                        ui.send(UiEvent::Availability(value.clone())).await?;
                        available=Some(value);
                    },
                    "client.disconnected"=>{available=None;microphone_epoch+=1;drop(capture.take());playback.stop();speech_enabled.store(false,Ordering::Release);*active.lock().await=None;ui.send(UiEvent::Microphone(false)).await?;ui.send(UiEvent::SpeechOutput(false)).await?;ui.send(UiEvent::Speaking(false)).await?;ui.send(UiEvent::Disconnected).await?;},
                    "turn.started"=>{playback.stop();*active.lock().await=event.generation_id.clone();},
                    "speech.segment" if current_generation=>{
                        if let (Some(g),Some(index),Some(id))=(event.generation_id.clone(),event.payload.get("segment_id").and_then(Value::as_u64),event.payload.pointer("/resource/id").and_then(Value::as_str))
                            && audio_tx.try_send((g.clone(),index,id.into(),playback.epoch())).is_err(){playback.stop();*active.lock().await=None;let c=client.clone();tokio::spawn(async move{let _=c.call(Method::POST,&format!("/v1/generations/{g}/cancel"),Some(&json!({}))).await;});ui.send(UiEvent::Error("音声の待ち行列が上限に達したため、返答を停止しました。".into())).await?;}
                    },
                    "turn.cancelled"|"turn.error" if current_generation=>{playback.stop();*active.lock().await=None;},
                    "turn.completed"|"memory.updated"|"activity.recorded"=>{let c=client.clone();let u=ui.clone();tokio::spawn(async move{let _=refresh(&c,&u).await;});},_=>{}
                }
                ui.send(UiEvent::Event(event)).await?;
            },
            voice=voice_rx.recv()=>{
                let Some(voice)=voice else{continue};
                match voice {
                    VoiceCaptureEvent::SpeechStarted{epoch} if epoch==microphone_epoch=>{
                        ui.send(UiEvent::Listening(true)).await?;
                        playback.stop();
                        if let Some(id)=active.lock().await.take(){let c=client.clone();tokio::spawn(async move{let _=c.call(Method::POST,&format!("/v1/generations/{id}/cancel"),Some(&json!({}))).await;});}
                    }
                    VoiceCaptureEvent::Segment{epoch,wav} if epoch==microphone_epoch && capture.is_some()=>{
                        ui.send(UiEvent::Listening(false)).await?;
                        let Ok(permit)=slots.clone().try_acquire_owned()else{let _=ui.send(UiEvent::Error("音声処理が混雑しています。しばらく待ってください。".into())).await;continue;};
                        let c=client.clone();let u=ui.clone();let s=session.clone();let a=active.clone();let speech=speech_enabled.clone();
                        tokio::spawn(async move{let _permit=permit;let result=async{let id=c.upload_wav(wav).await?;let value=c.call::<Value>(Method::POST,&format!("/v1/resources/{id}/transcribe"),Some(&json!({}))).await?;let text=value.get("text").and_then(Value::as_str).context("transcription missing")?.to_owned();u.send(UiEvent::Transcript(text.clone())).await?;let speak=speech.load(Ordering::Acquire);send_chat(&c,&s,&a,text,DialogueMode::Auto,speak).await?;Ok::<_,anyhow::Error>(())}.await;if let Err(error)=result{let _=u.send(UiEvent::Error(error.to_string())).await;}});
                    }
                    VoiceCaptureEvent::Error{epoch,message} if epoch==microphone_epoch=>{let _=ui.send(UiEvent::Error(message)).await;}
                    VoiceCaptureEvent::Stopped{epoch} if epoch==microphone_epoch=>{
                        microphone_epoch+=1;
                        drop(capture.take());
                        ui.send(UiEvent::Microphone(false)).await?;
                    }
                    _=>{}
                }
            },
            command=commands.recv()=>{
                let Some(command)=command else{break};
                if let Err(error)=check_available(&command,available.as_ref()){
                    ui.send(UiEvent::Error(error.to_string())).await?;continue;
                }
                if matches!(command,ClientCommand::Interrupt){playback.stop();if let Some(id)=active.lock().await.take(){let c=client.clone();tokio::spawn(async move{let _=c.call(Method::POST,&format!("/v1/generations/{id}/cancel"),Some(&json!({}))).await;});}}
                match command{
                    ClientCommand::Interrupt=>{*active.lock().await=None;},
                    ClientCommand::SetSpeechOutput(value)=>{speech_enabled.store(value,Ordering::Release);if !value{playback.stop();}ui.send(UiEvent::SpeechOutput(value)).await?;},
                    ClientCommand::SetMicrophone(false)=>{microphone_epoch+=1;if let Some(old)=capture.take(){old.stop();}ui.send(UiEvent::Microphone(false)).await?;},
                    ClientCommand::SetMicrophone(true)=>{if capture.is_some(){continue;}microphone_epoch+=1;match VoiceCapture::start(playback.clone(),voice_tx.clone(),microphone_epoch){Ok(value)=>{capture=Some(value);ui.send(UiEvent::Microphone(true)).await?;},Err(error)=>{ui.send(UiEvent::Microphone(false)).await?;ui.send(UiEvent::Error(error.to_string())).await?;}}},
                    ClientCommand::Presence{present:value}=>{present=value;let c=client.clone();tokio::spawn(async move{let _=c.call(Method::PUT,"/v1/presence",Some(&json!({"present":value}))).await;});},
                    command=>{
                        let Ok(permit)=slots.clone().try_acquire_owned()else{ui.send(UiEvent::Error("処理中です。少し待ってから再試行してください。".into())).await?;continue;};
                        let c=client.clone();let u=ui.clone();let s=session.clone();let a=active.clone();let p=playback.clone();
                        tokio::spawn(async move{let _permit=permit;let result=handle(command,&c,&u,&s,&a,&p).await;if let Err(error)=result{let _=u.send(UiEvent::Error(error.to_string())).await;}});
                    }
                }
            }
        }
    }
    drop(capture);
    playback.stop();
    if let Some(id) = active.lock().await.clone() {
        let _ = client
            .call(
                Method::POST,
                &format!("/v1/generations/{id}/cancel"),
                Some(&json!({})),
            )
            .await;
    }
    let _ = client
        .call(Method::PUT, "/v1/presence", Some(&json!({"present":false})))
        .await;
    Ok(())
}
fn check_available(command: &ClientCommand, available: Option<&ServerCapabilities>) -> Result<()> {
    let (text, record, speech, actions) = match command {
        ClientCommand::Chat { speak, .. } => (true, false, *speak, false),
        ClientCommand::SetMicrophone(true) => (true, true, false, false),
        ClientCommand::SetSpeechOutput(true) => (false, false, true, false),
        ClientCommand::Invoke { .. } => (false, false, false, true),
        _ => return Ok(()),
    };
    let cap = available.context("サーバーの機能を確認するまでお待ちください。")?;
    if text && !cap.text_chat {
        bail!(
            "{}",
            cap.reason
                .as_deref()
                .unwrap_or("対話機能を利用できません。")
        );
    }
    if record && (!cap.transcription) {
        bail!("音声認識を利用できません。サーバーの構成を確認してください。");
    }
    if speech && !cap.speech {
        bail!("音声合成を利用できません。サーバーの構成を確認してください。");
    }
    if actions && !cap.actions {
        bail!("拡張操作を利用できません。");
    }
    Ok(())
}

fn reconcile_voice_capabilities(
    capabilities: &ServerCapabilities,
    capture: &mut Option<VoiceCapture>,
    microphone_epoch: &mut u64,
    playback: &PlaybackController,
    speech_enabled: &AtomicBool,
) -> (bool, bool) {
    let microphone_off = !capabilities.text_chat || !capabilities.transcription;
    if microphone_off {
        *microphone_epoch += 1;
        drop(capture.take());
    }
    let speech_off = !capabilities.speech;
    if speech_off {
        speech_enabled.store(false, Ordering::Release);
        playback.stop();
    }
    (microphone_off, speech_off)
}
async fn send_chat(
    client: &ApiClient,
    session: &str,
    active: &Mutex<Option<String>>,
    text: String,
    mode: DialogueMode,
    speak: bool,
) -> Result<()> {
    let r = ChatRequest {
        text,
        audience: Audience::Private,
        mode,
        session_id: session.into(),
        speak,
    };
    let accepted: ChatAccepted =
        serde_json::from_value(client.call(Method::POST, "/v1/chat", Some(&r)).await?)?;
    *active.lock().await = Some(accepted.generation_id);
    Ok(())
}
async fn handle(
    command: ClientCommand,
    client: &ApiClient,
    ui: &mpsc::Sender<UiEvent>,
    session: &str,
    active: &Mutex<Option<String>>,
    playback: &PlaybackController,
) -> Result<()> {
    match command {
        ClientCommand::Chat { text, mode, speak } => {
            playback.stop();
            send_chat(client, session, active, text, mode, speak).await?;
        }
        ClientCommand::Refresh => {}
        ClientCommand::SavePersona {
            name,
            persona,
            revision,
        } => {
            client
                .call(
                    Method::PUT,
                    "/v1/character",
                    Some(&json!({"name":name,"persona":persona,"expected_revision":revision})),
                )
                .await?;
        }
        ClientCommand::AddMemory { text, revision } => {
            client
                .call(
                    Method::POST,
                    "/v1/memories",
                    Some(&json!({"text":text,"audience":"private","expected_revision":revision})),
                )
                .await?;
        }
        ClientCommand::EditMemory { id, text, revision } => {
            client
                .call(
                    Method::PUT,
                    &format!("/v1/memories/{id}"),
                    Some(&json!({"text":text,"expected_revision":revision})),
                )
                .await?;
        }
        ClientCommand::DeleteMemory { id, revision } => {
            client
                .call(
                    Method::DELETE,
                    &format!("/v1/memories/{id}"),
                    Some(&json!({"expected_revision":revision})),
                )
                .await?;
        }
        ClientCommand::RestoreMemory {
            id,
            version,
            revision,
        } => {
            client
                .call(
                    Method::POST,
                    &format!("/v1/memories/{id}/restore"),
                    Some(&json!({"version_id":version,"expected_revision":revision})),
                )
                .await?;
        }
        ClientCommand::SwitchProvider { id, revision } => {
            client
                .call(
                    Method::PUT,
                    "/v1/providers/active",
                    Some(&json!({"provider":id,"expected_revision":revision})),
                )
                .await?;
        }
        ClientCommand::Review { provider } => {
            let review = client
                .call(
                    Method::POST,
                    "/v1/memories/review",
                    Some(&json!({"provider":provider,"audience":"private"})),
                )
                .await?;
            ui.send(UiEvent::Review(review)).await?;
        }
        ClientCommand::SetGrant {
            extension,
            capability,
            action,
            target,
            allowed,
        } => {
            client.call(Method::PUT,"/v1/grants",Some(&json!({"extension":extension,"capability":capability,"action":action,"target":target,"allowed":allowed}))).await?;
            capabilities(client, ui).await?;
        }
        ClientCommand::Invoke {
            extension,
            capability,
            arguments,
        } => {
            let result=client.call(Method::POST,"/v1/operations",Some(&json!({"id":Uuid::new_v4().to_string(),"extension":extension,"capability":capability,"action":"invoke","target":"server","arguments":arguments}))).await?;
            ui.send(UiEvent::Review(result)).await?;
        }
        _ => bail!("unsupported command in request worker"),
    }
    refresh(client, ui).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_or_unknown_capabilities_block_chat_and_voice_but_not_management() {
        assert!(check_available(&ClientCommand::SetMicrophone(true), None).is_err());
        assert!(check_available(&ClientCommand::Refresh, None).is_ok());
        let mut capabilities = ServerCapabilities {
            protocol_major: 1,
            text_chat: false,
            transcription: true,
            speech: true,
            presentation: true,
            actions: false,
            reason: Some("dialogue disabled".into()),
        };
        assert!(check_available(&ClientCommand::SetMicrophone(true), Some(&capabilities)).is_err());
        capabilities.text_chat = true;
        assert!(check_available(&ClientCommand::SetMicrophone(true), Some(&capabilities)).is_ok());
        capabilities.transcription = false;
        assert!(check_available(&ClientCommand::SetMicrophone(true), Some(&capabilities)).is_err());
        capabilities.speech = false;
        assert!(
            check_available(&ClientCommand::SetSpeechOutput(true), Some(&capabilities)).is_err()
        );
        assert!(
            check_available(
                &ClientCommand::Chat {
                    text: "test".into(),
                    mode: DialogueMode::Auto,
                    speak: true
                },
                Some(&capabilities)
            )
            .is_err()
        );
        assert!(
            check_available(
                &ClientCommand::Chat {
                    text: "test".into(),
                    mode: DialogueMode::Auto,
                    speak: false
                },
                Some(&capabilities)
            )
            .is_ok()
        );
    }
}
