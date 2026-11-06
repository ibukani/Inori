use crate::{modules::ModuleControl, ports::*, providers};
use anyhow::{Context, Result, bail};
use inori_composition::ResolvedComposition;
use inori_domain::*;
use inori_protocol::{ChatAccepted, ChatRequest, DialogueMode, EventEnvelope, PROTOCOL_VERSION};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

pub fn domain_audience(value: inori_protocol::Audience) -> Audience {
    match value {
        inori_protocol::Audience::Private => Audience::Private,
        inori_protocol::Audience::Public => Audience::Public,
    }
}
fn wire_audience(value: Audience) -> inori_protocol::Audience {
    match value {
        Audience::Private => inori_protocol::Audience::Private,
        Audience::Public => inori_protocol::Audience::Public,
    }
}
struct ActiveTurn {
    cancel: CancellationToken,
    device: String,
    session: String,
    audience: Audience,
}

pub struct Service {
    pub repo: Arc<dyn Repository>,
    pub clock: Arc<dyn Clock>,
    providers: BTreeMap<String, Arc<dyn InferenceProvider>>,
    events: broadcast::Sender<EventEnvelope>,
    turns: Mutex<BTreeMap<String, ActiveTurn>>,
    turn_gate: Mutex<()>,
    background_gate: Mutex<()>,
    background_cancel: Mutex<Option<CancellationToken>>,
    presence_until: AtomicI64,
    policy: AutonomyPolicy,
    pub(crate) voice: VoiceSettings,
    pub modules: Modules,
    pub composition: ResolvedComposition,
    pub(crate) control: ModuleControl,
    pub(crate) shutdown: CancellationToken,
    tasks: TaskTracker,
}

impl Service {
    pub async fn new(
        repo: Arc<dyn Repository>,
        clock: Arc<dyn Clock>,
        providers: Vec<Arc<dyn InferenceProvider>>,
        policy: AutonomyPolicy,
        voice: VoiceSettings,
        modules: Modules,
        composition: ResolvedComposition,
    ) -> Result<Arc<Self>> {
        if policy.interval_seconds < 60
            || policy.max_actions > 3
            || policy.max_actions == 0
            || policy.daily_inference_limit > 1000
        {
            bail!("invalid autonomy limits");
        }
        let mut registry = BTreeMap::new();
        for provider in providers {
            if registry
                .insert(provider.profile().id.clone(), provider)
                .is_some()
            {
                bail!("duplicate provider ID");
            }
        }
        let character = repo.character().await?;
        if composition.catalog.iter().any(|d| {
            composition.enabled(&d.id) && d.provides.iter().any(|c| c.id == "inference.generate")
        }) && !registry.contains_key(&character.active_provider)
        {
            bail!(
                "persisted provider is missing; explicitly configure it instead of silently falling back"
            );
        }
        repo.cancel_all_generations().await?;
        repo.recover_operations().await?;
        let (events, _) = broadcast::channel(512);
        let control = ModuleControl::new(&composition);
        let service = Arc::new(Self {
            repo,
            clock,
            providers: registry,
            events,
            turns: Mutex::new(BTreeMap::new()),
            turn_gate: Mutex::new(()),
            background_gate: Mutex::new(()),
            background_cancel: Mutex::new(None),
            presence_until: AtomicI64::new(0),
            policy,
            voice,
            modules,
            composition,
            control,
            shutdown: CancellationToken::new(),
            tasks: TaskTracker::new(),
        });
        service.initialize_modules().await?;
        Ok(service)
    }
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.events.subscribe()
    }
    pub fn provider_profiles(&self) -> Vec<ProviderProfile> {
        self.providers
            .values()
            .map(|p| p.profile().clone())
            .collect()
    }
    pub fn voice_settings(&self) -> &VoiceSettings {
        &self.voice
    }
    pub fn set_presence(&self, present: bool) {
        self.presence_until.store(
            if present { self.clock.now() + 60 } else { 0 },
            Ordering::Relaxed,
        );
    }
    pub fn emit(
        &self,
        kind: &str,
        session: Option<&str>,
        generation: Option<&str>,
        audience: Audience,
        payload: Value,
    ) {
        let _ = self.events.send(EventEnvelope {
            protocol_version: PROTOCOL_VERSION.into(),
            id: Uuid::new_v4().to_string(),
            kind: kind.into(),
            session_id: session.map(Into::into),
            generation_id: generation.map(Into::into),
            audience: wire_audience(audience),
            payload,
        });
    }
    pub async fn snapshot(&self, audience: Audience) -> Result<Snapshot> {
        // Do not return an optimistic revision paired with older memory contents.
        for _ in 0..4 {
            let before = self.repo.character().await?;
            let memories = self
                .repo
                .memories(audience, false, self.clock.now(), 100)
                .await?;
            let sources = self.repo.sources(audience, 100).await?;
            let activities = self.repo.activities(audience, 50).await?;
            let operations = if audience == Audience::Private {
                self.repo.operations(50).await?
            } else {
                Vec::new()
            };
            if self.repo.character().await?.revision == before.revision {
                let mut visible = before;
                if audience == Audience::Public {
                    visible.persona.clear();
                }
                return Ok(Snapshot {
                    character: visible,
                    memories,
                    sources,
                    activities,
                    operations,
                });
            }
        }
        bail!("state is busy; retry snapshot")
    }
    pub async fn switch_provider(&self, id: &str, revision: u64) -> Result<Character> {
        self.require_capability("inference.generate")?;
        if !self.providers.contains_key(id) {
            bail!("provider is not configured");
        }
        let _gate = self.turn_gate.lock().await;
        if !self.turns.lock().await.is_empty() {
            bail!("cancel the active turn before switching provider");
        }
        let result = self
            .repo
            .switch_provider(id, revision, self.clock.now())
            .await?;
        self.emit(
            "character.provider_changed",
            None,
            None,
            Audience::Private,
            json!({"provider":id,"revision":result.revision}),
        );
        Ok(result)
    }
    async fn provider(
        &self,
        audience: Audience,
        selected: Option<&str>,
    ) -> Result<Arc<dyn InferenceProvider>> {
        self.require_module("inference.providers")?;
        let id = match selected {
            Some(value) => value.to_owned(),
            None => self.repo.character().await?.active_provider,
        };
        let provider = self
            .providers
            .get(&id)
            .context("provider is not configured")?
            .clone();
        if provider.profile().cloud
            && audience == Audience::Private
            && !provider.profile().allow_private
        {
            bail!("private data is not allowed for this cloud provider");
        }
        Ok(provider)
    }
    async fn context(
        &self,
        provider: &dyn InferenceProvider,
        a: Audience,
        mode: DialogueMode,
        request: &str,
    ) -> Result<GenerationContext> {
        let policy = self
            .modules
            .dialogue
            .as_ref()
            .ok_or_else(|| inori_module_api::unavailable("dialogue.respond", "disabled"))?;
        let mut character = self.repo.character().await?;
        if a == Audience::Public {
            character.persona.clear();
        }
        let memories = self.repo.memories(a, false, self.clock.now(), 30).await?;
        let sources = self.repo.sources(a, 40).await?;
        let awareness = if self.composition.connected(policy.id(), "awareness") {
            if let Some(module) = &self.modules.awareness {
                let _slot = self.module_slot(module.id())?;
                Some(module.observe(
                    self.clock.now(),
                    self.presence_until.load(Ordering::Relaxed) > self.clock.now(),
                )?)
            } else {
                None
            }
        } else {
            None
        };
        let _slot = self.module_slot(policy.id())?;
        policy.prepare(DialogueInput {
            character,
            memories,
            sources,
            audience: a,
            mode,
            request: request.into(),
            profile: provider.profile().clone(),
            awareness,
        })
    }
    fn validate_inference(
        &self,
        context: &GenerationContext,
        provider: &dyn InferenceProvider,
        a: Audience,
    ) -> Result<()> {
        if context.audience != a {
            bail!(DomainError::PrivateEvidence);
        }
        if provider.profile().cloud && a == Audience::Private && !provider.profile().allow_private {
            bail!("private data is not allowed for this cloud provider");
        }
        if context.max_output_tokens > provider.profile().max_output_tokens
            || context.system.chars().count()
                + context
                    .messages
                    .iter()
                    .map(|(_, s)| s.chars().count())
                    .sum::<usize>()
                > provider.profile().max_context_chars
        {
            bail!("module output exceeds inference budget");
        }
        Ok(())
    }
    async fn collect(
        &self,
        provider: &dyn InferenceProvider,
        mut context: GenerationContext,
        cancel: CancellationToken,
        a: Audience,
    ) -> Result<String> {
        context.max_output_tokens = context
            .max_output_tokens
            .min(provider.profile().max_output_tokens);
        self.validate_inference(&context, provider, a)?;
        let _slot = self.inference_slot(cancel.clone()).await?;
        providers::collect(provider, context, cancel).await
    }
    pub async fn start_chat(
        self: &Arc<Self>,
        request: ChatRequest,
        device_id: &str,
    ) -> Result<ChatAccepted> {
        let dialogue = self
            .modules
            .dialogue
            .as_ref()
            .ok_or_else(|| inori_module_api::unavailable("dialogue.respond", "disabled"))?;
        self.require_module(dialogue.id())?;
        validate_text(&request.text, MAX_TEXT_BYTES)?;
        validate_text(&request.session_id, 128)?;
        let a = domain_audience(request.audience);
        let provider = self.provider(a, None).await?;
        if request.speak && self.modules.synthesizer.is_none() {
            return Err(inori_module_api::unavailable(
                "speech.synthesize",
                "disabled or unconfigured",
            ));
        }
        if request.speak && !self.composition.connected(dialogue.id(), "synthesis") {
            return Err(inori_module_api::unavailable(
                "speech.synthesize",
                "dialogue synthesis is not connected",
            ));
        }
        if request.text.chars().count() > provider.profile().max_context_chars / 2 {
            bail!("message exceeds the selected provider context budget");
        }
        if request.speak {
            let synth = self.modules.synthesizer.as_ref().unwrap();
            self.require_module(synth.id())?;
            if a == Audience::Private && synth.remote() && !self.voice.allow_private_remote {
                bail!(DomainError::PermissionDenied);
            }
        }
        let _gate = self.turn_gate.lock().await;
        if let Some(background) = self.background_cancel.lock().await.as_ref() {
            background.cancel();
        }
        {
            let mut turns = self.turns.lock().await;
            if turns.values().any(|t| {
                t.device != device_id || t.session != request.session_id || t.audience != a
            }) {
                bail!(
                    "another session is speaking; it must be explicitly interrupted by its owner"
                );
            }
            for (id, turn) in turns.iter() {
                turn.cancel.cancel();
                self.repo.cancel_generation(id).await?;
            }
            turns.clear();
        }
        self.repo.cancel_all_generations().await?;
        let turn_id = Uuid::new_v4().to_string();
        let generation_id = Uuid::new_v4().to_string();
        let source_id = Uuid::new_v4().to_string();
        self.repo
            .append_source(SourceRecord {
                id: source_id.clone(),
                actor: Actor::User,
                text: request.text.clone(),
                created_at: self.clock.now(),
                audience: a,
                turn_id: Some(turn_id.clone()),
                model: None,
                suppressed: false,
            })
            .await?;
        self.repo
            .new_generation(&generation_id, &turn_id, a)
            .await?;
        let cancel = self.shutdown.child_token();
        self.turns.lock().await.insert(
            generation_id.clone(),
            ActiveTurn {
                cancel: cancel.clone(),
                device: device_id.into(),
                session: request.session_id.clone(),
                audience: a,
            },
        );
        self.set_presence(true);
        let accepted = ChatAccepted {
            turn_id: turn_id.clone(),
            generation_id: generation_id.clone(),
            source_id: source_id.clone(),
        };
        self.emit(
            "turn.started",
            Some(&request.session_id),
            Some(&generation_id),
            a,
            json!({"turn_id":turn_id,"source_id":source_id}),
        );
        let this = self.clone();
        self.tasks.spawn(async move {
            let result = this
                .run_turn(
                    request.clone(),
                    provider,
                    generation_id.clone(),
                    turn_id,
                    source_id,
                    cancel.clone(),
                )
                .await;
            if let Err(error) = result {
                cancel.cancel();
                let _ = this.repo.cancel_generation(&generation_id).await;
                tracing::warn!(error=%error,"turn failed");
                this.emit(
                    "turn.error",
                    Some(&request.session_id),
                    Some(&generation_id),
                    a,
                    json!({"message":error.to_string()}),
                );
            }
            this.turns.lock().await.remove(&generation_id);
        });
        Ok(accepted)
    }
    async fn run_turn(
        self: &Arc<Self>,
        request: ChatRequest,
        provider: Arc<dyn InferenceProvider>,
        generation: String,
        turn: String,
        source_id: String,
        cancel: CancellationToken,
    ) -> Result<()> {
        let a = domain_audience(request.audience);
        let started = std::time::Instant::now();
        let context = self
            .context(provider.as_ref(), a, request.mode, &request.text)
            .await?;
        self.validate_inference(&context, provider.as_ref(), a)?;
        let inference_slot = self.inference_slot(cancel.clone()).await?;
        let (tx, mut rx) = mpsc::channel::<String>(16);
        let model = provider.profile().model.clone();
        let p = provider.clone();
        let c = cancel.clone();
        let producer = self
            .tasks
            .spawn(async move {
                let _inference_slot=inference_slot;
                tokio::select! {
                    biased;
                    _=c.cancelled()=>Ok(()),
                    result=tokio::time::timeout(std::time::Duration::from_secs(p.profile().timeout_seconds),p.generate(context,tx,c.clone()))=>result.context("provider generation timed out")?,
                }
            });
        let (voice_tx, voice_rx) = mpsc::channel::<(u64, String)>(2);
        let voice_worker = if request.speak {
            let this = self.clone();
            let g = generation.clone();
            let s = request.session_id.clone();
            let c = cancel.clone();
            Some(
                self.tasks
                    .spawn(async move { this.speech_worker(g, s, a, voice_rx, c).await }),
            )
        } else {
            drop(voice_rx);
            None
        };
        let mut whole = String::new();
        let mut sentence = String::new();
        let mut index = 0;
        let mut first = None;
        loop {
            let chunk = tokio::select! {_=cancel.cancelled()=>break,chunk=rx.recv()=>chunk};
            let Some(chunk) = chunk else { break };
            if first.is_none() {
                first = Some(started.elapsed().as_millis());
            }
            if whole.len() + chunk.len() > 65_536 {
                cancel.cancel();
                bail!("response is too large");
            }
            whole.push_str(&chunk);
            self.emit(
                "text.delta",
                Some(&request.session_id),
                Some(&generation),
                a,
                json!({"text":chunk}),
            );
            if request.speak {
                sentence.push_str(&chunk);
                while let Some(end) = sentence_boundary(&sentence) {
                    let text: String = sentence.drain(..end).collect();
                    tokio::select! {_=cancel.cancelled()=>break,result=voice_tx.send((index,text))=>{result.context("speech worker stopped")?;index+=1;}}
                }
            }
        }
        if !cancel.is_cancelled() && request.speak && !sentence.trim().is_empty() {
            voice_tx
                .send((index, sentence))
                .await
                .context("speech worker stopped")?;
        }
        drop(voice_tx);
        producer.await??;
        if let Some(worker) = voice_worker {
            worker.await??;
        }
        if cancel.is_cancelled() {
            self.emit(
                "turn.cancelled",
                Some(&request.session_id),
                Some(&generation),
                a,
                json!({}),
            );
            return Ok(());
        }
        if !request.speak && !whole.trim().is_empty() {
            self.repo
                .append_source(SourceRecord {
                    id: Uuid::new_v4().to_string(),
                    actor: Actor::Assistant,
                    text: whole.clone(),
                    created_at: self.clock.now(),
                    audience: a,
                    turn_id: Some(turn),
                    model: Some(model),
                    suppressed: false,
                })
                .await?;
        }
        self.emit("turn.completed",Some(&request.session_id),Some(&generation),a,json!({"text":whole,"first_text_ms":first,"total_ms":started.elapsed().as_millis(),"memory_source":source_id}));
        Ok(())
    }
    pub async fn interrupt(&self, id: &str, device: &str, admin: bool) -> Result<()> {
        let turns = self.turns.lock().await;
        if let Some(turn) = turns.get(id) {
            if !admin && turn.device != device {
                bail!(DomainError::PermissionDenied);
            }
            turn.cancel.cancel();
        } else if !admin {
            bail!("active generation not found");
        }
        self.repo.cancel_generation(id).await?;
        self.emit(
            "turn.cancelled",
            None,
            Some(id),
            Audience::Private,
            json!({}),
        );
        Ok(())
    }
    async fn speech_worker(
        &self,
        generation: String,
        session: String,
        a: Audience,
        mut queue: mpsc::Receiver<(u64, String)>,
        cancel: CancellationToken,
    ) -> Result<()> {
        while let Some((index, text)) =
            tokio::select! {_=cancel.cancelled()=>None,item=queue.recv()=>item}
        {
            if !self.repo.generation_active(&generation).await? {
                return Ok(());
            }
            let synth =
                self.modules.synthesizer.as_ref().ok_or_else(|| {
                    inori_module_api::unavailable("speech.synthesize", "disabled")
                })?;
            if a == Audience::Private && synth.remote() && !self.voice.allow_private_remote {
                bail!(DomainError::PermissionDenied);
            }
            let _slot = self.module_slot(synth.id())?;
            let data = synth.synthesize(&text, cancel.clone()).await?;
            if cancel.is_cancelled() || !self.repo.generation_active(&generation).await? {
                return Ok(());
            }
            self.repo.add_segment(&generation, index, &text).await?;
            let resource = self
                .repo
                .put_resource("audio/wav", a, &data, self.clock.now())
                .await?;
            self.emit(
                "speech.segment",
                Some(&session),
                Some(&generation),
                a,
                json!({"segment_id":index,"text":text,"resource":resource}),
            );
        }
        Ok(())
    }
    pub async fn transcribe(&self, id: &str, a: Audience) -> Result<String> {
        let (resource, bytes) = self
            .repo
            .resource(id)
            .await?
            .context("resource not found")?;
        if !a.permits(resource.audience) || resource.mime != "audio/wav" {
            bail!(DomainError::PermissionDenied);
        }
        if bytes.get(..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
            bail!("expected a WAV recording");
        }
        let recognizer = self
            .modules
            .recognizer
            .as_ref()
            .ok_or_else(|| inori_module_api::unavailable("speech.recognize", "disabled"))?;
        if a == Audience::Private && recognizer.remote() && !self.voice.allow_private_remote {
            bail!(DomainError::PermissionDenied);
        }
        let _slot = self.module_slot(recognizer.id())?;
        recognizer
            .transcribe(bytes, self.shutdown.child_token())
            .await
    }
    pub fn start_workers(self: &Arc<Self>) {
        let maintenance = self.clone();
        self.tasks.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {_=maintenance.shutdown.cancelled()=>break,_=interval.tick()=>{}}
                if let Err(error) = maintenance.repo.housekeeping(maintenance.clock.now()).await {
                    tracing::warn!(error=%error,"maintenance failed");
                }
            }
        });
        let this = self.clone();
        self.tasks.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {_=this.shutdown.cancelled()=>break,_=interval.tick()=>{}}
                if !this.turns.lock().await.is_empty() {
                    continue;
                }
                if let Err(error) = this.background_pass().await {
                    tracing::debug!(error=%error,"background pass deferred");
                }
            }
        });
    }
    async fn background_pass(&self) -> Result<()> {
        let _gate = self
            .background_gate
            .try_lock()
            .context("background work already running")?;
        let cancel = self.shutdown.child_token();
        *self.background_cancel.lock().await = Some(cancel.clone());
        if self.modules.memory.is_none() && self.modules.autonomy.is_none() {
            return Ok(());
        }
        let provider = self.provider(Audience::Private, None).await?;
        if provider.profile().cloud && !self.policy.cloud_enabled {
            return Ok(());
        }
        let now = self.clock.now();
        let used = self
            .repo
            .count_activities("inference", now - now.rem_euclid(86400))
            .await?;
        if used >= self.policy.daily_inference_limit {
            return Ok(());
        }
        if self.modules.memory.is_some()
            && let Some(source) = self
                .repo
                .pending_memory_sources(1)
                .await?
                .into_iter()
                .next()
        {
            self.extract_memory(provider.as_ref(), source, cancel)
                .await?;
            return Ok(());
        }
        if self.modules.autonomy.is_some() {
            self.autonomy_tick_inner(provider).await?;
        }
        Ok(())
    }
    async fn record_inference(&self, purpose: &str) -> Result<()> {
        self.repo
            .add_activity(Activity {
                id: Uuid::new_v4().to_string(),
                kind: "inference".into(),
                text: purpose.into(),
                created_at: self.clock.now(),
                audience: Audience::Private,
            })
            .await
    }
    async fn extract_memory(
        &self,
        provider: &dyn InferenceProvider,
        source: SourceRecord,
        cancel: CancellationToken,
    ) -> Result<()> {
        if source.suppressed {
            return Ok(());
        }
        let expected = self.repo.character().await?.revision;
        self.record_inference("memory extraction attempt").await?;
        let processor = self
            .modules
            .memory
            .as_ref()
            .ok_or_else(|| inori_module_api::unavailable("memory.process", "disabled"))?;
        let _slot = self.module_slot(processor.id())?;
        let context = processor.extraction(&source);
        let text = self
            .collect(provider, context, cancel.clone(), source.audience)
            .await?;
        if cancel.is_cancelled() {
            return Ok(());
        }
        let mut revision = expected;
        for item in processor.interpret(&text, self.policy.max_actions)? {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let proposal = MemoryProposal {
                text: item.text,
                kind: MemoryKind::Inference,
                audience: source.audience,
                source_ids: vec![source.id.clone()],
                expires_at: item.expires_at,
                model: Some(format!(
                    "{}/{}",
                    provider.profile().id,
                    provider.profile().model
                )),
                processor_version: format!(
                    "{}/{}:extract/1",
                    processor.id(),
                    self.composition.descriptor(processor.id()).unwrap().version
                ),
                expected_revision: revision,
            };
            match self.repo.propose(proposal, false, self.clock.now()).await {
                Ok(m) => revision = m.revision,
                Err(error) => {
                    if error.downcast_ref::<DomainError>() == Some(&DomainError::StaleRevision) {
                        break;
                    }
                    return Err(error);
                }
            }
        }
        self.repo
            .add_activity(Activity {
                id: format!("extract:{}", source.id),
                kind: format!("extract:{}", source.id),
                text: "原記録に基づく記憶候補の抽出を処理しました。".into(),
                created_at: self.clock.now(),
                audience: source.audience,
            })
            .await?;
        self.emit(
            "memory.updated",
            None,
            None,
            source.audience,
            json!({"revision":revision}),
        );
        Ok(())
    }
    pub async fn review_memory(&self, id: &str, a: Audience) -> Result<Value> {
        let processor = self
            .modules
            .memory
            .as_ref()
            .ok_or_else(|| inori_module_api::unavailable("memory.process", "disabled"))?;
        let _slot = self.module_slot(processor.id())?;
        let provider = self.provider(a, Some(id)).await?;
        let snapshot = self.snapshot(a).await?;
        let mut evidence = Vec::new();
        let mut used = 512usize;
        for memory in &snapshot.memories {
            let mut originals = Vec::new();
            for id in &memory.source_ids {
                if let Some(source) = self.repo.source(id).await?
                    && !source.suppressed
                    && a.permits(source.audience)
                {
                    originals.push(source);
                }
            }
            let item =
                json!({"memory":memory,"originals":originals,"verifiable":!originals.is_empty()});
            let n = item.to_string().chars().count();
            if used + n > provider.profile().max_context_chars {
                break;
            }
            used += n;
            evidence.push(item);
        }
        let context = processor.review(
            a,
            serde_json::to_value(evidence)?,
            provider.profile().max_output_tokens,
        )?;
        let text = self
            .collect(provider.as_ref(), context, self.shutdown.child_token(), a)
            .await?;
        Ok(
            json!({"provider":id,"revision":snapshot.character.revision,"review":text,"applied":false}),
        )
    }
    pub async fn autonomy_tick(&self, provider: Arc<dyn InferenceProvider>) -> Result<bool> {
        let _gate = self
            .background_gate
            .try_lock()
            .context("background work already running")?;
        *self.background_cancel.lock().await = Some(self.shutdown.child_token());
        self.autonomy_tick_inner(provider).await
    }
    async fn autonomy_tick_inner(&self, provider: Arc<dyn InferenceProvider>) -> Result<bool> {
        let planner = self
            .modules
            .autonomy
            .as_ref()
            .ok_or_else(|| inori_module_api::unavailable("autonomy.plan", "disabled"))?;
        let _slot = self.module_slot(planner.id())?;
        let now = self.clock.now();
        if !self.policy.enabled
            || self.presence_until.load(Ordering::Relaxed) > now
            || !self.turns.lock().await.is_empty()
        {
            return Ok(false);
        }
        if provider.profile().cloud && !self.policy.cloud_enabled {
            return Ok(false);
        }
        let latest = self.repo.last_activity_at("autonomy").await?.unwrap_or(0);
        if now.saturating_sub(latest) < self.policy.interval_seconds as i64 {
            return Ok(false);
        }
        let used = self
            .repo
            .count_activities("inference", now - now.rem_euclid(86400))
            .await?;
        if used >= self.policy.daily_inference_limit {
            return Ok(false);
        }
        self.record_inference("autonomy reflection attempt").await?;
        let character = self.repo.character().await?;
        let awareness = self
            .modules
            .awareness
            .as_ref()
            .ok_or_else(|| inori_module_api::unavailable("awareness.context", "disabled"))?;
        let observation = {
            let _slot = self.module_slot(awareness.id())?;
            awareness.observe(now, false)?
        };
        let context = planner.prepare(&character, observation);
        let cancel = self
            .background_cancel
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| self.shutdown.child_token());
        let text = self
            .collect(
                provider.as_ref(),
                context,
                cancel.clone(),
                Audience::Private,
            )
            .await?;
        if cancel.is_cancelled()
            || !self.turns.lock().await.is_empty()
            || self.repo.character().await?.revision != character.revision
        {
            return Ok(false);
        }
        self.repo
            .add_activity(Activity {
                id: Uuid::new_v4().to_string(),
                kind: "autonomy".into(),
                text,
                created_at: now,
                audience: Audience::Private,
            })
            .await?;
        self.emit(
            "activity.recorded",
            None,
            None,
            Audience::Private,
            json!({"kind":"autonomy"}),
        );
        Ok(true)
    }
    pub async fn stop(&self) {
        self.control.ready.store(false, Ordering::Release);
        self.shutdown.cancel();
        for turn in self.turns.lock().await.values() {
            turn.cancel.cancel();
        }
        let _ = self.repo.cancel_all_generations().await;
        self.tasks.close();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let _ = tokio::time::timeout_at(deadline, async {
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(1), self.tasks.wait()).await;
            self.stop_modules().await;
        })
        .await;
        self.control.ready.store(false, Ordering::Release);
    }
}

fn sentence_boundary(text: &str) -> Option<usize> {
    for (index, ch) in text.char_indices() {
        if "。！？!?\n".contains(ch) {
            return Some(index + ch.len_utf8());
        }
    }
    if text.chars().count() > 120 {
        text.char_indices().nth(100).map(|(index, _)| index)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sentence_split_does_not_cut_multibyte_text() {
        let text = "こんにちは。次の文です。";
        let end = sentence_boundary(text).unwrap();
        assert_eq!(&text[..end], "こんにちは。");
    }
}
