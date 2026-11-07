# Inori サーバー

Windowsは `setup.cmd`、macOS/Linuxは `./setup.sh` を実行してください。
Rust・Python・Node.jsは不要です。質問に答えると設定と保存先を準備します。
既定は外部送信のない模擬応答です。AI・音声エンジンとモデルは事前に用意したものを接続します。

Windows PowerShellでは手動起動が `./inori-server.exe`、診断が `./inori-server.exe doctor` です。
macOS/Linuxでは `./inori-server`、`./inori-server doctor` を使います。以下の `inori-server` もOSに合わせてこの表記に読み替えてください。
保存先を変更した場合は、以後も `--data-dir "保存先"` を指定してください。
OS起動時の常駐はセットアップで選択するか、管理者/rootとして `service install` を実行します。
Windowsのサービス管理に使用する名前は `InoriServer`、macOSは `dev.inori.server`、Linuxは `inori.service` です。
常駐登録の解除は `service uninstall`。解除しても保存データは残ります。

常駐登録済みのサーバーを更新するには、新しい配布物を展開して更新スクリプトを実行します。
Windowsは管理者PowerShellで `powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\update.ps1`、
macOS/Linuxは `sudo ./update.sh` を使います。更新前DBを保存し、起動確認に失敗した場合は旧バイナリとDBへ戻します。

別PCへ渡す接続用ファイル:

```
inori-server export-connection desktop.inori-connection.json --app desktop --name laptop
```

このファイルは端末用認証情報を含みます。安全な経路で対象端末へ渡してください。
サーバー秘密鍵・owner.tokenは渡しません。LAN/VPN内の運用を前提にしています。

更新前は停止し、`backup "新しいバックアップ先.sqlite"` を実行してください。
既存データを常駐用保存先へコピーする場合は、停止後に管理者/rootで
`--data-dir "既存の保存先" service install --migrate-data` を明示して実行します。
元データは保持し、移行先に既存データがあれば上書きしません。

この配布物は開発中の試作です。署名・自動更新・24時間運用・GPUI実機適合性の保証を含みません。
