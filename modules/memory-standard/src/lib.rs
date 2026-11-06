use anyhow::{Context, Result};
use inori_domain::{Audience, SourceRecord};
use inori_module_api::*;
use serde_json::Value;
pub struct StandardMemory;
module!(StandardMemory, "memory.standard");
impl MemoryProcessor for StandardMemory {
    fn extraction(&self, source: &SourceRecord) -> GenerationContext {
        GenerationContext{audience:source.audience,system:"発言から将来役立つ情報の候補だけを抽出してください。一時的な状態を恒久的な好みにしないこと。推測は推測のままにし、必要な期限をUnix秒で付けること。JSONのみ返す: {\"memories\":[{\"text\":\"短い候補\",\"expires_at\":null}]}。最大3件。".into(),messages:vec![("user".into(),source.text.clone())],max_output_tokens:384,purpose:"extract_memory".into()}
    }
    fn interpret(&self, text: &str, max: usize) -> Result<Vec<MemoryCandidate>> {
        let value: Value =
            serde_json::from_str(text.trim()).context("memory extractor did not return JSON")?;
        Ok(value
            .get("memories")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(max.min(3))
            .filter_map(|item| {
                Some(MemoryCandidate {
                    text: item.get("text")?.as_str()?.into(),
                    expires_at: item.get("expires_at").and_then(Value::as_i64),
                })
            })
            .collect())
    }
    fn review(
        &self,
        audience: Audience,
        evidence: Value,
        max_tokens: u32,
    ) -> Result<GenerationContext> {
        Ok(GenerationContext{audience,system:"記憶の再評価を行います。原記録と推測を区別し、矛盾・期限・根拠不足を報告してください。基本人格と確認済み情報を勝手に変更しないこと。これは変更案の作成であり、保存内容の変更ではありません。".into(),messages:vec![("user".into(),serde_json::to_string(&evidence)?)],max_output_tokens:max_tokens,purpose:"review".into()})
    }
}
pub fn definition() -> ModuleDescriptor {
    let mut d = descriptor(
        "memory.standard",
        "memory.process",
        &[("inference", "inference.generate", false)],
        no_settings(),
    );
    d.permissions = vec![
        "memory.read.scoped".into(),
        "memory.propose.inference".into(),
    ];
    d
}
