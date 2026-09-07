// Nothing consumes the trait scaffold yet — callers land in later phases.
// Suppress the churn of dead-code warnings on public API that will be wired
// up module-by-module.
#![allow(dead_code)]

//! Provider registry.
//!
//! `init()` populates a process-wide `Vec<Box<dyn Provider>>` — one entry per
//! feature-gated per-provider module. Callers reach providers through
//! `all()` (iteration order = registration order) or `get(slug)` (lookup by
//! `Provider::provider_id`).
//!
//! Per-provider modules do not exist yet; they land in later phases behind
//! `#[cfg(feature = "<slug>")]` gates that are already declared in
//! `Cargo.toml`. With zero features enabled this file still compiles: `init`
//! just registers an empty vector.

pub mod state;
pub mod trait_def;
pub use trait_def::*;

use std::sync::OnceLock;

// Per-provider modules will land in later phases. Each declaration below is
// gated on its Cargo feature; the modules themselves do not yet exist, so
// these lines are intentionally commented out until the corresponding phase
// creates the file. The Cargo `[features]` table already lists every slug so
// the gates line up when the modules land.
//
#[cfg(feature = "claude")]        pub mod claude;
#[cfg(feature = "codex")]         pub mod codex;
#[cfg(feature = "opencode")]      pub mod opencode;
#[cfg(feature = "gemini-cli")]    pub mod gemini_cli;
#[cfg(feature = "qwen-code")]     pub mod qwen_code;
#[cfg(feature = "copilot-cli")]   pub mod copilot_cli;
#[cfg(feature = "cursor-agent")]  pub mod cursor_agent;
#[cfg(feature = "amazon-q")]      pub mod amazon_q;
#[cfg(feature = "cline")]         pub mod cline;
#[cfg(feature = "grok")]          pub mod grok;
#[cfg(feature = "kimi")]          pub mod kimi;
#[cfg(feature = "openrouter")]    pub mod openrouter;
#[cfg(feature = "deepseek")]      pub mod deepseek;
#[cfg(feature = "zai")]           pub mod zai;
#[cfg(feature = "fireworks")]     pub mod fireworks;
#[cfg(feature = "synthetic")]     pub mod synthetic;
#[cfg(feature = "vertex-ai")]     pub mod vertex_ai;

static REGISTRY: OnceLock<Vec<Box<dyn Provider>>> = OnceLock::new();

/// Build the registry. Called once from `main` and once from `menubar::run`
/// (idempotent — the second `set` returns `Err` and is discarded).
pub fn init() {
    let _ = REGISTRY.set(build());
}

/// All registered providers in registration order.
///
/// If `init` has not been called yet (e.g. from a unit test that only exercises
/// a pure helper), this lazily populates the registry with the production build
/// so callers never see a panic. Production code paths still `init()` up-front
/// during process start; the lazy path is a safety net.
pub fn all() -> &'static [Box<dyn Provider>] {
    if REGISTRY.get().is_none() {
        let _ = REGISTRY.set(build());
    }
    REGISTRY.get().map(|v| v.as_slice()).unwrap_or(&[])
}

/// Look up a provider by its `provider_id` slug. Returns `None` if the slug is
/// not registered (either because the feature is disabled or because a caller
/// passed an unknown slug — the caller decides how to treat that).
pub fn get(id: &str) -> Option<&'static dyn Provider> {
    all().iter().map(|b| &**b).find(|p| p.provider_id() == id)
}

/// Test-only entry point: seed the same `OnceLock` with a fixture set. Safe
/// to call multiple times per process, but only the first call wins (matches
/// production semantics).
#[cfg(test)]
pub fn init_for_test(v: Vec<Box<dyn Provider>>) {
    let _ = REGISTRY.set(v);
}

// One `#[cfg(feature = "<slug>")] v.push(<slug>::new());` per provider. The
// `vec![]` macro can't express these per-element feature gates, so the
// `vec_init_then_push` lint is intentionally allowed here.
#[allow(clippy::vec_init_then_push)]
fn build() -> Vec<Box<dyn Provider>> {
    #[allow(unused_mut)]
    let mut v: Vec<Box<dyn Provider>> = Vec::new();

    #[cfg(feature = "claude")]        v.push(claude::new());
    #[cfg(feature = "codex")]         v.push(codex::new());
    #[cfg(feature = "opencode")]      v.push(opencode::new());
    #[cfg(feature = "gemini-cli")]    v.push(gemini_cli::new());
    #[cfg(feature = "qwen-code")]     v.push(qwen_code::new());
    #[cfg(feature = "copilot-cli")]   v.push(copilot_cli::new());
    #[cfg(feature = "cursor-agent")]  v.push(cursor_agent::new());
    #[cfg(feature = "amazon-q")]      v.push(amazon_q::new());
    #[cfg(feature = "cline")]         v.push(cline::new());
    #[cfg(feature = "grok")]          v.push(grok::new());
    #[cfg(feature = "kimi")]          v.push(kimi::new());
    #[cfg(feature = "openrouter")]    v.push(openrouter::new());
    #[cfg(feature = "deepseek")]      v.push(deepseek::new());
    #[cfg(feature = "zai")]           v.push(zai::new());
    #[cfg(feature = "fireworks")]     v.push(fireworks::new());
    #[cfg(feature = "synthetic")]     v.push(synthetic::new());
    #[cfg(feature = "vertex-ai")]     v.push(vertex_ai::new());

    v
}
