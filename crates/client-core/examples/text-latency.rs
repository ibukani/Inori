//! cargo run -p inori-client-core --example text-latency -- client.toml 3
//! Measures streamed text, not audio. Uses synthetic Japanese prompts.
use anyhow::{Context, Result, bail};
use inori_client_core::{ApiClient, ClientConfig};
use inori_protocol::{Audience, ChatAccepted, ChatRequest, DialogueMode};
use reqwest::Method;
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("provide a client TOML config path")?;
    let samples = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "3".into())
        .parse::<usize>()?;
    if !(1..=20).contains(&samples) {
        bail!("samples must be 1..20");
    }
    let config: ClientConfig = toml::from_str(&std::fs::read_to_string(path)?)?;
    let client = Arc::new(ApiClient::new(&config)?);
    let session = uuid::Uuid::new_v4().to_string();
    let stop = CancellationToken::new();
    let (tx, mut events) = mpsc::channel(64);
    let c = client.clone();
    let s = session.clone();
    let token = stop.clone();
    let listener = tokio::spawn(async move { c.listen(&s, tx, token).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            if event.kind == "client.connected" {
                return Ok::<_, anyhow::Error>(());
            }
        }
        bail!("WebSocket disconnected")
    })
    .await??;
    let mut rows = Vec::new();
    for sample in 0..samples {
        let start = Instant::now();
        let request = ChatRequest {
            text: "小さなロボットが主人公の物語の始まりを、一文だけ作って。".into(),
            audience: Audience::Private,
            mode: DialogueMode::Quick,
            session_id: session.clone(),
            speak: false,
        };
        let accepted: ChatAccepted = serde_json::from_value(
            client
                .call(Method::POST, "/v1/chat", Some(&request))
                .await?,
        )?;
        let mut first = None;
        let mut sentence = None;
        let mut text = String::new();
        tokio::time::timeout(Duration::from_secs(180), async {
            loop {
                let event = events.recv().await.context("stream ended")?;
                if event.generation_id.as_deref() != Some(&accepted.generation_id) {
                    continue;
                }
                match event.kind.as_str() {
                    "text.delta" => {
                        let chunk = event.payload["text"]
                            .as_str()
                            .context("invalid text delta")?;
                        if first.is_none() {
                            first = Some(start.elapsed().as_millis());
                        }
                        text.push_str(chunk);
                        if sentence.is_none()
                            && text
                                .chars()
                                .any(|c| matches!(c, '。' | '！' | '？' | '!' | '?' | '\n'))
                        {
                            sentence = Some(start.elapsed().as_millis());
                        }
                    }
                    "turn.completed" => return Ok::<_, anyhow::Error>(()),
                    "turn.error" => bail!("{}", event.payload),
                    _ => {}
                }
            }
        })
        .await??;
        rows.push(json!({"sample":sample,"first_text_ms":first,"first_sentence_ms":sentence,"full_text_ms":start.elapsed().as_millis(),"output_characters":text.chars().count()}));
    }
    stop.cancel();
    listener.await??;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"kind":"streamed text only; excludes STT, TTS and playback","samples":rows})
        )?
    );
    Ok(())
}
