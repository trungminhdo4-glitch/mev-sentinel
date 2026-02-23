use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub network: NetworkConfig,
    pub pool: PoolConfig,
    pub thresholds: ThresholdConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NetworkConfig {
    pub binance_ws: String,
    pub mainnet_rpc: String,
    pub arbitrum_rpc: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PoolConfig {
    pub address: String,
    pub fee_tier: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ThresholdConfig {
    pub critical_lvr_usd: f64,
    pub stale_rpc_ms: u64,
    pub vola_interval_sec: f64,
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let content = fs::read_to_string("config.toml")?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}
