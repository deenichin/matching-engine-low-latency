//! `risk.toml` loading (SPEC §5).

use core::types::{MAX_NOTIONAL_TICKS, MAX_OPEN_ORDERS, PRICE_BAND_PCT};

/// The three risk-control values SPEC §5 defines, with the SPEC-mandated
/// defaults baked in via `core`'s own named constants — so a fresh
/// `docker compose up` needs no host setup even if `risk.toml` is missing
/// or unreadable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskConfig {
    pub max_open_orders: usize,
    pub max_notional: u128,
    pub price_band_pct: u64,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_open_orders: MAX_OPEN_ORDERS,
            max_notional: MAX_NOTIONAL_TICKS,
            price_band_pct: PRICE_BAND_PCT,
        }
    }
}

impl RiskConfig {
    /// Loads `max_open_orders`, `max_notional_ticks`, and `price_band_pct`
    /// from a `key = value` file at `path`.
    ///
    /// `risk.toml` has exactly three flat, plain-integer keys — a full
    /// TOML parser is not worth a new dependency for that (SPEC §9
    /// justifies every dependency in this project individually; this one
    /// would not clear that bar). Parses the minimal subset this file
    /// actually needs: one `key = value` per line, `#` starts a comment,
    /// blank lines ignored, `_` digit separators stripped before parsing
    /// (TOML allows them in integers).
    ///
    /// A missing file, an unreadable file, a missing key, or a key that
    /// fails to parse all fall back to that field's default rather than
    /// erroring — consistent with "no host setup required" (SPEC §5):
    /// the daemon must start correctly with no `risk.toml` present at
    /// all, not just with a well-formed one.
    pub fn load(path: &std::path::Path) -> Self {
        let mut config = Self::default();
        let Ok(contents) = std::fs::read_to_string(path) else {
            return config;
        };

        for line in contents.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim().replace('_', "");

            match key {
                "max_open_orders" => {
                    if let Ok(v) = value.parse() {
                        config.max_open_orders = v;
                    }
                }
                "max_notional_ticks" => {
                    if let Ok(v) = value.parse() {
                        config.max_notional = v;
                    }
                }
                "price_band_pct" => {
                    if let Ok(v) = value.parse() {
                        config.price_band_pct = v;
                    }
                }
                _ => {}
            }
        }

        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_the_spec_defaults() {
        let config = RiskConfig::load(std::path::Path::new("/nonexistent/risk.toml"));
        assert_eq!(config, RiskConfig::default());
        assert_eq!(config.max_open_orders, 50);
        assert_eq!(config.max_notional, 100_000_000);
        assert_eq!(config.price_band_pct, 10);
    }

    #[test]
    fn loads_the_repo_risk_toml() {
        // The actual file this project ships, so a change to its values
        // or format is caught here, not just in a synthetic fixture.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../risk.toml");
        let config = RiskConfig::load(&path);
        assert_eq!(
            config,
            RiskConfig::default(),
            "risk.toml should match SPEC §5's baked-in defaults"
        );
    }

    #[test]
    fn parses_underscored_integers_and_ignores_comments_and_blank_lines() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("risk-config-test-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "# comment\n\nmax_open_orders = 7\nmax_notional_ticks = 1_000_000\nprice_band_pct = 5\n",
        )
        .unwrap();

        let config = RiskConfig::load(&path);
        std::fs::remove_file(&path).ok();

        assert_eq!(config.max_open_orders, 7);
        assert_eq!(config.max_notional, 1_000_000);
        assert_eq!(config.price_band_pct, 5);
    }

    #[test]
    fn missing_keys_keep_their_individual_defaults() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "risk-config-partial-test-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "max_open_orders = 3\n").unwrap();

        let config = RiskConfig::load(&path);
        std::fs::remove_file(&path).ok();

        assert_eq!(config.max_open_orders, 3);
        assert_eq!(config.max_notional, MAX_NOTIONAL_TICKS);
        assert_eq!(config.price_band_pct, PRICE_BAND_PCT);
    }
}
