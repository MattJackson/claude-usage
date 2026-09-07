//! Per-provider 16px PNG menu icons.
//!
//! Icons live in `assets/icons/16/<file>.png` and are pulled into the binary at
//! compile time via `include_bytes!`. `png16_for` maps a provider slug (the
//! stable `Provider::provider_id`) to its bundled bytes, returning `None` when
//! no icon is shipped for that slug (e.g. `vertex-ai`, or an unknown slug from
//! a future provider). Attribution for each icon lives in
//! `THIRD_PARTY_NOTICES.md` at the repo root.
//!
//! The macOS menu-bar renderer calls `png16_for` while walking the native
//! `NSMenu` and passes the bytes into `NSImage::initWithData` to decorate the
//! section header row for each provider. A missing icon just leaves the row
//! text-only — the menu still renders correctly.

/// Bytes of the 16px PNG for the provider identified by `slug`. Returns `None`
/// if no icon is bundled for that slug. Pure function — safe to call from tests.
pub fn png16_for(slug: &str) -> Option<&'static [u8]> {
    match slug {
        "claude" => Some(include_bytes!("../assets/icons/16/claude-code.png").as_slice()),
        "codex" => Some(include_bytes!("../assets/icons/16/codex.png").as_slice()),
        "opencode" => Some(include_bytes!("../assets/icons/16/opencode.png").as_slice()),
        "gemini-cli" => Some(include_bytes!("../assets/icons/16/gemini-cli.png").as_slice()),
        "qwen-code" => Some(include_bytes!("../assets/icons/16/qwen-code.png").as_slice()),
        "copilot-cli" => Some(include_bytes!("../assets/icons/16/copilot-cli.png").as_slice()),
        "cursor-agent" => Some(include_bytes!("../assets/icons/16/cursor-agent.png").as_slice()),
        "amazon-q" => Some(include_bytes!("../assets/icons/16/amazon-q.png").as_slice()),
        "cline" => Some(include_bytes!("../assets/icons/16/cline.png").as_slice()),
        "grok" => Some(include_bytes!("../assets/icons/16/grok.png").as_slice()),
        "kimi" => Some(include_bytes!("../assets/icons/16/kimi.png").as_slice()),
        "openrouter" => Some(include_bytes!("../assets/icons/16/openrouter.png").as_slice()),
        "deepseek" => Some(include_bytes!("../assets/icons/16/deepseek.png").as_slice()),
        "zai" => Some(include_bytes!("../assets/icons/16/zai.png").as_slice()),
        "fireworks" => Some(include_bytes!("../assets/icons/16/fireworks.png").as_slice()),
        "synthetic" => Some(include_bytes!("../assets/icons/16/synthetic.png").as_slice()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png16_for_returns_bytes_for_registered_providers() {
        // Spot-check a representative slug from each icon source (CC0 + MIT).
        assert!(png16_for("claude").is_some(), "claude bundled");
        assert!(png16_for("codex").is_some(), "codex bundled");
        assert!(png16_for("grok").is_some(), "grok bundled");
        assert!(png16_for("synthetic").is_some(), "synthetic placeholder bundled");
    }

    #[test]
    fn png16_bytes_are_valid_png_header() {
        // First 8 bytes of any PNG are the fixed signature.
        const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let bytes = png16_for("claude").expect("bundled");
        assert!(bytes.len() > 8);
        assert_eq!(&bytes[..8], &PNG_SIG, "expected PNG magic");
    }

    #[test]
    fn png16_for_returns_none_for_unknown_slug() {
        assert!(png16_for("does-not-exist").is_none());
        // vertex-ai is registered as a provider but no icon is bundled for it.
        assert!(png16_for("vertex-ai").is_none());
    }

    #[test]
    fn png16_for_covers_every_provider_with_shipped_icon() {
        // Guards against forgetting a slug when the icon set is extended.
        // (`vertex-ai` deliberately excluded — no icon shipped.)
        for slug in [
            "claude", "codex", "opencode", "gemini-cli", "qwen-code", "copilot-cli",
            "cursor-agent", "amazon-q", "cline", "grok", "kimi", "openrouter",
            "deepseek", "zai", "fireworks", "synthetic",
        ] {
            assert!(
                png16_for(slug).is_some(),
                "png16_for(\"{slug}\") returned None — missing include_bytes! branch?",
            );
        }
    }
}
