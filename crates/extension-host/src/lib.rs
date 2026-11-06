//! Trusted subprocess hosting. Process separation is not a security sandbox.
use anyhow::{Context, Result, bail};
use inori_extension_sdk::{read_frame, write_frame};
use inori_protocol::{ExtensionManifest, PROTOCOL_MAJOR, RpcRequest, RpcResponse};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};
use tokio_util::sync::CancellationToken;

pub struct ExtensionProcess {
    manifest: ExtensionManifest,
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
    failed: bool,
}
/// Static validation only: never executes the configured command.
pub fn read_manifest(path: &std::path::Path) -> Result<inori_protocol::ExtensionManifest> {
    if std::fs::metadata(path)?.len() > 65_536 {
        bail!("manifest exceeds byte limit");
    }
    let manifest: inori_protocol::ExtensionManifest =
        serde_json::from_slice(&std::fs::read(path)?)?;
    if manifest.protocol_major != PROTOCOL_MAJOR || manifest.placement != "server" {
        bail!("incompatible extension manifest");
    }
    if manifest.id.is_empty()
        || manifest.id.len() > 128
        || manifest.max_message_bytes == 0
        || manifest.max_message_bytes > 1_048_576
        || manifest.timeout_ms == 0
        || manifest.timeout_ms > 300_000
    {
        bail!("invalid manifest limits");
    }
    let mut ids = std::collections::BTreeSet::new();
    for cap in &manifest.capabilities {
        let id = cap["id"].as_str().context("capability ID missing")?;
        if !ids.insert(id) {
            bail!("duplicate capability ID");
        }
        jsonschema::validator_for(cap.get("input_schema").unwrap_or(&json!({})))
            .map_err(|_| anyhow::anyhow!("invalid capability schema"))?;
    }
    Ok(manifest)
}

// Dropping a request future must not leave an unread response on the stream.
struct ExchangeGuard<'a> {
    child: &'a mut Child,
    failed: &'a mut bool,
    completed: bool,
}
impl Drop for ExchangeGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            *self.failed = true;
            let _ = self.child.start_kill();
        }
    }
}
impl ExtensionProcess {
    pub async fn launch(path: &Path) -> Result<Self> {
        let bytes = tokio::fs::read(path).await?;
        if bytes.len() > 65_536 {
            bail!("manifest exceeds byte limit");
        }
        let manifest: ExtensionManifest = serde_json::from_slice(&bytes)?;
        if manifest.protocol_major != PROTOCOL_MAJOR {
            bail!("incompatible extension protocol major");
        }
        if manifest.id.is_empty()
            || manifest.id.len() > 128
            || manifest.max_message_bytes == 0
            || manifest.max_message_bytes > 1_048_576
            || manifest.timeout_ms == 0
            || manifest.timeout_ms > 300_000
        {
            bail!("invalid manifest limits");
        }
        let parent = path.parent().context("manifest has no directory")?;
        let mut command = Command::new(&manifest.entrypoint);
        command
            .args(&manifest.args)
            .current_dir(parent)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn().context("extension failed to start")?;
        if let Some(mut errors) = child.stderr.take() {
            tokio::spawn(async move {
                let mut bytes = [0u8; 4096];
                loop {
                    match errors.read(&mut bytes).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            });
        }
        let input = child.stdin.take().context("missing extension stdin")?;
        let output = BufReader::new(child.stdout.take().context("missing extension stdout")?);
        let mut process = Self {
            manifest,
            child,
            input,
            output,
            next_id: 0,
            failed: false,
        };
        let hello = process
            .call(
                "initialize",
                json!({"protocol_major":PROTOCOL_MAJOR}),
                CancellationToken::new(),
            )
            .await?;
        if hello.get("protocol_major").and_then(Value::as_u64) != Some(PROTOCOL_MAJOR) {
            process.failed = true;
            let _ = process.child.kill().await;
            bail!("extension handshake rejected");
        }
        Ok(process)
    }
    pub fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }
    pub fn provides(&self, id: &str) -> bool {
        self.manifest
            .capabilities
            .iter()
            .any(|c| c.get("id").and_then(Value::as_str) == Some(id))
    }
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        if self.failed {
            bail!("extension is offline after a previous failure");
        }
        self.next_id += 1;
        let id = self.next_id.to_string();
        let request = RpcRequest {
            jsonrpc: "2.0".into(),
            id: id.clone(),
            method: method.into(),
            params,
        };
        let max = self.manifest.max_message_bytes as usize;
        let mut guard = ExchangeGuard {
            child: &mut self.child,
            failed: &mut self.failed,
            completed: false,
        };
        let exchange = async {
            write_frame(&mut self.input, &request, max).await?;
            let bytes = read_frame(&mut self.output, max)
                .await?
                .context("extension closed stdout")?;
            let response: RpcResponse = serde_json::from_slice(&bytes)?;
            if response.jsonrpc != "2.0" || response.id != id {
                bail!("invalid JSON-RPC response correlation");
            }
            if !response.error.is_null() {
                bail!("extension reported an RPC error");
            }
            Ok::<Value, anyhow::Error>(response.result)
        };
        let result = tokio::select! {_=cancel.cancelled()=>Err(anyhow::anyhow!("extension invocation cancelled; external effects may be unknown")),result=tokio::time::timeout(Duration::from_millis(self.manifest.timeout_ms),exchange)=>match result {Ok(value)=>value,Err(_)=>Err(anyhow::anyhow!("extension timeout; external effects may be unknown"))}};
        if result.is_err() {
            *guard.failed = true;
            let _ = guard.child.kill().await;
        }
        guard.completed = true;
        result
    }
    pub async fn stop(&mut self) {
        if !self.failed {
            let _ = self
                .call("shutdown", json!({}), CancellationToken::new())
                .await;
        }
        let _ = self.child.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_timed_out_extension_is_killed_and_not_invoked_again() {
        let dir = tempfile::tempdir().unwrap();
        let program = "import sys,json,time\nfor line in sys.stdin:\n r=json.loads(line)\n if r['method']=='initialize':\n  print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':{'protocol_major':1},'error':None}),flush=True)\n else: time.sleep(10)\n";
        let path = dir.path().join("slow.json");
        let manifest = json!({"id":"slow","protocol_major":1,"placement":"server","entrypoint":"python","args":["-u","-c",program],"capabilities":[],"permissions":[],"max_message_bytes":4096,"timeout_ms":1000});
        tokio::fs::write(&path, serde_json::to_vec(&manifest).unwrap())
            .await
            .unwrap();
        let mut process = ExtensionProcess::launch(&path).await.unwrap();
        let error = process
            .call("invoke", json!({}), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timeout"));
        assert!(
            process
                .call("invoke", json!({}), CancellationToken::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("offline")
        );
    }
    #[tokio::test]
    async fn incompatible_manifest_is_rejected_before_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        let manifest = json!({"id":"bad","protocol_major":99,"placement":"server","entrypoint":"must-not-run","args":[],"capabilities":[],"permissions":[],"max_message_bytes":1024,"timeout_ms":100});
        tokio::fs::write(&path, serde_json::to_vec(&manifest).unwrap())
            .await
            .unwrap();
        assert!(
            ExtensionProcess::launch(&path)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("incompatible")
        );
    }
}

#[derive(Clone)]
pub struct RegisteredExtension {
    pub manifest: inori_protocol::ExtensionManifest,
    pub process: std::sync::Arc<tokio::sync::Mutex<ExtensionProcess>>,
    pub schemas: std::collections::BTreeMap<String, std::sync::Arc<jsonschema::Validator>>,
}
pub async fn load_extensions(
    paths: &[std::path::PathBuf],
) -> Result<std::collections::BTreeMap<String, RegisteredExtension>> {
    let mut result = std::collections::BTreeMap::new();
    for path in paths {
        let process = ExtensionProcess::launch(path).await?;
        if process.manifest().placement != "server" {
            bail!("client extensions belong on the client");
        }
        let mut schemas = std::collections::BTreeMap::new();
        for capability in &process.manifest().capabilities {
            let id = capability
                .get("id")
                .and_then(Value::as_str)
                .context("capability ID missing")?;
            let schema = capability.get("input_schema").cloned().unwrap_or(json!({}));
            let validator = jsonschema::validator_for(&schema)
                .map_err(|_| anyhow::anyhow!("invalid capability schema"))?;
            if schemas
                .insert(id.into(), std::sync::Arc::new(validator))
                .is_some()
            {
                bail!("duplicate capability ID");
            }
        }
        let manifest = process.manifest().clone();
        let id = manifest.id.clone();
        if result
            .insert(
                id,
                RegisteredExtension {
                    manifest,
                    process: std::sync::Arc::new(tokio::sync::Mutex::new(process)),
                    schemas,
                },
            )
            .is_some()
        {
            bail!("duplicate extension ID");
        }
    }
    Ok(result)
}
