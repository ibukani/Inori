# セットアップと配布

ユーザー確定事項: Windowsアプリの初回設定画面、Windows/macOS/Linuxサーバーの対話式設定、ビルド済み配布物、開発用の共通入口を提供する。
外部エンジン・モデルの導入は自動化しない。設計判断は [ADR 0006](../adr/0006-shared-setup.md) を参照。

## ビルド済みアプリを使う

配布物を展開し、サーバーはWindowsの `setup.cmd` またはmacOS/Linuxの `./setup.sh` を実行する。
Rust・Python・Node.jsは利用時に要求しない。最初は外部送信のないmockで確認できる。
保存先、LAN/VPN接続、証明書へ登録する接続名、利用する既存エンジンを案内する。
クラウド、私的文脈の送信、無人クラウド推論、別PCへの私的音声送信は別々に明示設定する。

非対話実行の例（配布物のディレクトリで実行）:

```powershell
./inori-server.exe --data-dir C:\InoriData setup --non-interactive
./inori-server.exe --data-dir C:\InoriData
./inori-server.exe --data-dir C:\InoriData doctor
```

初回に `server.toml`、`launch.toml`、DB、証明書・秘密鍵、所有者トークンを保存する。
既存の設定・証明書・端末・人格・記憶を初期化しない。稼働中の設定書き換えはロックで拒否する。
`--start` は設定後にそのターミナルでサーバーを起動する。対話中も起動するか選べる。
ターミナルを閉じても稼働させる場合はOSサービスを使う。

LANの例:

```powershell
./inori-server.exe --data-dir C:\InoriData setup --non-interactive --lan --server-url https://home-server:9473
```

DNSやVPN内の名前解決を先に用意する。接続名変更で既存証明書を自動更新しない。
名前・有効期限が一致しなければ元の接続名を使うか、証明書更新と再ペアリングを行う。
LAN/VPN内から待受ポートへのファイアウォール許可が必要。ファイアウォール・ルーターは自動変更しない。

既存Ollamaの例:

```powershell
./inori-server.exe --data-dir C:\InoriData setup --non-interactive --provider ollama --provider-url http://127.0.0.1:11434 --model YOUR_INSTALLED_MODEL --allow-private
```

新しいDBは指定AIを初期選択する。既存DBは保存済みモデルを維持する。
別のモデルを設定した場合はプロファイルを追加し、以前の選択済みプロファイルを置換しない。
選択の変更は管理用 `PUT /v1/providers/active` に現在のrevisionを添えて明示的に行う。

互換HTTPでは `--provider compatible_http --provider-url ... --model ... --api-key-env INORI_AI_API_KEY` を指定する。
対話ではキーを非表示入力でき、非対話では `--api-key-file` で既存ファイルから読む。
キーはデータ領域の `server.secrets.toml` へUnix 0600／Windowsの本人専用ACLで保存する。
環境変数を優先し、未設定時だけ保存済みのキーをプロバイダーへ渡す。実行時の環境変数は書き換えない。
`--secrets-file` は別の秘密情報ファイルを明示して起動するための指定。
常駐設定では再起動後に端末の環境変数を引き継げないため、現在の環境変数にだけ存在する設定済みキーも保護した秘密情報ファイルへ保存する。
クラウドには `--cloud`、私的会話の送信には `--allow-private`、無人利用には `--autonomy-cloud` を指定する。
音声は `--whisper-url`、`--voicevox-url`。別PCへの私的音声・発話文の送信には `--allow-private-remote-voice` を明示する。

`doctor --engines` は設定したエンジンのメタデータ接続だけを確認する。推論・録音・合成・モデル取得はしない。
`doctor --offline` はネットワーク接続せず、設定と既存保存領域を読む。診断ではDB作成・移行を行わない。

## アプリを接続する

```powershell
./inori-server.exe --data-dir C:\InoriData export-connection laptop.inori-connection.json --app desktop --name laptop
```

稼働中は所有者認証付きHTTPS、停止中はデータのロックを取って端末を発行する。
既定はprivateの会話用端末で管理権限なし。管理アプリにだけ `--admin` を明示する。
public端末は `--public`。publicとadminの組み合わせは拒否する。

接続用ファイルはURL・公開証明書・アプリID・端末ID・端末トークンを含む。
所有者トークンとサーバー秘密鍵は含めない。安全な経路で対象端末へ渡す。
既存ファイルは上書きしない。再発行時には古い端末の失効も行う。

Windowsデスクトップの「接続用ファイルを選択」で読み込み、接続先・証明書を確認して「接続して保存」を押す。
TLS・認証・機能確認に成功した設定だけを保存する。認証情報はアプリ専用領域へコピーする。
失敗・キャンセル時は既定設定を変更しない。従来の同じPCの接続や、URL・トークン・証明書の手動指定も残す。
互換用のローカル接続は従来の所有者資格を使用する。新しい通常アプリには、管理権限のない専用端末を接続用ファイルで発行する方式を推奨する。

読込順は、個別CLI引数 → `--config` → OS設定領域の `apps/<アプリID>/client.toml` → 実行ファイル横の `inori-client.toml` → 従来のローカル接続。
設定中の相対パスは設定ファイルの場所が基準。保存先は通常アプリ専用領域、`--config` 指定時はそのファイル。
保存失敗は画面へ表示する。マイク・AI音声は起動時OFF。アプリ終了後もサーバーは独立して動く。

## OS起動時の常駐

自動起動を選ぶ場合は管理者/rootとして実行する。新規データは常駐専用の場所へ作成し、登録は明示操作に限定する。

| OS | 実行方式 | 実行アカウント | 常駐用データ | 固定バイナリ |
|---|---|---|---|---|
| Windows x64 | InoriServerサービス | LocalService | ProgramData/Inori/server | Program Files/Inori/server |
| macOS Intel/Apple Silicon | dev.inori.server LaunchDaemon | _inori | /Library/Application Support/Inori/server | /usr/local/lib/inori |
| Linux x64 | inori.service / systemd | inori | /var/lib/inori | /opt/inori |

```text
inori-server setup --autostart
inori-server service status
inori-server service start
inori-server service stop
inori-server service uninstall
```

既存の手動運用を移す場合は停止し、元の保存先を指定して `service install --migrate-data` を管理者/rootとして実行する。
既定保存先に既存DBがある場合、`setup --autostart` で黙って別の新規DBへ切り替えず、この明示移行へ案内する。
常駐用データの設定をやり直す場合も、停止後に同じ保存先と `setup --autostart` を指定し、専用アカウントのアクセス権を再設定する。
元領域のbackupsへDBバックアップを作り、空の常駐用領域へDBと既知の設定・認証情報だけをコピーする。
元領域は削除しない。移行先が空でなければ上書きしない。
従来のログオン時タスクを利用している場合は、移行前にそのタスクを停止・無効化する。
拡張を使う構成の自動移行は拒否し、専用アカウントがアクセスできる配置を先に準備する。

UnixのSIGTERM／Windowsのサービス停止を既存のキャンセル処理へ接続する。
OSの停止待機枠は10秒、サーバー内の期限は従来の5秒を維持する。
登録解除でデータ・証明書・キーを削除しない。
異なる版への更新は停止・バックアップ後に固定バイナリを更新する。旧版へ戻す場合は対応するDBバックアップも戻す。

常駐登録済みなら、Windows/macOS/Linuxの配布物に含む更新スクリプトを実行できます。
チェックサムを検査し、サービス停止・DBバックアップ・バイナリ差し替え・起動確認を行います。
起動確認に失敗した場合は旧バイナリと更新前DBへ戻します。配布物の作成と転送は引き続き手動です。

## 開発と将来アプリの追加

Windowsは `./dev.ps1`、Mac/Linuxは `sh ./dev.sh` が共通入口。Python 3.11以上が必要。
Rust・C++ツール等の不足を検出して案内し、自動インストールはしない。Node.js・Corepack/pnpmは `doctor --sdk` の場合だけ要求する。

```powershell
./dev.ps1 doctor --app server
./dev.ps1 setup --app server -- --non-interactive --data-dir C:\InoriData
./dev.ps1 build --app desktop
./dev.ps1 run --app desktop
./dev.ps1 package --app server
./dev.ps1 package --app desktop
```

開発機から、checkout中のソースでサーバーを常駐導入・更新・解除できます。`install` と `update` はreleaseビルドです。
Windowsでは管理者PowerShellから実行し、macOS/Linuxでは必要な操作に `sudo` を使います。

macOS/Linux:

```sh
sh ./dev.sh install --app server
sh ./dev.sh update --app server
sh ./dev.sh uninstall --app server
```

Windowsでは管理者PowerShellから同じコマンド名を `./dev.ps1` で実行します。

```powershell
./dev.ps1 install --app server
./dev.ps1 update --app server
./dev.ps1 uninstall --app server
```

更新前にGitで使う変更をcheckoutしてください。`uninstall` はOSサービスと固定バイナリを取り除き、
DB・証明書・設定は各OSのサーバーデータ領域に保持します。

通常は固定ツールチェーンでビルドして実行する。既存ビルドには `--no-build`、release実行には `--release`。
`package` は常にreleaseを梱包し、Git対象外の `artifacts/dist` へ出力する。
同名配布物は上書きしない。別の `--output` を指定するか、古い配布物を別名へ移す。

Cargo workspaceへ新しいcrateを追加したうえで、`apps/manifest.toml` にID、Cargo package、実行ファイル、対応OS、初回設定の引数、配布対象を登録する。
新しいクライアントでは `inori-app-support` の設定・接続ファイル・診断処理とアプリ固有の初回画面を組み合わせる。
`client-core::ClientConfig` は引き続き使える。共通設定基盤は人格・記憶・権限を所有しない。

WindowsはZIP、Mac/Linuxはtar.gz。指定バイナリ、許可リストの文書・起動入口、依存ライセンス、チェックサムだけを梱包する。
作業ディレクトリやデータ領域を丸ごと取り込まない。接続ファイル、キー、DB、録音、モデル、ログは対象外。
プロジェクト本体の利用許諾は未指定のため、現在は私有プロジェクト向けの配布物とする。

手動実行のpackage CIも同じツールを使い、成果物だけ保存する。Releaseの公開はしない。
LinuxはDebian 12でビルドし、Debian 12・Ubuntu 24.04で検証するジョブを定義する。
Windows x64とMac両アーキテクチャも定義し、実行結果のない環境は未検証として扱う。

通常のRust・SDK検査に加え、`python tools/test_distribution.py` と `python tools/test_setup.py` を使う。
秘密情報の明示注入と環境変数の優先は、実外部サービスを使わない `python tools/test_setup_credentials.py` で検査する。
WindowsのGUIなし接続診断には `--desktop target/debug/inori-desktop.exe` を付ける。
検証には `.cache` 内の架空データを使う。OSサービス実登録・再起動・クリーン環境・GPUI・実音声の試験は別に実施結果を記録する。
