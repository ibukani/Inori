# Inori

自宅サーバーで生きる、自分に合わせて育つキャラクターの開発基盤です。

Rust の実行核と GPUI クライアントを分離し、AI・音声・表示・行動を交換可能にします。
実装状態と実機未検証の条件は [開発状況](docs/development/status.md) に記載します。

- [確定要件と対応表](docs/requirements.md)
- [統合プラン](docs/plans/inori.md)
- [アーキテクチャ](docs/architecture.md)
- [サーバーの構成を変更する](docs/development/composition.md)
- [開発・起動手順](docs/development/quickstart.md)
- [受け入れ条件と検証](docs/development/acceptance.md)
- [公開契約](docs/protocols/v1.md)
- [設計判断](docs/adr/0001-foundation.md)

初期状態で外部 AI への送信はありません。ローカルの mock 接続で動作を確認でき、
Ollama・互換 HTTP API・whisper.cpp・VOICEVOX は設定した場合のみ利用します。

会話、認証トークン、記憶、モデル、録音は Git に保存しません。
サーバーは OS のアプリデータ領域を利用し、クライアントから切断されても稼働を続けます。
