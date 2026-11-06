# サーバーの構成を変更する

構成は起動時に確定します。設定を編集し、静的検証してからサーバーを再起動します。
クライアントにはモジュール登録器を置きません。

```powershell
target/debug/inori-server.exe --data-dir C:\path\to\data --config configs/examples/companion.toml validate-config
target/debug/inori-server.exe --data-dir C:\path\to\data --config configs/examples/companion.toml
```

validate-configはDBやディレクトリを作成・更新せず、移行・拡張起動・外部接続を行いません。
既存DBがあればactive_providerだけをread-onlyで読み、未設定のモデルへ黙って切り替えないことを確認します。
有効な拡張のマニフェストと入力スキーマもファイルから静的に検査します。

## 設定例

| ファイル | 用途 |
|---|---|
| configs/examples/minimal.toml | 全能力なし。認証、履歴・人格・記憶・権限の手動管理、保存と保守は維持 |
| configs/examples/autonomous.toml | 認識・記憶整理・自律活動・mock推論。対話と標準接続なし |
| configs/examples/companion.toml | 従来相当のテキスト会話と自律活動を明示した構成 |
| configs/examples/server.toml | 従来の設定形式。composition省略で従来相当へ展開。実エンジンの設定例 |

自律構成は既存の原記録を整理し、次回の話題を予定として保存します。
高度な興味・日課・関係性の成長は後続です。全能力なしでも原記録と本人の記憶を閲覧・編集できます。

```toml
[composition]
schema_version = 1
enabled = ["inference.providers", "dialogue.standard", "channel.standard"]
settings = { "dialogue.standard" = { quick_output_tokens = 128 } }

[[composition.bindings]]
module = "dialogue.standard"
port = "inference"
provider = "inference.providers"
major = 1

[[composition.bindings]]
module = "channel.standard"
port = "dialogue"
provider = "dialogue.standard"
major = 1
```

settingsはモジュールのJSON Schemaで検査します。初期の固有設定は対話のquick_output_tokensと
perception.basicのomit_presenceです。推論の接続先は既存のproviders、音声はvoice、
共通の背景予算はautonomyを引き継ぎます。autonomy.enabledは自律判断の有効化で、記憶整理とは別です。
未知ID、重複ID、依存不足、循環、同じ能力の提供元の競合、非対応の契約版、設定の誤りは起動を拒否します。
bindingsは必須の依存だけでなく、使いたい任意の接続にも指定します。未接続の機能へ要求を流しません。
全体で有効モジュール64個・接続256個・固有設定1件64KiBを上限とします。

## 標準能力

| モジュール | 提供能力 | 必須の接続 | 任意の接続 |
|---|---|---|---|
| perception.basic | awareness.context | — | — |
| dialogue.standard | dialogue.respond | inference | awareness, synthesis |
| memory.standard | memory.process | inference | — |
| autonomy.standard | autonomy.plan | inference, awareness | — |
| inference.providers | inference.generate | — | — |
| speech.whisper | speech.recognize | — | — |
| speech.voicevox | speech.synthesize | — | — |
| actions.extensions | actions.execute | — | — |
| channel.standard | channel.standard | — | dialogue, recognition, synthesis, actions |

音声を有効にするにはenabledにspeech.whisper/speech.voicevoxを加え、voiceのURLを明示します。
channel.standardのrecognition/synthesis、発話を使うdialogue.standardのsynthesisをそれぞれ接続します。
別PCへ私的な音声や発話文を送る許可はvoice.allow_private_remoteで明示します。
composition省略時のみ、URLのある音声モジュールとその接続を自動展開します。
クラウド推論・背景利用・個別拡張の操作許可を、モジュールの有効化だけでは付与しません。

## 状態と運用

GET /v1/capabilitiesは端末の認証と公開範囲に応じた対話・音声・表示・操作の可否を返します。
GET /v1/modulesは管理端末専用で、定義・接続先・開始順・状態・利用不能の理由を返します。
設定の実値や認証情報は含めません。無効機能は認証・権限確認後、副作用の前に503と
code=module_unavailableを返します。同時処理上限は503 module_busyです。

固有状態はサーバーでmodule_id / state_schema_version / revisionと共に保存します。
無効化で削除せず、形式版が合わなければ起動を拒否します。構成履歴は人格のrevisionを増やしません。
変更・更新前はサーバーを止めてバックアップを取り、古いバイナリへ戻す場合は対応するDBも戻します。
旧DBへの追加移行はロック取得と旧形式バックアップの成功後に行います。
backupコマンド自体は移行を行わず、古いDBをその形式のまま保存できます。

標準実装の追加はcrateと起動元の登録を変更して再ビルドします。
Rust traitの外部ABIや動的ロードは提供しません。既存のJSON-RPC拡張とSDKはそのまま利用できます。
新しい第三者判断SDK、GUI構成編集、無停止差し替えは対象外です。
