use anyhow::{Result, bail};
use inori_domain::{MAX_TEXT_BYTES, validate_text};
use inori_module_api::*;
use inori_protocol::ChatRequest;
pub struct StandardChannel;
module!(StandardChannel, "channel.standard");
impl ChannelAdapter for StandardChannel {
    fn normalize(&self, request: ChatRequest) -> Result<ChatRequest> {
        validate_text(&request.text, MAX_TEXT_BYTES)?;
        validate_text(&request.session_id, 128)?;
        Ok(request)
    }
    fn negotiate(
        &self,
        hello: &ClientHello,
        mut available: ServerCapabilities,
    ) -> Result<ServerCapabilities> {
        if hello.kind != "client.hello" || hello.protocol_major != 1 {
            bail!("unsupported client capability contract");
        }
        validate_text(&hello.session_id, 128)?;
        if hello.audio_mime_types.len() > 16 {
            bail!("too many media formats");
        }
        let wav = hello.audio_mime_types.iter().any(|m| m == "audio/wav");
        available.text_chat &= hello.text;
        available.transcription &= hello.audio_input && wav;
        available.speech &= hello.audio_output && wav;
        available.presentation &= hello.presentation;
        Ok(available)
    }
    fn outgoing(
        &self,
        mut event: inori_protocol::EventEnvelope,
        negotiated: Option<&ServerCapabilities>,
    ) -> Option<inori_protocol::EventEnvelope> {
        if let Some(capabilities) = negotiated {
            if event.kind == "speech.segment" && !capabilities.speech
                || event.kind == "text.delta" && !capabilities.text_chat
                || event.kind == "presentation.state" && !capabilities.presentation
            {
                return None;
            }
            if event.kind == "turn.completed"
                && !capabilities.text_chat
                && let Some(payload) = event.payload.as_object_mut()
            {
                payload.remove("text");
            }
        }
        Some(event)
    }
}
pub fn definition() -> ModuleDescriptor {
    let mut d = descriptor(
        "channel.standard",
        "channel.standard",
        &[
            ("dialogue", "dialogue.respond", true),
            ("recognition", "speech.recognize", true),
            ("synthesis", "speech.synthesize", true),
            ("actions", "actions.execute", true),
        ],
        no_settings(),
    );
    d.max_concurrency = 8;
    d
}
