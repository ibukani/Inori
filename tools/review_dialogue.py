"""Create a local, blinded review of synthetic dialogue runs; never calls an AI.

Generation is performed by the server's dialogue-review example through the real
inference adapter. Human ratings are distinct from test results and never imply
that the user has approved a personality or the next implementation stage.
"""
import argparse
import hashlib
import html
import json
import math
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DIMENSIONS = ("personality", "naturalness", "situational_fit")


def read(path):
    return json.loads(path.read_text(encoding="utf-8"))


def cache_output(path):
    path = path.resolve()
    if not path.is_relative_to((ROOT / ".cache").resolve()):
        raise ValueError("review output must stay inside repository .cache")
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


def context_hash(case):
    fields = {name: case[name] for name in ("system", "messages", "max_output_tokens")}
    return hashlib.sha256(json.dumps(fields, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def validate_pair(before, after, *, allow_output_limit_change=False):
    for run in (before, after):
        if run.get("schema_version") != 1 or run.get("synthetic") is not True:
            raise ValueError("expected version 1 synthetic runs")
        if len(run.get("cases", [])) != 12 or run.get("repeats") != 3:
            raise ValueError("quality review requires 12 cases, each repeated 3 times")
    if before["profile"] != after["profile"] or before["suite_sha256"] != after["suite_sha256"]:
        raise ValueError("provider settings and evaluation suite must match")
    cases = []
    indexed = []
    for run in (before, after):
        current = {case["id"]: case for case in run["cases"]}
        if len(current) != 12:
            raise ValueError("case IDs must be unique")
        if any(context_hash(case) != case["context_sha256"] for case in current.values()):
            raise ValueError("prepared context digest does not match its content")
        for case in current.values():
            limit = case["max_output_tokens"]
            if type(limit) is not int or limit <= 0 or limit > run["profile"].get("max_output_tokens", limit):
                raise ValueError("case output limit exceeds the provider budget")
        cases.append(current)
        samples = {}
        for sample in run["samples"]:
            key = (sample["case_id"], sample["repeat"])
            if key in samples or key[0] not in current or key[1] not in range(3):
                raise ValueError("unexpected or duplicated sample")
            if sample.get("error") is not None or not sample.get("text", "").strip():
                raise ValueError("failed or empty generation cannot pass as a comparison")
            if sample["context_sha256"] != current[key[0]]["context_sha256"]:
                raise ValueError("sample does not belong to the prepared context")
            samples[key] = sample
        if len(samples) != 36:
            raise ValueError("comparison requires all 36 samples in each run")
        indexed.append(samples)
    if set(cases[0]) != set(cases[1]):
        raise ValueError("case IDs must match")
    for case_id, case in cases[0].items():
        other = cases[1][case_id]
        for field in ("messages", "criteria", "category", "reference_ids"):
            if case[field] != other[field]:
                raise ValueError(f"review case {case_id} differs in {field}")
        if case["max_output_tokens"] != other["max_output_tokens"] and not allow_output_limit_change:
            raise ValueError("case output limits differ; use --allow-output-limit-change for an intentional policy comparison")
    return cases[0], indexed


def render(before, after, *, allow_output_limit_change=False):
    cases, indexed = validate_pair(before, after, allow_output_limit_change=allow_output_limit_change)
    after_cases = {case["id"]: case for case in after["cases"]}
    limit_changes = [{"case_id": case_id, "before": case["max_output_tokens"], "after": after_cases[case_id]["max_output_tokens"]} for case_id, case in cases.items() if case["max_output_tokens"] != after_cases[case_id]["max_output_tokens"]]
    fingerprint = hashlib.sha256(json.dumps([before, after], ensure_ascii=False, sort_keys=True).encode()).hexdigest()
    rows = []
    assignments = []
    for index, (case_id, repeat) in enumerate(indexed[0]):
        case = cases[case_id]
        # Stable for a particular report without labelling the variant.
        after_side = "A" if int(hashlib.sha256(f"{fingerprint}:{index}".encode()).hexdigest(), 16) % 2 else "B"
        pair_id = f"pair-{index}"
        assignments.append({"id": pair_id, "case_id": case_id, "repeat": repeat, "after_side": after_side})
        choices = []
        for side in ("A", "B"):
            sample = indexed[int(side == after_side)][(case_id, repeat)]
            limit = (after_cases if side == after_side else cases)[case_id]["max_output_tokens"]
            rating = "".join(f'<label>{label}<select data-pair="{pair_id}" data-field="{side}_{dimension}"><option value="">未評価</option>'+"".join(f'<option value="{n}">{n}</option>' for n in range(1, 6))+"</select></label>" for dimension, label in zip(DIMENSIONS, ("本人らしさ", "自然さ", "場面への適切さ")))
            choices.append(f'<section><h3>{side}</h3><p class="criteria">出力上限: {limit} tokens</p><p class="reply">{html.escape(sample["text"])}</p><div class="ratings">{rating}</div><label><input type="checkbox" data-pair="{pair_id}" data-field="{side}_blocking">モデル応答の問題を観測（体験の捏造など）</label></section>')
        criteria = " / ".join(case["criteria"])
        request = case["messages"][-1][1]
        rows.append(f'<article><h2>{index + 1}. {html.escape(case["category"])} · {repeat + 1}/3</h2><p class="request">{html.escape(request)}</p><p class="criteria">観察点: {html.escape(criteria)}</p><div class="pair">'+"".join(choices)+f'</div><label>好ましい返答 <select data-pair="{pair_id}" data-field="winner"><option value="">未評価</option><option>A</option><option>B</option><option value="tie">同等</option></select></label><label>気になった点 <textarea data-pair="{pair_id}" data-field="comment"></textarea></label></article>')
    key = {"schema_version": 1, "report_id": fingerprint, "profile": before["profile"], "before_label": before["label"], "after_label": after["label"], "pairs": assignments, "comparison": {"output_limits_equal": not limit_changes, "output_limit_changes": limit_changes}, "human_approval": "pending"}
    mock = before["profile"]["kind"] == "mock"
    banner = "mockによる動作確認です。会話品質の証拠にはできません。" if mock else "実AIの生成結果です。人による採点と実会話の確認は未完了です。"
    comparison = "入力・モデル・出力上限は同一で、人格と対話方針を比較します。"
    if limit_changes:
        comparison = f"入力・モデル設定は同一ですが、{len(limit_changes)}場面で出力上限が異なります。明示的に許可した方針変更の比較です。各返答に上限を表示しており、出力上限が同一の比較とは区別してください。"
    document = '''<!doctype html><html lang="ja"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Inori 第1段階の会話比較</title>
<style>body{font:16px/1.7 system-ui,sans-serif;color:#243647;background:#f4faff;max-width:1200px;margin:auto;padding:24px}header,article{background:white;border:1px solid #c5dfef;border-radius:12px;padding:24px;margin:0 0 24px}.pair{display:grid;grid-template-columns:1fr 1fr;gap:24px}.reply,.request{white-space:pre-wrap;overflow-wrap:anywhere}.reply{min-height:100px}.request{background:#eaf5ff;padding:12px}.criteria{color:#526879}.ratings{display:flex;flex-wrap:wrap;gap:8px}label{display:block;margin:8px 0}select,textarea,button{font:inherit;padding:5px}textarea{display:block;box-sizing:border-box;width:100%}button{background:#d8edff;border:1px solid #a7cce5;border-radius:8px;cursor:pointer}@media(max-width:720px){.pair{grid-template-columns:1fr}body{padding:10px}}</style>
<header><h1>Inori 第1段階の会話比較</h1><p>BANNER</p><p>12場面 × 3回。COMPARISON_DESCRIPTION A/Bは場面ごとに入れ替えています。入力は架空の検査用発言で、返答の正解文は設定していません。</p><p>1 = 強い違和感、3 = 許容、5 = 自然に感じる。採点と問題のチェックはモデル応答の観測です。実装の合否は入力・状態・権限・保存の検査で判定し、採点からユーザー承認を自動決定しません。</p><button id="export">採点結果をJSONで保存</button><p id="progress"></p></header>ROWS
<script>
const reportId = "FINGERPRINT";
let scores = {};
try { scores = JSON.parse(localStorage.getItem(reportId) || '{}'); } catch {}
const controls = [...document.querySelectorAll('[data-pair]')];
function update() {
  const rated = controls.filter(x => x.dataset.field === 'winner' && x.value).length;
  document.getElementById('progress').textContent = `${rated} / 36 組の比較を選択済み`;
  try { localStorage.setItem(reportId, JSON.stringify(scores)); } catch {}
}
for (const control of controls) {
  const p = control.dataset.pair, f = control.dataset.field;
  if (scores[p] && scores[p][f] !== undefined) {
    if (control.type === 'checkbox') control.checked = scores[p][f]; else control.value = scores[p][f];
  }
  control.addEventListener('change', () => {
    scores[p] ||= {};
    scores[p][f] = control.type === 'checkbox' ? control.checked : control.value;
    update();
  });
}
document.getElementById('export').onclick = () => {
  const url = URL.createObjectURL(new Blob([JSON.stringify({schema_version:1, report_id:reportId, scores}, null, 2)], {type:'application/json'}));
  const a = document.createElement('a'); a.href=url; a.download='dialogue-stage1-ratings.json'; a.click(); setTimeout(()=>URL.revokeObjectURL(url),1000);
};
update();
</script></html>'''
    # Insert response text last so literal template words in model output stay intact.
    return document.replace("FINGERPRINT", fingerprint).replace("BANNER", banner).replace("COMPARISON_DESCRIPTION", comparison).replace("ROWS", "\n".join(rows)), key


def score(key, ratings):
    if ratings.get("schema_version") != 1 or ratings.get("report_id") != key["report_id"]:
        raise ValueError("ratings belong to a different report")
    values = {dimension: [] for dimension in DIMENSIONS}
    winners = 0
    complete = 0
    blocking = []
    for pair in key["pairs"]:
        row = ratings.get("scores", {}).get(pair["id"], {})
        if row.get(f'{pair["after_side"]}_blocking'):
            blocking.append(pair["id"])
        if row.get("winner") not in ("A", "B", "tie"):
            continue
        try:
            all_ratings = {f"{side}_{dim}": row[f"{side}_{dim}"] for side in ("A", "B") for dim in DIMENSIONS}
        except (KeyError, ValueError, TypeError):
            continue
        if any(type(n) not in (int, str) or str(n) not in ("1", "2", "3", "4", "5") for n in all_ratings.values()):
            continue
        all_ratings = {field: int(n) for field, n in all_ratings.items()}
        complete += 1
        winners += row["winner"] == pair["after_side"]
        for dimension in DIMENSIONS:
            values[dimension].append(all_ratings[f'{pair["after_side"]}_{dimension}'])
    averages = {dim: sum(numbers) / len(numbers) if numbers else None for dim, numbers in values.items()}
    real_ai = key["profile"]["kind"] != "mock"
    passed = real_ai and complete == 36 and not blocking and winners >= math.ceil(36 * 2 / 3) and all(n is not None and n >= 4 for n in averages.values())
    # Keep the old blocking_pairs field readable as model-quality observations;
    # neither that field nor the reference score decides implementation acceptance.
    return {"schema_version": 1, "report_id": key["report_id"], "complete_pairs": complete, "total_pairs": 36, "after_wins": winners, "after_averages": averages, "blocking_pairs": blocking, "model_observation_pairs": blocking, "real_ai": real_ai, "provisional_thresholds_met": passed, "threshold_scope": "model_quality_reference_only", "implementation_verdict": "not_determined_by_ratings", "output_limit_changes": key.get("comparison", {}).get("output_limit_changes", []), "human_approval": "pending; explicit user confirmation and live conversation still required"}


def main():
    parser = argparse.ArgumentParser(__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    create = commands.add_parser("create")
    create.add_argument("--before", type=Path, required=True)
    create.add_argument("--after", type=Path, required=True)
    create.add_argument("--output", type=Path, required=True)
    create.add_argument("--allow-output-limit-change", action="store_true", help="Allow intentional per-case output limit changes; input and provider settings must still match")
    grade = commands.add_parser("score")
    grade.add_argument("--key", type=Path, required=True)
    grade.add_argument("--ratings", type=Path, required=True)
    grade.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    destination = cache_output(args.output)
    if args.command == "create":
        document, key = render(read(args.before), read(args.after), allow_output_limit_change=args.allow_output_limit_change)
        destination.write_text(document, encoding="utf-8")
        destination.with_suffix(".key.json").write_text(json.dumps(key, ensure_ascii=False, indent=2), encoding="utf-8")
        print(f"Created {len(key['pairs'])} blinded pairs; human review pending.")
    else:
        result = score(read(args.key), read(args.ratings))
        destination.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
        print(json.dumps(result, ensure_ascii=False))


if __name__ == "__main__":
    main()
