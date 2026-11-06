use crate::ports::*;
use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
pub async fn collect(
    provider: &dyn InferenceProvider,
    context: GenerationContext,
    cancel: CancellationToken,
) -> Result<String> {
    let (tx, mut rx) = mpsc::channel(16);
    let producer = provider.generate(context, tx, cancel.clone());
    let consumer = async {
        let mut text = String::new();
        while let Some(chunk) = rx.recv().await {
            text.push_str(&chunk);
            if text.len() > 65_536 {
                bail!("output limit exceeded");
            }
        }
        Ok::<_, anyhow::Error>(text)
    };
    tokio::select! {
        biased;
        _=cancel.cancelled()=>bail!("inference cancelled"),
        result=tokio::time::timeout(std::time::Duration::from_secs(provider.profile().timeout_seconds),async {let ((),text)=tokio::try_join!(producer,consumer)?;Ok(text)})=>result.context("provider generation timed out")?,
    }
}
