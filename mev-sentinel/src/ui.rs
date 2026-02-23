use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};

use crate::engine::{ChainSnapshot, FlowType, PoolStats};

pub struct UiState {
    pub cex_price: f64,
    pub volatility: f64,
    pub binance_latency_ms: u64,
    pub rpc_latency_ms:     u64,
    pub mainnet_history: Vec<ChainSnapshot>,
    pub arbitrum_history: Vec<ChainSnapshot>,
    pub mainnet_stats: PoolStats,
    pub arbitrum_stats: PoolStats,
}

impl UiState {
    pub fn new() -> Self {
        UiState {
            cex_price: 0.0,
            volatility: 0.0,
            binance_latency_ms: 0,
            rpc_latency_ms: 0,
            mainnet_history: Vec::with_capacity(50),
            arbitrum_history: Vec::with_capacity(50),
            mainnet_stats: PoolStats::default(),
            arbitrum_stats: PoolStats::default(),
        }
    }

    pub fn get_latency_status(&self) -> (&'static str, Color) {
        let total = self.binance_latency_ms + self.rpc_latency_ms;
        if total < 150 {
            ("🟢 HEALTHY SYNC", Color::Green)
        } else if total <= 300 {
            ("🟡 HIGH INERTIA", Color::Yellow)
        } else {
            ("🔴 DANGER: STALE DATA", Color::Red)
        }
    }

    pub fn push_mainnet(&mut self, snap: ChainSnapshot) {
        if self.mainnet_history.len() >= 50 { self.mainnet_history.remove(0); }
        self.mainnet_history.push(snap);
    }

    pub fn push_arbitrum(&mut self, snap: ChainSnapshot) {
        if self.arbitrum_history.len() >= 50 { self.arbitrum_history.remove(0); }
        self.arbitrum_history.push(snap);
    }
}

pub fn render(f: &mut Frame, state: &UiState) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),  // header (expanded for lag info)
            Constraint::Min(10),    // tables
            Constraint::Length(6),  // stats
        ])
        .split(f.area());

    render_header(f, root[0], state);
    render_tables(f, root[1], state);
    render_stats(f, root[2], state);
}

fn render_header(f: &mut Frame, area: Rect, state: &UiState) {
    let (status_text, status_color) = state.get_latency_status();
    
    let txt = vec![
        Line::from(vec![
            Span::styled("  MEV SENTINEL  ", Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled("Binance ETH/USDC: ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("${:.2}", state.cex_price), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled("σ (Ann.): ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:.1}%", state.volatility * 100.0), Style::default().fg(Color::Magenta)),
        ]),
        Line::from(vec![
            Span::styled("  Lag: ", Style::default().fg(Color::DarkGray)),
            Span::raw("Binance ["),
            Span::styled(format!("{}ms", state.binance_latency_ms), Style::default().fg(Color::White)),
            Span::raw("] │ RPC ["),
            Span::styled(format!("{}ms", state.rpc_latency_ms), Style::default().fg(Color::White)),
            Span::raw("] │ Status: "),
            Span::styled(status_text, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled("  Chains: ", Style::default().fg(Color::DarkGray)),
            Span::styled("ETH Mainnet (0.05%) ", Style::default().fg(Color::Cyan)),
            Span::raw("│ "),
            Span::styled("Arbitrum (0.05%)", Style::default().fg(Color::Green)),
            Span::raw("   Press Q or Ctrl+C to exit and see Pitch Report"),
        ]),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 🛡 LVR & Adverse Selection Tracker — Rust Edition ")
        .border_style(Style::default().fg(Color::Blue));
    let para = Paragraph::new(txt).block(block);
    f.render_widget(para, area);
}

fn render_tables(f: &mut Frame, area: Rect, state: &UiState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    render_chain_table(f, chunks[0], "Ethereum Mainnet", &state.mainnet_history, Color::Cyan);
    render_chain_table(f, chunks[1], "Arbitrum One", &state.arbitrum_history, Color::Green);
}

fn render_chain_table(f: &mut Frame, area: Rect, title: &str, history: &[ChainSnapshot], color: Color) {
    let header = Row::new(vec!["Time", "DEX Price", "Spread%", "Gas(Gwei)", "Hedge PnL", "Flow"])
        .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD))
        .bottom_margin(1);

    let rows: Vec<Row> = history.iter().rev().take(12).map(|s| {
        let flow_color = match s.flow {
            FlowType::Retail => Color::Green,
            FlowType::PotentialLvr => Color::Yellow,
            FlowType::CriticalLvr => Color::Red,
            FlowType::JitAttack => Color::LightRed,
        };
        let pnl_color = if s.net_hedge_pnl > 0.0 { Color::Red } else { Color::White };
        Row::new(vec![
            Cell::from(s.time.clone()),
            Cell::from(format!("{:.2}", s.dex_price)),
            Cell::from(format!("{:.4}%", s.spread_pct * 100.0)),
            Cell::from(format!("{:.1}", s.gas_gwei)),
            Cell::from(format!("${:+.3}", s.net_hedge_pnl)).style(Style::default().fg(pnl_color)),
            Cell::from(s.flow.to_string()).style(Style::default().fg(flow_color)),
        ])
    }).collect();

    let table = Table::new(rows, [
        Constraint::Length(8),
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(9),
        Constraint::Min(14),
    ])
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(format!(" {title} ")).border_style(Style::default().fg(color)));

    f.render_widget(table, area);
}

fn render_stats(f: &mut Frame, area: Rect, state: &UiState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    render_chain_stats(f, chunks[0], "Mainnet Backtest", &state.mainnet_stats, state.cex_price, state.volatility, Color::Cyan);
    render_chain_stats(f, chunks[1], "Arbitrum Backtest", &state.arbitrum_stats, state.cex_price, state.volatility, Color::Green);
}

fn render_chain_stats(f: &mut Frame, area: Rect, title: &str, stats: &PoolStats, cex: f64, vola: f64, color: Color) {
    let resistance = stats.lvr_resistance(vola);
    let resistance_txt = if resistance == f64::INFINITY {
        "∞ (no events)".to_string()
    } else {
        format!("{:.5}", resistance)
    };

    let txt = vec![
        Line::from(vec![
            Span::styled("Toxic Events: ", Style::default().fg(Color::DarkGray)),
            Span::styled(stats.toxic_event_count.to_string(), Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled("Total LVR Lost (1ETH): ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("${:.4}", stats.total_lvr_lost), Style::default().fg(Color::Red)),
        ]),
        Line::from(vec![
            Span::styled("Est. LP Loss ($100k TVL): ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("${:.2}", stats.lp_estimated_loss(cex)), Style::default().fg(Color::LightRed)),
        ]),
        Line::from(vec![
            Span::styled("LVR-Resistance: ", Style::default().fg(Color::DarkGray)),
            Span::styled(resistance_txt, Style::default().fg(Color::Green)),
        ]),
    ];
    let para = Paragraph::new(txt)
        .block(Block::default().borders(Borders::ALL).title(format!(" {title} ")).border_style(Style::default().fg(color)));
    f.render_widget(para, area);
}
