use anyhow::{Context as _, Result, bail};
use gpui::*;
use gpui_component::{
    Disableable, Root,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};
use inori_client_core::{BackendControl, ClientCommand, ClientConfig, UiEvent, spawn_backend};
use inori_domain::{MemoryKind, Snapshot};
use inori_protocol::{DialogueMode, ModelPack, ServerCapabilities};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

const BG: u32 = 0x0e1422;
const PANEL: u32 = 0x172033;
const BORDER: u32 = 0x2b3751;
const TEXT: u32 = 0xebedf5;
const MUTED: u32 = 0x97a7bf;
const ACCENT: u32 = 0xc9b7ff;
actions!(inori, [Quit, StopSpeaking]);
struct View {
    backend: BackendControl,
    connection_epoch: u64,
    config: ClientConfig,
    snapshot: Option<Snapshot>,
    connected: bool,
    available: Option<ServerCapabilities>,
    status: String,
    composer: Entity<InputState>,
    history_scroll: ScrollHandle,
    memory_input: Entity<InputState>,
    name: Entity<InputState>,
    persona: Entity<InputState>,
    url: Entity<InputState>,
    token: Entity<InputState>,
    certificate: Entity<InputState>,
    tab: usize,
    mode: DialogueMode,
    voice: bool,
    recording: bool,
    speaking: bool,
    mouth_open: bool,
    speech_epoch: u64,
    busy: bool,
    live_text: String,
    last_user: String,
    generation: Option<String>,
    providers: Value,
    extensions: Value,
    grants: Value,
    present: bool,
    review: String,
    selected_memory: Option<String>,
    memory_revision: u64,
    persona_revision: u64,
    persona_dirty: bool,
    avatar: Option<PathBuf>,
    _subscriptions: Vec<Subscription>,
}
impl View {
    fn new(config: ClientConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer =
            cx.new(|cx| InputState::new(window, cx).placeholder("話しかけてみてください…"));
        let memory_input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(3)
                .placeholder("覚えておいてほしいこと")
        });
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("キャラクターの名前"));
        let persona = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(5)
                .placeholder("基本となる性格、話し方、価値観")
        });
        let url = cx.new(|cx| InputState::new(window, cx).default_value(config.server_url.clone()));
        let token = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(config.token_file.to_string_lossy().to_string())
        });
        let certificate = cx.new(|cx| {
            InputState::new(window, cx).default_value(
                config
                    .certificate_file
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
            )
        });
        let (backend, receiver) = spawn_backend(config.clone());
        let mut subscriptions =
            vec![
                cx.subscribe_in(&composer, window, |this, _, event, window, cx| {
                    if let InputEvent::PressEnter { secondary: false } = event {
                        this.send_chat(window, cx);
                    }
                }),
            ];
        for input in [&name, &persona] {
            subscriptions.push(cx.subscribe_in(input, window, |this, _, event, _, _| {
                if matches!(event, InputEvent::Change) {
                    this.persona_dirty = true;
                }
            }));
        }
        let avatar = config
            .model_pack
            .as_ref()
            .map(|p| resolve_pack(p, "idle"))
            .transpose();
        let (avatar, status) = match avatar {
            Ok(value) => (value, "接続しています…".into()),
            Err(error) => (None, format!("モデル素材: {error}")),
        };
        attach(receiver, 1, window, cx);
        Self {
            backend,
            connection_epoch: 1,
            config,
            snapshot: None,
            connected: false,
            available: None,
            status,
            composer,
            history_scroll: ScrollHandle::default(),
            memory_input,
            name,
            persona,
            url,
            token,
            certificate,
            tab: 0,
            mode: DialogueMode::Auto,
            voice: false,
            recording: false,
            speaking: false,
            mouth_open: false,
            speech_epoch: 0,
            busy: false,
            live_text: String::new(),
            last_user: String::new(),
            generation: None,
            providers: Value::Null,
            extensions: Value::Null,
            grants: Value::Null,
            present: true,
            review: String::new(),
            selected_memory: None,
            memory_revision: 0,
            persona_revision: 0,
            persona_dirty: false,
            avatar,
            _subscriptions: subscriptions,
        }
    }
    fn revision(&self) -> u64 {
        self.snapshot
            .as_ref()
            .map(|s| s.character.revision)
            .unwrap_or(0)
    }
    fn command(&mut self, command: ClientCommand) {
        if let Err(error) = self.backend.send(command) {
            self.status = error.to_string();
        }
    }
    fn send_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().to_string();
        if text.trim().is_empty()
            || !self.connected
            || !self.available.as_ref().is_some_and(|c| c.text_chat)
        {
            return;
        }
        self.last_user = text.clone();
        self.history_scroll.scroll_to_bottom();
        self.live_text.clear();
        self.busy = true;
        self.backend.playback.stop();
        self.command(ClientCommand::Chat {
            text,
            mode: self.mode,
            speak: self.voice,
        });
        self.composer
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }
    fn stop(&mut self, cx: &mut Context<Self>) {
        let _ = self.backend.interrupt();
        self.speaking = false;
        self.mouth_open = false;
        self.busy = false;
        self.status = "返答を停止しました".into();
        cx.notify();
    }
    fn reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.config.server_url = self.url.read(cx).value().to_string();
        self.config.token_file = self.token.read(cx).value().to_string().into();
        let certificate = self.certificate.read(cx).value().to_string();
        self.config.certificate_file = if certificate.trim().is_empty() {
            None
        } else {
            Some(certificate.into())
        };
        self.connection_epoch += 1;
        let (backend, receiver) = spawn_backend(self.config.clone());
        self.backend = backend;
        self.available = None;
        self.command(ClientCommand::Presence {
            present: self.present,
        });
        self.connected = false;
        self.snapshot = None;
        self.persona_dirty = false;
        self.live_text.clear();
        self.status = "接続しています…".into();
        attach(receiver, self.connection_epoch, window, cx);
        cx.notify();
    }
    fn receive(&mut self, event: UiEvent, window: &mut Window, cx: &mut Context<Self>) {
        let follow_tail =
            self.history_scroll.max_offset().height + self.history_scroll.offset().y <= px(32.);
        match event {
            UiEvent::Connected => {
                self.connected = true;
                self.status = "そばにいるよ".into();
            }
            UiEvent::Disconnected => {
                self.connected = false;
                self.available = None;
                self.busy = false;
                self.status = "再接続を待っています".into();
            }
            UiEvent::Error(error) => {
                self.busy = false;
                self.status = error;
            }
            UiEvent::Providers(value) => self.providers = value,
            UiEvent::Availability(value) => {
                if !value.speech {
                    self.voice = false;
                }
                self.status = value
                    .reason
                    .clone()
                    .unwrap_or_else(|| "そばにいるよ".into());
                self.available = Some(value);
            }
            UiEvent::Capabilities { extensions, grants } => {
                self.extensions = extensions;
                self.grants = grants;
            }
            UiEvent::Review(value) => {
                self.review = value
                    .get("review")
                    .and_then(Value::as_str)
                    .map(Into::into)
                    .unwrap_or_else(|| value.to_string())
            }
            UiEvent::Recording(value) => {
                self.recording = value;
                self.status = if value {
                    "聞いているよ。話し終わったら「録音を終える」を押してね。"
                } else {
                    "言葉を受け取っています…"
                }
                .into();
            }
            UiEvent::Speaking(value) => {
                self.speech_epoch += 1;
                let epoch = self.speech_epoch;
                self.speaking = value;
                self.mouth_open = value;
                if value {
                    cx.spawn(async move |weak, cx| {
                        loop {
                            Timer::after(Duration::from_millis(120)).await;
                            let Ok(keep) = weak.update(cx, |this, cx| {
                                if !this.speaking || this.speech_epoch != epoch {
                                    return false;
                                }
                                this.mouth_open = !this.mouth_open;
                                cx.notify();
                                true
                            }) else {
                                break;
                            };
                            if !keep {
                                break;
                            }
                        }
                    })
                    .detach();
                }
            }
            UiEvent::Transcript(text) => {
                self.last_user = text;
                self.live_text.clear();
                self.busy = true;
            }
            UiEvent::Snapshot(snapshot) => {
                if self
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.character.revision > snapshot.character.revision)
                {
                    return;
                }
                if !self.persona_dirty
                    || self.persona.read(cx).value().as_str() == snapshot.character.persona
                        && self.name.read(cx).value().as_str() == snapshot.character.name
                {
                    self.name.update(cx, |input, cx| {
                        input.set_value(snapshot.character.name.clone(), window, cx)
                    });
                    self.persona.update(cx, |input, cx| {
                        input.set_value(snapshot.character.persona.clone(), window, cx)
                    });
                    self.persona_dirty = false;
                    self.persona_revision = snapshot.character.revision;
                }
                if !self.busy
                    && snapshot.sources.iter().any(|s| {
                        s.actor == inori_domain::Actor::Assistant && s.text == self.live_text
                    })
                {
                    self.live_text.clear();
                }
                if self
                    .selected_memory
                    .as_ref()
                    .is_some_and(|id| !snapshot.memories.iter().any(|m| &m.id == id))
                {
                    self.selected_memory = None;
                }
                self.snapshot = Some(snapshot);
            }
            UiEvent::Event(event) => match event.kind.as_str() {
                "turn.started" => {
                    self.generation = event.generation_id;
                    self.live_text.clear();
                    self.busy = true;
                    self.status = "考えているよ…".into();
                }
                "text.delta" => {
                    if event.generation_id == self.generation {
                        if let Some(text) = event.payload.get("text").and_then(Value::as_str) {
                            if self.live_text.len() + text.len() <= 65_536 {
                                self.live_text.push_str(text);
                            }
                        }
                    }
                }
                "turn.completed" => {
                    if event.generation_id == self.generation {
                        self.busy = false;
                        self.status = "そばにいるよ".into();
                    }
                }
                "turn.cancelled" => {
                    if event.generation_id == self.generation {
                        self.busy = false;
                        self.speaking = false;
                        self.mouth_open = false;
                    }
                }
                "turn.error" if event.generation_id == self.generation => {
                    self.busy = false;
                    self.status = event
                        .payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("返答できませんでした")
                        .into();
                }
                _ => {}
            },
        }
        if follow_tail {
            self.history_scroll.scroll_to_bottom();
        }
        cx.notify();
    }
    fn talk(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let mut history = div()
            .id("history")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.history_scroll)
            .p_4()
            .gap_3()
            .flex()
            .flex_col();
        if let Some(snapshot) = &self.snapshot {
            for source in &snapshot.sources {
                if source.turn_id.is_none() {
                    continue;
                }
                let user = source.actor == inori_domain::Actor::User;
                history = history.child(
                    div()
                        .p_3()
                        .rounded_lg()
                        .bg(rgb(if user { 0x233047 } else { PANEL }))
                        .child(div().text_sm().text_color(rgb(MUTED)).child(if user {
                            "あなた".to_owned()
                        } else {
                            snapshot.character.name.clone()
                        }))
                        .child(source.text.clone()),
                );
            }
        }
        if !self.live_text.is_empty() {
            history = history.child(
                div()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(ACCENT))
                    .bg(rgb(PANEL))
                    .child(div().text_sm().text_color(rgb(ACCENT)).child("いまの返答"))
                    .child(self.live_text.clone()),
            );
        }
        if self
            .snapshot
            .as_ref()
            .is_none_or(|s| s.sources.iter().all(|source| source.turn_id.is_none()))
            && self.live_text.is_empty()
        {
            history = history.child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .items_center()
                    .gap_3()
                    .child(div().text_2xl().child("今日は、何を話そうか。"))
                    .child(
                        div()
                            .text_color(rgb(MUTED))
                            .child("少しずつ、あなたのことを覚えていきます。"),
                    ),
            );
        }
        let mode = match self.mode {
            DialogueMode::Auto => "状況に合わせる",
            DialogueMode::Quick => "気軽に話す",
            DialogueMode::Deliberate => "じっくり考える",
        };
        let text_available = self.connected && self.available.as_ref().is_some_and(|c| c.text_chat);
        let speech_available = self.connected && self.available.as_ref().is_some_and(|c| c.speech);
        let record_available =
            text_available && self.available.as_ref().is_some_and(|c| c.transcription);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(history)
            .child(
                div().text_sm().text_color(rgb(MUTED)).child(
                    self.available
                        .as_ref()
                        .map(|c| {
                            c.reason.clone().unwrap_or_else(|| {
                                format!(
                                    "マイク: {}　音声再生: {}",
                                    if c.transcription {
                                        "利用可能"
                                    } else {
                                        "サーバーで無効"
                                    },
                                    if c.speech {
                                        "利用可能"
                                    } else {
                                        "サーバーで無効"
                                    }
                                )
                            })
                        })
                        .unwrap_or_else(|| "利用できる機能を確認しています…".into()),
                ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(Button::new("mode").label(mode).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.mode = match this.mode {
                                DialogueMode::Auto => DialogueMode::Quick,
                                DialogueMode::Quick => DialogueMode::Deliberate,
                                DialogueMode::Deliberate => DialogueMode::Auto,
                            };
                            cx.notify();
                        },
                    )))
                    .child(
                        Button::new("voice")
                            .disabled(!speech_available)
                            .label(if self.voice {
                                "音声 ON"
                            } else {
                                "音声 OFF"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.voice = !this.voice;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("record")
                            .disabled(!record_available)
                            .label(if self.recording {
                                "録音を終える"
                            } else {
                                "マイクで話す"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.recording {
                                    this.command(ClientCommand::FinishRecording {
                                        mode: this.mode,
                                        speak: this.voice,
                                    });
                                } else {
                                    this.stop(cx);
                                    this.command(ClientCommand::StartRecording);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("stop")
                            .label("止める")
                            .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.composer).disabled(!text_available)),
                    )
                    .child(
                        Button::new("send")
                            .primary()
                            .label("送る")
                            .disabled(!text_available)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.send_chat(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }
    fn memory(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .id("memory-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2();
        if let Some(snapshot) = &self.snapshot {
            for memory in &snapshot.memories {
                let id = memory.id.clone();
                let text = memory.text.clone();
                let kind = match memory.kind {
                    MemoryKind::Confirmed => "確認済み",
                    MemoryKind::Inference => "推測",
                    MemoryKind::Summary => "要約",
                };
                let revision = snapshot.character.revision;
                list = list.child(
                    div()
                        .p_3()
                        .rounded_lg()
                        .bg(rgb(PANEL))
                        .child(div().text_sm().text_color(rgb(ACCENT)).child(kind))
                        .child(text.clone())
                        .child(
                            Button::new(SharedString::from(format!("edit-{id}")))
                                .label("編集")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.selected_memory = Some(id.clone());
                                    this.memory_revision = revision;
                                    this.memory_input.update(cx, |input, cx| {
                                        input.set_value(text.clone(), window, cx)
                                    });
                                    cx.notify();
                                })),
                        ),
                );
            }
        }
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(div().text_xl().child("覚えていること"))
            .child(div().text_sm().text_color(rgb(MUTED)).child(
                "推測は確認済みの情報と区別して保存します。訂正や削除は後の会話にも反映されます。",
            ))
            .child(list)
            .child(Input::new(&self.memory_input).h(px(90.)).flex_shrink_0())
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("new-memory")
                            .label("新しく覚える")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.selected_memory = None;
                                this.memory_revision = this.revision();
                                this.memory_input
                                    .update(cx, |i, cx| i.set_value("", window, cx));
                                cx.notify();
                            })),
                    )
                    .child(Button::new("save-memory").primary().label("保存").on_click(
                        cx.listener(|this, _, _, cx| {
                            let text = this.memory_input.read(cx).value().to_string();
                            let revision = if this.selected_memory.is_some() {
                                this.memory_revision
                            } else {
                                this.revision()
                            };
                            let command = match &this.selected_memory {
                                Some(id) => ClientCommand::EditMemory {
                                    id: id.clone(),
                                    text,
                                    revision,
                                },
                                None => ClientCommand::AddMemory { text, revision },
                            };
                            this.command(command);
                            cx.notify();
                        }),
                    ))
                    .child(
                        Button::new("delete-memory")
                            .label("削除")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(id) = this.selected_memory.clone() {
                                    this.command(ClientCommand::DeleteMemory {
                                        id,
                                        revision: this.revision(),
                                    });
                                    this.selected_memory = None;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("restore-memory")
                            .label("直前の内容に戻す")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(snapshot) = &this.snapshot {
                                    if let Some(memory) = snapshot
                                        .memories
                                        .iter()
                                        .find(|m| Some(&m.id) == this.selected_memory.as_ref())
                                    {
                                        if let Some(version) = &memory.supersedes {
                                            this.command(ClientCommand::RestoreMemory {
                                                id: memory.id.clone(),
                                                version: version.clone(),
                                                revision: snapshot.character.revision,
                                            });
                                        }
                                    }
                                }
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
    fn settings(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let mut providers = div().flex().gap_2().flex_wrap();
        if let Some(profiles) = self.providers.get("providers").and_then(Value::as_array) {
            for profile in profiles {
                if let Some(id) = profile.get("id").and_then(Value::as_str) {
                    let id = id.to_owned();
                    let label = id.clone();
                    providers = providers.child(
                        Button::new(SharedString::from(format!("provider-{id}")))
                            .label(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.command(ClientCommand::SwitchProvider {
                                    id: id.clone(),
                                    revision: this.revision(),
                                });
                                cx.notify();
                            })),
                    );
                }
            }
        }
        let mut permissions = div().flex().flex_col().gap_2();
        if let Some(extensions) = self.extensions.get("extensions").and_then(Value::as_array) {
            for extension in extensions {
                let Some(id) = extension.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(declarations) = extension.get("permissions").and_then(Value::as_array) {
                    for (index, permission) in declarations.iter().enumerate() {
                        let capability = permission
                            .get("capability")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let action = permission
                            .get("action")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let target = permission
                            .get("target")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let extension = id.to_owned();
                        let allowed = self.grants.as_array().is_some_and(|grants| {
                            grants.iter().any(|g| {
                                g["extension"] == extension
                                    && g["capability"] == capability
                                    && g["action"] == action
                                    && g["target"] == target
                                    && g["allowed"] == true
                            })
                        });
                        let label = format!(
                            "{extension} / {capability} / {action} / {target}：{}",
                            if allowed {
                                "許可中 → 拒否"
                            } else {
                                "拒否中 → 許可"
                            }
                        );
                        permissions = permissions.child(
                            Button::new(SharedString::from(format!("grant-{id}-{index}")))
                                .label(label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.command(ClientCommand::SetGrant {
                                        extension: extension.clone(),
                                        capability: capability.clone(),
                                        action: action.clone(),
                                        target: target.clone(),
                                        allowed: !allowed,
                                    });
                                    cx.notify();
                                })),
                        );
                    }
                }
            }
        }
        div()
            .id("settings")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_3()
            .child(div().text_xl().child("キャラクターと接続"))
            .child(
                div().text_sm().text_color(rgb(MUTED)).child(
                    "基本設定はあなたが変更します。AIによる日々の適応とは別に保存されます。",
                ),
            )
            .child(Input::new(&self.name))
            .child(Input::new(&self.persona).h(px(140.)).flex_shrink_0())
            .child(
                Button::new("save-persona")
                    .primary()
                    .label("基本設定を保存")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.command(ClientCommand::SavePersona {
                            name: this.name.read(cx).value().to_string(),
                            persona: this.persona.read(cx).value().to_string(),
                            revision: this.persona_revision,
                        });
                        cx.notify();
                    })),
            )
            .child(div().text_lg().child("会話に使うAI"))
            .child(providers)
            .child(
                Button::new("review")
                    .label("記憶の再評価案を作る")
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(snapshot) = &this.snapshot {
                            this.command(ClientCommand::Review {
                                provider: snapshot.character.active_provider.clone(),
                            });
                            this.status = "記憶の根拠を確認しています…".into();
                        }
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(PANEL))
                    .child(self.review.clone()),
            )
            .child(div().text_lg().child("拡張の権限"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child("導入した拡張の能力ごとに、対象と操作を確認して許可します。"),
            )
            .child(permissions)
            .child(div().text_lg().child("サーバーへの接続"))
            .child(div().text_sm().child("サーバーURL"))
            .child(Input::new(&self.url))
            .child(div().text_sm().child("端末トークンのファイル"))
            .child(Input::new(&self.token))
            .child(div().text_sm().child("信頼するサーバー証明書のファイル"))
            .child(Input::new(&self.certificate))
            .child(
                Button::new("connect")
                    .label("接続する")
                    .on_click(cx.listener(|this, _, window, cx| this.reconnect(window, cx))),
            )
            .into_any_element()
    }
}
fn attach(
    mut events: tokio::sync::mpsc::Receiver<UiEvent>,
    epoch: u64,
    window: &Window,
    cx: &mut Context<View>,
) {
    cx.spawn_in(window, async move |weak, cx| {
        while let Some(event) = events.recv().await {
            let Ok(current) = weak.update_in(cx, |view, window, cx| {
                if view.connection_epoch != epoch {
                    return false;
                }
                view.receive(event, window, cx);
                true
            }) else {
                break;
            };
            if !current {
                break;
            }
        }
    })
    .detach();
}

impl Render for View {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self
            .snapshot
            .as_ref()
            .map(|s| s.character.name.clone())
            .unwrap_or_else(|| "Inori".into());
        let provider = self
            .snapshot
            .as_ref()
            .map(|s| s.character.active_provider.clone())
            .unwrap_or_default();
        let content = match self.tab {
            1 => self.memory(window, cx),
            2 => self.settings(window, cx),
            _ => self.talk(window, cx),
        };
        let mut tabs = div().flex().gap_2();
        for (index, title) in ["会話", "記憶", "設定"].iter().enumerate() {
            tabs = tabs.child(
                Button::new(SharedString::from(format!("tab-{index}")))
                    .label(*title)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = index;
                        cx.notify();
                    })),
            );
        }
        div()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Yu Gothic UI")
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            .on_action(cx.listener(|this, _: &StopSpeaking, _, cx| this.stop(cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_2xl().text_color(rgb(ACCENT)).child("Inori"))
                    .child(tabs),
            )
            .child(
                div()
                    .flex()
                    .gap_5()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .w(px(220.))
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_4()
                            .p_4()
                            .rounded_xl()
                            .bg(rgb(PANEL))
                            .child(match &self.avatar {
                                Some(path) => {
                                    img(path.clone()).w(px(175.)).h(px(200.)).into_any_element()
                                }
                                None => avatar(self.mouth_open),
                            })
                            .child(div().text_xl().child(name))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(if self.connected { 0xa5ddc0 } else { MUTED }))
                                    .child(if self.connected {
                                        "● つながっています"
                                    } else {
                                        "○ 接続待ち"
                                    }),
                            )
                            .child(div().text_sm().text_color(rgb(MUTED)).child(provider))
                            .child(
                                Button::new("presence")
                                    .label(if self.present {
                                        "在席中 → 離席する"
                                    } else {
                                        "離席中 → 戻ったよ"
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.present = !this.present;
                                        this.command(ClientCommand::Presence {
                                            present: this.present,
                                        });
                                        cx.notify();
                                    })),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(MUTED))
                                    .child("画面を閉じても、サーバーの記憶と活動は続きます。"),
                            )
                            .child(
                                Button::new("refresh")
                                    .label("読み直す")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.persona_dirty = false;
                                        this.command(ClientCommand::Refresh);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(content),
            )
            .child(
                div()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .pt_3()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(self.status.clone()),
            )
    }
}
fn avatar(open: bool) -> AnyElement {
    div()
        .relative()
        .w(px(180.))
        .h(px(210.))
        .child(
            div()
                .absolute()
                .top(px(16.))
                .left(px(22.))
                .w(px(136.))
                .h(px(156.))
                .rounded_full()
                .bg(rgb(0x8e86bb)),
        )
        .child(
            div()
                .absolute()
                .top(px(42.))
                .left(px(40.))
                .w(px(100.))
                .h(px(112.))
                .rounded_full()
                .bg(rgb(0xffdfd8)),
        )
        .child(
            div()
                .absolute()
                .top(px(22.))
                .left(px(34.))
                .w(px(113.))
                .h(px(52.))
                .rounded_full()
                .bg(rgb(0xb2a6dc)),
        )
        .child(
            div()
                .absolute()
                .top(px(8.))
                .left(px(106.))
                .w(px(35.))
                .h(px(24.))
                .rounded_lg()
                .bg(rgb(0xf0b8d3)),
        )
        .child(
            div()
                .absolute()
                .top(px(91.))
                .left(px(61.))
                .w(px(10.))
                .h(px(19.))
                .rounded_full()
                .bg(rgb(0x51466e)),
        )
        .child(
            div()
                .absolute()
                .top(px(91.))
                .left(px(108.))
                .w(px(10.))
                .h(px(19.))
                .rounded_full()
                .bg(rgb(0x51466e)),
        )
        .child(
            div()
                .absolute()
                .top(px(116.))
                .left(px(48.))
                .w(px(19.))
                .h(px(8.))
                .rounded_full()
                .bg(rgb(0xf1a9b9)),
        )
        .child(
            div()
                .absolute()
                .top(px(116.))
                .left(px(115.))
                .w(px(19.))
                .h(px(8.))
                .rounded_full()
                .bg(rgb(0xf1a9b9)),
        )
        .child(
            div()
                .absolute()
                .top(px(125.))
                .left(px(84.))
                .w(px(13.))
                .h(px(if open { 13. } else { 4. }))
                .rounded_full()
                .bg(rgb(0xbb718a)),
        )
        .child(
            div()
                .absolute()
                .top(px(156.))
                .left(px(30.))
                .w(px(120.))
                .h(px(54.))
                .rounded_t_xl()
                .bg(rgb(0xe7deff)),
        )
        .child(
            div()
                .absolute()
                .top(px(155.))
                .left(px(77.))
                .w(px(28.))
                .h(px(23.))
                .rounded_lg()
                .bg(rgb(0xd194b9)),
        )
        .into_any_element()
}
fn resolve_pack(path: &Path, expression: &str) -> Result<PathBuf> {
    let bytes = std::fs::read(path)?;
    if bytes.len() > 65_536 {
        bail!("model pack manifest too large");
    }
    let pack: ModelPack = serde_json::from_slice(&bytes)?;
    if pack.renderer != "basic-2d" {
        bail!("this pack requires an external renderer: {}", pack.renderer);
    }
    let file = pack
        .expressions
        .get(expression)
        .or_else(|| pack.expressions.get("idle"))
        .context("model pack has no idle expression")?;
    let root = path
        .parent()
        .context("model pack directory missing")?
        .canonicalize()?;
    let asset = root.join(file).canonicalize()?;
    if !asset.starts_with(&root) || std::fs::metadata(&asset)?.len() > 4 * 1024 * 1024 {
        bail!("model asset exceeds its allowed directory or size");
    }
    Ok(asset)
}

pub fn run(config: ClientConfig, probe: Option<u64>) -> Result<()> {
    Application::new().run(move |cx| {
        gpui_component::init(cx);
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("escape", StopSpeaking, None),
        ]);
        let bounds = Bounds::centered(None, size(px(1080.), px(760.)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(800.), px(600.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Inori".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                let view = cx.new(|cx| View::new(config, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            },
        );
        let handle = match result {
            Ok(handle) => handle,
            Err(error) => {
                eprintln!("window creation failed: {error}");
                cx.quit();
                return;
            }
        };
        if let Ok(tray) = create_tray() {
            let open_id = tray.open.clone();
            let quit_id = tray.quit.clone();
            cx.set_global(tray);
            let _ = handle.update(cx, |_, window, cx| {
                window.on_window_should_close(cx, |window, _| {
                    window.minimize_window();
                    false
                });
            });
            cx.spawn(async move |cx| {
                loop {
                    Timer::after(Duration::from_millis(200)).await;
                    while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                        if event.id == quit_id {
                            let _ = cx.update(|cx| cx.quit());
                            return;
                        }
                        if event.id == open_id {
                            let _ = handle.update(cx, |_, window, cx| {
                                window.activate_window();
                                cx.activate(true);
                            });
                        }
                    }
                }
            })
            .detach();
        }
        cx.activate(true);
        if let Some(seconds) = probe {
            cx.spawn(async move |cx| {
                Timer::after(Duration::from_secs(seconds)).await;
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });
    Ok(())
}

struct TrayState {
    _icon: tray_icon::TrayIcon,
    open: tray_icon::menu::MenuId,
    quit: tray_icon::menu::MenuId,
}
impl Global for TrayState {}
fn create_tray() -> Result<TrayState> {
    use tray_icon::{
        Icon, TrayIconBuilder,
        menu::{Menu, MenuItem},
    };
    let menu = Menu::new();
    let open = MenuItem::new("Inoriを開く", true, None);
    let quit = MenuItem::new("終了", true, None);
    menu.append_items(&[&open, &quit])?;
    let mut rgba = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32 {
        for x in 0..32 {
            let inside = (x - 16_i32).pow(2) + (y - 16_i32).pow(2) < 200;
            rgba.extend_from_slice(if inside {
                &[194, 177, 235, 255]
            } else {
                &[0, 0, 0, 0]
            });
        }
    }
    let icon = Icon::from_rgba(rgba, 32, 32)?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Inori")
        .with_icon(icon)
        .build()?;
    Ok(TrayState {
        _icon: tray,
        open: open.id().clone(),
        quit: quit.id().clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[::core::prelude::v1::test]
    fn renderer_pack_rejects_path_escape() {
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("pack");
        std::fs::create_dir(&inside).unwrap();
        std::fs::write(root.path().join("outside.png"), b"fake").unwrap();
        let p = inside.join("pack.json");
        std::fs::write(&p,r#"{"id":"bad","version":"1","renderer":"basic-2d","expressions":{"idle":"../outside.png"},"license":"test"}"#).unwrap();
        assert!(resolve_pack(&p, "idle").is_err());
    }
}
