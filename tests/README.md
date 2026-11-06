# 検証の配置

各Rustプロジェクトは個別に `cargo test -p <name>` で実行できます。

| 種別 | 正本 |
|---|---|
| 契約 | `apps/server/tests/api.rs` のスキーマ照合、`tools/generate_contracts.py --check` |
| ドメイン・保存 | `crates/domain/src/lib.rs`、`crates/storage-sqlite/src/lib.rs` のテスト |
| 統合 | `apps/server/tests/api.rs`、`apps/server/tests/extensions.rs` |
| 構成・互換性 | `apps/server/tests/composition.rs`、`crates/composition`、`tools/check_config_examples.py` |
| 移行 | `crates/storage-sqlite/tests/migration.rs`（旧形式backupとロック） |
| 実プロセスE2E | `python tools/test_e2e.py`。TLS・端末認証・再起動・バックアップ復旧 |
| 性能・運用 | `tests/performance/measure-windows.ps1`、`tests/performance/soak.py` |
| 改修前後の比較 | `tests/performance/compare-composition.py --before <旧exe> --after <新exe>`（mock・合成データのみ） |

一時データは `.cache` またはOSの一時ディレクトリへ作ります。実際の会話・認証情報をfixtureにしません。
性能の実測結果と未実測の条件は `docs/development/status.md` に分離します。
