use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::protocol::WebSocketConfig};
use tokio_tungstenite::Connector;
use tracing::{error, info, warn};

const SLOT0_SELECTOR: &str = "0x3850c7bd";

// ── Binance WebSocket ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct BinanceTicker {
    pub best_bid: f64,
    pub best_ask: f64,
    pub latency_ms: u64,
}

pub async fn run_binance(ws_url: String, sender: watch::Sender<Option<BinanceTicker>>) {
    let tls = native_tls::TlsConnector::new().expect("TLS init failed");
    let connector = Connector::NativeTls(tls);
    let mut backoff = 1;

    loop {
        let cfg: Option<WebSocketConfig> = None;
        match connect_async_tls_with_config(&ws_url, cfg, false, Some(connector.clone())).await {
            Ok((mut ws, _)) => {
                info!("Connected to Binance WS");
                backoff = 1;
                while let Some(Ok(msg)) = ws.next().await {
                    let text = msg.into_text().unwrap_or_default();
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        let bid = v["b"].as_str().and_then(|s| s.parse::<f64>().ok());
                        let ask = v["a"].as_str().and_then(|s| s.parse::<f64>().ok());
                        let event_time = v["E"].as_u64();
                        
                        if let (Some(b), Some(a), Some(e_ms)) = (bid, ask, event_time) {
                            if !b.is_finite() || !a.is_finite() || b <= 0.0 || a < b {
                                continue;
                            }
                            let now_ms = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64;
                            let latency = now_ms.saturating_sub(e_ms);
                            let _ = sender.send(Some(BinanceTicker {
                                best_bid: b,
                                best_ask: a,
                                latency_ms: latency,
                            }));
                        }
                    }
                }
                warn!("Binance WS connection lost, reconnecting...");
            }
            Err(e) => {
                error!("Binance WS connection failed: {}. Retrying in {}s...", e, backoff);
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

// ── Raw JSON-RPC helpers ──────────────────────────────────────────────────

fn sqrt_price_x96_to_eth_usdc(hex: &str) -> Option<f64> {
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

    let ratio = (sqrt / 2f64.powi(96)).powi(2);
    let price = 1e12 / ratio;
    (price.is_finite() && price > 0.0).then_some(price)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_rpc_response, sqrt_price_x96_to_eth_usdc};

    const VALID_SLOT0_RESPONSE: &str = concat!(
        "0x0000000000000000000000000000000000004e8455ae2c26d4ba3f97c7d0a2c9",
        "0000000000000000000000000000000000000000000000000000000000030623",
        "00000000000000000000000000000000000000000000000000000000000002b6",
        "00000000000000000000000000000000000000000000000000000000000002d3",
        "00000000000000000000000000000000000000000000000000000000000002d3",
        "0000000000000000000000000000000000000000000000000000000000000044",
        "0000000000000000000000000000000000000000000000000000000000000001",
    );

    #[test]
    fn decodes_sqrt_price_from_first_slot0_word() {
        let price =
            sqrt_price_x96_to_eth_usdc(VALID_SLOT0_RESPONSE).expect("valid slot0 response");

        assert!((price - 2_475.10383023333).abs() < 0.01);
    }

    #[test]
    fn rejects_invalid_slot0_payloads() {
        let truncated = &VALID_SLOT0_RESPONSE[..VALID_SLOT0_RESPONSE.len() - 64];
        let zero = format!("0x{}", "0".repeat(7 * 64));
        let mut non_hex = VALID_SLOT0_RESPONSE.to_owned();
        non_hex.replace_range(non_hex.len() - 1.., "z");

        for payload in ["", "0x", truncated, zero.as_str(), non_hex.as_str()] {
            assert!(sqrt_price_x96_to_eth_usdc(payload).is_none());
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

// ── Public types & poller ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ChainData {
    pub dex_price: f64,
    pub gas_gwei:  f64,
    pub rpc_latency_ms: u64,
}

pub async fn run_chain_poller(
    client: reqwest::Client,
    rpc_url: String,
    pool_addr: String,
    sender: watch::Sender<Option<ChainData>>,
) {
    loop {
        let p_params = json!([{"to": pool_addr, "data": SLOT0_SELECTOR}, "latest"]);
        let g_params = json!([]);

        let (p_res, g_res) = tokio::join!(
            rpc_call(&client, &rpc_url, "eth_call", p_params),
            rpc_call(&client, &rpc_url, "eth_gasPrice", g_params)
        );

        let (price, price_rtt) = match p_res {
            Ok((value, rtt)) => match value.as_str().and_then(sqrt_price_x96_to_eth_usdc) {
                Some(price) => (price, rtt),
                None => {
                    warn!("Invalid eth_call slot0 result; skipping DEX observation");
                    sender.send_if_modified(|current| current.take().is_some());
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            },
            Err(error) => {
                warn!("eth_call failed; skipping DEX observation: {error}");
                sender.send_if_modified(|current| current.take().is_some());
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let (gas, gas_rtt) = match g_res {
            Ok((value, rtt)) => (
                value
                    .as_str()
                    .and_then(|hex| u64::from_str_radix(hex.trim_start_matches("0x"), 16).ok())
                    .map(|wei| wei as f64 / 1e9)
                    .unwrap_or(20.0),
                rtt,
            ),
            Err(error) => {
                warn!("eth_gasPrice failed; using fallback gas price: {error}");
                (20.0, price_rtt)
            }
        };

        let _ = sender.send(Some(ChainData {
            dex_price: price,
            gas_gwei: gas,
            rpc_latency_ms: (price_rtt + gas_rtt) / 2,
        }));
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

