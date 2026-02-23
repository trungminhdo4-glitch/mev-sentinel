🛡️ MEV-Sentinel: High-Frequency LVR & Adverse Selection TrackerA lightweight, production-grade quantitative research tool built in Rust to monitor, quantify, and analyze Loss-Versus-Rebalancing (LVR) and Adverse Selection across Ethereum Mainnet and Arbitrum in real-time.

🧠 The Problem: The LP's Invisible TaxIn the highly competitive DeFi landscape of 2026, passive Liquidity Providers (LPs) in Constant Product AMMs (like Uniswap V3) bleed capital to sophisticated arbitrageurs. This structural loss, known as LVR (Loss-Versus-Rebalancing), scales directly with market volatility and network latency.Furthermore, the dynamics shift drastically across layers: While high L1 gas fees act as a natural barrier against micro-arbitrage (protecting LPs), ultra-fast and cheap L2s like Arbitrum expose LPs to relentless toxic flow if their RPC latency exceeds the block time (0.25s).

⚡ The Solution mev-sentinel is a high-frequency tracking engine designed to empirically measure this phenomenon. Built under strict hardware constraints (< 6GB RAM environment, no heavy C-compilers), it proves that production-grade market microstructure analysis can be achieved with absolute resource efficiency. Performance Profile:Binary Size: ~3.5 MB (Release, stripped, LTO)Runtime Memory: < 40 MBCPU: Near-zero at rest (Event-driven rendering)

🚀 Core FeaturesCross-Layer Execution Tracking: Simultaneously monitors slot0 and Swap events on Ethereum Mainnet and Arbitrum.Microsecond Latency Gating ("Network Inertia"): Tracks WS event latency and RPC Round-Trip-Time (RTT). Stale data (Lag > 300ms) is automatically gated out to prevent false LVR calculations.Advanced Quant Engine: * Replaces naive mid-price calculations with Best Bid/Ask execution routing via Binance Websockets.Calculates rolling annualized volatility: $\sigma = \sigma_{interval} \cdot \sqrt{\frac{31,536,000}{interval\_seconds}}$Gas-adjusted net profit heuristics, including L1 calldata estimations for Arbitrum.Event-Driven Architecture: Utilizes a single connection-pooled reqwest::Client and fixed-capacity VecDeque ring buffers to eliminate allocation hotspots.

📊 Output: The Pitch ReportUpon graceful shutdown (Ctrl+C), the Sentinel generates a researcher-friendly CLI report and exports session data to report.csv for post-trade analysis.

=== RESEARCHER'S PITCH REPORT - LVR & MEV SENTINEL ===

Metric                           ETH Mainnet         Arbitrum
------------------------------------------------------------------------
Toxic Events                              12                2
Total LVR Lost ($, 1ETH)              4.5241           0.1120
Est. LP Loss ($100k TVL)             452.41            11.20
LVR-Resistance                       0.01041              inf

VERDICT: Arbitrum showed lower LVR losses this session.
(Lower Arbitrum gas = smaller toxic flow profits = less LVR)

🛠️ Build Instructions (Windows without MSVC)
This project was engineered to compile in restrictive environments without a full Visual Studio MSVC toolchain.

Ensure Rust is installed via rustup.

Install LLVM tools: rustup component add llvm-tools-preview

Install MSYS2 and ensure dlltool.exe is available.

Set the environment variable: $env:DLLTOOL = "C:\msys64\mingw64\bin\dlltool.exe"

Run: cargo build --release

🗺️ Roadmap to Production (Enterprise Architecture Vision)
While this MVP successfully quantifies LVR, scaling this into a Tier-1 Market Making execution engine requires transitioning to bare-metal infrastructure. Future architectural upgrades include:

Mempool Monitoring & Frontrunning Detection: Integrating a local reth node or Flashbots streaming to simulate pending transactions and detect sandwich attacks pre-inclusion.

Lock-Free Data Structures: Replacing Mutex-protected shared state with crossbeam lock-free queues for sub-millisecond IPC under massive load.

Dynamic Slippage Modeling: Moving beyond fixed swap amounts by incorporating full orderbook depth and on-chain tick liquidity to calculate precise price impact.

Risk Metrics Dashboard: Implementing real-time VaR (Value at Risk) and Sharpe Ratio estimations for LP positions.

Supervisor Task Resilience: Wrapping critical async tasks in a robust actor model to handle silent WS disconnects with exponential backoff and state recovery.
