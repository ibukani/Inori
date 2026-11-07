# 第1段階の会話品質の検証

対象は私的なテキスト会話。[人物像と参考作品](../character/inori.md)は候補であり、ユーザー確認待ち。
この手順は実運用のDB・人格・接続設定を更新しない。

現在の方針はR22と [ADR 0008](../adr/0008-minimal-dialogue-policy.md)。
実装の合否は入力・状態・権限・保存の検査で判定し、モデル応答の品質観測や人物像の採否と分ける。
モデルの振る舞いを補う禁止文、出力書き換え、追加LLMの監視・再生成は追加しない。

## 生成済みの比較を見る

最新の比較は `.cache/dialogue-simplification-2026-11-07/review.html` をブラウザーで開く。
Autoの単語判定の撤去に伴い、11場面で出力上限が160から384へ変わっている。
各返答に上限を表示し、入力・モデル設定が同じで出力上限が異なる方針比較として扱う。
従来の上限が同一の比較は `.cache/dialogue-stage1/qwen9b/review.html` に維持している。
12場面×3回のA/B比較で、本人らしさ・自然さ・場面への適切さを評価する。
A/Bの割り当ては組ごとに変わり、正解文は設定していない。
捏造や言葉遣いの問題は該当するモデル応答のチェック欄へ観測として記録する。
同じディレクトリの `review.key.json` が割り当て情報で、採点中は開く必要がない。

「採点結果をJSONで保存」で保存したファイルを次のように集計する。
このファイルは評点とコメントであり、実際の会話の記録を追記しない。

```powershell
python tools/review_dialogue.py score --key .cache/dialogue-simplification-2026-11-07/review.key.json --ratings C:/path/to/dialogue-stage1-ratings.json --output .cache/dialogue-simplification-2026-11-07/scores.json
```

モデル評価の仮目安は、各項目が平均4以上、36組のうち24組以上で改修版を支持、問題の指摘なし。
未評価を高得点として扱わず、生成失敗・入力やモデルの不一致・別の比較に対する採点は拒否する。
mockは比較画面の動作確認に使えても、実AIの品質の証拠には使えない。
集計の `provisional_thresholds_met` はモデル評価の目安にすぎず、`implementation_verdict` は採点から判定しない。
旧 `blocking_pairs` はモデル応答の問題として読み、同じ内容を `model_observation_pairs` にも出す。
上限差は `output_limit_changes` に残す。旧キーに比較情報がなくても採点を読み込める。
人物像の採否やユーザー承認は点数から自動決定しない。確認済み情報への誤昇格、削除内容の復活、
私的情報の公開入力への混入は引き続き実装上の不合格。

## 候補の人格で直接会話する

既存のローカルモデルが動作している状態で実行する。`/exit` またはEOFで終了する。
会話はプロセス内だけに保持し、終了時に破棄する。DBや会話ファイルを作らない。
人物像や原作の話し方を毎回説明するか、質問やからかいを押し付けるか、相手の訂正や相談を受け止めるかも確認する。

```powershell
cargo run --locked --offline -p inori-server --example dialogue-review -- chat --config configs/examples/dialogue-review.toml --persona-file configs/personas/inori-stage1.txt
```

評価用設定の `review-local` は既存の `qwen3.5:9b` を明示している。
未導入のモデルを自動取得せず、別のモデルで評価する場合は設定と比較の両側を揃える。
実クラウドはこの検証で使用していない。HTTP例を使う場合も送信先・cloud・allow_private・認証環境変数を明示する。

## 比較を再現する

改修前に、その時点の対話モジュールをビルドして文脈を保存する。
`prepare` はモデルを呼ばず、`generate` が明示したプロファイルへ架空入力を送る。
改修前後の人格・対話方針は異なり、モデルと入力を揃える。
既定では出力上限の差も拒否する。意図した上限変更の比較だけ `--allow-output-limit-change` を指定する。
このオプションでも入力・モデル設定の不一致、生成失敗・不完全な組、文脈ハッシュ不一致は許可しない。

```powershell
cargo run --locked --offline -p inori-server --example dialogue-review -- prepare --config configs/examples/dialogue-review.toml --suite tests/dialogue/stage1.json --persona-file configs/personas/inori-stage1.txt --label before-simplification --output .cache/dialogue-simplification-2026-11-07/before-contexts.json
```

改修後は人格候補を指定して別の文脈ファイルを作る。

```powershell
cargo run --locked --offline -p inori-server --example dialogue-review -- prepare --config configs/examples/dialogue-review.toml --suite tests/dialogue/stage1.json --persona-file configs/personas/inori-stage1.txt --label after-simplification --output .cache/dialogue-simplification-2026-11-07/after-contexts.json
cargo run --locked --offline -p inori-server --example dialogue-review -- generate --bundle .cache/dialogue-simplification-2026-11-07/before-contexts.json --output .cache/dialogue-simplification-2026-11-07/before-results.json --repeats 3
cargo run --locked --offline -p inori-server --example dialogue-review -- generate --bundle .cache/dialogue-simplification-2026-11-07/after-contexts.json --output .cache/dialogue-simplification-2026-11-07/after-results.json --repeats 3
python tools/review_dialogue.py create --before .cache/dialogue-simplification-2026-11-07/before-results.json --after .cache/dialogue-simplification-2026-11-07/after-results.json --output .cache/dialogue-simplification-2026-11-07/review.html --allow-output-limit-change
```

生成結果は `.cache` 内のみに出力する。`.cache` は架空入力の検証専用で、実会話を保存しない。
生成結果は文脈のハッシュを持ち、不一致や失敗では停止する。自動再試行で失敗を隠さない。
現在のコードで後から `before` と名付けても改修前の再現にはならない。
初回の改修前はコミット `b73d17dc75e74ced00e3139cf2a854f15bc4e08f` の対話処理から書き出した。
今回の修正前はその後の作業ツリーで書き出し、`before-manifest.json` と `before-persona.txt` に対応を保存した。
Windowsでは実行中のexeを再ビルドで置換できないため、生成とビルド・テストは同じ実行ファイルで並行しない。

## 現在の確認範囲

2026-11-07、Windows、debugビルド、Ollama 0.30.4。
CPUはAMD Ryzen 7 5700X、GPUはRTX 4070 SUPER（12282MiB、driver 610.88）。
実AIの条件・時間・問題は [今回の検証記録](dialogue-simplification-2026-11-07.md) と
[初回の検証記録](dialogue-validation-2026-11-07.md) に分ける。
対象8GB・内蔵GPUのGPUI適合性、実音声、24時間稼働を検証した結果ではない。
