# Inori development

- Read `docs/requirements.md`, `docs/plans/inori.md`, and `docs/development/status.md` before changing architecture. Preserve the user's confirmed requirements and label assumptions, decisions, provisional targets, and unverified conditions separately.
- The server owns personality, memory, and autonomous activity. Keep domain independent of IO and keep GPUI out of the server/runtime/storage dependency graph.
- Edit public wire definitions in `contracts/v1.json`, then run `python tools/generate_contracts.py`. Do not edit generated bindings independently.
- Preserve original evidence, revision checks, correction/deletion suppression, audience filtering, explicit cloud settings, default-deny grants, operation IDs, and speech playback acknowledgement. Model confidence is not confirmation.
- Extensions are trusted subprocesses, not an OS security sandbox. Never give an AI authority to grant itself permissions.
- GPUI remains a suitability prototype until `docs/development/gpui-gate.md` is satisfied. Report measured results with their actual hardware, build profile, engine state, and sample duration.
- Never put real conversations, recordings, tokens, model weights, runtime databases, or execution logs in Git. Use `.cache` for synthetic local validation only.
- Usual validation: `cargo fmt --all -- --check`, `python tools/generate_contracts.py --check`, `python tools/check_boundaries.py`, `cargo test --locked --workspace`, and `cargo clippy --locked --workspace --exclude inori-desktop --all-targets -- -D warnings`. SDK checks are in `docs/development/quickstart.md`.
- Update the implementation status and ADRs when a boundary or decision changes. Do not claim hardware/audio/24-hour checks that were not performed.
