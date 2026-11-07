use anyhow::{Result, bail};
use inori_domain::{Actor, Audience, MemoryStatus};
use inori_module_api::*;
use inori_protocol::DialogueMode;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    quick_output_tokens: u32,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            quick_output_tokens: 160,
        }
    }
}
pub struct StandardDialogue {
    settings: Settings,
}
impl StandardDialogue {
    pub fn new(settings: Value) -> Result<Self> {
        let settings: Settings = serde_json::from_value(settings)?;
        if !(16..=384).contains(&settings.quick_output_tokens) {
            bail!("quick_output_tokens must be between 16 and 384");
        }
        Ok(Self { settings })
    }
}
module!(StandardDialogue, "dialogue.standard");
impl DialoguePolicy for StandardDialogue {
    fn prepare(&self, input: DialogueInput) -> Result<GenerationContext> {
        let mode = input.mode;
        let persona = if input.audience == Audience::Private {
            input.character.persona.as_str()
        } else {
            "親しみやすいキャラクター。"
        };
        let mode_policy = match mode {
            DialogueMode::Auto => "会話の流れと相手が求める深さに合わせて話してください。",
            DialogueMode::Quick => "会話のテンポを大切に、簡潔に話してください。",
            DialogueMode::Deliberate => "相手が求める深さで、考えや理由を整理して話してください。",
        };
        let mut system = format!(
            "キャラクター名: {}\n基本人格:\n{}\n\n応答方針:\n{}\n",
            input.character.name, persona, mode_policy
        );
        let max = input.profile.max_context_chars;
        let mandatory = system.chars().count() + input.request.chars().count();
        if input.request.trim().is_empty() || input.request_source_id.is_empty() {
            bail!("current user input and source identity are required");
        }
        if mandatory > max {
            bail!(
                "persona, conversation policy, and current input exceed the selected provider context budget"
            );
        }
        // Reserve the complete current input first. Add the newest history as
        // whole turns, then optional evidence; old memory must not crowd it out.
        let mut remaining = max - mandatory;
        let current = input
            .sources
            .iter()
            .position(|s| s.id == input.request_source_id);
        if let Some(index) = current {
            let source = &input.sources[index];
            if source.text != input.request
                || source.actor != Actor::User
                || source.suppressed
                || !input.audience.permits(source.audience)
            {
                bail!("current input does not match its original source");
            }
        }
        let end = current.unwrap_or(input.sources.len());
        let mut turns: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for source in input.sources.into_iter().take(end) {
            if source.suppressed || !input.audience.permits(source.audience) {
                continue;
            }
            let Some(turn_id) = source.turn_id else {
                continue;
            };
            let role = match source.actor {
                Actor::User => "user",
                Actor::Assistant => "assistant",
                _ => continue,
            };
            if turns.last().is_none_or(|(id, _)| *id != turn_id) {
                turns.push((turn_id, Vec::new()));
            }
            turns.last_mut().unwrap().1.push((role.into(), source.text));
        }
        let mut selected = Vec::new();
        for (_, turn) in turns.into_iter().rev() {
            // A limited history query can start in the middle of a spoken turn.
            if turn.first().is_none_or(|(role, _)| role != "user") {
                continue;
            }
            let cost = turn
                .iter()
                .map(|(_, text)| text.chars().count())
                .sum::<usize>();
            if cost > remaining {
                break;
            }
            remaining -= cost;
            selected.push(turn);
        }
        selected.reverse();
        let mut messages: Vec<_> = selected.into_iter().flatten().collect();
        messages.push(("user".into(), input.request));

        for memory in input.memories {
            if memory.status != MemoryStatus::Active || !input.audience.permits(memory.audience) {
                continue;
            }
            let line = format!(
                "\n参考情報（記憶、日時はUnix秒）:\n{}\n",
                serde_json::to_string(&json!({
                    "id": memory.id,
                    "kind": memory.kind,
                    "source_ids": memory.source_ids,
                    "created_at": memory.created_at,
                    "expires_at": memory.expires_at,
                    "text": memory.text,
                }))?
            );
            append_optional(&mut system, &mut remaining, &line);
        }
        if let Some(a) = input.awareness {
            append_optional(
                &mut system,
                &mut remaining,
                &format!(
                    "\n参考情報（認識）:\n{}\n",
                    serde_json::to_string(&json!({"utc": a.utc, "present": a.present}))?
                ),
            );
        }
        Ok(GenerationContext {
            audience: input.audience,
            system,
            messages,
            max_output_tokens: if mode == DialogueMode::Quick {
                input
                    .profile
                    .max_output_tokens
                    .min(self.settings.quick_output_tokens)
            } else {
                input.profile.max_output_tokens
            },
            purpose: "dialogue".into(),
        })
    }
}

fn append_optional(system: &mut String, remaining: &mut usize, text: &str) {
    let cost = text.chars().count();
    if cost <= *remaining {
        system.push_str(text);
        *remaining -= cost;
    }
}
pub fn definition() -> ModuleDescriptor {
    descriptor(
        "dialogue.standard",
        "dialogue.respond",
        &[
            ("inference", "inference.generate", false),
            ("awareness", "awareness.context", true),
            ("synthesis", "speech.synthesize", true),
        ],
        json!({"type":"object","properties":{"quick_output_tokens":{"type":"integer","minimum":16,"maximum":384}},"additionalProperties":false}),
    )
}

#[cfg(test)]
mod tests;
