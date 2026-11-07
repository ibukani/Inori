# 運用

`inori-server setup` と `service install|start|stop|status|uninstall` が共通の導入・運用入口です。
Windowsサービス、macOS LaunchDaemon、Linux systemdへ明示的に登録でき、ログイン前から動く構成を用意します。
この開発環境への常駐登録は自動実行しません。従来の `windows/register-server.ps1` はログオン時タスク用の旧手順です。
サーバーはGUIを必要としないため、常時稼働する別PCで実行できます。
実行アカウントはWindows LocalService／Unixの専用非ログインアカウントです。

配布は `dev.ps1 package --app server` または `sh dev.sh package --app server`。WindowsはZIP、Mac/Linuxはtar.gzです。
署名・自動更新・OS別インストーラーは対象外です。DB更新前にバックアップと別領域への復旧を確認します。
初回設定・常駐用保存先・既存データの明示移行は [セットアップ](../docs/development/setup.md) を参照してください。
自宅LAN/VPN限定です。ルーターのポート転送や一般公開は初期範囲に含めません。
端末に配るのは端末用トークンと公開証明書です。サーバー秘密鍵やownerトークンを他端末へ共用しないでください。
