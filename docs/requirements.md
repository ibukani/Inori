# 要件と対応表

状態: user = ユーザーの要件、design = 設計上の選択、target = 未実測の仮目標。
要件IDと確定内容はこの文書を正本とします。[文書案内](README.md)に各文書の役割、
[受け入れ表](development/acceptance.md)に確認根拠、[次に行うこと](plans/next-steps.md)に作業順の提案をまとめます。

| ID | 状態 | 要件 | 実装・検証の担当 |
|---|---|---|---|
| R01 | user | 自律キャラクターを中心に、会話・支援・配信を能力として追加する | runtime / extension-host |
| R02 | user | テキスト以外の認識・判断・記憶・行動・音声・表示を拡張する | contracts / extension-sdk / Python例 |
| R03 | user | クラウド、RTX3070級PC、メモリの多いMacへAIの処理を切り替える | provider adapter / model-switch tests |
| R04 | user | 基本人格を保ち、好み・記憶・距離感を自動適応する。確認・修正・取り消しを行える | domain / storage-sqlite / memory history |
| R05 | user | 自宅サーバーは常時稼働し、画面の終了で人格や活動が消えない | server / persisted state / scheduler |
| R06 | user | Windows、8GB RAM、専用GPUなし。自宅LAN/VPNから接続 | desktop-gpui / TLS / device auth / hardware gate |
| R07 | user | 拡張・対象・操作ごとに権限を設定する | permissions / operation ledger |
| R08 | user | 初期は信頼済みの第三者製拡張。未信頼コードの強い隔離は対象外 | extension-host / trust documentation |
| R09 | user | 初期は会話・時刻・在席。画面・カメラ・配信・家電は後続 | runtime / client-core |
| R10 | user | 会話の状況に合うテンポと深さ、音声の割り込み | dialogue modes / generation ID / playback ack |
| R11 | user | モデルの品質差で記憶を汚染・破損させない | provenance / revision CAS / deletion suppression / review |
| R12 | user | 見た目は素材と描画エンジンを追加・交換できる | renderer contracts / assets |
| R13 | user | 継続開発・信頼性を優先。AIと共同開発し、言語制約はない | workspace / CI / ADR / contract generation |
| R14 | user | 既存サーバーの能力を設定ファイルで組み替え、再起動で適用する | module-api / composition / modules / composition tests |
| R15 | user | クライアントは通常のアプリとし、共通接続と必要な専用アダプターで対応する | channel.standard / capabilities / client-core |
| R16 | user | 全能力なしでも管理・保存・保守を維持し、構成変更でも状態と保護規則を保つ | runtime kernel / module state CAS / migration tests |
| R17 | user | アプリ実行時の負荷・メモリ使用量の抑制を重視し、WindowsクライアントにGPUIを使用する | desktop-gpui / resource measurements / GPUI gate |
| R18 | user | デスクトップは単一時系列のチャット専用。白と空色を基調にし、画像・キャラクター描画と記憶・人格・モデル・権限の管理画面を含めない。接続設定は残し、管理アプリは別にする | desktop-gpui / ADR 0005 |
| R19 | user | マイク入力とAI音声出力を独立トグルにし、起動時は両方OFF。マイクON中は発話を自動区切りし、トレイへ隠しても会話を続け、終了操作で停止する | client-core audio / desktop-gpui / GPUI gate |
| R20 | user | 利用開始と開発のセットアップを共通化する。Windowsアプリは初回画面、Windows/macOS/Linuxサーバーは対話式。展開型配布、端末別接続ファイル、選択式のOS起動時常駐を提供し、今後のアプリにも再利用する | app-support / server setup / application manifest / ADR 0006 |
| D01 | design | Rust + Tokio + Axum、SQLx + SQLite | server / storage-sqlite |
| D02 | design | R17に従いRust + GPUIで継続開発する。対象環境での適合性合格を本採用の条件とする | desktop-gpui / GPUI gate / ADR 0004 |
| D03 | design | 一つの私有Gitリポジトリ内の複数プロジェクト | Cargo workspace |
| D04 | design | 標準能力は独立Rust crate、同一プロセス。構成は能力契約と明示した接続先で検証 | modules / ADR 0003 |
| T01 | target | 雑談音声の初動p50 1〜2秒、p95 3秒以内（warm/LAN） | performance harness |
| T02 | target | 発話検出後の再生停止150ms以下 | playback tests + device measurements |
| T03 | target | クライアント300MB、実行核200MB。外部処理とGPUは別途総量報告 | resource measurements |
| T04 | target | 無人活動の最短間隔30分、1計画最大3行動 | configurable autonomy policy |

## 互換性と情報の保護

原記録・明示情報・推測・要約・人格・一時状態を分離する。
記憶に出典・日時・生成モデル・バージョン・公開範囲を持たせる。
AIの自己申告や反復を根拠に、推測を確認済み情報へ昇格しない。
訂正・削除を派生記憶にも反映し、古い記録からの復活を拒否する。
原記録が失われた情報を再検証済みとして扱わない。

## 初期対象外

複数ユーザー、プラグインストア、未信頼コードの安全な実行、インターネット一般公開、
Mac版GUI、Live2D/VRM本体、画面・カメラ認識、実際のPC/家電操作。
Live2Dの無料サンプルは後段候補とし、モデル素材とSDKの条件を別々に確認する。
