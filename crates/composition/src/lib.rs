//! Deterministic graph validation, independent of runtime, databases and GUI.
use anyhow::{Context, Result, bail};
use inori_module_api::{CapabilityBinding, CompositionConfig, ModuleDescriptor};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedComposition {
    pub fingerprint: String,
    pub order: Vec<String>,
    pub config: CompositionConfig,
    pub catalog: Vec<ModuleDescriptor>,
}
impl ResolvedComposition {
    pub fn with_context(mut self, settings: &Value) -> Result<Self> {
        self.fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(
                &json!({"graph":self.fingerprint,"settings":settings})
            )?)
        );
        Ok(self)
    }
    pub fn enabled(&self, id: &str) -> bool {
        self.order.iter().any(|item| item == id)
    }
    pub fn descriptor(&self, id: &str) -> Option<&ModuleDescriptor> {
        self.catalog.iter().find(|d| d.id == id)
    }
    pub fn settings(&self, id: &str) -> Value {
        self.config
            .settings
            .get(id)
            .cloned()
            .unwrap_or_else(|| json!({}))
    }
    pub fn connected(&self, module: &str, port: &str) -> bool {
        self.config
            .bindings
            .iter()
            .any(|b| b.module == module && b.port == port)
    }
}

pub fn resolve(
    mut config: CompositionConfig,
    mut catalog: Vec<ModuleDescriptor>,
) -> Result<ResolvedComposition> {
    if config.schema_version != 1 {
        bail!("unsupported composition schema version");
    }
    if config.enabled.len() > 64 || config.bindings.len() > 256 || config.settings.len() > 64 {
        bail!("composition exceeds limits");
    }
    catalog.sort_by(|a, b| a.id.cmp(&b.id));
    let mut definitions = BTreeMap::new();
    for d in &catalog {
        if d.id.is_empty()
            || d.state_schema_version == 0
            || d.max_concurrency == 0
            || d.max_concurrency > 8
        {
            bail!("invalid module descriptor: {}", d.id);
        }
        if definitions.insert(d.id.as_str(), d).is_some() {
            bail!("duplicate module descriptor: {}", d.id);
        }
    }
    let enabled: BTreeSet<_> = config.enabled.iter().map(String::as_str).collect();
    if enabled.len() != config.enabled.len() {
        bail!("duplicate enabled module");
    }
    for id in config.settings.keys() {
        if !definitions.contains_key(id.as_str()) {
            bail!("settings for unknown module: {id}");
        }
        let validator = jsonschema::validator_for(&definitions[id.as_str()].settings_schema)
            .context("invalid module settings schema")?;
        let settings = &config.settings[id];
        if settings.to_string().len() > 65_536 || !validator.is_valid(settings) {
            bail!("invalid settings for module: {id}");
        }
    }
    let mut capabilities = BTreeMap::new();
    for id in &enabled {
        let d = definitions
            .get(id)
            .with_context(|| format!("unknown module: {id}"))?;
        let settings = config
            .settings
            .get(*id)
            .cloned()
            .unwrap_or_else(|| json!({}));
        let validator = jsonschema::validator_for(&d.settings_schema)
            .context("invalid module settings schema")?;
        if !validator.is_valid(&settings) {
            bail!("invalid settings for module: {id}");
        }
        if settings.to_string().len() > 65_536 {
            bail!("module settings exceed limit: {id}");
        }
        for cap in &d.provides {
            if capabilities.insert((&cap.id, cap.major), *id).is_some() {
                bail!("conflicting providers for capability: {}", cap.id);
            }
        }
    }
    let mut bound = BTreeSet::new();
    let mut edges: BTreeMap<String, BTreeSet<String>> = enabled
        .iter()
        .map(|id| (id.to_string(), BTreeSet::new()))
        .collect();
    for binding in &config.bindings {
        if !enabled.contains(binding.module.as_str())
            || !enabled.contains(binding.provider.as_str())
        {
            bail!("binding references a disabled or unknown module");
        }
        if !bound.insert((binding.module.clone(), binding.port.clone())) {
            bail!("duplicate capability binding");
        }
        let consumer = definitions[binding.module.as_str()];
        let needed = consumer
            .requires
            .iter()
            .find(|r| r.port == binding.port)
            .context("binding references an unknown port")?;
        if needed.major != binding.major
            || !definitions[binding.provider.as_str()]
                .provides
                .iter()
                .any(|p| p.id == needed.capability && p.major == binding.major)
        {
            bail!(
                "incompatible capability binding: {}.{}",
                binding.module,
                binding.port
            );
        }
        edges
            .get_mut(&binding.module)
            .unwrap()
            .insert(binding.provider.clone());
    }
    for id in &enabled {
        for need in &definitions[id].requires {
            if !need.optional && !bound.contains(&(id.to_string(), need.port.clone())) {
                bail!("missing dependency: {id}.{}", need.port);
            }
        }
    }
    let mut order = Vec::new();
    while !edges.is_empty() {
        let ready: Vec<_> = edges
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        if ready.is_empty() {
            bail!("cyclic module dependency");
        }
        for id in ready {
            edges.remove(&id);
            for deps in edges.values_mut() {
                deps.remove(&id);
            }
            order.push(id);
        }
    }
    config.enabled.sort();
    config
        .bindings
        .sort_by(|a, b| (&a.module, &a.port).cmp(&(&b.module, &b.port)));
    // Configuration contains environment-variable REFERENCES, never resolved credentials.
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(
            &json!({"config":config,"modules":catalog})
        )?)
    );
    Ok(ResolvedComposition {
        fingerprint,
        order,
        config,
        catalog,
    })
}

/// Legacy settings expand into an explicit graph, not an implicit cloud fallback.
pub fn legacy(catalog: &[ModuleDescriptor], whisper: bool, voicevox: bool) -> CompositionConfig {
    let enabled: Vec<_> = catalog
        .iter()
        .filter(|m| m.id != "speech.whisper" || whisper)
        .filter(|m| m.id != "speech.voicevox" || voicevox)
        .map(|m| m.id.clone())
        .collect();
    let mut bindings = Vec::new();
    for module in catalog.iter().filter(|m| enabled.contains(&m.id)) {
        for need in &module.requires {
            if let Some(provider) = catalog.iter().find(|p| {
                enabled.contains(&p.id)
                    && p.provides
                        .iter()
                        .any(|c| c.id == need.capability && c.major == need.major)
            }) {
                bindings.push(CapabilityBinding {
                    module: module.id.clone(),
                    port: need.port.clone(),
                    provider: provider.id.clone(),
                    major: need.major,
                });
            }
        }
    }
    CompositionConfig {
        schema_version: 1,
        enabled,
        bindings,
        settings: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inori_module_api::{descriptor, no_settings};
    #[test]
    fn validation_rejects_missing_cycles_versions_and_duplicates() {
        let a = descriptor("a", "one", &[("two", "two", false)], no_settings());
        let b = descriptor("b", "two", &[], no_settings());
        let catalog = vec![a.clone(), b.clone()];
        let mut config = legacy(&catalog, false, false);
        assert!(resolve(config.clone(), catalog.clone()).is_ok());
        config.bindings.clear();
        assert!(resolve(config, catalog.clone()).is_err());
        let mut config = legacy(&catalog, false, false);
        config.bindings[0].major = 99;
        assert!(resolve(config, catalog.clone()).is_err());
        let mut cyclic = b.clone();
        cyclic.requires = a.requires.clone();
        cyclic.requires[0].capability = "one".into();
        let cats = vec![a, cyclic];
        assert!(resolve(legacy(&cats, false, false), cats).is_err());
        let mut config = legacy(&catalog, false, false);
        config.enabled.push("a".into());
        assert!(resolve(config, catalog).is_err());
    }
    #[test]
    fn fingerprint_and_order_are_deterministic() {
        let catalog = vec![
            descriptor("b", "b", &[], no_settings()),
            descriptor("a", "a", &[], no_settings()),
        ];
        let mut c = legacy(&catalog, false, false);
        let first = resolve(c.clone(), catalog.clone()).unwrap();
        c.enabled.reverse();
        assert_eq!(first.fingerprint, resolve(c, catalog).unwrap().fingerprint);
        assert_eq!(first.order, vec!["a", "b"]);
    }
    #[test]
    fn unknown_settings_conflicts_and_unknown_ports_fail_closed() {
        let a = descriptor("a", "one", &[], no_settings());
        let b = descriptor("b", "one", &[], no_settings());
        let catalog = vec![a, b];
        let config = legacy(&catalog, false, false);
        assert!(resolve(config, catalog.clone()).is_err());
        let mut config = legacy(&catalog[..1], false, false);
        config.enabled.push("unknown".into());
        assert!(resolve(config, catalog.clone()).is_err());
        let mut config = legacy(&catalog[..1], false, false);
        config.bindings.push(CapabilityBinding {
            module: "a".into(),
            port: "unknown".into(),
            provider: "a".into(),
            major: 1,
        });
        assert!(resolve(config, catalog.clone()).is_err());
        let mut config = legacy(&catalog[..1], false, false);
        config.settings.insert("a".into(), json!({"typo":true}));
        assert!(resolve(config.clone(), catalog.clone()).is_err());
        config.enabled.clear();
        assert!(resolve(config, catalog).is_err());
    }
}
