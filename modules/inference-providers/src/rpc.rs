use super::HttpProvider;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use inori_extension_host::{ExtensionProcess, RegisteredExtension};
use inori_module_api::*;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
struct RpcProvider {
    profile: ProviderProfile,
    process: Arc<Mutex<ExtensionProcess>>,
    guard: Arc<dyn PermissionGuard>,
    extension: String,
    capability: String,
}
#[async_trait]
impl InferenceProvider for RpcProvider {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }
    async fn generate(
        &self,
        context: GenerationContext,
        output: mpsc::Sender<String>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let mut process = self.process.lock().await;
        self.guard
            .check(&self.extension, &self.capability, "infer", "server")
            .await?;
        let arguments = json!({"system":context.system,"messages":context.messages.iter().map(|(role,content)|json!({"role":role,"content":content})).collect::<Vec<_>>(),"max_output_tokens":context.max_output_tokens,"purpose":context.purpose,"audience":context.audience,"model":self.profile.model});
        let result = process
            .call(
                "invoke",
                json!({"capability":self.capability,"arguments":arguments}),
                cancel.clone(),
            )
            .await?;
        let text = result
            .get("text")
            .and_then(Value::as_str)
            .context("inference extension must return text")?;
        if text.len() > 65_536 {
            bail!("extension response exceeds output limit");
        }
        tokio::select! {_=cancel.cancelled()=>Ok(()),result=output.send(text.into())=>{result?;Ok(())}}
    }
}
pub fn build_providers(
    profiles: Vec<ProviderProfile>,
    extensions: &BTreeMap<String, RegisteredExtension>,
    guard: Arc<dyn PermissionGuard>,
) -> Result<Vec<Arc<dyn InferenceProvider>>> {
    build_providers_with_credentials(profiles, extensions, guard, &BTreeMap::new())
}
pub fn build_providers_with_credentials(
    profiles: Vec<ProviderProfile>,
    extensions: &BTreeMap<String, RegisteredExtension>,
    guard: Arc<dyn PermissionGuard>,
    credentials: &BTreeMap<String, String>,
) -> Result<Vec<Arc<dyn InferenceProvider>>> {
    profiles
        .into_iter()
        .map(|profile| {
            if profile.kind != "extension" {
                let credential = profile
                    .api_key_env
                    .as_ref()
                    .and_then(|key| credentials.get(key))
                    .cloned();
                return Ok(
                    Arc::new(HttpProvider::with_credential(profile, credential)?)
                        as Arc<dyn InferenceProvider>,
                );
            }
            if !(1024..=100_000).contains(&profile.max_context_chars)
                || profile.max_output_tokens == 0
                || profile.max_output_tokens > 4096
                || !(1..=300).contains(&profile.timeout_seconds)
            {
                bail!("invalid extension provider budgets");
            }
            let extension = profile
                .extension
                .clone()
                .context("extension provider needs extension ID")?;
            let capability = profile
                .capability
                .clone()
                .context("extension provider needs capability ID")?;
            let installed = extensions
                .get(&extension)
                .context("inference extension is not installed")?;
            if !installed.schemas.contains_key(&capability) {
                bail!("inference capability is not declared");
            }
            Ok(Arc::new(RpcProvider {
                profile,
                process: installed.process.clone(),
                guard: guard.clone(),
                extension,
                capability,
            }) as Arc<dyn InferenceProvider>)
        })
        .collect()
}
