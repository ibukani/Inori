use anyhow::Result;
use inori_module_api::*;
use serde::Deserialize;
use serde_json::{Value, json};
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub omit_presence: bool,
}
pub struct BasicPerception {
    settings: Settings,
}
impl BasicPerception {
    pub fn new(settings: Value) -> Result<Self> {
        Ok(Self {
            settings: serde_json::from_value(settings)?,
        })
    }
}
module!(BasicPerception, "perception.basic");
impl AwarenessProvider for BasicPerception {
    fn observe(&self, now: i64, present: bool) -> Result<Awareness> {
        Ok(Awareness {
            utc: time::OffsetDateTime::from_unix_timestamp(now)?
                .format(&time::format_description::well_known::Rfc3339)?,
            present: present && !self.settings.omit_presence,
        })
    }
}
pub fn definition() -> ModuleDescriptor {
    descriptor(
        "perception.basic",
        "awareness.context",
        &[],
        json!({"type":"object","properties":{"omit_presence":{"type":"boolean"}},"additionalProperties":false}),
    )
}
