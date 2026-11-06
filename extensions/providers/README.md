# AI・音声の接続

初期の `mock` / `ollama` / `compatible_http` は `runtime::InferenceProvider` を実装します。
`kind = "extension"` は同じ契約を公開 JSON-RPC の `invoke` に変換します。
第三者拡張は実行核の Rust 型を参照しません。設定例は `configs/examples/extensions.toml`。

推論拡張には `(extension, capability, infer, server)` の明示的な許可が必要です。
入力には `system`, `messages`, `max_output_tokens`, `purpose`, `audience`, `model` を送り、
結果は `{ "text": "..." }`。現行の別プロセス推論アダプターは一括応答です。
HTTP アダプターは増分ストリームを処理します。音声の組み込み接続は whisper.cpp / VOICEVOX。

自動で接続先を発見したり、クラウドにフォールバックしたりしません。
