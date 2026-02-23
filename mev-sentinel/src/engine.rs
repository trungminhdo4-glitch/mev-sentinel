use std::collections::VecDeque;
use crate::network::BinanceTicker;

pub const RING_CAPACITY: usize = 50;
pub const BINANCE_TAKER_FEE: f64 = 0.001; // 0.10%
pub const GAS_ESTIMATE_SWAP: f64 = 150_000.0;
pub const LP_TVL_REFERENCE: f64 = 100_000.0;
pub const REFERENZ_AMOUNT_ETH: f64 = 1.0;

// Arbitrum L1 Calldata Heuristic
pub const ARB_L1_CALLDATA_GAS: f64 = 16_000.0; 

// ── Flow classification ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum FlowType {
    Retail,
    PotentialLvr,
    CriticalLvr,
    JitAttack,
}

impl std::fmt::Display for FlowType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlowType::Retail       => write!(f, "Retail"),
            FlowType::PotentialLvr => write!(f, "LVR Opp"),
            FlowType::CriticalLvr  => write!(f, "CRITICAL"),
            FlowType::JitAttack    => write!(f, "JIT ATTACK"),
        }
    }
}

// ── Per-chain live snapshot ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ChainSnapshot {
    pub time:         String,
    pub dex_price:    f64,
    pub spread_pct:   f64,
    pub gas_gwei:     f64,
    pub net_hedge_pnl: f64,
    pub flow:         FlowType,
}

// ── Pool statistics accumulator ───────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub struct PoolStats {
    pub total_lvr_lost:    f64,
    pub toxic_event_count: u64,
    pub iterations:        u64,
}

impl PoolStats {
    pub fn lp_estimated_loss(&self, cex_price: f64) -> f64 {
        if cex_price == 0.0 { return 0.0; }
        (self.total_lvr_lost / cex_price) * (LP_TVL_REFERENCE / 10.0)
    }

    pub fn lvr_resistance(&self, volatility: f64) -> f64 {
        if self.toxic_event_count == 0 { return f64::INFINITY; }
        volatility / self.toxic_event_count as f64
    }
}

// ── Quant engine ──────────────────────────────────────────────────────────

pub struct QuantEngine {
    pub prices: VecDeque<f64>,
    pub mainnet_last_dex:     f64,
    pub mainnet_stale:        u32,
    pub arbitrum_last_dex:    f64,
    pub arbitrum_stale:       u32,
    pub mainnet_prev_spread:  f64,
    pub arbitrum_prev_spread: f64,
    pub binance_latency_ms:   u64,
    pub rpc_latency_ms:       u64,
    pub vola_interval_sec:    f64,
}

impl QuantEngine {
    pub fn new(vola_interval_sec: f64) -> Self {
        Self {
            prices:               VecDeque::with_capacity(RING_CAPACITY),
            mainnet_last_dex:     0.0,
            mainnet_stale:        0,
            arbitrum_last_dex:    0.0,
            arbitrum_stale:       0,
            mainnet_prev_spread:  0.0,
            arbitrum_prev_spread: 0.0,
            binance_latency_ms:   0,
            rpc_latency_ms:       0,
            vola_interval_sec,
        }
    }

    pub fn get_latency_status(&self) -> (&'static str, ratatui::style::Color) {
        let total = self.binance_latency_ms + self.rpc_latency_ms;
        if total < 150 {
            ("🟢 HEALTHY SYNC", ratatui::style::Color::Green)
        } else if total <= 300 {
            ("🟡 HIGH INERTIA", ratatui::style::Color::Yellow)
        } else {
            ("🔴 DANGER: STALE DATA", ratatui::style::Color::Red)
        }
    }

    pub fn push_price(&mut self, ticker: BinanceTicker) {
        let mid = (ticker.best_bid + ticker.best_ask) / 2.0;
        if self.prices.len() == RING_CAPACITY {
            self.prices.pop_front();
        }
        self.prices.push_back(mid);
    }

    pub fn rolling_volatility(&self) -> f64 {
        if self.prices.len() < 2 { return 0.0; }
        let returns: Vec<f64> = self.prices.iter()
            .zip(self.prices.iter().skip(1))
            .map(|(a, b)| (b / a).ln())
            .collect();
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>()
            / (returns.len() - 1) as f64;
        
        let ann_factor = 31_536_000_f64 / self.vola_interval_sec;
        variance.sqrt() * ann_factor.sqrt()
    }

    pub fn classify(
        &mut self,
        chain:    &str,
        ticker:   BinanceTicker,
        dex:      f64,
        fee:      f64,
        gas_gwei: f64,
        l1_base_fee_gwei: f64, // For Arb heuristic
    ) -> (f64, f64, FlowType) {
        // Spread vs Best Bid/Ask
        let (relevant_cex, spread) = if dex < ticker.best_bid {
            (ticker.best_bid, (ticker.best_bid - dex) / ticker.best_bid)
        } else if dex > ticker.best_ask {
            (ticker.best_ask, (dex - ticker.best_ask) / ticker.best_ask)
        } else {
            ((ticker.best_bid + ticker.best_ask) / 2.0, 0.0)
        };

        let mut gas_cost = gas_gwei * 1e-9 * GAS_ESTIMATE_SWAP * relevant_cex;
        
        // Arbitrum L1 Heuristic
        if chain == "arbitrum" {
            let l1_cost = l1_base_fee_gwei * 1e-9 * ARB_L1_CALLDATA_GAS * relevant_cex;
            gas_cost += l1_cost;
        }

        let gross     = (relevant_cex - dex).abs() * REFERENZ_AMOUNT_ETH
                        - dex * REFERENZ_AMOUNT_ETH * fee;
        let net_pnl   = gross - gas_cost - (relevant_cex * REFERENZ_AMOUNT_ETH * BINANCE_TAKER_FEE);

        let (last_dex, stale, prev_spread) = if chain == "mainnet" {
            (&mut self.mainnet_last_dex, &mut self.mainnet_stale, &mut self.mainnet_prev_spread)
        } else {
            (&mut self.arbitrum_last_dex, &mut self.arbitrum_stale, &mut self.arbitrum_prev_spread)
        };

        if (*last_dex - dex).abs() < 0.01 {
            *stale += 1;
        } else {
            *stale = 0;
        }
        *last_dex = dex;

        let jit = *stale >= 3 && (spread - *prev_spread).abs() > fee * 3.0;
        *prev_spread = spread;

        let flow = if jit {
            FlowType::JitAttack
        } else if net_pnl > 0.0 {
            FlowType::CriticalLvr
        } else if spread > fee {
            FlowType::PotentialLvr
        } else {
            FlowType::Retail
        };

        (spread, net_pnl, flow)
    }
}
