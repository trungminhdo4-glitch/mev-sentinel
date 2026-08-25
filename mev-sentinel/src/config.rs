use std::fs;

use anyhow::{bail, Context};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub network: NetworkConfig,
    pub chains: ChainsConfig,
    pub thresholds: ThresholdConfig,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    pub binance_ws: String,
    pub binance_base_asset: String,
    pub binance_quote_asset: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ChainsConfig {
    pub mainnet: ChainConfig,
    pub arbitrum: ChainConfig,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ChainConfig {
    pub rpc_url: String,
    pub chain_id: u64,
    pub pool_address: String,
    pub token0_address: String,
    pub token0_decimals: u8,
    pub token1_address: String,
    pub token1_decimals: u8,
    pub base_token: PoolToken,
    pub quote_token: PoolToken,
    pub fee: u32,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoolToken {
    Token0,
    Token1,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ThresholdConfig {
    pub critical_lvr_usd: f64,
    pub stale_rpc_ms: u64,
    pub vola_interval_sec: f64,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let content = fs::read_to_string("config.toml").context("read config.toml")?;
        Self::parse(&content)
    }

    fn parse(content: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(content).context("deserialize config.toml")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        validate_url(
            &self.network.binance_ws,
            &["ws", "wss"],
            "network.binance_ws",
        )?;
        for (field, asset) in [
            ("binance_base_asset", &self.network.binance_base_asset),
            ("binance_quote_asset", &self.network.binance_quote_asset),
        ] {
            if asset.is_empty()
                || !asset
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
            {
                bail!("network.{field} must contain only uppercase ASCII letters and digits");
            }
        }
        if self
            .network
            .binance_base_asset
            .eq(&self.network.binance_quote_asset)
        {
            bail!("network Binance base and quote assets must be different");
        }
        self.chains.mainnet.validate("chains.mainnet")?;
        self.chains.arbitrum.validate("chains.arbitrum")?;

        if !self.thresholds.critical_lvr_usd.is_finite() || self.thresholds.critical_lvr_usd < 0.0 {
            bail!("thresholds.critical_lvr_usd must be finite and non-negative");
        }
        if self.thresholds.stale_rpc_ms == 0 {
            bail!("thresholds.stale_rpc_ms must be greater than zero");
        }
        if !self.thresholds.vola_interval_sec.is_finite()
            || self.thresholds.vola_interval_sec <= 0.0
        {
            bail!("thresholds.vola_interval_sec must be finite and greater than zero");
        }
        Ok(())
    }
}

impl NetworkConfig {
    pub fn matches_binance_symbol(&self, symbol: &str) -> bool {
        symbol.strip_prefix(&self.binance_base_asset) == Some(self.binance_quote_asset.as_str())
    }
}

impl ChainConfig {
    pub fn fee_rate(&self) -> f64 {
        self.fee as f64 / 1_000_000.0
    }

    fn validate(&self, path: &str) -> anyhow::Result<()> {
        validate_url(
            &self.rpc_url,
            &["http", "https"],
            &format!("{path}.rpc_url"),
        )?;
        if self.chain_id == 0 {
            bail!("{path}.chain_id must be greater than zero");
        }
        for (field, address) in [
            ("pool_address", &self.pool_address),
            ("token0_address", &self.token0_address),
            ("token1_address", &self.token1_address),
        ] {
            if !is_nonzero_address(address) {
                bail!("{path}.{field} must be a non-zero 20-byte hex address");
            }
        }
        if self
            .token0_address
            .eq_ignore_ascii_case(&self.token1_address)
        {
            bail!("{path} token addresses must be distinct");
        }
        if self.base_token == self.quote_token {
            bail!("{path} base_token and quote_token must be different");
        }
        if self.fee == 0 || self.fee >= 1_000_000 {
            bail!("{path}.fee must be between 1 and 999999 millionths");
        }
        Ok(())
    }
}

fn validate_url(value: &str, schemes: &[&str], field: &str) -> anyhow::Result<()> {
    let url = reqwest::Url::parse(value).with_context(|| format!("{field} is not a valid URL"))?;
    if !schemes.contains(&url.scheme()) || url.host_str().is_none() {
        bail!(
            "{field} must use one of these schemes: {}",
            schemes.join(", ")
        );
    }
    Ok(())
}

fn is_nonzero_address(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        && value[2..].bytes().any(|byte| byte != b'0')
}

#[cfg(test)]
mod tests {
    use super::{Config, PoolToken};

    const VALID_CONFIG: &str = r#"
[network]
    binance_ws = "wss://example.com/ethusdc@ticker"
binance_base_asset = "ETH"
binance_quote_asset = "USDC"

[chains.mainnet]
rpc_url = "https://example.com/mainnet"
chain_id = 1
pool_address = "0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640"
token0_address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
token0_decimals = 6
token1_address = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
token1_decimals = 18
base_token = "token1"
quote_token = "token0"
fee = 500

[chains.arbitrum]
rpc_url = "https://example.com/arbitrum"
chain_id = 42161
pool_address = "0xc31e54c7a869b9fcbecc14363cf510d1c41fa443"
token0_address = "0x82af49447d8a07e3bd95bd0d56f35241523fbab1"
token0_decimals = 18
token1_address = "0xff970a61a04b1ca14834a43f5de4533ebddb5cc8"
token1_decimals = 6
base_token = "token0"
quote_token = "token1"
fee = 500

[thresholds]
critical_lvr_usd = 1.0
stale_rpc_ms = 300
vola_interval_sec = 2.0
"#;

    #[test]
    fn deserializes_per_chain_pool_metadata() {
        let config = Config::parse(VALID_CONFIG).expect("valid config");

        assert!(config.network.matches_binance_symbol("ETHUSDC"));
        assert!(!config.network.matches_binance_symbol("ETHUSDT"));
        assert_eq!(config.chains.mainnet.chain_id, 1);
        assert_eq!(config.chains.mainnet.token0_decimals, 6);
        assert_eq!(config.chains.mainnet.token1_decimals, 18);
        assert_eq!(config.chains.mainnet.base_token, PoolToken::Token1);
        assert_eq!(config.chains.mainnet.quote_token, PoolToken::Token0);
        assert_eq!(config.chains.arbitrum.chain_id, 42161);
        assert_eq!(config.chains.arbitrum.token0_decimals, 18);
        assert_eq!(config.chains.arbitrum.token1_decimals, 6);
        assert_eq!(config.chains.arbitrum.base_token, PoolToken::Token0);
        assert_eq!(config.chains.arbitrum.quote_token, PoolToken::Token1);
        assert!((config.chains.arbitrum.fee_rate() - 0.0005).abs() < f64::EPSILON);
    }

    #[test]
    fn rejects_invalid_binance_pair_configuration() {
        for content in [
            VALID_CONFIG.replace(
                "binance_base_asset = \"ETH\"",
                "binance_base_asset = \"eth\"",
            ),
            VALID_CONFIG.replace(
                "binance_quote_asset = \"USDC\"",
                "binance_quote_asset = \"ETH\"",
            ),
            VALID_CONFIG.replace(
                "binance_quote_asset = \"USDC\"",
                "binance_quote_asset = \"\"",
            ),
        ] {
            assert!(Config::parse(&content).is_err());
        }
    }

    #[test]
    fn rejects_malformed_or_ambiguous_pool_metadata() {
        let malformed = [
            VALID_CONFIG.replace("chain_id = 42161", "chain_id = 0"),
            VALID_CONFIG.replace(
                "pool_address = \"0xc31e54c7a869b9fcbecc14363cf510d1c41fa443\"",
                "pool_address = \"not-an-address\"",
            ),
            VALID_CONFIG.replacen("token1_decimals = 18", "token1_decimals = 256", 1),
            VALID_CONFIG.replacen("quote_token = \"token0\"", "quote_token = \"token1\"", 1),
            format!("{VALID_CONFIG}\n[pool]\naddress = \"shared\"\n"),
        ];

        for content in malformed {
            assert!(
                Config::parse(&content).is_err(),
                "accepted malformed config"
            );
        }
    }
}
