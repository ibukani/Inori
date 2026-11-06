"""Example trusted extension. No kernel imports, no third-party Python packages."""
import json
import sys

sys.stdout.reconfigure(encoding="utf-8", newline="\n")
MAX_FRAME = 65536
while True:
    line = sys.stdin.buffer.readline(MAX_FRAME + 1)
    if not line:
        break
    if len(line) > MAX_FRAME:
        raise SystemExit("frame too large")
    request = json.loads(line)
    method = request["method"]
    params = request["params"]
    error = None
    if method == "initialize" and params.get("protocol_major") == 1:
        result = {"protocol_major": 1}
    elif method == "invoke":
        if params.get("capability") == "example.inference":
            args = params.get("arguments", {})
            if args.get("purpose") == "extract_memory":
                result = {"text": '{"memories":[]}'}
            else:
                result = {"text": "Python拡張からの模擬応答です。別の実行環境へ交換できました。"}
        else:
            # Numeric pose information demonstrates a non-text capability.
            result = {"kind": "example.sensor.pose", "pose": [0.25, 0.5, 0.0], "unit": "normalized", "arguments": params.get("arguments")}
    elif method == "shutdown":
        result = {}
    else:
        result = None
        error = {"code": -32601, "message": "unknown method"}
    response = {"jsonrpc": "2.0", "id": request["id"], "result": result, "error": error}
    sys.stdout.write(json.dumps(response, ensure_ascii=False) + "\n")
    sys.stdout.flush()
    if method == "shutdown":
        break
