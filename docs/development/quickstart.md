# 開発と起動

Rust 1.95.0、Windows用MSVC/C++ビルドツールが必要です。Node.jsはSDK開発時のみ必要です。
GPUIは `0.2.2`、gpui-componentは互換性を確認した `0.5.1`、依存は Cargo.lock に固定しています。

```powershell
cargo build --locked -p inori-server -p inori-desktop
cargo test --locked --workspace --exclude inori-desktop
python tools/generate_contracts.py --check
python tools/check_boundaries.py
```

サーバーを起動します。

サーバーの組み合わせは [構成手順](composition.md) を参照してください。
minimal / autonomous / companionの設定例を用意しています。変更前に検証できます。

```powershell
cargo run --locked -p inori-server -- --config configs/examples/companion.toml validate-config
```

```powershell
cargo run --locked -p inori-server -- --config configs/examples/server.toml
```

初回にOSのアプリデータディレクトリにDB、`owner.token`、`server.pem`、`server.key`を作ります。
ログの `data` が実際の保存先です。`--data-dir C:\path\to\data` で変更できます。
既定の接続先は `https://localhost:9473`。別のターミナルでクライアントを起動します。

```powershell
cargo run --locked -p inori-desktop
```

同じユーザー・同じPC・既定のデータディレクトリならそのまま接続します。
サーバーを別PCへ置くときは `--listen 0.0.0.0:9473 --tls-name home-server` のように、
実際に接続に使う名前/IPを証明書生成時に渡します。LAN/VPN内に制限してください。
既存の証明書には後からSANを追加しません。名前を変える場合は新しい証明書と鍵を用意して再ペアリングします。
サーバーから端末用トークンと**公開証明書だけ**を安全な方法でクライアントへ渡します。

```powershell
target/debug/inori-server.exe --data-dir C:\path\to\data create-device --name laptop --admin
target/debug/inori-desktop.exe --server https://home-server:9473 --token-file C:\pairing\laptop.token --certificate-file C:\pairing\server.pem
```

専用データディレクトリを使用すると、既定のクライアントはそこを自動検出しません。
`--config configs/examples/client.toml` か接続画面でファイルを指定します。
実行ファイルと同じ場所の `inori-client.toml` も読み込みます。`--config` が優先です。
設定画面の接続変更は現在のプロセスのみで、設定ファイルへの自動保存はしません。

会話は模擬応答でまず確認できます。実AIでは `server.toml` のOllamaまたは互換HTTP設定を追加し、
サーバー再起動後に設定画面からモデルを選びます。既存DBのモデルを設定から消すと起動を拒否します。
`server.toml` はGroqCloudの `qwen/qwen3.8-27b` を選択済みです。起動前に同じPowerShellでAPIキーを設定します。

```powershell
$env:GROQ_API_KEY = "<Groq API key>"
cargo run --locked -p inori-server -- --config configs/examples/server.toml
```

新しいDBではGroqが選択されます。既存DBは保存済みのモデルを維持するため、起動後に設定画面でGroqへ切り替えます。
このプロファイルは私的会話とその文脈をGroqへ送る設定です。自律活動のクラウド推論は `cloud_enabled = false` のままです。
VOICEVOXとwhisper.cppも明示的に設定した場合のみ接続します。
別PCの音声エンジンへ私的な音声や発話文を送るには `voice.allow_private_remote = true` も設定します。
マイクはボタンで開始・終了します。Escapeまたは「止める」で音声と生成を停止します。
ウィンドウを閉じるとトレイ常駐のため最小化します。Ctrl+Qまたはトレイの「終了」で終了します。
クライアント終了後もサーバーは独立して動きます。

## バックアップと更新

```powershell
target/debug/inori-server.exe --data-dir C:\path\to\data backup C:\backups\inori-before-update.sqlite
target/debug/inori-server.exe --data-dir C:\path\to\new-data restore C:\backups\inori-before-update.sqlite
```

バックアップ先は既存ファイルを上書きしません。リストア先はDBが存在しないディレクトリにします。
backupコマンドはDB移行を行いません。DB形式を更新する起動では、サーバー停止・ロック取得後に
データディレクトリのbackupsへ旧形式のコピーを作り、成功してから移行します。
バックアップにも私的会話と端末認証ハッシュが含まれます。元の証明書・鍵・端末トークンはDBに含まれません。
運用更新前にサーバーを停止しバックアップを作り、新バージョンで別ディレクトリへ復旧検証してください。
旧バージョンへ戻す場合は、そのバージョンに対応するDBバックアップも一緒に戻します。

SDK開発:

```powershell
corepack pnpm install --frozen-lockfile
corepack pnpm --filter @inori/extension-sdk check
corepack pnpm --filter @inori/extension-sdk test
```
