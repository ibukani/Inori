use anyhow::Result;
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    Disableable, Root,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};
use inori_client_core::{BackendControl, ClientCommand, ClientConfig, UiEvent, spawn_backend};
use inori_domain::Snapshot;
use inori_protocol::{DialogueMode, ServerCapabilities};
use std::time::Duration;

const BG: u32 = 0xf7fbff;
const SURFACE: u32 = 0xffffff;
const BORDER: u32 = 0xdceaf4;
const TEXT: u32 = 0x233342;
const MUTED: u32 = 0x6b8091;
const ACCENT: u32 = 0x318fbd;
const USER_BUBBLE: u32 = 0xe7f5fc;

actions!(inori, [Quit, StopSpeaking]);

#[derive(Clone)]
struct TrayMenuItems {
    microphone: tray_icon::menu::MenuItem,
    speech: tray_icon::menu::MenuItem,
}

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
    url: Entity<InputState>,
    token: Entity<InputState>,
    certificate: Entity<InputState>,
    settings_open: bool,
    microphone: bool,
    listening: bool,
    speech_output: bool,
    speaking: bool,
    busy: bool,
    live_text: String,
    pending_voice_text: Option<String>,
    generation: Option<String>,
    tray_items: Option<TrayMenuItems>,
    _subscriptions: Vec<Subscription>,
}

struct ViewRef(Entity<View>);

impl Global for ViewRef {}

impl View {
    fn new(config: ClientConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let composer = cx.new(|cx| InputState::new(window, cx).placeholder("メッセージを入力…"));
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
                    .map(|path| path.to_string_lossy().to_string())
                    .unwrap_or_default(),
            )
        });
        let (backend, receiver) = spawn_backend(config.clone());
        let subscriptions =
            vec![
                cx.subscribe_in(&composer, window, |this, _, event, window, cx| {
                    if let InputEvent::PressEnter { secondary: false } = event {
                        this.send_chat(window, cx);
                    }
                }),
            ];
        attach(receiver, 1, window, cx);
        Self {
            backend,
            connection_epoch: 1,
            config,
            snapshot: None,
            connected: false,
            available: None,
            status: "サーバーへ接続しています…".into(),
            composer,
            history_scroll: ScrollHandle::default(),
            url,
            token,
            certificate,
            settings_open: false,
            microphone: false,
            listening: false,
            speech_output: false,
            speaking: false,
            busy: false,
            live_text: String::new(),
            pending_voice_text: None,
            generation: None,
            tray_items: None,
            _subscriptions: subscriptions,
        }
    }

    fn text_available(&self) -> bool {
        self.connected && self.available.as_ref().is_some_and(|value| value.text_chat)
    }

    fn microphone_available(&self) -> bool {
        self.text_available()
            && self
                .available
                .as_ref()
                .is_some_and(|value| value.transcription)
    }

    fn speech_available(&self) -> bool {
        self.connected && self.available.as_ref().is_some_and(|value| value.speech)
    }

    fn send_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().to_string();
        if text.trim().is_empty() || !self.text_available() {
            return;
        }
        self.history_scroll.scroll_to_bottom();
        self.live_text.clear();
        self.pending_voice_text = None;
        self.busy = true;
        self.backend.playback.stop();
        self.command(ClientCommand::Chat {
            text,
            mode: DialogueMode::Auto,
            speak: self.speech_output,
        });
        self.composer
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    fn command(&mut self, command: ClientCommand) {
        if let Err(error) = self.backend.send(command) {
            self.status = error.to_string();
        }
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        let _ = self.backend.interrupt();
        self.speaking = false;
        self.busy = false;
        self.status = "返答を停止しました".into();
        cx.notify();
    }

    fn set_microphone(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if enabled == self.microphone {
            return;
        }
        if enabled && !self.microphone_available() {
            self.status = "このサーバーでは音声入力を利用できません".into();
            cx.notify();
            return;
        }
        if let Err(error) = self.backend.send(ClientCommand::SetMicrophone(enabled)) {
            self.status = error.to_string();
            cx.notify();
            return;
        }
        self.microphone = enabled;
        self.listening = false;
        self.status = if enabled {
            "音声会話を開始しました".into()
        } else {
            "マイクをミュートしました".into()
        };
        self.update_tray();
        cx.notify();
    }

    fn set_speech_output(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if enabled == self.speech_output {
            return;
        }
        if enabled && !self.speech_available() {
            self.status = "このサーバーではAI音声を利用できません".into();
            cx.notify();
            return;
        }
        if let Err(error) = self.backend.send(ClientCommand::SetSpeechOutput(enabled)) {
            self.status = error.to_string();
            cx.notify();
            return;
        }
        self.speech_output = enabled;
        if !enabled {
            self.speaking = false;
        }
        self.status = if enabled {
            "AI音声をONにしました".into()
        } else {
            "AI音声をOFFにしました".into()
        };
        self.update_tray();
        cx.notify();
    }

    fn update_tray(&self) {
        let Some(items) = &self.tray_items else {
            return;
        };
        items.microphone.set_text(if !self.microphone_available() {
            "マイク 利用不可"
        } else if self.microphone {
            "マイクをミュート"
        } else {
            "マイクをON"
        });
        items
            .microphone
            .set_enabled(self.microphone_available() || self.microphone);
        items.speech.set_text(if !self.speech_available() {
            "AI音声 利用不可"
        } else if self.speech_output {
            "AI音声をOFF"
        } else {
            "AI音声をON"
        });
        items
            .speech
            .set_enabled(self.speech_available() || self.speech_output);
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
        self.connected = false;
        self.available = None;
        self.snapshot = None;
        self.microphone = false;
        self.listening = false;
        self.speech_output = false;
        self.speaking = false;
        self.busy = false;
        self.live_text.clear();
        self.pending_voice_text = None;
        self.status = "サーバーへ接続しています…".into();
        self.update_tray();
        attach(receiver, self.connection_epoch, window, cx);
        cx.notify();
    }

    fn receive(&mut self, event: UiEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let follow_tail =
            self.history_scroll.max_offset().height + self.history_scroll.offset().y <= px(32.);
        match event {
            UiEvent::Connected => {
                self.connected = true;
                self.status = "接続しました".into();
            }
            UiEvent::Disconnected => {
                self.connected = false;
                self.available = None;
                self.microphone = false;
                self.listening = false;
                self.speech_output = false;
                self.speaking = false;
                self.busy = false;
                self.status = "再接続を待っています".into();
            }
            UiEvent::Error(error) => {
                self.status = error;
            }
            UiEvent::Availability(value) => {
                self.available = Some(value);
                if !self.microphone_available() && self.microphone {
                    self.command(ClientCommand::SetMicrophone(false));
                    self.microphone = false;
                    self.listening = false;
                }
                if !self.speech_available() && self.speech_output {
                    self.command(ClientCommand::SetSpeechOutput(false));
                    self.speech_output = false;
                    self.speaking = false;
                }
                self.update_tray();
            }
            UiEvent::Transcript(text) => {
                self.pending_voice_text = Some(text);
                self.status = "認識した発話を送信しました".into();
            }
            UiEvent::Microphone(value) => {
                self.microphone = value;
                if !value {
                    self.listening = false;
                }
                self.update_tray();
            }
            UiEvent::Listening(value) => {
                self.listening = value;
                self.status = if value {
                    "聞き取っています…".into()
                } else {
                    "発話を送信しています…".into()
                };
            }
            UiEvent::SpeechOutput(value) => {
                self.speech_output = value;
                if !value {
                    self.speaking = false;
                }
                self.update_tray();
            }
            UiEvent::Speaking(value) => {
                self.speaking = value;
            }
            UiEvent::Snapshot(snapshot) => {
                if self
                    .snapshot
                    .as_ref()
                    .is_some_and(|current| current.character.revision > snapshot.character.revision)
                {
                    return;
                }
                if self.pending_voice_text.as_ref().is_some_and(|text| {
                    snapshot.sources.iter().any(|source| {
                        source.actor == inori_domain::Actor::User && &source.text == text
                    })
                }) {
                    self.pending_voice_text = None;
                }
                if !self.busy
                    && snapshot.sources.iter().any(|source| {
                        source.actor == inori_domain::Actor::Assistant
                            && source.text == self.live_text
                    })
                {
                    self.live_text.clear();
                }
                self.snapshot = Some(snapshot);
            }
            UiEvent::Event(event) => match event.kind.as_str() {
                "turn.started" => {
                    self.generation = event.generation_id;
                    self.live_text.clear();
                    self.busy = true;
                    self.status = if self.microphone {
                        "考えています…　マイクはONです".into()
                    } else {
                        "考えています…".into()
                    };
                }
                "text.delta" => {
                    if event.generation_id == self.generation
                        && let Some(text) = event
                            .payload
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                        && self.live_text.len() + text.len() <= 65_536
                    {
                        self.live_text.push_str(text);
                    }
                }
                "turn.completed" => {
                    if event.generation_id == self.generation {
                        self.busy = false;
                        self.status = if self.microphone {
                            "音声会話中".into()
                        } else {
                            "接続しています".into()
                        };
                    }
                }
                "turn.cancelled" => {
                    if event.generation_id == self.generation {
                        self.busy = false;
                        self.speaking = false;
                    }
                }
                "turn.error" if event.generation_id == self.generation => {
                    self.busy = false;
                    self.status = event
                        .payload
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("返答できませんでした")
                        .into();
                }
                _ => {}
            },
            UiEvent::Providers(_) | UiEvent::Capabilities { .. } | UiEvent::Review(_) => {}
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
            .py_5()
            .gap_5()
            .flex()
            .flex_col();
        let has_messages = self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .sources
                .iter()
                .any(|source| source.turn_id.is_some())
        });
        if let Some(snapshot) = &self.snapshot {
            for source in snapshot.sources.iter().rev() {
                if source.turn_id.is_none() {
                    continue;
                }
                let user = source.actor == inori_domain::Actor::User;
                let message = if user {
                    div()
                        .max_w(px(620.))
                        .p_4()
                        .rounded_xl()
                        .bg(rgb(USER_BUBBLE))
                        .text_color(rgb(TEXT))
                        .child(source.text.clone())
                        .into_any_element()
                } else {
                    div()
                        .max_w(px(660.))
                        .py_2()
                        .text_color(rgb(TEXT))
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(ACCENT))
                                .child(snapshot.character.name.clone()),
                        )
                        .child(source.text.clone())
                        .into_any_element()
                };
                history = history.child(
                    div()
                        .w_full()
                        .flex()
                        .justify_end()
                        .when(!user, |element| element.justify_start())
                        .child(message),
                );
            }
        }
        if let Some(text) = &self.pending_voice_text {
            history = history.child(
                div().w_full().flex().justify_end().child(
                    div()
                        .max_w(px(620.))
                        .p_4()
                        .rounded_xl()
                        .bg(rgb(USER_BUBBLE))
                        .child(text.clone()),
                ),
            );
        }
        if !self.live_text.is_empty() {
            history = history.child(
                div().w_full().flex().justify_start().child(
                    div()
                        .max_w(px(660.))
                        .py_2()
                        .child(div().text_sm().text_color(rgb(ACCENT)).child("Inori"))
                        .child(self.live_text.clone()),
                ),
            );
        }
        if !has_messages && self.pending_voice_text.is_none() && self.live_text.is_empty() {
            history = history.child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_2xl()
                            .text_color(rgb(TEXT))
                            .child("今日は、何を話しましょうか。"),
                    )
                    .child(
                        div()
                            .text_color(rgb(MUTED))
                            .child("メッセージを送るか、マイクをONにして話しかけてください。"),
                    ),
            );
        }

        let text_available = self.text_available();
        let microphone_available = self.microphone_available();
        let speech_available = self.speech_available();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(history)
            .child(
                div()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .pt_3()
                    .pb_2()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(if self.microphone && self.listening {
                                ACCENT
                            } else {
                                MUTED
                            }))
                            .child(self.status.clone()),
                    )
                    .child(
                        div().flex().gap_2().child(
                            Button::new("stop")
                                .label(if self.speaking {
                                    "音声を止める"
                                } else {
                                    "返答を止める"
                                })
                                .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.composer).disabled(!text_available)),
                    )
                    .child(
                        Button::new("send")
                            .primary()
                            .label("送信")
                            .disabled(!text_available)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.send_chat(window, cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .pb_2()
                    .child(
                        Button::new("microphone")
                            .label(if !microphone_available {
                                "マイク 利用不可"
                            } else if self.microphone {
                                "マイク ON"
                            } else {
                                "マイク OFF"
                            })
                            .disabled(!microphone_available && !self.microphone)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_microphone(!this.microphone, cx)
                            })),
                    )
                    .child(
                        Button::new("speech-output")
                            .label(if !speech_available {
                                "AI音声 利用不可"
                            } else if self.speech_output {
                                "AI音声 ON"
                            } else {
                                "AI音声 OFF"
                            })
                            .disabled(!speech_available && !self.speech_output)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_speech_output(!this.speech_output, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn connection_settings(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .rounded_xl()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(SURFACE))
            .p_5()
            .child(div().text_xl().child("サーバーへの接続"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child("接続先と端末認証を設定します。AIや記憶の管理は別のアプリで行います。"),
            )
            .child(div().text_sm().child("サーバーURL"))
            .child(Input::new(&self.url))
            .child(div().text_sm().child("端末トークンのファイル"))
            .child(Input::new(&self.token))
            .child(div().text_sm().child("信頼するサーバー証明書のファイル"))
            .child(Input::new(&self.certificate))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("connect").primary().label("接続する").on_click(
                            cx.listener(|this, _, window, cx| this.reconnect(window, cx)),
                        ),
                    )
                    .child(
                        Button::new("back-to-chat")
                            .label("チャットへ戻る")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.settings_open = false;
                                cx.notify();
                            })),
                    ),
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
        let status_color = if self.connected { 0x3e9b73 } else { MUTED };
        let content = if self.settings_open {
            self.connection_settings(window, cx)
        } else {
            self.talk(window, cx)
        };
        div()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Yu Gothic UI")
            .flex()
            .flex_col()
            .on_action(cx.listener(|this, _: &StopSpeaking, _, cx| this.stop(cx)))
            .child(
                div()
                    .h(px(62.))
                    .px_6()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(SURFACE))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(div().text_2xl().text_color(rgb(ACCENT)).child("Inori"))
                            .child(div().text_sm().text_color(rgb(status_color)).child(
                                if self.connected {
                                    "● 接続中"
                                } else {
                                    "○ 接続待ち"
                                },
                            )),
                    )
                    .child(
                        Button::new("connection-settings")
                            .label(if self.settings_open {
                                "チャットへ戻る"
                            } else {
                                "接続設定"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.settings_open = !this.settings_open;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .justify_center()
                    .p_5()
                    .child(
                        div()
                            .w(px(720.))
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .child(content),
                    ),
            )
    }
}

struct TrayState {
    _icon: tray_icon::TrayIcon,
    open: tray_icon::menu::MenuId,
    microphone: tray_icon::menu::MenuItem,
    speech: tray_icon::menu::MenuItem,
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
    let microphone = MenuItem::new("マイクをON", false, None);
    let speech = MenuItem::new("AI音声をON", false, None);
    let quit = MenuItem::new("終了", true, None);
    menu.append_items(&[&open, &microphone, &speech, &quit])?;
    let mut rgba = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32 {
        for x in 0..32 {
            let inside = (x - 16_i32).pow(2) + (y - 16_i32).pow(2) < 200;
            rgba.extend_from_slice(if inside {
                &[76, 175, 219, 255]
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
        microphone,
        speech,
        quit: quit.id().clone(),
    })
}

pub fn run(config: ClientConfig, probe: Option<u64>) -> Result<()> {
    Application::new().run(move |cx| {
        gpui_component::init(cx);
        gpui_component::Theme::change(gpui_component::ThemeMode::Light, None, cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("escape", StopSpeaking, None),
        ]);
        let bounds = Bounds::centered(None, size(px(1024.), px(760.)), cx);
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
                cx.set_global(ViewRef(view.clone()));
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
            let microphone_id = tray.microphone.id().clone();
            let speech_id = tray.speech.id().clone();
            let quit_id = tray.quit.clone();
            let items = TrayMenuItems {
                microphone: tray.microphone.clone(),
                speech: tray.speech.clone(),
            };
            cx.set_global(tray);
            let view_ref = cx.global::<ViewRef>().0.clone();
            let _ = view_ref.update(cx, |view, cx| {
                view.tray_items = Some(items);
                view.update_tray();
                cx.notify();
            });
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
                        } else if event.id == microphone_id {
                            let _ = cx.update(|cx| {
                                let view = cx.global::<ViewRef>().0.clone();
                                view.update(cx, |view, cx| {
                                    view.set_microphone(!view.microphone, cx);
                                });
                            });
                        } else if event.id == speech_id {
                            let _ = cx.update(|cx| {
                                let view = cx.global::<ViewRef>().0.clone();
                                view.update(cx, |view, cx| {
                                    view.set_speech_output(!view.speech_output, cx);
                                });
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
