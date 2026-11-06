use anyhow::{Context, Result, bail};
use inori_extension_host::{ExtensionProcess, RegisteredExtension};
use inori_module_api::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;
pub struct ExtensionActions {
    extensions: BTreeMap<String, RegisteredExtension>,
}
impl ExtensionActions {
    pub fn new(extensions: &BTreeMap<String, RegisteredExtension>) -> Self {
        Self {
            extensions: extensions.clone(),
        }
    }
}
module!(ExtensionActions, "actions.extensions");
struct Prepared {
    process: OwnedMutexGuard<ExtensionProcess>,
    capability: String,
    arguments: Value,
}
#[async_trait::async_trait]
impl ActionCall for Prepared {
    async fn invoke(mut self: Box<Self>, cancel: CancellationToken) -> Result<Value> {
        self.process
            .call(
                "invoke",
                json!({"capability":self.capability,"arguments":self.arguments}),
                cancel,
            )
            .await
    }
}
impl ActionExecutor for ExtensionActions {
    fn prepare(
        &self,
        extension: &str,
        capability: &str,
        arguments: Value,
    ) -> Result<Box<dyn ActionCall>> {
        let e = self
            .extensions
            .get(extension)
            .context("extension not installed")?;
        let validator = e
            .schemas
            .get(capability)
            .context("capability not declared")?;
        if !validator.is_valid(&arguments) {
            bail!("arguments do not match capability schema");
        }
        let process = e
            .process
            .clone()
            .try_lock_owned()
            .context("extension busy; no operation started")?;
        Ok(Box::new(Prepared {
            process,
            capability: capability.into(),
            arguments,
        }))
    }
}
pub fn definition() -> ModuleDescriptor {
    let mut d = descriptor("actions.extensions", "actions.execute", &[], no_settings());
    d.permissions = vec!["operations.invoke.granted".into()];
    d
}
