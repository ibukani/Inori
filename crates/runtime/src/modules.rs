use crate::{Repository, Service};
use anyhow::{Result, bail};
use async_trait::async_trait;
use inori_domain::check_permission;
use inori_module_api::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

pub struct RepositoryPermissionGuard {
    pub repo: Arc<dyn Repository>,
}
#[async_trait]
impl PermissionGuard for RepositoryPermissionGuard {
    async fn check(&self, e: &str, c: &str, a: &str, t: &str) -> Result<()> {
        Ok(check_permission(&self.repo.grants().await?, e, c, a, t)?)
    }
}

pub(crate) struct ModuleControl {
    pub ready: Arc<AtomicBool>,
    pub states: Mutex<BTreeMap<String, ModuleStatus>>,
    pub slots: BTreeMap<String, Arc<Semaphore>>,
    commits: Arc<tokio::sync::Mutex<()>>,
}
impl ModuleControl {
    pub fn new(graph: &inori_composition::ResolvedComposition) -> Self {
        Self {
            ready: Arc::new(AtomicBool::new(false)),
            commits: Arc::new(tokio::sync::Mutex::new(())),
            states: Mutex::new(
                graph
                    .catalog
                    .iter()
                    .map(|d| {
                        (
                            d.id.clone(),
                            ModuleStatus {
                                id: d.id.clone(),
                                enabled: graph.enabled(&d.id),
                                state: if graph.enabled(&d.id) {
                                    "starting"
                                } else {
                                    "disabled"
                                }
                                .into(),
                                reason: if graph.enabled(&d.id) {
                                    None
                                } else {
                                    Some("disabled by composition".into())
                                },
                            },
                        )
                    })
                    .collect(),
            ),
            slots: graph
                .catalog
                .iter()
                .filter(|d| graph.enabled(&d.id))
                .map(|d| {
                    (
                        d.id.clone(),
                        Arc::new(Semaphore::new(d.max_concurrency as usize)),
                    )
                })
                .collect(),
        }
    }
    pub fn status(&self, id: &str, state: &str, reason: Option<&str>) {
        if let Some(s) = self
            .states
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(id)
        {
            s.state = state.into();
            s.reason = reason.map(Into::into);
        }
    }
}
struct StateHost {
    repo: Arc<dyn Repository>,
    id: String,
    version: u64,
    ready: Arc<AtomicBool>,
    cancel: CancellationToken,
    commits: Arc<tokio::sync::Mutex<()>>,
}
#[async_trait]
impl ModuleHost for StateHost {
    async fn save_state(&self, revision: u64, data: Value) -> Result<ModuleState> {
        let _commit = self.commits.lock().await;
        if !self.ready.load(Ordering::Acquire) || self.cancel.is_cancelled() {
            bail!("module invocation is no longer active");
        }
        self.repo
            .save_module_state(&self.id, self.version, revision, data)
            .await
    }
}
impl Service {
    pub(crate) async fn initialize_modules(&self) -> Result<()> {
        let ids: Vec<_> = self.modules.lifecycle.iter().map(|m| m.id()).collect();
        if ids.len() != self.composition.order.len()
            || self
                .composition
                .order
                .iter()
                .any(|id| ids.iter().filter(|candidate| **candidate == id).count() != 1)
        {
            bail!("module instances do not match resolved composition");
        }
        for id in &self.composition.order {
            let module = self
                .modules
                .lifecycle
                .iter()
                .find(|m| m.id() == id)
                .unwrap();
            let version = self
                .composition
                .descriptor(id)
                .unwrap()
                .state_schema_version;
            let result = async {
                let state = self.repo.ensure_module_state(id, version).await?;
                let host = Arc::new(StateHost {
                    repo: self.repo.clone(),
                    id: id.clone(),
                    version,
                    ready: self.control.ready.clone(),
                    cancel: self.shutdown.child_token(),
                    commits: self.control.commits.clone(),
                });
                module
                    .start(ModuleContext {
                        state,
                        host,
                        cancel: self.shutdown.child_token(),
                    })
                    .await
            }
            .await;
            if let Err(error) = result {
                self.control
                    .status(id, "failed", Some("module preparation failed"));
                self.shutdown.cancel();
                self.stop_modules().await;
                return Err(error);
            }
            self.control.status(id, "ready", None);
        }
        if let Err(error)=self.repo.record_composition(&self.composition.fingerprint,json!({"composition":self.composition.config,"versions":self.composition.catalog.iter().map(|d|(&d.id,&d.version)).collect::<BTreeMap<_,_>>()}),self.clock.now()).await{self.shutdown.cancel();self.stop_modules().await;return Err(error);}
        self.control.ready.store(true, Ordering::Release);
        for id in &self.composition.order {
            self.control.status(id, "running", None);
        }
        Ok(())
    }
    pub fn require_module(&self, id: &str) -> Result<()> {
        if !self.control.ready.load(Ordering::Acquire)
            || self.shutdown.is_cancelled()
            || !self.composition.enabled(id)
        {
            return Err(unavailable(id, "disabled or stopping"));
        }
        let states = self
            .control
            .states
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if states
            .get(id)
            .is_some_and(|s| s.state == "failed" || s.state == "stopped")
        {
            return Err(unavailable(id, "module failed"));
        }
        Ok(())
    }
    pub fn require_standard(&self) -> Result<()> {
        let module = self
            .modules
            .channel
            .as_ref()
            .ok_or_else(|| unavailable("channel.standard", "disabled"))?;
        self.require_module(module.id())
    }
    pub fn require_capability(&self, capability: &str) -> Result<()> {
        let module = self
            .composition
            .catalog
            .iter()
            .find(|d| {
                self.composition.enabled(&d.id) && d.provides.iter().any(|c| c.id == capability)
            })
            .ok_or_else(|| unavailable(capability, "disabled"))?;
        self.require_module(&module.id)
    }
    pub fn require_channel_port(&self, port: &str) -> Result<()> {
        self.require_standard()?;
        let channel = self.modules.channel.as_ref().unwrap();
        let binding = self
            .composition
            .config
            .bindings
            .iter()
            .find(|b| b.module == channel.id() && b.port == port)
            .ok_or_else(|| unavailable(port, "channel capability is not connected"))?;
        self.require_module(&binding.provider)
    }
    pub(crate) fn module_slot(&self, id: &str) -> Result<OwnedSemaphorePermit> {
        self.require_module(id)?;
        self.control
            .slots
            .get(id)
            .ok_or_else(|| unavailable(id, "not registered"))?
            .clone()
            .try_acquire_owned()
            .map_err(|_| ModuleError::Busy(id.into()).into())
    }
    pub(crate) async fn inference_slot(
        &self,
        cancel: CancellationToken,
    ) -> Result<OwnedSemaphorePermit> {
        let id = self
            .composition
            .catalog
            .iter()
            .find(|d| {
                self.composition.enabled(&d.id)
                    && d.provides.iter().any(|c| c.id == "inference.generate")
            })
            .map(|d| d.id.as_str())
            .ok_or_else(|| unavailable("inference.generate", "disabled"))?;
        self.require_module(id)?;
        let semaphore = self.control.slots[id].clone();
        let permit = tokio::select! {biased;_=cancel.cancelled()=>bail!("inference cancelled"),result=tokio::time::timeout(std::time::Duration::from_secs(5),semaphore.acquire_owned())=>result.map_err(|_|ModuleError::Busy(id.into()))??};
        self.require_module(id)?;
        if cancel.is_cancelled() {
            bail!("inference cancelled");
        }
        Ok(permit)
    }
    pub fn module_report(&self) -> Value {
        json!({"fingerprint":self.composition.fingerprint,"order":self.composition.order,"definitions":self.composition.catalog,"bindings":self.composition.config.bindings,"statuses":self.control.states.lock().unwrap_or_else(|p|p.into_inner()).values().cloned().collect::<Vec<_>>()})
    }
    pub fn capabilities(&self, a: inori_domain::Audience) -> ServerCapabilities {
        let channel = self
            .modules
            .channel
            .as_ref()
            .is_some_and(|m| self.require_module(m.id()).is_ok());
        let inference = self.composition.catalog.iter().any(|d| {
            d.provides.iter().any(|c| c.id == "inference.generate")
                && self.require_module(&d.id).is_ok()
        });
        let dialogue = self
            .modules
            .dialogue
            .as_ref()
            .is_some_and(|m| self.require_module(m.id()).is_ok());
        let private_voice =
            a == inori_domain::Audience::Private && !self.voice.allow_private_remote;
        let text_chat =
            channel && dialogue && inference && self.require_channel_port("dialogue").is_ok();
        ServerCapabilities {
            protocol_major: 1,
            text_chat,
            transcription: channel
                && self.require_channel_port("recognition").is_ok()
                && self.modules.recognizer.as_ref().is_some_and(|m| {
                    self.require_module(m.id()).is_ok() && !(private_voice && m.remote())
                }),
            speech: text_chat
                && self.require_channel_port("synthesis").is_ok()
                && self
                    .modules
                    .dialogue
                    .as_ref()
                    .is_some_and(|m| self.composition.connected(m.id(), "synthesis"))
                && self.modules.synthesizer.as_ref().is_some_and(|m| {
                    self.require_module(m.id()).is_ok() && !(private_voice && m.remote())
                }),
            presentation: channel,
            actions: channel
                && self.require_channel_port("actions").is_ok()
                && self
                    .modules
                    .actions
                    .as_ref()
                    .is_some_and(|m| self.require_module(m.id()).is_ok()),
            reason: if text_chat {
                None
            } else {
                Some("対話機能はサーバーの構成で無効になっています。".into())
            },
        }
    }
    pub fn negotiate(
        &self,
        hello: &ClientHello,
        a: inori_domain::Audience,
    ) -> Result<ServerCapabilities> {
        if let Some(channel) = &self.modules.channel {
            self.require_module(channel.id())?;
            channel.negotiate(hello, self.capabilities(a))
        } else {
            Ok(self.capabilities(a))
        }
    }
    pub(crate) async fn stop_modules(&self) {
        self.control.ready.store(false, Ordering::Release);
        let stop = async {
            {
                let _commit = self.control.commits.lock().await;
            }
            for id in self.composition.order.iter().rev() {
                let initialized = self
                    .control
                    .states
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(id)
                    .is_some_and(|s| {
                        matches!(
                            s.state.as_str(),
                            "ready" | "running" | "degraded" | "failed"
                        )
                    });
                if !initialized {
                    continue;
                }
                if let Some(module) = self.modules.lifecycle.iter().find(|m| m.id() == id) {
                    if let Ok(Some(data)) = module.stop().await
                        && let Ok(Some(state)) = self.repo.module_state(id).await
                    {
                        let _ = self
                            .repo
                            .save_module_state(id, state.state_schema_version, state.revision, data)
                            .await;
                    }
                    self.control.status(id, "stopped", None);
                }
            }
        };
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), stop).await;
    }
    pub async fn execute_operation(&self, r: inori_protocol::OperationRequest) -> Result<Value> {
        use inori_domain::{Operation, OperationStatus, validate_text};
        validate_text(&r.id, 128)?;
        if r.target != "server" || r.action != "invoke" {
            bail!("unsupported action or target");
        }
        // Recheck permission before preparing a transport and immediately before committing the ledger.
        check_permission(
            &self.repo.grants().await?,
            &r.extension,
            &r.capability,
            &r.action,
            &r.target,
        )?;
        let executor = self
            .modules
            .actions
            .as_ref()
            .ok_or_else(|| unavailable("actions.execute", "disabled"))?;
        let _slot = self.module_slot(executor.id())?;
        let call = executor.prepare(&r.extension, &r.capability, r.arguments)?;
        self.repo
            .begin_operation(Operation {
                id: r.id.clone(),
                extension: r.extension.clone(),
                capability: r.capability.clone(),
                action: r.action.clone(),
                target: r.target.clone(),
                status: OperationStatus::Running,
                result: None,
                created_at: self.clock.now(),
            })
            .await?;
        if let Err(error) = check_permission(
            &self.repo.grants().await?,
            &r.extension,
            &r.capability,
            &r.action,
            &r.target,
        ) {
            self.repo
                .finish_operation(
                    &r.id,
                    OperationStatus::Failed,
                    json!({"message":"permission revoked before invocation"}),
                )
                .await?;
            return Err(error.into());
        }
        match call.invoke(self.shutdown.child_token()).await {
            Ok(result) => {
                self.repo
                    .finish_operation(&r.id, OperationStatus::Succeeded, result.clone())
                    .await?;
                Ok(json!({"id":r.id,"status":"succeeded","result":result}))
            }
            Err(error) => {
                self.control.status(
                    executor.id(),
                    "degraded",
                    Some("extension completion unknown"),
                );
                self.repo
                    .finish_operation(
                        &r.id,
                        OperationStatus::Unknown,
                        json!({"message":"completion unknown; inspect before retrying"}),
                    )
                    .await?;
                Err(error)
            }
        }
    }
}
