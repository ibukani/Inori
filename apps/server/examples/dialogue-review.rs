//! Synthetic, database-free dialogue review. Exports the real policy's inputs so
//! before/after runs can use exactly the same provider without retaining live chats.
use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use inori_dialogue_standard::StandardDialogue;
use inori_domain::{Actor, Audience, Character, MAX_TEXT_BYTES, SourceRecord, validate_text};
use inori_inference_providers::HttpProvider;
use inori_module_api::{DialogueInput, DialoguePolicy, GenerationContext, ProviderProfile};
use inori_protocol::DialogueMode;
use inori_server::ServerConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Component, PathBuf},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// In-memory conversation only. Does not open a database or write a transcript.
    Chat {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        persona_file: PathBuf,
    },
    /// No inference or network access. Run this before changing the policy.
    Prepare {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        suite: PathBuf,
        #[arg(long)]
        persona_file: Option<PathBuf>,
        #[arg(long)]
        label: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Explicitly sends only the exported synthetic contexts to their provider.
    Generate {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=10))]
        repeats: u32,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Suite {
    schema_version: u32,
    synthetic: bool,
    references: Vec<Value>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    category: String,
    request: String,
    criteria: Vec<String>,
    reference_ids: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedCase {
    id: String,
    category: String,
    criteria: Vec<String>,
    reference_ids: Vec<String>,
    system: String,
    messages: Vec<(String, String)>,
    max_output_tokens: u32,
    context_sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bundle {
    schema_version: u32,
    synthetic: bool,
    label: String,
    suite_sha256: String,
    profile: ProviderProfile,
    references: Vec<Value>,
    cases: Vec<PreparedCase>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn context_digest(
    system: &str,
    messages: &[(String, String)],
    max_output_tokens: u32,
) -> Result<String> {
    Ok(digest(&serde_json::to_vec(
        &json!({"system":system,"messages":messages,"max_output_tokens":max_output_tokens}),
    )?))
}

fn save(path: &std::path::Path, value: &impl Serialize) -> Result<()> {
    // Never let evaluation output become a tracked fixture or a live database.
    let cache = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.cache");
    std::fs::create_dir_all(&cache)?;
    ensure!(
        !path.components().any(|part| part == Component::ParentDir),
        "review output must not contain parent traversal"
    );
    let parent = path.parent().context("output needs a parent directory")?;
    let ancestor = parent
        .ancestors()
        .find(|p| p.exists())
        .context("output parent has no existing ancestor")?;
    ensure!(
        ancestor.canonicalize()?.starts_with(cache.canonicalize()?),
        "review output must be inside the repository .cache directory"
    );
    std::fs::create_dir_all(parent)?;
    ensure!(
        parent.canonicalize()?.starts_with(cache.canonicalize()?),
        "review output must be inside the repository .cache directory"
    );
    ensure!(
        !path.is_symlink(),
        "review output must not be a symbolic link"
    );
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

fn configuration(
    config: PathBuf,
    persona_file: Option<PathBuf>,
) -> Result<(Character, ProviderProfile, Value)> {
    let config: ServerConfig = toml::from_str(&std::fs::read_to_string(config)?)?;
    let mut character = config.character;
    if let Some(path) = persona_file {
        character.persona = std::fs::read_to_string(path)?.trim().to_owned();
    }
    let profile = config
        .providers
        .into_iter()
        .find(|p| p.id == character.active_provider)
        .context("active provider is not configured")?;
    inori_inference_providers::validate_profile(&profile)?;
    ensure!(
        profile.allow_private,
        "review provider must explicitly allow private inputs"
    );
    let settings = config
        .composition
        .as_ref()
        .and_then(|c| c.settings.get("dialogue.standard"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok((character, profile, settings))
}

fn prepare(
    config: PathBuf,
    suite: PathBuf,
    persona_file: Option<PathBuf>,
    label: String,
    output: PathBuf,
) -> Result<()> {
    let (character, profile, settings) = configuration(config, persona_file)?;
    let bytes = std::fs::read(suite)?;
    let suite: Suite = serde_json::from_slice(&bytes)?;
    ensure!(
        suite.schema_version == 1 && suite.synthetic && suite.cases.len() == 12,
        "expected a version 1 synthetic suite with 12 cases"
    );
    let policy = StandardDialogue::new(settings)?;
    let mut ids = BTreeSet::new();
    let mut cases = Vec::new();
    for case in suite.cases {
        ensure!(
            !case.id.is_empty() && ids.insert(case.id.clone()),
            "duplicate or empty case ID"
        );
        let context = policy.prepare(DialogueInput {
            character: character.clone(),
            memories: vec![],
            sources: vec![SourceRecord {
                id: format!("review-{}", case.id),
                actor: Actor::User,
                text: case.request.clone(),
                created_at: 1,
                audience: Audience::Private,
                turn_id: Some(case.id.clone()),
                model: None,
                suppressed: false,
            }],
            audience: Audience::Private,
            mode: DialogueMode::Auto,
            request_source_id: format!("review-{}", case.id),
            request: case.request,
            profile: profile.clone(),
            awareness: None,
        })?;
        let hash = context_digest(
            &context.system,
            &context.messages,
            context.max_output_tokens,
        )?;
        cases.push(PreparedCase {
            id: case.id,
            category: case.category,
            criteria: case.criteria,
            reference_ids: case.reference_ids,
            system: context.system,
            messages: context.messages,
            max_output_tokens: context.max_output_tokens,
            context_sha256: hash,
        });
    }
    save(
        &output,
        &Bundle {
            schema_version: 1,
            synthetic: true,
            label,
            suite_sha256: digest(&bytes),
            profile,
            references: suite.references,
            cases,
        },
    )?;
    println!("Prepared 12 synthetic contexts; no inference performed.");
    Ok(())
}

async fn chat(config: PathBuf, persona_file: PathBuf) -> Result<()> {
    let (character, profile, settings) = configuration(config, Some(persona_file))?;
    let policy = StandardDialogue::new(settings)?;
    let provider = HttpProvider::new(profile.clone())?;
    let mut sources = Vec::new();
    let mut turn = 0_u64;
    println!("In-memory persona review. No transcript or database is written. /exit to end.");
    loop {
        print!("you> ");
        std::io::stdout().flush()?;
        let mut text = String::new();
        if std::io::stdin().read_line(&mut text)? == 0 {
            break;
        }
        let text = text.trim().to_owned();
        if text == "/exit" {
            break;
        }
        if text.is_empty() {
            continue;
        }
        validate_text(&text, MAX_TEXT_BYTES)?;
        ensure!(
            text.chars().count() <= profile.max_context_chars / 2,
            "message exceeds the selected provider context budget"
        );
        turn += 1;
        let id = format!("review-chat-{turn}");
        sources.push(SourceRecord {
            id: id.clone(),
            actor: Actor::User,
            text: text.clone(),
            created_at: turn as i64,
            audience: Audience::Private,
            turn_id: Some(id.clone()),
            model: None,
            suppressed: false,
        });
        let context = policy.prepare(DialogueInput {
            character: character.clone(),
            memories: vec![],
            sources: sources.clone(),
            audience: Audience::Private,
            mode: DialogueMode::Auto,
            request_source_id: id.clone(),
            request: text,
            profile: profile.clone(),
            awareness: None,
        })?;
        let reply = tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            result = inori_runtime::providers::collect(&provider, context, CancellationToken::new()) => result.context("review generation failed")?,
        };
        println!("{}> {}", character.name, reply);
        sources.push(SourceRecord {
            id: format!("{id}-reply"),
            actor: Actor::Assistant,
            text: reply,
            created_at: turn as i64,
            audience: Audience::Private,
            turn_id: Some(id),
            model: Some(profile.model.clone()),
            suppressed: false,
        });
        if sources.len() > 40 {
            sources.drain(..sources.len() - 40);
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    match Args::parse().command {
        Command::Chat {
            config,
            persona_file,
        } => chat(config, persona_file).await,
        Command::Prepare {
            config,
            suite,
            persona_file,
            label,
            output,
        } => prepare(config, suite, persona_file, label, output),
        Command::Generate {
            bundle,
            output,
            repeats,
        } => {
            let bundle: Bundle = serde_json::from_slice(&std::fs::read(bundle)?)?;
            ensure!(
                bundle.schema_version == 1 && bundle.synthetic && bundle.profile.allow_private,
                "unsupported or non-synthetic review bundle"
            );
            ensure!(bundle.cases.len() == 12, "expected 12 review cases");
            if bundle.profile.kind == "extension" {
                bail!("review requires an explicit HTTP or mock provider");
            }
            let provider = HttpProvider::new(bundle.profile.clone())?;
            let mut report = json!({"schema_version":1,"synthetic":true,"label":bundle.label,"suite_sha256":bundle.suite_sha256,"profile":bundle.profile,"repeats":repeats,"engine_state":"existing configured engine; no model download requested","build_profile":if cfg!(debug_assertions){"debug"}else{"release"},"human_review":"pending","references":bundle.references,"cases":bundle.cases,"samples":[]});
            save(&output, &report)?;
            for repeat in 0..repeats {
                for case in &bundle.cases {
                    ensure!(
                        context_digest(&case.system, &case.messages, case.max_output_tokens)?
                            == case.context_sha256,
                        "prepared review context digest does not match its content"
                    );
                    let context = GenerationContext {
                        audience: Audience::Private,
                        system: case.system.clone(),
                        messages: case.messages.clone(),
                        max_output_tokens: case.max_output_tokens,
                        purpose: "dialogue".into(),
                    };
                    ensure!(
                        context.system.chars().count()
                            + context
                                .messages
                                .iter()
                                .map(|(_, s)| s.chars().count())
                                .sum::<usize>()
                            <= bundle.profile.max_context_chars
                            && context.max_output_tokens <= bundle.profile.max_output_tokens,
                        "review context exceeds provider limits"
                    );
                    let started = Instant::now();
                    let result = inori_runtime::providers::collect(
                        &provider,
                        context,
                        CancellationToken::new(),
                    )
                    .await;
                    let row = match result {
                        Ok(text) => {
                            json!({"case_id":case.id,"repeat":repeat,"context_sha256":case.context_sha256,"text":text,"elapsed_ms":started.elapsed().as_millis(),"error":Value::Null})
                        }
                        Err(_) => {
                            json!({"case_id":case.id,"repeat":repeat,"context_sha256":case.context_sha256,"text":Value::Null,"elapsed_ms":started.elapsed().as_millis(),"error":"provider generation failed; no credential or engine error body retained"})
                        }
                    };
                    let failed = !row["error"].is_null();
                    report["samples"].as_array_mut().unwrap().push(row);
                    save(&output, &report)?;
                    println!(
                        "{} repeat {}: {}",
                        case.id,
                        repeat + 1,
                        if failed { "failed" } else { "generated" }
                    );
                    if failed {
                        bail!(
                            "review stopped at first failed inference; partial report saved without an automatic retry"
                        );
                    }
                }
            }
            Ok(())
        }
    }
}
