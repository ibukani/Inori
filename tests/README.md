# 検証の配置

各Rustプロジェクトは個別に `cargo test -p <name>` で実行できます。
通常の検査・SDK・実プロセス検証のコマンドは [quickstart](../docs/development/quickstart.md#通常の検査) にまとめています。

| 種別 | テスト・ツールの配置 |
|---|---|
| 人格と会話品質 | dialogue-standardの文脈・モード・参考情報・上限・公開範囲17件、`tests/dialogue/stage1.json`、serverの`dialogue-review` example、`tools/review_dialogue.py`と`tools/test_review_dialogue.py`（10件）。実装判定とモデル応答の観測は[専用手順](../docs/development/dialogue-review.md)で分離 |
| 契約 | `apps/server/tests/api.rs` のスキーマ照合、`tools/generate_contracts.py --check` |
| ドメイン・保存 | `crates/domain/src/lib.rs`、`crates/storage-sqlite/src/lib.rs` のテスト |
| 統合 | `apps/server/tests/api.rs`、`apps/server/tests/extensions.rs` |
| 構成・互換性 | `apps/server/tests/composition.rs`、`crates/composition`、`tools/check_config_examples.py` |
| 移行 | `crates/storage-sqlite/tests/migration.rs`（旧形式backupとロック） |
| 実プロセスE2E | `python tools/test_e2e.py`。TLS・端末認証・再起動・バックアップ復旧 |
| 共通設定・端末接続 | `crates/app-support` のテスト、`python tools/test_setup.py`。WindowsのGUIなし診断は `--desktop target/debug/inori-desktop.exe` |
| 秘密情報・モデル保持 | `python tools/test_setup_credentials.py`。模擬HTTPでキー注入・環境変数優先・既存モデル保持を検査 |
| 配布 | `python tools/test_distribution.py`。アプリ宣言・許可リスト・同梱対象を検査 |
| クライアント音声 | `crates/client-core/src/audio.rs` の合成入力・発話区切り・上限・ミュート・再生世代のテスト。実デバイス試験とは区別 |
| 性能・運用 | `tests/performance/measure-windows.ps1`、`tests/performance/soak.py` |
| 改修前後の比較 | `tests/performance/compare-composition.py --before <旧exe> --after <新exe>`（mock・合成データのみ） |

一時データは `.cache` またはOSの一時ディレクトリへ作ります。実際の会話・認証情報をfixtureにしません。
結果の概要は [実装状況](../docs/development/status.md)、条件付きの数値は
[過去の性能測定記録](../docs/development/performance-history.md)、実画面は
[GUI検証記録](../docs/development/gui-validation-2026-11-07.md) を参照してください。
テストコード・CI定義があることと、各環境で実行・合格したことは区別します。
