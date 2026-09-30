//! TOON list rendering for the CLI.
//!
//! `docs/06-axi-mcp.md` §"CLI output": lists default to 3-4 fields plus a
//! `count: N of TOTAL` line, and bodies truncate with a `(truncated, N chars)`
//! marker. P0 lists are small enough that the truncation path is dead code —
//! a provider chain does not produce megabyte rows — so it is not written. It
//! arrives with the P4 `providers`/`combo` commands that do have long fields.
//!
//! Format: a header line naming the fields, then one row per record, then the
//! count. Column-aligned so a human reads it and an agent tokenises it cheaply.

use crate::models::ModelCard;

/// Default fields for a model listing, per the 3-4 field rule.
pub const DEFAULT_FIELDS: &[&str] = &["id", "provider", "model"];

/// Renders a model list as TOON.
///
/// `fields` names the columns; unknown names are ignored, and an empty result
/// after filtering renders the empty-state line rather than a bare header.
#[must_use]
pub fn models(cards: &[ModelCard], fields: &[&str]) -> String {
    let mut cols: Vec<&str> = fields.iter().copied().filter(|f| DEFAULT_FIELDS.contains(f)).collect();
    if cols.is_empty() {
        cols = DEFAULT_FIELDS.to_vec();
    }

    if cards.is_empty() {
        return "models: 0 configured — set AR_UPSTREAM_MODEL to expose one\n".to_owned();
    }

    let mut out = String::with_capacity(64 * cards.len());
    out.push_str("models[");
    out.push_str(&cards.len().to_string());
    out.push_str("]{");
    out.push_str(&cols.join(","));
    out.push_str("}\n");

    for card in cards {
        for (i, col) in cols.iter().enumerate() {
            if i > 0 {
                out.push('\t');
            }
            out.push_str(match *col {
                "provider" => &card.provider,
                "model" => &card.upstream_model,
                _ => &card.id,
            });
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_FIELDS, models};
    use crate::models::ModelCard;

    fn cards() -> Vec<ModelCard> {
        vec![
            ModelCard::new("openai", "gpt-4o-mini"),
            ModelCard::new("groq", "llama-3.3-70b"),
        ]
    }

    #[test]
    fn renders_header_with_count_and_fields() {
        let got = models(&cards(), DEFAULT_FIELDS);
        assert!(got.starts_with("models[2]{id,provider,model}\n"));
    }

    #[test]
    fn renders_one_tab_separated_row_per_model() {
        let got = models(&cards(), DEFAULT_FIELDS);
        let rows: Vec<&str> = got.lines().skip(1).collect();
        assert_eq!(rows[0], "openai/gpt-4o-mini\topenai\tgpt-4o-mini");
    }

    #[test]
    fn renders_definitive_empty_state() {
        let got = models(&[], DEFAULT_FIELDS);
        assert!(got.contains("0 configured"));
    }

    #[test]
    fn falls_back_to_default_fields_when_none_requested() {
        let got = models(&cards(), &["nonsense"]);
        assert!(got.starts_with("models[2]{id,provider,model}"));
    }

    #[test]
    fn honours_a_narrower_field_selection() {
        let got = models(&cards(), &["provider", "model"]);
        assert!(got.starts_with("models[2]{provider,model}\n"));
    }
}
