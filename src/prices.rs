//! Estimated cost from token counts and a price table.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::event::Usage;

const BUNDLED: &str = include_str!("prices.toml");

#[derive(Debug, Clone, Deserialize)]
pub struct Multipliers {
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
    pub cache_read: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PriceTable {
    pub multipliers: Multipliers,
    pub models: BTreeMap<String, ModelPrice>,
}

impl PriceTable {
    pub fn bundled() -> Self {
        toml::from_str(BUNDLED).expect("bundled prices.toml is valid")
    }

    /// Bundled prices with user overrides (from config) layered on top.
    pub fn with_overrides(overrides: &BTreeMap<String, ModelPrice>) -> Self {
        let mut table = Self::bundled();
        table.models.extend(overrides.clone());
        table
    }

    pub fn parse(toml_text: &str) -> Result<Self> {
        toml::from_str(toml_text).context("invalid price table")
    }

    /// Find the price for a model id as it appears in logs. Tolerates date suffixes
    /// (`claude-haiku-4-5-20251001`), provider prefixes (`us.anthropic.`) and context
    /// markers (`[1m]`) by picking the longest known id the name starts with.
    pub fn lookup(&self, model: &str) -> Option<&ModelPrice> {
        let name = normalise(model);
        if let Some(p) = self.models.get(name) {
            return Some(p);
        }
        self.models
            .iter()
            .filter(|(id, _)| is_dated_variant(name, id))
            .max_by_key(|(id, _)| id.len())
            .map(|(_, p)| p)
    }

    pub fn cost(&self, model: &str, usage: &Usage) -> Option<f64> {
        let p = self.lookup(model)?;
        let m = &self.multipliers;
        let w5 = p.cache_write_5m.unwrap_or(p.input * m.cache_write_5m);
        let w1h = p.cache_write_1h.unwrap_or(p.input * m.cache_write_1h);
        let read = p.cache_read.unwrap_or(p.input * m.cache_read);
        let total = usage.input as f64 * p.input
            + usage.output as f64 * p.output
            + usage.cache_read as f64 * read
            + usage.cache_write_5m as f64 * w5
            + usage.cache_write_1h as f64 * w1h;
        Some(total / 1_000_000.0)
    }
}

/// `claude-haiku-4-5-20251001` is a dated variant of `claude-haiku-4-5`;
/// `claude-opus-5-6` is not a variant of `claude-opus-5`.
fn is_dated_variant(name: &str, id: &str) -> bool {
    name.strip_prefix(id)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|date| date.len() >= 6 && date.bytes().all(|b| b.is_ascii_digit()))
}

fn normalise(model: &str) -> &str {
    let m = model.split('[').next().unwrap_or(model);
    let m = m.rsplit_once("anthropic.").map_or(m, |(_, rest)| rest);
    m.split('@').next().unwrap_or(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_dated_and_prefixed_ids() {
        let t = PriceTable::bundled();
        assert_eq!(t.lookup("claude-haiku-4-5-20251001").unwrap().input, 1.0);
        assert_eq!(t.lookup("us.anthropic.claude-opus-5").unwrap().input, 5.0);
        assert_eq!(t.lookup("claude-opus-5[1m]").unwrap().input, 5.0);
        // `claude-opus-5-5` must not fall back to `claude-opus-5`.
        assert_eq!(t.lookup("claude-opus-5-5").unwrap().input, 4.0);
        assert!(t.lookup("gpt-5.2").is_none());
    }

    #[test]
    fn cost_uses_cache_multipliers() {
        let t = PriceTable::bundled();
        let u = Usage {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
        };
        // opus-5: 5 + 25 + 0.5 + 6.25 + 10
        let c = t.cost("claude-opus-5", &u).unwrap();
        assert!((c - 46.75).abs() < 1e-9, "{c}");
        // fable-5-1 has an explicit cache read price.
        let fable = t
            .cost(
                "claude-fable-5-1",
                &Usage {
                    cache_read: 1_000_000,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!((fable - 0.25).abs() < 1e-9);
    }
}
