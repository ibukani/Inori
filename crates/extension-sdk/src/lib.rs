//! Stdio framing shared by hosts and trusted extensions. No kernel imports.
use anyhow::{Result, bail};
use inori_protocol::{PROTOCOL_MAJOR, RpcRequest, RpcResponse};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const DEFAULT_MAX_FRAME: usize = 65_536;
pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Option<Vec<u8>>> {
    let mut result = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            if result.is_empty() {
                return Ok(None);
            }
            bail!("unterminated JSON-RPC frame");
        }
        let end = buf.iter().position(|b| *b == b'\n');
        let take = end.map(|i| i + 1).unwrap_or(buf.len());
        if result.len() + take > max {
            bail!("JSON-RPC frame exceeds byte limit");
        }
        result.extend_from_slice(&buf[..take]);
        reader.consume(take);
        if end.is_some() {
            return Ok(Some(result));
        }
    }
}
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &impl serde::Serialize,
    max: usize,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() > max {
        bail!("JSON-RPC frame exceeds byte limit");
    }
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
pub fn success(id: String, result: Value) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result,
        error: Value::Null,
    }
}
pub fn failure(id: String, code: i64, message: &str) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: Value::Null,
        error: json!({"code":code,"message":message}),
    }
}

/// Minimal extension runner. Protocol messages are stdout-only; diagnostics belong on stderr.
pub async fn serve<F>(mut handler: F) -> Result<()>
where
    F: FnMut(&str, Value) -> Result<Value>,
{
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    while let Some(line) = read_frame(&mut input, DEFAULT_MAX_FRAME).await? {
        let request: RpcRequest = serde_json::from_slice(&line)?;
        if request.jsonrpc != "2.0" {
            bail!("unsupported JSON-RPC version");
        }
        let response = if request.method == "initialize" {
            if request.params.get("protocol_major").and_then(Value::as_u64) != Some(PROTOCOL_MAJOR)
            {
                failure(request.id, -32602, "incompatible protocol major")
            } else {
                success(request.id, json!({"protocol_major":PROTOCOL_MAJOR}))
            }
        } else if request.method == "shutdown" {
            write_frame(
                &mut output,
                &success(request.id, json!({})),
                DEFAULT_MAX_FRAME,
            )
            .await?;
            break;
        } else {
            match handler(&request.method, request.params) {
                Ok(value) => success(request.id, value),
                Err(error) => failure(request.id, -32000, &error.to_string()),
            }
        };
        write_frame(&mut output, &response, DEFAULT_MAX_FRAME).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn oversized_and_unterminated_frames_fail_without_unbounded_buffering() {
        let mut huge = tokio::io::BufReader::new(&b"12345678901234567890\n"[..]);
        assert!(read_frame(&mut huge, 8).await.is_err());
        let mut partial = tokio::io::BufReader::new(&b"{}"[..]);
        assert!(read_frame(&mut partial, 64).await.is_err());
    }
}
