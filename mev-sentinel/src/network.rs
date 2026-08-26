use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_tungstenite::Connector;
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::protocol::WebSocketConfig};
use tracing::{error, info, warn};

use crate::config::{ChainConfig, NetworkConfig, PoolToken};

const SLOT0_SELECTOR: &str = "0x3850c7bd";
const TOKEN0_SELECTOR: &str = "0x0dfe1681";
const TOKEN1_SELECTOR: &str = "0xd21220a7";
const FEE_SELECTOR: &str = "0xddca3f43";
const DECIMALS_SELECTOR: &str = "0x313ce567";

// Binance's <symbol>@ticker stream updates every 1,000ms.
pub const BINANCE_TICKER_CADENCE: Duration = Duration::from_secs(1);
pub const CHAIN_POLL_CADENCE: Duration = Duration::from_secs(2);

// ── Binance WebSocket ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct BinanceTicker {
    pub sequence: u64,
    pub observed_at: Instant,
    pub received_at: Instant,
    pub best_bid: f64,
    pub best_ask: f64,
    pub latency_ms: u64,
}

#[derive(Deserialize)]
struct BinanceTickerPayload {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "E")]
    event_time_ms: u64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "b")]
    best_bid: String,
    #[serde(rename = "B")]
    best_bid_quantity: String,
    #[serde(rename = "a")]
    best_ask: String,
    #[serde(rename = "A")]
    best_ask_quantity: String,
}

fn parse_binance_ticker(
    payload: &str,
    config: &NetworkConfig,
    received_at_ms: u64,
    received_at: Instant,
    sequence: u64,
) -> Option<BinanceTicker> {
    let raw: BinanceTickerPayload = serde_json::from_str(payload).ok()?;
    if raw.event_type != "24hrTicker"
        || raw.event_time_ms == 0
        || !config.matches_binance_symbol(&raw.symbol)
    {
        return None;
    }

    let bid = raw.best_bid.parse::<f64>().ok()?;
    let ask = raw.best_ask.parse::<f64>().ok()?;
    let bid_quantity = raw.best_bid_quantity.parse::<f64>().ok()?;
    let ask_quantity = raw.best_ask_quantity.parse::<f64>().ok()?;
    if !bid.is_finite()
        || !ask.is_finite()
        || !bid_quantity.is_finite()
        || !ask_quantity.is_finite()
        || bid <= 0.0
        || ask < bid
        || bid_quantity < 0.0
        || ask_quantity < 0.0
    {
        return None;
    }

    let latency_ms = received_at_ms.saturating_sub(raw.event_time_ms);
    Some(BinanceTicker {
        sequence,
        observed_at: received_at
            .checked_sub(Duration::from_millis(latency_ms))
            .unwrap_or(received_at),
        received_at,
        best_bid: bid,
        best_ask: ask,
        latency_ms,
    })
}

pub async fn run_binance(config: NetworkConfig, sender: watch::Sender<Option<BinanceTicker>>) {
    let tls = native_tls::TlsConnector::new().expect("TLS init failed");
    let connector = Connector::NativeTls(tls);
    let mut backoff = 1;
    let mut sequence = 0;

    loop {
        let cfg: Option<WebSocketConfig> = None;
        match connect_async_tls_with_config(&config.binance_ws, cfg, false, Some(connector.clone()))
            .await
        {
            Ok((mut ws, _)) => {
                info!("Connected to Binance WS");
                backoff = 1;
                while let Some(Ok(msg)) = ws.next().await {
                    let Ok(text) = msg.into_text() else {
                        continue;
                    };
                    let received_at = Instant::now();
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let next_sequence = sequence + 1;
                    if let Some(ticker) =
                        parse_binance_ticker(&text, &config, now_ms, received_at, next_sequence)
                    {
                        sequence = next_sequence;
                        let _ = sender.send(Some(ticker));
                    } else {
                        sender.send_if_modified(|current| current.take().is_some());
                    }
                }
                sender.send_if_modified(|current| current.take().is_some());
                warn!("Binance WS connection lost, reconnecting...");
            }
            Err(e) => {
                error!(
                    "Binance WS connection failed: {}. Retrying in {}s...",
                    e, backoff
                );
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

// ── Raw JSON-RPC helpers ──────────────────────────────────────────────────

fn sqrt_price_x96_to_eth_usdc(hex: &str, config: &ChainConfig) -> Option<f64> {
    const SLOT0_HEX_LEN: usize = 7 * 64;

    let data = hex.strip_prefix("0x")?;
    if data.len() != SLOT0_HEX_LEN || !data.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }

    // sqrtPriceX96 is a uint160, left-padded to one 32-byte ABI word.
    if !data[..24].bytes().all(|byte| byte == b'0') {
        return None;
    }
    let sqrt = data[24..64].chars().try_fold(0.0, |value, digit| {
        digit.to_digit(16).map(|digit| value * 16.0 + digit as f64)
    })?;
    if sqrt == 0.0 {
        return None;
    }

    // slot0 is a raw token1/token0 ratio; adjust token units before orienting it.
    let raw_token1_per_token0 = (sqrt / 2f64.powi(96)).powi(2);
    let decimal_scale = 10f64.powi(config.token0_decimals as i32 - config.token1_decimals as i32);
    let token1_per_token0 = raw_token1_per_token0 * decimal_scale;
    let price = match (config.base_token, config.quote_token) {
        (PoolToken::Token0, PoolToken::Token1) => token1_per_token0,
        (PoolToken::Token1, PoolToken::Token0) => 1.0 / token1_per_token0,
        _ => return None,
    };
    (price.is_finite() && price > 0.0).then_some(price)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::time::Instant;

    use super::{
        gas_gwei_from_rpc, parse_binance_ticker, parse_rpc_response, sqrt_price_x96_to_eth_usdc,
    };
    use crate::config::{ChainConfig, NetworkConfig, PoolToken};

    const VALID_SLOT0_RESPONSE: &str = concat!(
        "0x0000000000000000000000000000000000004e8455ae2c26d4ba3f97c7d0a2c9",
        "0000000000000000000000000000000000000000000000000000000000030623",
        "00000000000000000000000000000000000000000000000000000000000002b6",
        "00000000000000000000000000000000000000000000000000000000000002d3",
        "00000000000000000000000000000000000000000000000000000000000002d3",
        "0000000000000000000000000000000000000000000000000000000000000044",
        "0000000000000000000000000000000000000000000000000000000000000001",
    );

    fn chain_config(
        token0_decimals: u8,
        token1_decimals: u8,
        base_token: PoolToken,
        quote_token: PoolToken,
    ) -> ChainConfig {
        ChainConfig {
            rpc_url: "https://example.com".to_string(),
            chain_id: 1,
            pool_address: "0x0000000000000000000000000000000000000001".to_string(),
            token0_address: "0x0000000000000000000000000000000000000002".to_string(),
            token0_decimals,
            token1_address: "0x0000000000000000000000000000000000000003".to_string(),
            token1_decimals,
            base_token,
            quote_token,
            fee: 500,
        }
    }

    fn slot0_response(sqrt_price_x96: u128) -> String {
        format!("0x{sqrt_price_x96:064x}{}", "0".repeat(6 * 64))
    }

    fn binance_config() -> NetworkConfig {
        NetworkConfig {
            binance_ws: "wss://example.com/ethusdc@ticker".to_string(),
            binance_base_asset: "ETH".to_string(),
            binance_quote_asset: "USDC".to_string(),
        }
    }

    #[test]
    fn parses_matching_ethusdc_book_ticker() {
        let ticker = parse_binance_ticker(
            r#"{"e":"24hrTicker","E":900,"s":"ETHUSDC","b":"2499.25","B":"1.5","a":"2500.75","A":"2.5"}"#,
            &binance_config(),
            1_000,
            Instant::now(),
            1,
        )
        .expect("matching ticker");

        assert_eq!(ticker.best_bid, 2_499.25);
        assert_eq!(ticker.best_ask, 2_500.75);
        assert_eq!(ticker.latency_ms, 100);
    }

    #[test]
    fn rejects_wrong_binance_pairs() {
        let config = binance_config();
        for symbol in ["ETHUSDT", "BTCUSDC"] {
            let payload = format!(
                r#"{{"e":"24hrTicker","E":900,"s":"{symbol}","b":"2499.25","B":"1.5","a":"2500.75","A":"2.5"}}"#
            );

            assert!(parse_binance_ticker(&payload, &config, 1_000, Instant::now(), 1).is_none());
        }
    }

    #[test]
    fn rejects_malformed_binance_tickers() {
        let config = binance_config();
        for payload in [
            "",
            "not json",
            r#"{"e":"24hrTicker","E":900,"b":"2499.25","B":"1.5","a":"2500.75","A":"2.5"}"#,
            r#"{"e":"24hrTicker","E":900,"s":"ETHUSDC","b":"bad","B":"1.5","a":"2500.75","A":"2.5"}"#,
            r#"{"e":"24hrTicker","E":900,"s":"ETHUSDC","b":"2501","B":"1.5","a":"2500","A":"2.5"}"#,
            r#"{"e":"24hrTicker","E":900,"s":"ETHUSDC","b":"2499.25","a":"2500.75"}"#,
            r#"{"e":"bookTicker","E":900,"s":"ETHUSDC","b":"2499.25","B":"1.5","a":"2500.75","A":"2.5"}"#,
            r#"{"e":"24hrTicker","s":"ETHUSDC","b":"2499.25","B":"1.5","a":"2500.75","A":"2.5"}"#,
        ] {
            assert!(parse_binance_ticker(payload, &config, 1_000, Instant::now(), 1).is_none());
        }
    }

    #[test]
    fn decodes_sqrt_price_from_first_slot0_word() {
        let config = chain_config(6, 18, PoolToken::Token1, PoolToken::Token0);
        let price = sqrt_price_x96_to_eth_usdc(VALID_SLOT0_RESPONSE, &config)
            .expect("valid slot0 response");

        assert!((price - 2_475.10383023333).abs() < 0.01);
    }

    #[test]
    fn derives_quote_per_base_for_token1_base_with_6_and_18_decimals() {
        let config = chain_config(6, 18, PoolToken::Token1, PoolToken::Token0);
        let payload = slot0_response((1u128 << 96) * 20_000);

        let price = sqrt_price_x96_to_eth_usdc(&payload, &config).expect("valid price");

        assert!((price - 2_500.0).abs() < 1e-9);
    }

    #[test]
    fn derives_quote_per_base_for_token0_base_with_18_and_6_decimals() {
        let config = chain_config(18, 6, PoolToken::Token0, PoolToken::Token1);
        let payload = slot0_response((1u128 << 96) / 20_000);

        let price = sqrt_price_x96_to_eth_usdc(&payload, &config).expect("valid price");

        assert!((price - 2_500.0).abs() < 1e-6);
    }

    #[test]
    fn rejects_invalid_slot0_payloads() {
        let truncated = &VALID_SLOT0_RESPONSE[..VALID_SLOT0_RESPONSE.len() - 64];
        let zero = format!("0x{}", "0".repeat(7 * 64));
        let mut non_hex = VALID_SLOT0_RESPONSE.to_owned();
        non_hex.replace_range(non_hex.len() - 1.., "z");
        let config = chain_config(6, 18, PoolToken::Token1, PoolToken::Token0);

        for payload in ["", "0x", truncated, zero.as_str(), non_hex.as_str()] {
            assert!(sqrt_price_x96_to_eth_usdc(payload, &config).is_none());
        }
    }

    #[test]
    fn rejects_missing_or_error_rpc_results() {
        assert_eq!(
            parse_rpc_response(r#"{"jsonrpc":"2.0","id":1,"result":"0x01"}"#).unwrap(),
            json!("0x01")
        );

        for payload in [
            "",
            "not json",
            r#"{"jsonrpc":"2.0","id":1}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":null}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"execution reverted"}}"#,
        ] {
            assert!(parse_rpc_response(payload).is_err());
        }
    }

    #[test]
    fn rejects_failed_or_malformed_gas_prices_instead_of_fabricating_one() {
        assert!(gas_gwei_from_rpc(Err(anyhow::anyhow!("RPC failed"))).is_err());
        for value in [
            json!(null),
            json!(""),
            json!("0x"),
            json!("20"),
            json!(-1),
            json!("0x0"),
            json!("0x00"),
            json!("0x01"),
            json!("0x04a817c800"),
        ] {
            assert!(gas_gwei_from_rpc(Ok((value, 1))).is_err());
        }

        assert_eq!(
            gas_gwei_from_rpc(Ok((json!("0x4a817c800"), 1))).expect("valid gas price"),
            20.0
        );
    }
}

fn parse_rpc_response(body: &str) -> anyhow::Result<Value> {
    let response: Value = serde_json::from_str(body).context("malformed JSON-RPC response")?;
    if let Some(error) = response.get("error") {
        bail!("JSON-RPC error: {error}");
    }

    response
        .get("result")
        .filter(|result| !result.is_null())
        .cloned()
        .context("JSON-RPC response has no result")
}

fn decode_rpc_quantity(value: &Value, field: &str) -> anyhow::Result<u64> {
    let encoded = value
        .as_str()
        .with_context(|| format!("{field} is not a hex quantity"))?;
    let data = encoded
        .strip_prefix("0x")
        .filter(|data| !data.is_empty() && data.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .with_context(|| format!("{field} is not a hex quantity"))?;
    u64::from_str_radix(data, 16).with_context(|| format!("{field} exceeds u64"))
}

fn gas_gwei_from_rpc(result: anyhow::Result<(Value, u64)>) -> anyhow::Result<f64> {
    let (value, _) = result?;
    let encoded = value
        .as_str()
        .context("eth_gasPrice is not a hex quantity")?;
    let data = encoded
        .strip_prefix("0x")
        .filter(|data| !data.is_empty() && data.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .context("eth_gasPrice is not a hex quantity")?;
    if data.len() > 1 && data.starts_with('0') {
        bail!("eth_gasPrice is not a canonical hex quantity");
    }

    let wei = decode_rpc_quantity(&value, "eth_gasPrice")?;
    if wei == 0 {
        bail!("eth_gasPrice must be greater than zero");
    }
    Ok(wei as f64 / 1e9)
}

fn decode_abi_u64(value: &Value, field: &str) -> anyhow::Result<u64> {
    let encoded = value
        .as_str()
        .with_context(|| format!("{field} is not an ABI word"))?;
    let data = encoded
        .strip_prefix("0x")
        .filter(|data| data.len() == 64 && data.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .with_context(|| format!("{field} is not one ABI word"))?;
    if !data[..48].bytes().all(|byte| byte == b'0') {
        bail!("{field} exceeds u64");
    }
    u64::from_str_radix(&data[48..], 16).with_context(|| format!("invalid {field}"))
}

fn decode_abi_address(value: &Value, field: &str) -> anyhow::Result<String> {
    let encoded = value
        .as_str()
        .with_context(|| format!("{field} is not an ABI address"))?;
    let data = encoded
        .strip_prefix("0x")
        .filter(|data| data.len() == 64 && data.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .with_context(|| format!("{field} is not one ABI word"))?;
    if !data[..24].bytes().all(|byte| byte == b'0') {
        bail!("{field} has non-zero ABI padding");
    }
    Ok(format!("0x{}", &data[24..]))
}

fn validate_contract_code(value: &Value) -> anyhow::Result<()> {
    let encoded = value.as_str().context("eth_getCode result is not hex")?;
    let data = encoded
        .strip_prefix("0x")
        .context("eth_getCode result has no 0x prefix")?;
    if data.is_empty()
        || data.len() % 2 != 0
        || !data.bytes().all(|byte| byte.is_ascii_hexdigit())
        || data.bytes().all(|byte| byte == b'0')
    {
        bail!("configured pool has no valid contract bytecode");
    }
    Ok(())
}

async fn rpc_call(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
) -> anyhow::Result<(Value, u64)> {
    let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
    let start = tokio::time::Instant::now();
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("RPC request failed ({method})"))?
        .error_for_status()
        .with_context(|| format!("RPC HTTP error ({method})"))?;
    let rtt = start.elapsed().as_millis() as u64;
    let body = resp
        .text()
        .await
        .with_context(|| format!("RPC response read failed ({method})"))?;
    Ok((parse_rpc_response(&body)?, rtt))
}

async fn verify_chain_config(client: &reqwest::Client, config: &ChainConfig) -> anyhow::Result<()> {
    let (chain_id, _) = rpc_call(client, &config.rpc_url, "eth_chainId", json!([])).await?;
    let actual_chain_id = decode_rpc_quantity(&chain_id, "eth_chainId")?;
    if actual_chain_id != config.chain_id {
        bail!(
            "RPC chain ID {actual_chain_id} does not match configured {}",
            config.chain_id
        );
    }

    let (code, _) = rpc_call(
        client,
        &config.rpc_url,
        "eth_getCode",
        json!([&config.pool_address, "latest"]),
    )
    .await?;
    validate_contract_code(&code)?;

    let (token0, _) = rpc_call(
        client,
        &config.rpc_url,
        "eth_call",
        json!([{"to": &config.pool_address, "data": TOKEN0_SELECTOR}, "latest"]),
    )
    .await?;
    let token0 = decode_abi_address(&token0, "token0()")?;
    if !token0.eq_ignore_ascii_case(&config.token0_address) {
        bail!(
            "pool token0 {token0} does not match configured {}",
            config.token0_address
        );
    }

    let (token1, _) = rpc_call(
        client,
        &config.rpc_url,
        "eth_call",
        json!([{"to": &config.pool_address, "data": TOKEN1_SELECTOR}, "latest"]),
    )
    .await?;
    let token1 = decode_abi_address(&token1, "token1()")?;
    if !token1.eq_ignore_ascii_case(&config.token1_address) {
        bail!(
            "pool token1 {token1} does not match configured {}",
            config.token1_address
        );
    }

    let (fee, _) = rpc_call(
        client,
        &config.rpc_url,
        "eth_call",
        json!([{"to": &config.pool_address, "data": FEE_SELECTOR}, "latest"]),
    )
    .await?;
    let fee = decode_abi_u64(&fee, "fee()")?;
    if fee != config.fee as u64 {
        bail!("pool fee {fee} does not match configured {}", config.fee);
    }

    for (token, expected_decimals, field) in [
        (
            &config.token0_address,
            config.token0_decimals,
            "token0 decimals",
        ),
        (
            &config.token1_address,
            config.token1_decimals,
            "token1 decimals",
        ),
    ] {
        let (decimals, _) = rpc_call(
            client,
            &config.rpc_url,
            "eth_call",
            json!([{"to": token, "data": DECIMALS_SELECTOR}, "latest"]),
        )
        .await?;
        let decimals = decode_abi_u64(&decimals, field)?;
        if decimals != expected_decimals as u64 {
            bail!("{field} {decimals} does not match configured {expected_decimals}");
        }
    }

    let (slot0, _) = rpc_call(
        client,
        &config.rpc_url,
        "eth_call",
        json!([{"to": &config.pool_address, "data": SLOT0_SELECTOR}, "latest"]),
    )
    .await?;
    let price = slot0
        .as_str()
        .and_then(|value| sqrt_price_x96_to_eth_usdc(value, config))
        .context("pool slot0() did not produce a valid configured price")?;

    info!(
        "Verified chain {} pool {} metadata; ETH/USDC price {:.2}",
        config.chain_id, config.pool_address, price
    );
    Ok(())
}

// ── Public types & poller ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct ChainData {
    pub sequence: u64,
    pub observed_at: Instant,
    pub received_at: Instant,
    pub dex_price: f64,
    pub gas_gwei: f64,
    pub rpc_latency_ms: u64,
}

pub async fn run_chain_poller(
    client: reqwest::Client,
    config: ChainConfig,
    sender: watch::Sender<Option<ChainData>>,
) {
    let mut sequence = 0;

    loop {
        match verify_chain_config(&client, &config).await {
            Ok(()) => break,
            Err(error) => {
                warn!(
                    "Chain {} pool metadata verification failed; refusing DEX observations: {error}",
                    config.chain_id
                );
                sender.send_if_modified(|current| current.take().is_some());
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    loop {
        let poll_started = Instant::now();
        let p_params = json!([{"to": &config.pool_address, "data": SLOT0_SELECTOR}, "latest"]);
        let g_params = json!([]);

        let (p_res, g_res) = tokio::join!(
            rpc_call(&client, &config.rpc_url, "eth_call", p_params),
            rpc_call(&client, &config.rpc_url, "eth_gasPrice", g_params)
        );

        let price = match p_res {
            Ok((value, _)) => match value
                .as_str()
                .and_then(|value| sqrt_price_x96_to_eth_usdc(value, &config))
            {
                Some(price) => price,
                None => {
                    warn!("Invalid eth_call slot0 result; skipping DEX observation");
                    sender.send_if_modified(|current| current.take().is_some());
                    tokio::time::sleep(CHAIN_POLL_CADENCE).await;
                    continue;
                }
            },
            Err(error) => {
                warn!("eth_call failed; skipping DEX observation: {error}");
                sender.send_if_modified(|current| current.take().is_some());
                tokio::time::sleep(CHAIN_POLL_CADENCE).await;
                continue;
            }
        };

        let gas = match gas_gwei_from_rpc(g_res) {
            Ok(gas) => gas,
            Err(error) => {
                warn!("eth_gasPrice failed or was malformed; skipping chain observation: {error}");
                sender.send_if_modified(|current| current.take().is_some());
                tokio::time::sleep(CHAIN_POLL_CADENCE).await;
                continue;
            }
        };

        sequence += 1;
        let received_at = Instant::now();
        let _ = sender.send(Some(ChainData {
            sequence,
            observed_at: poll_started,
            received_at,
            dex_price: price,
            gas_gwei: gas,
            rpc_latency_ms: received_at
                .duration_since(poll_started)
                .as_millis()
                .min(u64::MAX as u128) as u64,
        }));
        tokio::time::sleep(CHAIN_POLL_CADENCE).await;
    }
}
