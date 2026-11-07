# Inori

自宅サーバーで生きる、自分に合わせて育つキャラクターの開発基盤です。

Rust の実行核と GPUI クライアントを分離し、AI・音声・表示・行動を交換可能にします。
Windowsクライアントは実行時の負荷・メモリ使用量の抑制を重視し、GPUIで継続開発します。
選定方針は [ADR 0004](docs/adr/0004-gpui-resource-priority.md)、実機適合性の本採用条件は [GPUI採用ゲート](docs/development/gpui-gate.md) を参照してください。
実装状態と実機未検証の条件は [開発状況](docs/development/status.md) に記載します。

- [ドキュメント案内](docs/README.md): 要件・設計・手順・検証記録の入口
- [セットアップと配布](docs/development/setup.md): 利用開始・端末接続・常駐
- [開発と起動](docs/development/quickstart.md): ソースからの起動・通常の検査
- [実装状況](docs/development/status.md): 実装済み・確認済み・未検証
- [次に行うこと](docs/plans/next-steps.md): 優先順の提案・依存関係・完了条件

初期状態で外部 AI への送信はありません。ローカルの mock 接続で動作を確認でき、
Ollama・互換 HTTP API・whisper.cpp・VOICEVOX は設定した場合のみ利用します。

会話、認証トークン、記憶、モデル、録音は Git に保存しません。
サーバーは OS のアプリデータ領域を利用し、クライアントから切断されても稼働を続けます。
