"""Check that incomplete or mismatched evidence cannot become a passing review."""
import copy
import unittest

import review_dialogue as review


def run(label):
    cases = [{"id": str(i), "category": "架空の検査", "criteria": ["本人らしさ"], "reference_ids": [], "system":label, "messages": [["user", "<script>入力</script>"]], "max_output_tokens": 160} for i in range(12)]
    for case in cases:
        case["context_sha256"] = review.context_hash(case)
    return {"schema_version": 1, "synthetic": True, "label": label, "suite_sha256": "same-suite", "profile": {"kind": "ollama", "model": "synthetic-test-profile", "max_output_tokens":384}, "repeats": 3, "cases": cases, "samples": [{"case_id": c["id"], "repeat": n, "context_sha256": c["context_sha256"], "text": "<script>返答</script>", "error": None} for n in range(3) for c in cases]}


def update_case(run_data, **fields):
    case = run_data["cases"][0]
    case.update(fields)
    case["context_sha256"] = review.context_hash(case)
    for sample in run_data["samples"]:
        if sample["case_id"] == case["id"]:
            sample["context_sha256"] = case["context_sha256"]


class ReviewTests(unittest.TestCase):
    def test_mismatched_profiles_or_inputs_are_rejected(self):
        before, after = run("before"), run("after")
        after["profile"]["model"] = "different"
        with self.assertRaises(ValueError):
            review.render(before, after)
        after = run("after")
        update_case(after, messages=[["user", "違う入力"]])
        with self.assertRaises(ValueError):
            review.render(before, after)

    def test_missing_failed_and_duplicated_generations_are_rejected(self):
        for change in (lambda r: r["samples"].pop(), lambda r: r["samples"][0].update(error="failed"), lambda r: r["samples"].append(r["samples"][0])):
            after = run("after")
            change(after)
            with self.assertRaises(ValueError):
                review.render(run("before"), after)

    def test_generated_html_escapes_prompt_and_response_content(self):
        document, key = review.render(run("before"), run("after"))
        self.assertNotIn("<script>返答", document)
        self.assertIn("&lt;script&gt;返答", document)
        self.assertIn("&lt;script&gt;入力", document)
        self.assertEqual(len(key["pairs"]), 36)

    def test_rendering_does_not_rewrite_template_words_in_model_output(self):
        after = run("after")
        literal = "FINGERPRINT ROWS BANNER COMPARISON_DESCRIPTION"
        after["samples"][0]["text"] = literal
        document, _ = review.render(run("before"), after)
        self.assertIn(literal, document)

    def test_unrated_comparison_is_pending(self):
        _, key = review.render(run("before"), run("after"))
        result = review.score(key, {"schema_version": 1, "report_id": key["report_id"], "scores": {}})
        self.assertFalse(result["provisional_thresholds_met"])
        self.assertEqual(result["complete_pairs"], 0)
        self.assertEqual(result["implementation_verdict"], "not_determined_by_ratings")
        self.assertEqual(result["threshold_scope"], "model_quality_reference_only")

    def test_output_limit_change_requires_opt_in_and_is_visible_in_report_and_scores(self):
        before, after = run("before"), run("after")
        update_case(after, max_output_tokens=256)
        with self.assertRaisesRegex(ValueError, "--allow-output-limit-change"):
            review.render(before, after)
        document, key = review.render(before, after, allow_output_limit_change=True)
        self.assertIn("出力上限が異なります", document)
        self.assertNotIn("入力・モデル・出力上限は同一", document)
        self.assertIn("出力上限: 160 tokens", document)
        self.assertIn("出力上限: 256 tokens", document)
        expected = [{"case_id":"0", "before":160, "after":256}]
        self.assertFalse(key["comparison"]["output_limits_equal"])
        self.assertEqual(key["comparison"]["output_limit_changes"], expected)
        scores = review.score(key, {"schema_version":1, "report_id":key["report_id"], "scores":{}})
        self.assertEqual(scores["output_limit_changes"], expected)
        self.assertFalse(scores["provisional_thresholds_met"])

    def test_output_limit_opt_in_still_rejects_different_input_or_provider(self):
        before, after = run("before"), run("after")
        update_case(after, messages=[["user", "別の入力"]], max_output_tokens=256)
        with self.assertRaises(ValueError):
            review.render(before, after, allow_output_limit_change=True)
        after = run("after")
        after["profile"]["model"] = "different"
        with self.assertRaises(ValueError):
            review.render(before, after, allow_output_limit_change=True)

    def test_output_limit_opt_in_cannot_exceed_the_provider_budget(self):
        after = run("after")
        update_case(after, max_output_tokens=385)
        with self.assertRaises(ValueError):
            review.render(run("before"), after, allow_output_limit_change=True)

    def test_legacy_keys_without_comparison_metadata_are_readable(self):
        _, key = review.render(run("before"), run("after"))
        key.pop("comparison")
        result = review.score(key, {"schema_version":1, "report_id":key["report_id"], "scores":{}})
        self.assertEqual(result["output_limit_changes"], [])
        self.assertFalse(result["provisional_thresholds_met"])
        self.assertIn("pending", result["human_approval"])

    def test_scores_follow_blinding_and_do_not_approve_the_next_stage(self):
        _, key = review.render(run("before"), run("after"))
        ratings = {"schema_version": 1, "report_id": key["report_id"], "scores": {}}
        for pair in key["pairs"]:
            row = {f"{side}_{dimension}": 4 if side == pair["after_side"] else 2 for side in ("A", "B") for dimension in review.DIMENSIONS}
            row["winner"] = pair["after_side"]
            ratings["scores"][pair["id"]] = row
        result = review.score(key, ratings)
        self.assertTrue(result["provisional_thresholds_met"])
        self.assertIn("pending", result["human_approval"])
        self.assertEqual(result["implementation_verdict"], "not_determined_by_ratings")
        first = key["pairs"][0]
        other = "B" if first["after_side"] == "A" else "A"
        ratings["scores"][first["id"]][f"{other}_blocking"] = True
        self.assertTrue(review.score(key, ratings)["provisional_thresholds_met"])
        ratings["scores"][first["id"]][f'{first["after_side"]}_blocking'] = True
        self.assertFalse(review.score(key, ratings)["provisional_thresholds_met"])
        mock = copy.deepcopy(key)
        mock["profile"]["kind"] = "mock"
        self.assertFalse(review.score(mock, ratings)["provisional_thresholds_met"])


if __name__ == "__main__":
    unittest.main()
