mod aggregation;
mod config;
mod engine;
mod network;
mod ui;

use std::{
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinError, JoinSet};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

use aggregation::run_aggregation;
use config::Config;
use engine::QuantEngine;
use network::{run_binance, run_chain_poller, BinanceTicker, ChainData};
use ui::UiState;

#[derive(Debug, Clone, Copy)]
enum BackgroundTask {
    Binance,
    Mainnet,
    Arbitrum,
    Aggregation,
}

impl std::fmt::Display for BackgroundTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Binance => write!(f, "Binance producer"),
            Self::Mainnet => write!(f, "mainnet producer"),
            Self::Arbitrum => write!(f, "Arbitrum producer"),
            Self::Aggregation => write!(f, "aggregation"),
        }
    }
}

struct BackgroundExit {
    task: BackgroundTask,
    expected_shutdown: bool,
}

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
    let ui_state = Arc::new(Mutex::new(UiState::new(cfg.thresholds.stale_rpc_ms)));
    let engine = QuantEngine::new(
        cfg.thresholds.vola_interval_sec,
        cfg.thresholds.critical_lvr_usd,
    );

    // ── Connection Pooling ────────────────────────────────────────────────
    let tls = native_tls::TlsConnector::new().expect("TLS init failed");
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .pool_max_idle_per_host(5)
        .build()
        .expect("HTTP client build failed");

    // ── TUI Setup ─────────────────────────────────────────────────────────
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    // ── Watch channels ────────────────────────────────────────────────────
    let (binance_tx, binance_rx) = watch::channel::<Option<BinanceTicker>>(None);
    let (mainnet_tx, mainnet_rx) = watch::channel::<Option<ChainData>>(None);
    let (arbitrum_tx, arbitrum_rx) = watch::channel::<Option<ChainData>>(None);

    // ── Background tasks ──────────────────────────────────────────────────
    let (redraw_tx, redraw_rx) = watch::channel(());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let cutoff = Arc::new(AtomicBool::new(false));
    let mut background_tasks = JoinSet::new();
    let task_cutoff = Arc::clone(&cutoff);
    let binance_config = cfg.network.clone();
    background_tasks.spawn(async move {
        run_binance(binance_config, binance_tx).await;
        background_exit(BackgroundTask::Binance, &task_cutoff)
    });
    let task_cutoff = Arc::clone(&cutoff);
    let mainnet_config = cfg.chains.mainnet.clone();
    let mainnet_client = client.clone();
    background_tasks.spawn(async move {
        run_chain_poller(mainnet_client, mainnet_config, mainnet_tx).await;
        background_exit(BackgroundTask::Mainnet, &task_cutoff)
    });
    let task_cutoff = Arc::clone(&cutoff);
    let arbitrum_config = cfg.chains.arbitrum.clone();
    background_tasks.spawn(async move {
        run_chain_poller(client, arbitrum_config, arbitrum_tx).await;
        background_exit(BackgroundTask::Arbitrum, &task_cutoff)
    });
    let task_cutoff = Arc::clone(&cutoff);
    let stale_rpc_ms = cfg.thresholds.stale_rpc_ms;
    let mainnet_fee = cfg.chains.mainnet.fee_rate();
    let arbitrum_fee = cfg.chains.arbitrum.fee_rate();
    let aggregation_ui = Arc::clone(&ui_state);
    background_tasks.spawn(async move {
        run_aggregation(
            binance_rx,
            mainnet_rx,
            arbitrum_rx,
            redraw_tx,
            aggregation_ui,
            engine,
            stale_rpc_ms,
            mainnet_fee,
            arbitrum_fee,
            shutdown_rx,
            Arc::clone(&task_cutoff),
        )
        .await;
        background_exit(BackgroundTask::Aggregation, &task_cutoff)
    });

    let result = run_ui_loop(
        &mut term,
        Arc::clone(&ui_state),
        redraw_rx,
        Arc::clone(&cutoff),
        &mut background_tasks,
    )
    .await;

    // The cutoff is established while holding the same lock used by aggregation.
    // Anything completed before this lock is included; unseen or later updates are excluded.
    establish_shutdown_cutoff(&ui_state, &cutoff);
    let _ = shutdown_tx.send(true);
    background_tasks.abort_all();
    let mut shutdown_result = Ok(());
    while let Some(completion) = background_tasks.join_next().await {
        match completion {
            Ok(exit) if !exit.expected_shutdown && shutdown_result.is_ok() => {
                shutdown_result = Err(anyhow::anyhow!("{} task exited unexpectedly", exit.task));
            }
            Err(error) if !error.is_cancelled() && shutdown_result.is_ok() => {
                shutdown_result = Err(anyhow::anyhow!("background task failed: {error}"));
            }
            _ => {}
        }
    }

    // ── Cleanup ───────────────────────────────────────────────────────────
    let cleanup_result = (|| -> anyhow::Result<()> {
        disable_raw_mode()?;
        execute!(term.backend_mut(), LeaveAlternateScreen)?;
        term.show_cursor()?;
        Ok(())
    })();
    if let Err(cleanup) = cleanup_result {
        shutdown_result = match shutdown_result {
            Ok(()) => Err(cleanup),
            Err(shutdown) => Err(anyhow::anyhow!(
                "{shutdown}; terminal cleanup also failed: {cleanup}"
            )),
        };
    }

    let final_ui = ui_state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    finish_after_artifacts(result, shutdown_result, || {
        print_final_report(&final_ui);
        save_report_csv(&final_ui)
    })
}

async fn run_ui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: Arc<Mutex<UiState>>,
    mut redraw_rx: watch::Receiver<()>,
    cutoff: Arc<AtomicBool>,
    background_tasks: &mut JoinSet<BackgroundExit>,
) -> anyhow::Result<()> {
    let mut redraw_open = true;
    draw_ui(terminal, &state)?;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let event_task = tokio::task::spawn_blocking(move || loop {
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => {
                if event_tx.send(event::read()).is_err() {
                    return;
                }
            }
            Ok(false) if event_tx.is_closed() => return,
            Ok(false) => {}
            Err(error) => {
                let _ = event_tx.send(Err(error));
                return;
            }
        }
    });

    let result = loop {
        tokio::select! {
            result = redraw_rx.changed(), if redraw_open => {
                match result {
                    Ok(()) => {
                        if let Err(error) = draw_ui(terminal, &state) {
                            break Err(error);
                        }
                    }
                    Err(_) => redraw_open = false,
                }
            },
            terminal_event = event_rx.recv() => {
                match terminal_event {
                    Some(Ok(Event::Key(key))) => {
                        match key.code {
                            KeyCode::Char('q') => break Ok(()),
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break Ok(()),
                            _ => {}
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        if let Err(error) = draw_ui(terminal, &state) {
                            break Err(error);
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => break Err(error.into()),
                    None => break Err(anyhow::anyhow!("terminal event task stopped")),
                }
            }
            completion = background_tasks.join_next() => {
                break Err(background_completion_error(completion));
            }
        }
    };

    establish_shutdown_cutoff(&state, &cutoff);
    drop(event_rx);
    if let Err(error) = event_task.await {
        if result.is_ok() {
            return Err(anyhow::anyhow!("terminal event task failed: {error}"));
        }
    }
    result
}

fn background_exit(task: BackgroundTask, cutoff: &AtomicBool) -> BackgroundExit {
    BackgroundExit {
        task,
        expected_shutdown: cutoff.load(Ordering::Acquire),
    }
}

fn background_completion_error(
    completion: Option<Result<BackgroundExit, JoinError>>,
) -> anyhow::Error {
    match completion {
        Some(Ok(exit)) => anyhow::anyhow!("{} task exited unexpectedly", exit.task),
        Some(Err(error)) => anyhow::anyhow!("background task failed: {error}"),
        None => anyhow::anyhow!("all background tasks stopped unexpectedly"),
    }
}

fn finish_after_artifacts(
    session_result: anyhow::Result<()>,
    shutdown_result: anyhow::Result<()>,
    generate_artifacts: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let runtime_error = match (session_result, shutdown_result) {
        (Err(session), Err(shutdown)) => Some(anyhow::anyhow!(
            "{session}; shutdown also failed: {shutdown}"
        )),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Some(error),
        (Ok(()), Ok(())) => None,
    };
    let artifact_result = generate_artifacts();

    match (runtime_error, artifact_result) {
        (Some(runtime), Err(artifact)) => Err(anyhow::anyhow!(
            "{runtime}; artifact generation also failed: {artifact}"
        )),
        (Some(runtime), Ok(())) => Err(runtime),
        (None, artifact_result) => artifact_result,
    }
}

fn establish_shutdown_cutoff(state: &Arc<Mutex<UiState>>, cutoff: &AtomicBool) {
    let _state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cutoff.store(true, Ordering::Release);
}

fn draw_ui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &Arc<Mutex<UiState>>,
) -> anyhow::Result<()> {
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    terminal.draw(|frame| ui::render(frame, &state))?;
    if state.critical_bell_pending {
        terminal.backend_mut().write_all(b"\x07")?;
        terminal.backend_mut().flush()?;
        state.critical_bell_pending = false;
    }
    Ok(())
}

fn save_report_csv(state: &UiState) -> anyhow::Result<()> {
    let mut wtr = csv::Writer::from_path("report.csv")?;
    wtr.write_record(&[
        "Chain",
        "Toxic_Events",
        "Total_LVR_Lost_USD",
        "LP_Est_Loss_100k",
    ])?;

    wtr.write_record(&[
        "Mainnet",
        &state.mainnet_stats.toxic_event_count.to_string(),
        &format!("{:.4}", state.mainnet_stats.total_lvr_lost),
        &format!(
            "{:.2}",
            state
                .mainnet_stats
                .lp_estimated_loss(state.final_cex_price())
        ),
    ])?;

    wtr.write_record(&[
        "Arbitrum",
        &state.arbitrum_stats.toxic_event_count.to_string(),
        &format!("{:.4}", state.arbitrum_stats.total_lvr_lost),
        &format!(
            "{:.2}",
            state
                .arbitrum_stats
                .lp_estimated_loss(state.final_cex_price())
        ),
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

    let mn = &state.mainnet_stats;
    let arb = &state.arbitrum_stats;
    let cex = state.final_cex_price();
    let vol = state.volatility;

    println!(
        "{:<30} {:>18} {:>18}",
        "Toxic Events", mn.toxic_event_count, arb.toxic_event_count
    );
    println!(
        "{:<30} {:>18.4} {:>18.4}",
        "Total LVR Lost ($, 1ETH)", mn.total_lvr_lost, arb.total_lvr_lost
    );
    println!(
        "{:<30} {:>18.2} {:>18.2}",
        "Est. LP Loss ($100k TVL)",
        mn.lp_estimated_loss(cex),
        arb.lp_estimated_loss(cex)
    );

    let mn_res = mn.lvr_resistance(vol);
    let arb_res = arb.lvr_resistance(vol);
    let fmt_res = |r: f64| {
        if r == f64::INFINITY {
            "inf".to_string()
        } else {
            format!("{:.5}", r)
        }
    };
    println!(
        "{:<30} {:>18} {:>18}",
        "LVR-Resistance",
        fmt_res(mn_res),
        fmt_res(arb_res)
    );
    println!("{sep}");

    let verdict = if mn.total_lvr_lost <= arb.total_lvr_lost {
        "Ethereum Mainnet"
    } else {
        "Arbitrum"
    };
    println!("\n\x1b[1;32mVERDICT:\x1b[0m {verdict} showed lower LVR losses this session.");
    println!("\x1b[2m(Lower Arbitrum gas = smaller toxic flow profits = less LVR)\x1b[0m\n");
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{
        background_completion_error, finish_after_artifacts, BackgroundExit, BackgroundTask,
    };

    #[test]
    fn unexpected_background_completion_is_an_error() {
        let error = background_completion_error(Some(Ok(BackgroundExit {
            task: BackgroundTask::Aggregation,
            expected_shutdown: false,
        })));

        assert!(error
            .to_string()
            .contains("aggregation task exited unexpectedly"));
    }

    #[test]
    fn artifacts_are_generated_before_background_error_is_returned() {
        let generated = Cell::new(false);
        let result = finish_after_artifacts(
            Err(anyhow::anyhow!("background task failed")),
            Ok(()),
            || {
                generated.set(true);
                Ok(())
            },
        );

        assert!(generated.get());
        assert!(result
            .expect_err("background error must be returned")
            .to_string()
            .contains("background task failed"));
    }
}
