use super::*;
use inori_domain::{Character, MemoryKind, MemoryRecord, SourceRecord};

fn source(id: &str, turn: &str, actor: Actor, text: &str) -> SourceRecord {
    SourceRecord {
        id: id.into(),
        actor,
        text: text.into(),
        created_at: 1,
        audience: Audience::Private,
        turn_id: Some(turn.into()),
        model: None,
        suppressed: false,
    }
}

fn input() -> DialogueInput {
    let request = "今の話を聞いて".to_owned();
    DialogueInput {
        character: Character::default(),
        memories: vec![],
        sources: vec![source("current", "now", Actor::User, &request)],
        audience: Audience::Private,
        mode: DialogueMode::Auto,
        request_source_id: "current".into(),
        request,
        profile: ProviderProfile::default(),
        awareness: None,
    }
}

fn prepare(input: DialogueInput) -> GenerationContext {
    StandardDialogue::new(json!({}))
        .unwrap()
        .prepare(input)
        .unwrap()
}

fn chars(context: &GenerationContext) -> usize {
    context.system.chars().count()
        + context
            .messages
            .iter()
            .map(|(_, t)| t.chars().count())
            .sum::<usize>()
}

fn memory(id: &str, text: &str) -> MemoryRecord {
    MemoryRecord {
        id: id.into(),
        text: text.into(),
        kind: MemoryKind::Inference,
        status: MemoryStatus::Active,
        audience: Audience::Private,
        source_ids: vec!["synthetic-evidence".into()],
        created_at: 1,
        expires_at: None,
        model: None,
        processor_version: "test/1".into(),
        supersedes: None,
        revision: 0,
    }
}

#[test]
fn current_input_is_present_exactly_once_and_repeated_history_is_preserved() {
    let mut data = input();
    data.sources.insert(
        0,
        source("old-user", "previous", Actor::User, &data.request),
    );
    data.sources.insert(
        1,
        source("old-reply", "previous", Actor::Assistant, "前の返事"),
    );
    let result = prepare(data);
    assert_eq!(
        result.messages,
        vec![
            ("user".into(), "今の話を聞いて".into()),
            ("assistant".into(), "前の返事".into()),
            ("user".into(), "今の話を聞いて".into())
        ]
    );
}

#[test]
fn newest_whole_turn_wins_over_old_history_and_large_memories() {
    let mut data = input();
    let mandatory = chars(&prepare(input()));
    data.profile.max_context_chars = mandatory + 4;
    data.sources = vec![
        source("old-user", "old", Actor::User, &"旧".repeat(100)),
        source("old-reply", "old", Actor::Assistant, "古い返答"),
        source("recent-user", "recent", Actor::User, "直近"),
        source("recent-reply", "recent", Actor::Assistant, "返答"),
        source("current", "now", Actor::User, &data.request),
    ];
    data.memories.push(memory("large", &"古い記憶".repeat(200)));
    let result = prepare(data);
    assert_eq!(
        result.messages,
        vec![
            ("user".into(), "直近".into()),
            ("assistant".into(), "返答".into()),
            ("user".into(), "今の話を聞いて".into())
        ]
    );
    assert_eq!(chars(&result), mandatory + 4);
}

#[test]
fn history_never_keeps_an_orphan_reply_or_splits_a_turn_to_fit() {
    let mut data = input();
    data.profile.max_context_chars = chars(&prepare(input())) + 1;
    data.sources = vec![
        source("orphan", "cut", Actor::Assistant, "切れた先頭"),
        source("u", "last", Actor::User, "長い入力"),
        source("a", "last", Actor::Assistant, "返"),
        source("current", "now", Actor::User, &data.request),
    ];
    assert_eq!(
        prepare(data).messages,
        vec![("user".into(), "今の話を聞いて".into())]
    );
}

#[test]
fn exact_unicode_budget_includes_all_policy_text_and_never_truncates_request() {
    let mut data = input();
    data.request = "🌸𠮷こんにちは".into();
    data.sources.clear();
    let mandatory = chars(&prepare(input()));
    // Use the exact mandatory size with a non-ASCII current input.
    let request = data.request.clone();
    let exact = mandatory - "今の話を聞いて".chars().count() + request.chars().count();
    data.profile.max_context_chars = exact;
    let result = prepare(data);
    assert_eq!(chars(&result), exact);
    assert_eq!(result.messages, vec![("user".into(), request)]);
}

#[test]
fn mandatory_context_that_does_not_fit_is_rejected_instead_of_dropping_input() {
    let mut data = input();
    data.profile.max_context_chars = chars(&prepare(input())) - 1;
    assert!(
        StandardDialogue::new(json!({}))
            .unwrap()
            .prepare(data)
            .is_err()
    );
}

#[test]
fn current_source_identity_must_match_the_input() {
    let mut data = input();
    data.sources[0].text = "別の入力".into();
    assert!(
        StandardDialogue::new(json!({}))
            .unwrap()
            .prepare(data)
            .is_err()
    );
}

#[test]
fn records_after_the_current_source_cannot_change_an_older_turn() {
    let mut data = input();
    data.sources
        .push(source("future", "future", Actor::User, "後の会話"));
    assert_eq!(
        prepare(data).messages,
        vec![("user".into(), "今の話を聞いて".into())]
    );
}

#[test]
fn public_context_excludes_private_persona_memory_and_history() {
    let mut data = input();
    data.audience = Audience::Public;
    data.sources.last_mut().unwrap().audience = Audience::Public;
    data.character.persona = "private-persona-secret".into();
    data.memories
        .push(memory("private", "private-memory-secret"));
    data.sources.insert(
        0,
        source("private", "private", Actor::User, "private-history-secret"),
    );
    let result = prepare(data);
    assert!(!result.system.contains("secret"));
    assert!(result.messages.iter().all(|(_, t)| !t.contains("secret")));
}

#[test]
fn suppressed_sources_and_inactive_memories_are_not_prompt_evidence() {
    let mut data = input();
    let mut hidden = source("suppressed", "deleted", Actor::User, "suppressed-secret");
    hidden.suppressed = true;
    data.sources.insert(0, hidden);
    let mut deleted = memory("deleted", "deleted-secret");
    deleted.status = MemoryStatus::Deleted;
    data.memories.push(deleted);
    let result = prepare(data);
    assert!(!result.system.contains("secret"));
    assert!(result.messages.iter().all(|(_, t)| !t.contains("secret")));
}

#[test]
fn oversized_optional_memory_does_not_block_a_later_small_memory() {
    let mut data = input();
    let small = memory("small", "小さな根拠");
    let limit = chars(&prepare(input())) + 2_000;
    data.profile.max_context_chars = limit;
    data.memories = vec![memory("large", &"長".repeat(20_000)), small];
    let result = prepare(data);
    assert!(result.system.contains("小さな根拠"));
    assert!(!result.system.contains("large"));
    assert!(chars(&result) <= limit);
}

#[test]
fn auto_does_not_classify_words_and_uses_the_provider_output_budget() {
    let mut system = None;
    for request in [
        "こんにちは",
        "理由はいらない",
        "詳しくなくていい",
        "設計という言葉",
        "比較はしない",
        "少し詳しく話して",
    ] {
        let mut data = input();
        data.request = request.into();
        data.sources[0].text = request.into();
        let result = prepare(data);
        assert_eq!(result.max_output_tokens, 384);
        if let Some(previous) = &system {
            assert_eq!(&result.system, previous);
        }
        system = Some(result.system);
    }
}

#[test]
fn explicit_modes_and_small_provider_budgets_are_preserved() {
    let mut policies = Vec::new();
    for (mode, expected) in [
        (DialogueMode::Auto, 384),
        (DialogueMode::Quick, 160),
        (DialogueMode::Deliberate, 384),
    ] {
        let mut data = input();
        data.mode = mode;
        let result = prepare(data);
        assert_eq!(result.max_output_tokens, expected);
        policies.push(result.system);
        let mut data = input();
        data.mode = mode;
        data.profile.max_output_tokens = 48;
        assert_eq!(prepare(data).max_output_tokens, 48);
    }
    assert_ne!(policies[0], policies[1]);
    assert_ne!(policies[0], policies[2]);
    assert_ne!(policies[1], policies[2]);
    let mut data = input();
    data.mode = DialogueMode::Quick;
    assert_eq!(
        StandardDialogue::new(json!({"quick_output_tokens":96}))
            .unwrap()
            .prepare(data)
            .unwrap()
            .max_output_tokens,
        96
    );
}

fn references(context: &GenerationContext) -> Vec<Value> {
    context
        .system
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn memory_metadata_keeps_provenance_and_interpretation_separate_from_text() {
    let mut data = input();
    for kind in [
        MemoryKind::Confirmed,
        MemoryKind::Inference,
        MemoryKind::Summary,
    ] {
        let mut record = memory(&format!("{kind:?}"), "本文\n\"区切り\" 🌸");
        record.kind = kind;
        record.source_ids = vec!["original-1".into(), "original-2".into()];
        record.created_at = 1_700_000_000;
        record.expires_at = Some(1_800_000_000);
        data.memories.push(record);
    }
    let result = prepare(data);
    let records = references(&result);
    assert_eq!(records.len(), 3);
    for (record, kind) in records.iter().zip(["confirmed", "inference", "summary"]) {
        assert_eq!(record["kind"], kind);
        assert_eq!(record["text"], "本文\n\"区切り\" 🌸");
        assert_eq!(record["source_ids"], json!(["original-1", "original-2"]));
        assert_eq!(record["created_at"], 1_700_000_000);
        assert_eq!(record["expires_at"], 1_800_000_000);
    }
    assert!(chars(&result) <= ProviderProfile::default().max_context_chars);
}

#[test]
fn awareness_is_optional_and_preserves_observed_false_instead_of_inventing_a_value() {
    assert!(references(&prepare(input())).is_empty());
    for present in [true, false] {
        let mut data = input();
        data.awareness = Some(Awareness {
            utc: "2026-11-07T00:00:00Z".into(),
            present,
        });
        let records = references(&prepare(data));
        assert_eq!(
            records,
            vec![json!({"utc":"2026-11-07T00:00:00Z", "present":present})]
        );
    }
}

#[test]
fn metadata_formatting_is_included_in_the_context_budget() {
    let mut data = input();
    data.memories.push(memory("one", "引用\n🌸"));
    let exact = chars(&prepare(data));
    let mut data = input();
    data.memories.push(memory("one", "引用\n🌸"));
    data.profile.max_context_chars = exact;
    assert_eq!(chars(&prepare(data)), exact);
    let mut data = input();
    data.memories.push(memory("one", "引用\n🌸"));
    data.profile.max_context_chars = exact - 1;
    let result = prepare(data);
    assert!(references(&result).is_empty());
    assert!(chars(&result) < exact);
    assert_eq!(
        result.messages,
        vec![("user".into(), "今の話を聞いて".into())]
    );
}

#[test]
fn personality_is_supplied_by_the_character_and_output_limits_remain_mode_specific() {
    let mut data = input();
    data.character.name = "別の名前".into();
    data.character.persona = "もの静かで慎重".into();
    data.profile.max_output_tokens = 64;
    let result = prepare(data);
    assert!(result.system.contains("もの静かで慎重"));
    assert!(!result.system.contains("Inori"));
    assert!(!result.system.contains("好奇心"));
    assert_eq!(result.max_output_tokens, 64);
    let mut data = input();
    data.mode = DialogueMode::Deliberate;
    assert_eq!(prepare(data).max_output_tokens, 384);
}

#[test]
fn settings_validate_the_same_output_limits_as_the_module_descriptor() {
    assert!(StandardDialogue::new(json!({"quick_output_tokens":0})).is_err());
    assert!(StandardDialogue::new(json!({"quick_output_tokens":385})).is_err());
    assert!(StandardDialogue::new(json!({"unknown":true})).is_err());
}
