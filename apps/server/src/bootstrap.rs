//! Composition root: concrete module implementations are selected only here.
use crate::{RegisteredExtension, ServerConfig};
use anyhow::{Context, Result, bail};
use inori_composition::ResolvedComposition;
use inori_domain::{AutonomyPolicy, Character};
use inori_module_api::*;
use inori_runtime::{Clock, Repository, RepositoryPermissionGuard, Service};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

pub fn module_catalog() -> Vec<ModuleDescriptor> {
    vec![
        inori_perception_basic::definition(),
        inori_dialogue_standard::definition(),
        inori_memory_standard::definition(),
        inori_autonomy_standard::definition(),
        inori_inference_providers::definition(),
        inori_speech_whisper::definition(),
        inori_speech_voicevox::definition(),
        inori_actions_extensions::definition(),
        inori_channel_standard::definition(),
    ]
}
pub fn resolve_config(config: &ServerConfig) -> Result<ResolvedComposition> {
    let catalog = module_catalog();
    let definition = config.composition.clone().unwrap_or_else(|| {
        inori_composition::legacy(
            &catalog,
            config.voice.whisper_url.is_some(),
            config.voice.voicevox_url.is_some(),
        )
    });
    let graph = inori_composition::resolve(definition, catalog)?;
    if config.autonomy.interval_seconds < 60
        || config.autonomy.max_actions == 0
        || config.autonomy.max_actions > 3
        || config.autonomy.daily_inference_limit > 1000
    {
        bail!("invalid global activity budget");
    }
    if graph.enabled("inference.providers") {
        if config.providers.is_empty() || config.providers.len() > 32 {
            bail!("inference module requires 1..32 profiles");
        }
        let mut ids = BTreeSet::new();
        for p in &config.providers {
            if !ids.insert(&p.id) {
                bail!("duplicate inference profile");
            }
            inori_inference_providers::validate_profile(p)?;
        }
    }
    if graph.enabled("speech.whisper") {
        inori_speech_whisper::validate_url(
            config
                .voice
                .whisper_url
                .as_deref()
                .context("speech.whisper needs voice.whisper_url")?,
        )?;
    }
    if graph.enabled("speech.voicevox") {
        inori_speech_voicevox::validate_url(
            config
                .voice
                .voicevox_url
                .as_deref()
                .context("speech.voicevox needs voice.voicevox_url")?,
        )?;
    }
    let paths = selected_extensions(config, &graph)?;
    let mut manifests = BTreeMap::new();
    for path in paths {
        let manifest = inori_extension_host::read_manifest(&path)?;
        if manifests.insert(manifest.id.clone(), manifest).is_some() {
            bail!("duplicate extension ID");
        }
    }
    if graph.enabled("inference.providers") {
        for p in config.providers.iter().filter(|p| p.kind == "extension") {
            let manifest = manifests
                .get(p.extension.as_deref().context("extension ID required")?)
                .context("configured inference extension is missing")?;
            if !manifest
                .capabilities
                .iter()
                .any(|c| c["id"].as_str() == p.capability.as_deref())
            {
                bail!("configured inference capability is missing");
            }
        }
    }
    let context = json!({"providers":if graph.enabled("inference.providers"){config.providers.clone()}else{vec![]},"voice":config.voice,"autonomy":config.autonomy,"extensions":manifests.values().map(|m|(&m.id,m.protocol_major)).collect::<Vec<_>>()});
    graph.with_context(&context)
}
pub fn selected_extensions(
    config: &ServerConfig,
    graph: &ResolvedComposition,
) -> Result<Vec<PathBuf>> {
    if !graph.enabled("actions.extensions") && !graph.enabled("inference.providers") {
        return Ok(vec![]);
    }
    if config.extensions.len() > 64 {
        bail!("too many extensions");
    }
    let inference_ids: BTreeSet<_> = if graph.enabled("inference.providers") {
        config
            .providers
            .iter()
            .filter_map(|p| p.extension.as_deref())
            .collect()
    } else {
        BTreeSet::new()
    };
    if graph.enabled("actions.extensions") {
        return Ok(config.extensions.clone());
    }
    if inference_ids.is_empty() {
        return Ok(vec![]);
    }
    let mut result = Vec::new();
    for path in &config.extensions {
        if inference_ids.contains(inori_extension_host::read_manifest(path)?.id.as_str()) {
            result.push(path.clone());
        }
    }
    Ok(result)
}
pub fn validate_character(
    config: &ServerConfig,
    character: Option<&Character>,
    graph: &ResolvedComposition,
) -> Result<()> {
    if graph.enabled("inference.providers") {
        let active = character
            .map(|c| c.active_provider.as_str())
            .unwrap_or(&config.character.active_provider);
        if !config.providers.iter().any(|p| p.id == active) {
            bail!("persisted active provider is missing; explicitly configure it");
        }
    }
    Ok(())
}
pub fn build_providers(
    profiles: Vec<ProviderProfile>,
    extensions: &BTreeMap<String, RegisteredExtension>,
    repo: Arc<dyn Repository>,
) -> Result<Vec<Arc<dyn InferenceProvider>>> {
    inori_inference_providers::build_providers(
        profiles,
        extensions,
        Arc::new(RepositoryPermissionGuard { repo }),
    )
}

pub fn instantiate(
    config: &ServerConfig,
    graph: &ResolvedComposition,
    extensions: &BTreeMap<String, RegisteredExtension>,
) -> Result<Modules> {
    let mut modules = Modules::empty();
    for id in &graph.order {
        macro_rules! install {
            ($field:ident,$value:expr) => {{
                let value = Arc::new($value);
                modules.lifecycle.push(value.clone());
                modules.$field = Some(value);
            }};
        }
        match id.as_str() {
            "perception.basic" => install!(
                awareness,
                inori_perception_basic::BasicPerception::new(graph.settings(id))?
            ),
            "dialogue.standard" => install!(
                dialogue,
                inori_dialogue_standard::StandardDialogue::new(graph.settings(id))?
            ),
            "memory.standard" => install!(memory, inori_memory_standard::StandardMemory),
            "autonomy.standard" => install!(autonomy, inori_autonomy_standard::StandardAutonomy),
            "inference.providers" => modules
                .lifecycle
                .push(Arc::new(inori_inference_providers::ProviderModule)),
            "speech.whisper" => install!(
                recognizer,
                inori_speech_whisper::Whisper::new(
                    config
                        .voice
                        .whisper_url
                        .clone()
                        .context("whisper URL missing")?
                )?
            ),
            "speech.voicevox" => install!(
                synthesizer,
                inori_speech_voicevox::Voicevox::new(
                    config
                        .voice
                        .voicevox_url
                        .clone()
                        .context("VOICEVOX URL missing")?,
                    config.voice.speaker,
                    config.voice.cancellable_synthesis
                )?
            ),
            "actions.extensions" => install!(
                actions,
                inori_actions_extensions::ExtensionActions::new(extensions)
            ),
            "channel.standard" => install!(channel, inori_channel_standard::StandardChannel),
            _ => bail!("module has no registered implementation: {id}"),
        }
    }
    Ok(modules)
}
pub async fn build_service(
    config: &ServerConfig,
    repo: Arc<dyn Repository>,
    clock: Arc<dyn Clock>,
    extensions: &BTreeMap<String, RegisteredExtension>,
) -> Result<Arc<Service>> {
    let graph = resolve_config(config)?;
    validate_character(config, Some(&repo.character().await?), &graph)?;
    let modules = instantiate(config, &graph, extensions)?;
    let providers = if graph.enabled("inference.providers") {
        build_providers(config.providers.clone(), extensions, repo.clone())?
    } else {
        vec![]
    };
    Service::new(
        repo,
        clock,
        providers,
        config.autonomy.clone(),
        config.voice.clone(),
        modules,
        graph,
    )
    .await
}
/// Test/application bridge for the previous constructor, with explicitly injected engines.
pub async fn legacy_service(
    repo: Arc<dyn Repository>,
    clock: Arc<dyn Clock>,
    providers: Vec<Arc<dyn InferenceProvider>>,
    policy: AutonomyPolicy,
    voice: VoiceSettings,
) -> Result<Arc<Service>> {
    let config = ServerConfig {
        character: repo.character().await?,
        providers: providers.iter().map(|p| p.profile().clone()).collect(),
        autonomy: policy.clone(),
        voice: voice.clone(),
        ..Default::default()
    };
    // Already-injected engines need no executable manifest lookup.
    let catalog = module_catalog();
    let graph = inori_composition::resolve(
        inori_composition::legacy(
            &catalog,
            voice.whisper_url.is_some(),
            voice.voicevox_url.is_some(),
        ),
        catalog,
    )?;
    let modules = instantiate(&config, &graph, &BTreeMap::new())?;
    Service::new(repo, clock, providers, policy, voice, modules, graph).await
}
