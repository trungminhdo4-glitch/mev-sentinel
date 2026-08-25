mod engine;
mod network;
mod ui;
mod config;

use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::Local;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::watch;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

use engine::{ChainSnapshot, FlowType, QuantEngine};
use network::{run_binance, run_chain_poller, ChainData, BinanceTicker};
use ui::UiState;
use config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load()?;

    // ── Logging Setup ─────────────────────────────────────────────────────
    let log_file = std::fs::File::create("sentinel.log")?;
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_writer(Mutex::new(log_file))
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Setting default subscriber failed");

    info!("Starting MEV Sentinel with hardening...");

    // ── Shared state ─────────────────────────────────────────────────────
    let ui_state = Arc::new(Mutex::new(UiState::new()));
    let engine   = Arc::new(Mutex::new(QuantEngine::new(cfg.thresholds.vola_interval_sec)));

    // ── Connection Pooling ────────────────────────────────────────────────
    let tls = native_tls::TlsConnector::new().expect("TLS init failed");
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .pool_max_idle_per_host(5)
        .build()
        .expect("HTTP client build failed");

    // ── Watch channels ────────────────────────────────────────────────────
    let (binance_tx, mut binance_rx) = watch::channel::<Option<BinanceTicker>>(None);
    let (mainnet_tx, mut mainnet_rx) = watch::channel::<Option<ChainData>>(None);
    let (arbitrum_tx, mut arbitrum_rx) = watch::channel::<Option<ChainData>>(None);

    // ── Spawn data tasks ──────────────────────────────────────────────────
    tokio::spawn(run_binance(cfg.network.clone(), binance_tx));
    tokio::spawn(run_chain_poller(client.clone(), cfg.chains.mainnet.clone(), mainnet_tx));
    tokio::spawn(run_chain_poller(client.clone(), cfg.chains.arbitrum.clone(), arbitrum_tx));

    // ── Aggregation task (Event-Driven) ───────────────────────────────────
    let (redraw_tx, mut redraw_rx) = watch::channel(());
    {
        let ui_state_agg = Arc::clone(&ui_state);
        let engine_agg   = Arc::clone(&engine);
        let stale_limit  = cfg.thresholds.stale_rpc_ms;
        
        tokio::spawn(async move {
            loop {
                // Wait for ANY of the data channels to update
                tokio::select! {
                    _ = binance_rx.changed() => {},
                    _ = mainnet_rx.changed() => {},
                    _ = arbitrum_rx.changed() => {},
                }

                let ticker = *binance_rx.borrow();
                let mn  = mainnet_rx.borrow().clone();
                let arb = arbitrum_rx.borrow().clone();
                // ... logic same ...
                let rpc_lag = match (&mn, &arb) {
                    (Some(mn), Some(arb)) => (mn.rpc_latency_ms + arb.rpc_latency_ms) / 2,
                    (Some(mn), None) => mn.rpc_latency_ms,
                    (None, Some(arb)) => arb.rpc_latency_ms,
                    (None, None) => 0,
                };

                let (mn_result, arb_result, vol) = {
                    let mut eng = engine_agg.lock().unwrap();
                    eng.rpc_latency_ms     = rpc_lag;

                    if let Some(ticker) = ticker {
                        eng.push_price(ticker);
                        eng.binance_latency_ms = ticker.latency_ms;

                        let vol = eng.rolling_volatility();
                        let mn_result = mn.as_ref().and_then(|data| {
                            eng.classify(
                                "mainnet",
                                ticker,
                                data.dex_price,
                                cfg.chains.mainnet.fee_rate(),
                                data.gas_gwei,
                                0.0,
                            )
                        });
                        let l1_base_fee_gwei = mn.as_ref().map_or(0.0, |data| data.gas_gwei);
                        let arb_result = arb.as_ref().and_then(|data| {
                            eng.classify(
                                "arbitrum",
                                ticker,
                                data.dex_price,
                                cfg.chains.arbitrum.fee_rate(),
                                data.gas_gwei,
                                l1_base_fee_gwei,
                            )
                        });
                        (mn_result, arb_result, vol)
                    } else {
                        (None, None, eng.rolling_volatility())
                    }
                };

                let now = Local::now().format("%H:%M:%S").to_string();
                {
                    let mut ui = ui_state_agg.lock().unwrap();
                    if let Some(ticker) = ticker {
                        ui.cex_price = (ticker.best_bid + ticker.best_ask) / 2.0;
                        ui.binance_latency_ms = ticker.latency_ms;
                    }
                    ui.volatility = vol;
                    ui.rpc_latency_ms     = rpc_lag;

                    if matches!(&mn_result, Some((_, _, FlowType::CriticalLvr)))
                        || matches!(&arb_result, Some((_, _, FlowType::CriticalLvr)))
                    {
                        let _ = io::stdout().write_all(b"\x07");
                        let _ = io::stdout().flush();
                    }

                    if rpc_lag <= stale_limit {
                        if let Some((_, pnl, flow)) = &mn_result {
                            if *pnl > 0.0 {
                                ui.mainnet_stats.total_lvr_lost += *pnl;
                            }
                            if matches!(flow, FlowType::CriticalLvr | FlowType::JitAttack) {
                                ui.mainnet_stats.toxic_event_count += 1;
                            }
                            ui.mainnet_stats.iterations += 1;
                        }

                        if let Some((_, pnl, flow)) = &arb_result {
                            if *pnl > 0.0 {
                                ui.arbitrum_stats.total_lvr_lost += *pnl;
                            }
                            if matches!(flow, FlowType::CriticalLvr | FlowType::JitAttack) {
                                ui.arbitrum_stats.toxic_event_count += 1;
                            }
                            ui.arbitrum_stats.iterations += 1;
                        }
                    }

                    if let (Some(data), Some((spread, pnl, flow))) = (mn, mn_result) {
                        ui.push_mainnet(ChainSnapshot {
                            time: now.clone(),
                            dex_price: data.dex_price,
                            spread_pct: spread,
                            gas_gwei: data.gas_gwei,
                            net_hedge_pnl: pnl,
                            flow,
                        });
                    }
                    if let (Some(data), Some((spread, pnl, flow))) = (arb, arb_result) {
                        ui.push_arbitrum(ChainSnapshot {
                            time: now,
                            dex_price: data.dex_price,
                            spread_pct: spread,
                            gas_gwei: data.gas_gwei,
                            net_hedge_pnl: pnl,
                            flow,
                        });
                    }
                }
                let _ = redraw_tx.send(()); // Trigger UI redraw
            }
        });
    }

    // ── TUI Setup ─────────────────────────────────────────────────────────
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend  = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let result = run_ui_loop(&mut term, Arc::clone(&ui_state), redraw_rx).await;

    // ── Cleanup ───────────────────────────────────────────────────────────
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;

    let final_ui = ui_state.lock().unwrap();
    print_final_report(&final_ui);
    save_report_csv(&final_ui)?;

    result
}

async fn run_ui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: Arc<Mutex<UiState>>,
    mut redraw_rx: watch::Receiver<()>,
) -> anyhow::Result<()> {
    loop {
        {
            let s = state.lock().unwrap();
            terminal.draw(|f| ui::render(f, &s))?;
        }
        tokio::select! {
            _ = redraw_rx.changed() => {},
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}, // Periodic fallback
            res = tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(100))) => {
                if let Ok(Ok(true)) = res {
                    if let Event::Key(key) = event::read()? {
                        match key.code {
                            KeyCode::Char('q') => return Ok(()),
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}

fn save_report_csv(state: &UiState) -> anyhow::Result<()> {
    let mut wtr = csv::Writer::from_path("report.csv")?;
    wtr.write_record(&["Chain", "Toxic_Events", "Total_LVR_Lost_USD", "LP_Est_Loss_100k"])?;
    
    wtr.write_record(&[
        "Mainnet",
        &state.mainnet_stats.toxic_event_count.to_string(),
        &format!("{:.4}", state.mainnet_stats.total_lvr_lost),
        &format!("{:.2}", state.mainnet_stats.lp_estimated_loss(state.cex_price)),
    ])?;

    wtr.write_record(&[
        "Arbitrum",
        &state.arbitrum_stats.toxic_event_count.to_string(),
        &format!("{:.4}", state.arbitrum_stats.total_lvr_lost),
        &format!("{:.2}", state.arbitrum_stats.lp_estimated_loss(state.cex_price)),
    ])?;

    wtr.flush()?;
    info!("Report saved to report.csv");
    Ok(())
}

fn print_final_report(state: &UiState) {
    let sep = "-".repeat(72);
    println!("\n\x1b[1;34m=== RESEARCHER'S PITCH REPORT - LVR & MEV SENTINEL ===\x1b[0m\n");
    println!("{sep}");
    println!("{:<30} {:>18} {:>18}", "Metric", "ETH Mainnet", "Arbitrum");
    println!("{sep}");

    let mn  = &state.mainnet_stats;
    let arb = &state.arbitrum_stats;
    let cex = state.cex_price;
    let vol = state.volatility;

    println!("{:<30} {:>18} {:>18}", "Toxic Events",
        mn.toxic_event_count, arb.toxic_event_count);
    println!("{:<30} {:>18.4} {:>18.4}", "Total LVR Lost ($, 1ETH)",
        mn.total_lvr_lost, arb.total_lvr_lost);
    println!("{:<30} {:>18.2} {:>18.2}", "Est. LP Loss ($100k TVL)",
        mn.lp_estimated_loss(cex), arb.lp_estimated_loss(cex));

    let mn_res  = mn.lvr_resistance(vol);
    let arb_res = arb.lvr_resistance(vol);
    let fmt_res = |r: f64| if r == f64::INFINITY { "inf".to_string() } else { format!("{:.5}", r) };
    println!("{:<30} {:>18} {:>18}", "LVR-Resistance", fmt_res(mn_res), fmt_res(arb_res));
    println!("{sep}");

    let verdict = if mn.total_lvr_lost <= arb.total_lvr_lost { "Ethereum Mainnet" } else { "Arbitrum" };
    println!("\n\x1b[1;32mVERDICT:\x1b[0m {verdict} showed lower LVR losses this session.");
    println!("\x1b[2m(Lower Arbitrum gas = smaller toxic flow profits = less LVR)\x1b[0m\n");
}
