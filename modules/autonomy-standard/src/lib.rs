use inori_domain::{Audience, Character};
use inori_module_api::*;
pub struct StandardAutonomy;
module!(StandardAutonomy, "autonomy.standard");
impl AutonomyPlanner for StandardAutonomy {
    fn prepare(&self, character: &Character, awareness: Awareness) -> GenerationContext {
        GenerationContext {
            audience: Audience::Private,
            system: format!(
                "{}\n不在時の振り返りです。次の会話候補か興味の整理を一つだけ行い、予定として表現してください。実行していない外部行動を体験として語らないこと。現在のUTC時刻: {}、在席: {}",
                character.persona, awareness.utc, awareness.present
            ),
            messages: vec![(
                "user".into(),
                "次に話したいことを短く考えてください。".into(),
            )],
            max_output_tokens: 160,
            purpose: "autonomy".into(),
        }
    }
}
pub fn definition() -> ModuleDescriptor {
    descriptor(
        "autonomy.standard",
        "autonomy.plan",
        &[
            ("inference", "inference.generate", false),
            ("awareness", "awareness.context", false),
        ],
        no_settings(),
    )
}
