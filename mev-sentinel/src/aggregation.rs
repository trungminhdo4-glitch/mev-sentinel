use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use chrono::Local;
use tokio::{
    sync::watch,
    time::{sleep_until, Instant},
};

use crate::{
    engine::{ChainSnapshot, FlowType, QuantEngine},
    network::{BinanceTicker, ChainData, BINANCE_TICKER_CADENCE, CHAIN_POLL_CADENCE},
    ui::UiState,
};

#[derive(Clone, Copy)]
enum Chain {
    Mainnet,
    Arbitrum,
}

#[derive(Default)]
struct Update {
    visible_changed: bool,
    critical: bool,
}

impl Update {
    fn merge(&mut self, other: Self) {
        self.visible_changed |= other.visible_changed;
        self.critical |= other.critical;
    }
}

struct Aggregator {
    engine: QuantEngine,
    max_delivery_latency: Duration,
    observation_ttl: Duration,
    mainnet_fee: f64,
    arbitrum_fee: f64,
    last_ticker_sequence: Option<u64>,
    last_mainnet_sequence: Option<u64>,
    last_arbitrum_sequence: Option<u64>,
    latest_ticker: Option<BinanceTicker>,
    latest_mainnet: Option<ChainData>,
    latest_arbitrum: Option<ChainData>,
    pending_mainnet: Option<ChainData>,
    pending_arbitrum: Option<ChainData>,
    ticker_health_stale: bool,
    mainnet_health_stale: bool,
    arbitrum_health_stale: bool,
}

impl Aggregator {
    fn new(
        engine: QuantEngine,
        max_delivery_latency: Duration,
        mainnet_fee: f64,
        arbitrum_fee: f64,
    ) -> Self {
        Self {
            engine,
            max_delivery_latency,
            observation_ttl: observation_ttl(max_delivery_latency),
            mainnet_fee,
            arbitrum_fee,
            last_ticker_sequence: None,
            last_mainnet_sequence: None,
            last_arbitrum_sequence: None,
            latest_ticker: None,
            latest_mainnet: None,
            latest_arbitrum: None,
            pending_mainnet: None,
            pending_arbitrum: None,
            ticker_health_stale: true,
            mainnet_health_stale: true,
            arbitrum_health_stale: true,
        }
    }

    fn process_updates(
        &mut self,
        ticker: Option<Option<BinanceTicker>>,
        mainnet: Option<Option<ChainData>>,
        arbitrum: Option<Option<ChainData>>,
        ui: &mut UiState,
        now: Instant,
    ) -> Update {
        let mut update = self.expire_sources(ui, now);
        update.merge(self.refresh_health(now));

        // Clear invalidated sources before applying other observations from the same wake-up.
        if matches!(ticker, Some(None)) {
            update.merge(self.clear_ticker(ui));
        }
        if matches!(mainnet, Some(None)) {
            update.merge(self.clear_chain(Chain::Mainnet, ui));
        }
        if matches!(arbitrum, Some(None)) {
            update.merge(self.clear_chain(Chain::Arbitrum, ui));
        }

        if let Some(Some(ticker)) = ticker {
            update.merge(self.observe_ticker(ticker, ui, now));
        }
        if let Some(Some(data)) = mainnet {
            update.merge(self.observe_chain(Chain::Mainnet, data, ui, now));
        }
        if let Some(Some(data)) = arbitrum {
            update.merge(self.observe_chain(Chain::Arbitrum, data, ui, now));
        }

        update.merge(self.classify_pending(ui, now));
        update
    }

    fn observe_ticker(&mut self, ticker: BinanceTicker, ui: &mut UiState, now: Instant) -> Update {
        if self
            .last_ticker_sequence
            .is_some_and(|sequence| ticker.sequence <= sequence)
        {
            return Update::default();
        }
        self.last_ticker_sequence = Some(ticker.sequence);

        if Duration::from_millis(ticker.latency_ms) > self.max_delivery_latency
            || !self.is_ticker_fresh(ticker, now)
        {
            return self.clear_ticker(ui);
        }

        self.latest_ticker = Some(ticker);
        self.ticker_health_stale = !self.observation_is_healthy(ticker.observed_at, now);
        let ticker_age_ms = end_to_end_age(ticker.received_at, ticker.latency_ms, now)
            .as_millis()
            .min(u64::MAX as u128) as u64;
        self.engine.binance_latency_ms = ticker_age_ms;

        self.engine.push_price(ticker);
        let cex_price = (ticker.best_bid + ticker.best_ask) / 2.0;
        let volatility = self.engine.rolling_volatility();
        let mut visible_changed = replace_if_changed(&mut ui.binance_latency_ms, ticker_age_ms);
        visible_changed |= replace_if_changed(&mut ui.cex_price, cex_price);
        visible_changed |= replace_if_changed(&mut ui.last_cex_price, cex_price);
        visible_changed |= replace_if_changed(&mut ui.volatility, volatility);
        visible_changed |= replace_if_changed(&mut ui.cex_available, true);
        visible_changed |= replace_if_changed(&mut ui.cex_observed_at, Some(ticker.observed_at));

        Update {
            visible_changed,
            critical: false,
        }
    }

    fn observe_chain(
        &mut self,
        chain: Chain,
        data: ChainData,
        ui: &mut UiState,
        now: Instant,
    ) -> Update {
        let last_sequence = match chain {
            Chain::Mainnet => &mut self.last_mainnet_sequence,
            Chain::Arbitrum => &mut self.last_arbitrum_sequence,
        };
        if last_sequence.is_some_and(|sequence| data.sequence <= sequence) {
            return Update::default();
        }
        *last_sequence = Some(data.sequence);

        if Duration::from_millis(data.rpc_latency_ms) > self.max_delivery_latency
            || !self.is_chain_fresh(data, now)
        {
            return self.clear_chain(chain, ui);
        }

        match chain {
            Chain::Mainnet => {
                self.latest_mainnet = Some(data);
                self.pending_mainnet = Some(data);
                self.mainnet_health_stale = !self.observation_is_healthy(data.observed_at, now);
            }
            Chain::Arbitrum => {
                self.latest_arbitrum = Some(data);
                self.pending_arbitrum = Some(data);
                self.arbitrum_health_stale = !self.observation_is_healthy(data.observed_at, now);
            }
        }

        Update {
            visible_changed: self.update_rpc_latency(ui),
            critical: false,
        }
    }

    fn classify_pending(&mut self, ui: &mut UiState, now: Instant) -> Update {
        let Some(ticker) = self
            .latest_ticker
            .filter(|ticker| self.is_ticker_fresh(*ticker, now))
        else {
            return Update::default();
        };

        let mut update = Update::default();
        if let Some(data) = self.pending_mainnet.filter(|data| {
            self.is_chain_fresh(*data, now)
                && self.observations_compatible(ticker.observed_at, data.observed_at)
        }) {
            self.pending_mainnet = None;
            let result = self.engine.classify(
                "mainnet",
                ticker,
                data.dex_price,
                self.mainnet_fee,
                data.gas_gwei,
                0.0,
            );
            update.merge(record_result(ui, Chain::Mainnet, data, result));
        }

        let fresh_mainnet = self.latest_mainnet.filter(|data| {
            self.is_chain_fresh(*data, now)
                && self.observations_compatible(ticker.observed_at, data.observed_at)
        });
        let arbitrum = self.pending_arbitrum.filter(|data| {
            self.is_chain_fresh(*data, now)
                && self.observations_compatible(ticker.observed_at, data.observed_at)
        });
        if let (Some(data), Some(mainnet)) = (arbitrum, fresh_mainnet) {
            if self.observations_compatible(data.observed_at, mainnet.observed_at) {
                self.pending_arbitrum = None;
                let result = self.engine.classify(
                    "arbitrum",
                    ticker,
                    data.dex_price,
                    self.arbitrum_fee,
                    data.gas_gwei,
                    mainnet.gas_gwei,
                );
                update.merge(record_result(ui, Chain::Arbitrum, data, result));
            }
        }

        update
    }

    fn clear_ticker(&mut self, ui: &mut UiState) -> Update {
        let had_ticker = self.latest_ticker.take().is_some();
        self.ticker_health_stale = true;
        self.engine.binance_latency_ms = 0;
        let mut visible_changed = replace_if_changed(&mut ui.cex_price, 0.0);
        visible_changed |= replace_if_changed(&mut ui.binance_latency_ms, 0);
        visible_changed |= replace_if_changed(&mut ui.cex_available, false);
        visible_changed |= replace_if_changed(&mut ui.cex_observed_at, None);

        Update {
            visible_changed: had_ticker || visible_changed,
            critical: false,
        }
    }

    fn clear_chain(&mut self, chain: Chain, ui: &mut UiState) -> Update {
        let had_source = match chain {
            Chain::Mainnet => {
                let had_source = self.latest_mainnet.take().is_some();
                self.pending_mainnet = None;
                self.mainnet_health_stale = true;
                had_source
            }
            Chain::Arbitrum => {
                let had_source = self.latest_arbitrum.take().is_some();
                self.pending_arbitrum = None;
                self.arbitrum_health_stale = true;
                had_source
            }
        };

        let latency_changed = self.update_rpc_latency(ui);
        Update {
            visible_changed: had_source || latency_changed,
            critical: false,
        }
    }

    fn expire_sources(&mut self, ui: &mut UiState, now: Instant) -> Update {
        let mut update = Update::default();
        if self
            .latest_ticker
            .is_some_and(|ticker| !self.is_ticker_fresh(ticker, now))
        {
            update.merge(self.clear_ticker(ui));
        }
        if self
            .latest_mainnet
            .is_some_and(|data| !self.is_chain_fresh(data, now))
        {
            update.merge(self.clear_chain(Chain::Mainnet, ui));
        }
        if self
            .latest_arbitrum
            .is_some_and(|data| !self.is_chain_fresh(data, now))
        {
            update.merge(self.clear_chain(Chain::Arbitrum, ui));
        }
        update
    }

    fn refresh_health(&mut self, now: Instant) -> Update {
        let mut visible_changed = false;
        if !self.ticker_health_stale
            && self
                .latest_ticker
                .is_some_and(|ticker| !self.observation_is_healthy(ticker.observed_at, now))
        {
            self.ticker_health_stale = true;
            visible_changed = true;
        }
        if !self.mainnet_health_stale
            && self
                .latest_mainnet
                .is_some_and(|data| !self.observation_is_healthy(data.observed_at, now))
        {
            self.mainnet_health_stale = true;
            visible_changed = true;
        }
        if !self.arbitrum_health_stale
            && self
                .latest_arbitrum
                .is_some_and(|data| !self.observation_is_healthy(data.observed_at, now))
        {
            self.arbitrum_health_stale = true;
            visible_changed = true;
        }
        Update {
            visible_changed,
            critical: false,
        }
    }

    fn next_expiry(&self) -> Option<Instant> {
        let cache_expiry = self
            .latest_ticker
            .into_iter()
            .map(|ticker| (ticker.received_at, ticker.latency_ms))
            .chain(
                self.latest_mainnet
                    .into_iter()
                    .map(|data| (data.received_at, data.rpc_latency_ms)),
            )
            .chain(
                self.latest_arbitrum
                    .into_iter()
                    .map(|data| (data.received_at, data.rpc_latency_ms)),
            )
            .filter_map(|(received_at, delivery_latency_ms)| {
                received_at
                    .checked_add(
                        self.observation_ttl
                            .saturating_sub(Duration::from_millis(delivery_latency_ms)),
                    )?
                    .checked_add(Duration::from_nanos(1))
            });
        let health_expiry = [
            (!self.ticker_health_stale)
                .then_some(self.latest_ticker.map(|ticker| ticker.observed_at))
                .flatten(),
            (!self.mainnet_health_stale)
                .then_some(self.latest_mainnet.map(|data| data.observed_at))
                .flatten(),
            (!self.arbitrum_health_stale)
                .then_some(self.latest_arbitrum.map(|data| data.observed_at))
                .flatten(),
        ]
        .into_iter()
        .flatten()
        .filter_map(|observed_at| {
            observed_at
                .checked_add(self.max_delivery_latency)?
                .checked_add(Duration::from_nanos(1))
        });
        cache_expiry.chain(health_expiry).min()
    }

    fn update_rpc_latency(&mut self, ui: &mut UiState) -> bool {
        let mut visible_changed =
            replace_if_changed(&mut ui.mainnet_rpc_available, self.latest_mainnet.is_some());
        visible_changed |= replace_if_changed(
            &mut ui.arbitrum_rpc_available,
            self.latest_arbitrum.is_some(),
        );
        visible_changed |= replace_if_changed(
            &mut ui.mainnet_rpc_observed_at,
            self.latest_mainnet.map(|data| data.observed_at),
        );
        visible_changed |= replace_if_changed(
            &mut ui.arbitrum_rpc_observed_at,
            self.latest_arbitrum.map(|data| data.observed_at),
        );
        let rpc_latency_ms = self
            .latest_mainnet
            .into_iter()
            .chain(self.latest_arbitrum)
            .map(|data| data.rpc_latency_ms)
            .max()
            .unwrap_or_default();
        self.engine.rpc_latency_ms = rpc_latency_ms;
        visible_changed |= replace_if_changed(&mut ui.rpc_latency_ms, rpc_latency_ms);
        visible_changed
    }

    fn is_chain_fresh(&self, data: ChainData, now: Instant) -> bool {
        end_to_end_age(data.received_at, data.rpc_latency_ms, now) <= self.observation_ttl
    }

    fn is_ticker_fresh(&self, ticker: BinanceTicker, now: Instant) -> bool {
        end_to_end_age(ticker.received_at, ticker.latency_ms, now) <= self.observation_ttl
    }

    fn observations_compatible(&self, left: Instant, right: Instant) -> bool {
        instant_skew(left, right) <= self.max_delivery_latency
    }

    fn observation_is_healthy(&self, observed_at: Instant, now: Instant) -> bool {
        now.saturating_duration_since(observed_at) <= self.max_delivery_latency
    }
}

// Covers accepted delivery latency, a 2s chain poll, and the following 1s ticker.
fn observation_ttl(max_delivery_latency: Duration) -> Duration {
    max_delivery_latency
        .saturating_add(CHAIN_POLL_CADENCE)
        .saturating_add(BINANCE_TICKER_CADENCE)
}

fn end_to_end_age(received_at: Instant, delivery_latency_ms: u64, now: Instant) -> Duration {
    Duration::from_millis(delivery_latency_ms)
        .saturating_add(now.saturating_duration_since(received_at))
}

fn instant_skew(left: Instant, right: Instant) -> Duration {
    if left >= right {
        left.duration_since(right)
    } else {
        right.duration_since(left)
    }
}

fn replace_if_changed<T: PartialEq>(target: &mut T, value: T) -> bool {
    if *target == value {
        false
    } else {
        *target = value;
        true
    }
}

fn record_result(
    ui: &mut UiState,
    chain: Chain,
    data: ChainData,
    result: Option<(f64, f64, FlowType)>,
) -> Update {
    let Some((spread, pnl, flow)) = result else {
        return Update::default();
    };

    let critical = matches!(flow, FlowType::CriticalLvr);
    let toxic = matches!(flow, FlowType::CriticalLvr | FlowType::JitAttack);
    let snapshot = ChainSnapshot {
        time: Local::now().format("%H:%M:%S").to_string(),
        dex_price: data.dex_price,
        spread_pct: spread,
        gas_gwei: data.gas_gwei,
        net_hedge_pnl: pnl,
        flow,
    };

    match chain {
        Chain::Mainnet => {
            if pnl > 0.0 {
                ui.mainnet_stats.total_lvr_lost += pnl;
            }
            if toxic {
                ui.mainnet_stats.toxic_event_count += 1;
            }
            ui.mainnet_stats.iterations += 1;
            ui.push_mainnet(snapshot);
        }
        Chain::Arbitrum => {
            if pnl > 0.0 {
                ui.arbitrum_stats.total_lvr_lost += pnl;
            }
            if toxic {
                ui.arbitrum_stats.toxic_event_count += 1;
            }
            ui.arbitrum_stats.iterations += 1;
            ui.push_arbitrum(snapshot);
        }
    }

    Update {
        visible_changed: true,
        critical,
    }
}

enum Wake {
    Binance(Result<(), watch::error::RecvError>),
    Mainnet(Result<(), watch::error::RecvError>),
    Arbitrum(Result<(), watch::error::RecvError>),
    Expiry,
    Shutdown,
}

pub async fn run_aggregation(
    mut binance_rx: watch::Receiver<Option<BinanceTicker>>,
    mut mainnet_rx: watch::Receiver<Option<ChainData>>,
    mut arbitrum_rx: watch::Receiver<Option<ChainData>>,
    redraw_tx: watch::Sender<()>,
    ui_state: Arc<Mutex<UiState>>,
    engine: QuantEngine,
    stale_rpc_ms: u64,
    mainnet_fee: f64,
    arbitrum_fee: f64,
    mut shutdown_rx: watch::Receiver<bool>,
    cutoff: Arc<AtomicBool>,
) {
    let mut aggregator = Aggregator::new(
        engine,
        Duration::from_millis(stale_rpc_ms),
        mainnet_fee,
        arbitrum_fee,
    );
    let mut binance_open = true;
    let mut mainnet_open = true;
    let mut arbitrum_open = true;

    loop {
        if cutoff.load(Ordering::Acquire) || !binance_open && !mainnet_open && !arbitrum_open {
            return;
        }

        let next_expiry = aggregator.next_expiry();
        let expiry = next_expiry.unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
        let wake = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => Wake::Shutdown,
            result = binance_rx.changed(), if binance_open => Wake::Binance(result),
            result = mainnet_rx.changed(), if mainnet_open => Wake::Mainnet(result),
            result = arbitrum_rx.changed(), if arbitrum_open => Wake::Arbitrum(result),
            _ = sleep_until(expiry), if next_expiry.is_some() => Wake::Expiry,
        };

        let mut read_binance = false;
        let mut read_mainnet = false;
        let mut read_arbitrum = false;
        match wake {
            Wake::Binance(result) => {
                read_binance = true;
                binance_open = result.is_ok();
            }
            Wake::Mainnet(result) => {
                read_mainnet = true;
                mainnet_open = result.is_ok();
            }
            Wake::Arbitrum(result) => {
                read_arbitrum = true;
                arbitrum_open = result.is_ok();
            }
            Wake::Expiry => {}
            Wake::Shutdown => return,
        }

        if binance_open && !read_binance {
            match binance_rx.has_changed() {
                Ok(true) => read_binance = true,
                Ok(false) => {}
                Err(_) => {
                    read_binance = true;
                    binance_open = false;
                }
            }
        }
        if mainnet_open && !read_mainnet {
            match mainnet_rx.has_changed() {
                Ok(true) => read_mainnet = true,
                Ok(false) => {}
                Err(_) => {
                    read_mainnet = true;
                    mainnet_open = false;
                }
            }
        }
        if arbitrum_open && !read_arbitrum {
            match arbitrum_rx.has_changed() {
                Ok(true) => read_arbitrum = true,
                Ok(false) => {}
                Err(_) => {
                    read_arbitrum = true;
                    arbitrum_open = false;
                }
            }
        }

        let ticker = read_binance.then(|| *binance_rx.borrow_and_update());
        let mainnet = read_mainnet.then(|| *mainnet_rx.borrow_and_update());
        let arbitrum = read_arbitrum.then(|| *arbitrum_rx.borrow_and_update());

        let now = Instant::now();
        let mut ui = ui_state.lock().unwrap();
        if cutoff.load(Ordering::Acquire) {
            return;
        }
        let update = aggregator.process_updates(ticker, mainnet, arbitrum, &mut ui, now);
        if update.critical {
            ui.critical_bell_pending = true;
        }
        drop(ui);

        if update.visible_changed || update.critical {
            let _ = redraw_tx.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };

    use tokio::{sync::watch, time::Instant};

    use super::{run_aggregation, Aggregator};
    use crate::{
        engine::QuantEngine,
        network::{BinanceTicker, ChainData},
        ui::UiState,
    };

    fn aggregator(stale_ms: u64) -> Aggregator {
        Aggregator::new(
            QuantEngine::new(2.0, 10_000.0),
            Duration::from_millis(stale_ms),
            0.0005,
            0.0005,
        )
    }

    fn ticker(sequence: u64, received_at: Instant) -> BinanceTicker {
        BinanceTicker {
            sequence,
            observed_at: received_at,
            received_at,
            best_bid: 2_499.0,
            best_ask: 2_501.0,
            latency_ms: 1,
        }
    }

    fn chain(sequence: u64, received_at: Instant, rpc_latency_ms: u64) -> ChainData {
        ChainData {
            sequence,
            observed_at: received_at,
            received_at,
            dex_price: 2_450.0,
            gas_gwei: 20.0,
            rpc_latency_ms,
        }
    }

    #[tokio::test]
    async fn pairs_chain_arriving_after_a_fresh_ticker() {
        let now = Instant::now();
        let mut aggregator = aggregator(100);
        let mut ui = UiState::new(100);

        aggregator.process_updates(Some(Some(ticker(1, now))), None, None, &mut ui, now);
        aggregator.process_updates(
            None,
            Some(Some(chain(1, now + Duration::from_millis(1), 1))),
            None,
            &mut ui,
            now + Duration::from_millis(1),
        );

        assert_eq!(ui.mainnet_stats.iterations, 1);
        assert_eq!(ui.mainnet_history.len(), 1);
    }

    #[tokio::test]
    async fn pairs_ticker_arriving_after_a_chain() {
        let now = Instant::now();
        let mut aggregator = aggregator(100);
        let mut ui = UiState::new(100);

        aggregator.process_updates(None, Some(Some(chain(1, now, 1))), None, &mut ui, now);
        assert_eq!(ui.mainnet_stats.iterations, 0);

        aggregator.process_updates(
            Some(Some(ticker(1, now + Duration::from_millis(1)))),
            None,
            None,
            &mut ui,
            now + Duration::from_millis(1),
        );
        assert_eq!(ui.mainnet_stats.iterations, 1);
    }

    #[tokio::test]
    async fn does_not_account_duplicate_sequences() {
        let now = Instant::now();
        let mut aggregator = aggregator(100);
        let mut ui = UiState::new(100);
        let first_ticker = ticker(1, now);
        let mainnet = chain(1, now, 1);

        aggregator.process_updates(
            Some(Some(first_ticker)),
            Some(Some(mainnet)),
            None,
            &mut ui,
            now,
        );
        let duplicate = aggregator.process_updates(
            Some(Some(first_ticker)),
            Some(Some(mainnet)),
            None,
            &mut ui,
            now,
        );

        assert_eq!(aggregator.engine.prices.len(), 1);
        assert_eq!(ui.mainnet_stats.iterations, 1);
        assert_eq!(ui.mainnet_history.len(), 1);
        assert!(!duplicate.visible_changed);

        aggregator.process_updates(
            Some(Some(ticker(2, now + Duration::from_millis(1)))),
            None,
            None,
            &mut ui,
            now + Duration::from_millis(1),
        );
        assert_eq!(ui.mainnet_stats.iterations, 1);
        assert_eq!(ui.mainnet_history.len(), 1);
    }

    #[tokio::test]
    async fn expires_sources_by_end_to_end_age() {
        let now = Instant::now();
        let mut aggregator = aggregator(100);
        let mut ui = UiState::new(100);

        aggregator.process_updates(
            Some(Some(ticker(1, now))),
            Some(Some(chain(1, now, 20))),
            None,
            &mut ui,
            now,
        );
        aggregator.engine.mainnet_stale = 3;
        aggregator.engine.mainnet_prev_spread = 0.25;
        let prices = aggregator.engine.prices.clone();
        let last_dex = aggregator.engine.mainnet_last_dex;
        let stale = aggregator.engine.mainnet_stale;
        let previous_spread = aggregator.engine.mainnet_prev_spread;
        let stats = ui.mainnet_stats.clone();
        let history_len = ui.mainnet_history.len();
        let expired_at = now + Duration::from_millis(3_201);
        let update = aggregator.process_updates(None, None, None, &mut ui, expired_at);

        assert!(update.visible_changed);
        assert!(aggregator.latest_ticker.is_none());
        assert!(aggregator.latest_mainnet.is_none());
        assert_eq!(aggregator.engine.prices, prices);
        assert_eq!(aggregator.engine.mainnet_last_dex, last_dex);
        assert_eq!(aggregator.engine.mainnet_stale, stale);
        assert_eq!(aggregator.engine.mainnet_prev_spread, previous_spread);
        assert_eq!(ui.mainnet_stats.iterations, stats.iterations);
        assert_eq!(ui.mainnet_stats.total_lvr_lost, stats.total_lvr_lost);
        assert_eq!(ui.mainnet_history.len(), history_len);
        assert_eq!(ui.cex_price, 0.0);
        assert_eq!(ui.rpc_latency_ms, 0);
        assert!(!ui.cex_available);
        assert!(!ui.mainnet_rpc_available);
    }

    #[tokio::test]
    async fn pairs_observations_at_normal_producer_cadence() {
        let now = Instant::now();
        let mut aggregator = aggregator(100);
        let mut ui = UiState::new(100);

        aggregator.process_updates(Some(Some(ticker(1, now))), None, None, &mut ui, now);
        let chain_arrival = now + Duration::from_secs(2);
        aggregator.process_updates(
            Some(Some(ticker(2, chain_arrival))),
            Some(Some(chain(1, chain_arrival, 1))),
            None,
            &mut ui,
            chain_arrival,
        );

        assert_eq!(ui.mainnet_stats.iterations, 1);
    }

    #[tokio::test]
    async fn health_expires_before_matching_cache() {
        let now = Instant::now();
        let mut aggregator = aggregator(100);
        let mut ui = UiState::new(100);

        aggregator.process_updates(
            Some(Some(ticker(1, now))),
            Some(Some(chain(1, now, 1))),
            Some(Some(chain(1, now, 1))),
            &mut ui,
            now,
        );
        let stale_at = now + Duration::from_millis(101);
        let update = aggregator.process_updates(None, None, None, &mut ui, stale_at);

        assert!(update.visible_changed);
        assert!(aggregator.latest_ticker.is_some());
        assert!(aggregator.latest_mainnet.is_some());
        assert!(aggregator.latest_arbitrum.is_some());
        assert_eq!(ui.get_latency_status_at(stale_at).0, "STALE DATA");
    }

    #[tokio::test]
    async fn enforces_pair_observation_skew_inclusively() {
        let now = Instant::now();

        let mut boundary = aggregator(100);
        let mut boundary_ui = UiState::new(100);
        boundary.process_updates(
            Some(Some(ticker(1, now))),
            None,
            None,
            &mut boundary_ui,
            now,
        );
        let boundary_time = now + Duration::from_millis(100);
        boundary.process_updates(
            None,
            Some(Some(chain(1, boundary_time, 1))),
            None,
            &mut boundary_ui,
            boundary_time,
        );
        assert_eq!(boundary_ui.mainnet_stats.iterations, 1);

        let mut mismatched = aggregator(100);
        let mut mismatched_ui = UiState::new(100);
        mismatched.process_updates(
            Some(Some(ticker(1, now))),
            None,
            None,
            &mut mismatched_ui,
            now,
        );
        let mismatched_time = now + Duration::from_millis(101);
        mismatched.process_updates(
            None,
            Some(Some(chain(1, mismatched_time, 1))),
            None,
            &mut mismatched_ui,
            mismatched_time,
        );
        assert_eq!(mismatched_ui.mainnet_stats.iterations, 0);
        assert!(mismatched.pending_mainnet.is_some());
    }

    #[tokio::test]
    async fn rejects_a_slow_chain_without_averaging_with_a_fast_chain() {
        let now = Instant::now();
        let mut aggregator = aggregator(100);
        let mut ui = UiState::new(100);

        aggregator.process_updates(
            Some(Some(ticker(1, now))),
            Some(Some(chain(1, now, 10))),
            Some(Some(chain(1, now, 150))),
            &mut ui,
            now,
        );

        assert_eq!(ui.mainnet_stats.iterations, 1);
        assert_eq!(ui.arbitrum_stats.iterations, 0);
        assert_eq!(ui.rpc_latency_ms, 10);
    }

    #[tokio::test]
    async fn arbitrum_waits_for_fresh_mainnet_gas_data() {
        let now = Instant::now();
        let mut aggregator = aggregator(100);
        let mut ui = UiState::new(100);

        aggregator.process_updates(
            Some(Some(ticker(1, now))),
            None,
            Some(Some(chain(1, now, 1))),
            &mut ui,
            now,
        );
        assert_eq!(ui.arbitrum_stats.iterations, 0);

        aggregator.process_updates(
            None,
            Some(Some(chain(1, now + Duration::from_millis(1), 1))),
            None,
            &mut ui,
            now + Duration::from_millis(1),
        );
        assert_eq!(ui.arbitrum_stats.iterations, 1);
    }

    #[tokio::test]
    async fn none_clears_live_source_state_and_requests_redraw() {
        let now = Instant::now();
        let mut aggregator = aggregator(100);
        let mut ui = UiState::new(100);

        aggregator.process_updates(
            Some(Some(ticker(1, now))),
            Some(Some(chain(1, now, 1))),
            None,
            &mut ui,
            now,
        );
        aggregator.engine.mainnet_stale = 3;
        aggregator.engine.mainnet_prev_spread = 0.25;
        let update = aggregator.process_updates(
            Some(None),
            Some(None),
            None,
            &mut ui,
            now + Duration::from_millis(1),
        );

        assert!(update.visible_changed);
        assert!(aggregator.latest_ticker.is_none());
        assert!(aggregator.latest_mainnet.is_none());
        assert_eq!(aggregator.engine.prices.len(), 1);
        assert!(aggregator.engine.mainnet_last_dex.is_some());
        assert_eq!(aggregator.engine.mainnet_stale, 3);
        assert_eq!(aggregator.engine.mainnet_prev_spread, 0.25);
        assert_eq!(ui.cex_price, 0.0);
        assert_eq!(ui.last_cex_price, 2_500.0);
        assert_eq!(ui.binance_latency_ms, 0);
        assert_eq!(ui.rpc_latency_ms, 0);
        assert!(!ui.cex_available);
        assert!(!ui.mainnet_rpc_available);
        assert_eq!(ui.mainnet_history.len(), 1);

        let duplicate_clear =
            aggregator.process_updates(Some(None), Some(None), None, &mut ui, now);
        assert!(!duplicate_clear.visible_changed);
    }

    #[tokio::test]
    async fn consumes_a_final_value_when_sender_sends_then_closes() {
        let (binance_tx, binance_rx) = watch::channel(None);
        let (mainnet_tx, mainnet_rx) = watch::channel(None);
        let (arbitrum_tx, arbitrum_rx) = watch::channel(None);
        let (redraw_tx, _redraw_rx) = watch::channel(());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let ui = Arc::new(Mutex::new(UiState::new(10_000)));
        let cutoff = Arc::new(AtomicBool::new(false));
        let now = Instant::now();

        let task = tokio::spawn(run_aggregation(
            binance_rx,
            mainnet_rx,
            arbitrum_rx,
            redraw_tx,
            Arc::clone(&ui),
            QuantEngine::new(2.0, 1.0),
            10_000,
            0.0005,
            0.0005,
            shutdown_rx,
            cutoff,
        ));
        binance_tx
            .send(Some(ticker(1, now)))
            .expect("receiver remains open");
        drop(binance_tx);
        drop(mainnet_tx);
        drop(arbitrum_tx);

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("aggregation did not exit")
            .expect("aggregation task panicked");
        assert_eq!(ui.lock().unwrap().cex_price, 2_500.0);
    }

    #[tokio::test]
    async fn none_update_clears_state_and_sends_redraw() {
        let (binance_tx, binance_rx) = watch::channel(None);
        let (mainnet_tx, mainnet_rx) = watch::channel(None);
        let (arbitrum_tx, arbitrum_rx) = watch::channel(None);
        let (redraw_tx, mut redraw_rx) = watch::channel(());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let ui = Arc::new(Mutex::new(UiState::new(10_000)));
        let cutoff = Arc::new(AtomicBool::new(false));
        let now = Instant::now();

        let task = tokio::spawn(run_aggregation(
            binance_rx,
            mainnet_rx,
            arbitrum_rx,
            redraw_tx,
            Arc::clone(&ui),
            QuantEngine::new(2.0, 1.0),
            10_000,
            0.0005,
            0.0005,
            shutdown_rx,
            cutoff,
        ));
        binance_tx
            .send(Some(ticker(1, now)))
            .expect("receiver remains open");
        tokio::time::timeout(Duration::from_secs(1), redraw_rx.changed())
            .await
            .expect("ticker redraw timed out")
            .expect("redraw sender remains open");

        binance_tx.send(None).expect("receiver remains open");
        tokio::time::timeout(Duration::from_secs(1), redraw_rx.changed())
            .await
            .expect("clear redraw timed out")
            .expect("redraw sender remains open");
        assert_eq!(ui.lock().unwrap().cex_price, 0.0);

        drop(binance_tx);
        drop(mainnet_tx);
        drop(arbitrum_tx);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("aggregation did not exit")
            .expect("aggregation task panicked");
    }

    #[tokio::test]
    async fn shutdown_cutoff_excludes_unseen_and_post_exit_updates() {
        let (binance_tx, binance_rx) = watch::channel(None);
        let (_mainnet_tx, mainnet_rx) = watch::channel(None);
        let (_arbitrum_tx, arbitrum_rx) = watch::channel(None);
        let (redraw_tx, mut redraw_rx) = watch::channel(());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let ui = Arc::new(Mutex::new(UiState::new(100)));
        let cutoff = Arc::new(AtomicBool::new(false));
        let now = Instant::now();

        let task = tokio::spawn(run_aggregation(
            binance_rx,
            mainnet_rx,
            arbitrum_rx,
            redraw_tx,
            Arc::clone(&ui),
            QuantEngine::new(2.0, 1.0),
            100,
            0.0005,
            0.0005,
            shutdown_rx,
            Arc::clone(&cutoff),
        ));
        binance_tx
            .send(Some(ticker(1, now)))
            .expect("receiver remains open");
        tokio::time::timeout(Duration::from_secs(1), redraw_rx.changed())
            .await
            .expect("initial redraw timed out")
            .expect("redraw sender remains open");

        let state_guard = ui.lock().unwrap();
        let mut post_cutoff = ticker(2, now + Duration::from_millis(1));
        post_cutoff.best_bid = 2_599.0;
        post_cutoff.best_ask = 2_601.0;
        binance_tx
            .send(Some(post_cutoff))
            .expect("receiver remains open");
        cutoff.store(true, Ordering::Release);
        drop(state_guard);
        shutdown_tx.send(true).expect("receiver remains open");

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("aggregation did not exit")
            .expect("aggregation task panicked");
        let state = ui.lock().unwrap();
        assert_eq!(state.cex_price, 2_500.0);
        assert_eq!(state.last_cex_price, 2_500.0);
    }

    #[tokio::test]
    async fn exits_after_all_input_senders_close() {
        let (binance_tx, binance_rx) = watch::channel(None);
        let (mainnet_tx, mainnet_rx) = watch::channel(None);
        let (arbitrum_tx, arbitrum_rx) = watch::channel(None);
        let (redraw_tx, _redraw_rx) = watch::channel(());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let ui = Arc::new(Mutex::new(UiState::new(100)));
        let cutoff = Arc::new(AtomicBool::new(false));

        let task = tokio::spawn(run_aggregation(
            binance_rx,
            mainnet_rx,
            arbitrum_rx,
            redraw_tx,
            ui,
            QuantEngine::new(2.0, 1.0),
            100,
            0.0005,
            0.0005,
            shutdown_rx,
            cutoff,
        ));
        drop(binance_tx);
        drop(mainnet_tx);
        drop(arbitrum_tx);

        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("aggregation did not exit")
            .expect("aggregation task panicked");
    }
}
