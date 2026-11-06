export * from "./generated.ts";
import type { RpcRequest, RpcResponse } from "./generated.ts";

/** Application-neutral dispatcher. The host owns permission checks and resources. */
export async function dispatch(
  request: RpcRequest,
  handler: (method: string, params: unknown) => Promise<unknown>,
): Promise<RpcResponse> {
  if (request.jsonrpc !== "2.0") throw new Error("unsupported JSON-RPC version");
  try {
    if (request.method === "initialize" && (request.params as {protocol_major?: number})?.protocol_major !== 1) {
      throw new Error("incompatible protocol major");
    }
    const result = request.method === "initialize"
      ? { protocol_major: 1 }
      : request.method === "shutdown" ? {} : await handler(request.method, request.params);
    return { jsonrpc: "2.0", id: request.id, result, error: null };
  } catch {
    return { jsonrpc: "2.0", id: request.id, result: null, error: { code: -32000, message: "extension invocation failed" } };
  }
}

/** Bounded NDJSON transport. stdout is reserved for RPC; diagnostics belong on stderr. */
export async function serveStdio(handler: (method: string, params: unknown) => Promise<unknown>, maxBytes = 65536): Promise<void> {
  let pending = Buffer.alloc(0);
  for await (const input of process.stdin) {
    const chunk = Buffer.from(input);
    let start = 0;
    for (let end = 0; end < chunk.length; end++) {
      if (chunk[end] !== 10) continue;
      if (pending.length + end - start > maxBytes) throw new Error("frame too large");
      const frame = Buffer.concat([pending, chunk.subarray(start, end)]);
      pending = Buffer.alloc(0); start = end + 1;
      const request = JSON.parse(frame.toString("utf8")) as RpcRequest;
      if (typeof request.id !== "string" || typeof request.method !== "string") throw new Error("invalid request");
      const response = Buffer.from(JSON.stringify(await dispatch(request, handler)) + "\n");
      if (response.length > maxBytes) throw new Error("response too large");
      await new Promise<void>((resolve, reject) => process.stdout.write(response, (error) => error ? reject(error) : resolve()));
      if (request.method === "shutdown") return;
    }
    const rest = chunk.subarray(start);
    if (pending.length + rest.length > maxBytes) throw new Error("frame too large");
    pending = Buffer.concat([pending, rest]);
  }
  if (pending.length) throw new Error("incomplete final frame");
}
