use std::time::{SystemTime, UNIX_EPOCH, Duration};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::protocol::WebSocketConfig};
use tokio_tungstenite::Connector;
use tracing::{info, error, warn};

const SLOT0_SELECTOR: &str = "0x3850c7bd";

// ── Binance WebSocket ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default)]
pub struct BinanceTicker {
    pub best_bid: f64,
    pub best_ask: f64,
    pub latency_ms: u64,
}

pub async fn run_binance(ws_url: String, sender: watch::Sender<BinanceTicker>) {
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
                            let now_ms = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64;
                            let latency = now_ms.saturating_sub(e_ms);
                            let _ = sender.send(BinanceTicker {
                                best_bid: b,
                                best_ask: a,
                                latency_ms: latency,
                            });
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
    let s = hex.trim_start_matches("0x");
    if s.len() < 64 { return None; }
    let tail = &s[s.len()-32..];
    let sqrt = u128::from_str_radix(tail, 16).ok()?;
    if sqrt == 0 { return None; }
    let ratio = (sqrt as f64 / 2f64.powi(96)).powi(2);
    if ratio == 0.0 { return None; }
    Some(1e12 / ratio)
}

async fn rpc_call(client: &reqwest::Client, url: &str, method: &str, params: Value) -> (Option<Value>, u64) {
    let body = json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
    let start = tokio::time::Instant::now();
    let resp = match client.post(url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            error!("RPC error ({}): {}", method, e);
            return (None, start.elapsed().as_millis() as u64);
        }
    };
    let rtt = start.elapsed().as_millis() as u64;
    let v: Value = resp.json().await.unwrap_or_default();
    (v.get("result").cloned(), rtt)
}

// ── Public types & poller ─────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct ChainData {
    pub dex_price: f64,
    pub gas_gwei:  f64,
    pub rpc_latency_ms: u64,
}

pub async fn run_chain_poller(
    client: reqwest::Client,
    rpc_url: String,
    pool_addr: String,
    sender: watch::Sender<ChainData>
) {
    loop {
        let p_params = json!([{"to": pool_addr, "data": SLOT0_SELECTOR}, "latest"]);
        let g_params = json!([]);

        let (p_res, g_res) = tokio::join!(
            rpc_call(&client, &rpc_url, "eth_call", p_params),
            rpc_call(&client, &rpc_url, "eth_gasPrice", g_params)
        );

        let (p_val, rtt1) = p_res;
        let (g_val, rtt2) = g_res;
        let avg_rtt = (rtt1 + rtt2) / 2;

        let price = p_val.and_then(|v| v.as_str().and_then(|h| sqrt_price_x96_to_eth_usdc(h))).unwrap_or(0.0);
        let gas = g_val.and_then(|v| v.as_str().and_then(|h| u64::from_str_radix(h.trim_start_matches("0x"), 16).ok()))
            .map(|wei| wei as f64 / 1e9).unwrap_or(20.0);

        let _ = sender.send(ChainData { 
            dex_price: price, 
            gas_gwei: gas,
            rpc_latency_ms: avg_rtt,
        });
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

