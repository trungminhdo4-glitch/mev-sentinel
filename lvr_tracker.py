import asyncio
import collections
import math
import statistics
import time
from decimal import Decimal
from datetime import datetime

import ccxt.async_support as ccxt
from rich.console import Console
from rich.live import Live
from rich.table import Table
from rich.layout import Layout
from rich.panel import Panel
from web3 import AsyncHTTPProvider, AsyncWeb3

# Configuration
RPC_URL = "https://ethereum.publicnode.com"
BINANCE_SYMBOL = "ETH/USDC"
REFERENZ_AMOUNT_ETH = 1.0
LP_TVL_REFERENCE = 100000.0  # $100k for loss estimation
GAS_ESTIMATE_SWAP = 150000

POOLS = {
    "0.05% (5bps)": {"address": "0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640", "fee": 0.0005},
    "0.30% (30bps)": {"address": "0x8ad599c3a0ff1de082011efddc58f1908eb6e6d8", "fee": 0.0030},
    "1.00% (100bps)": {"address": "0x7bea39867e4169dbe237d55c8242a8f2fcdcc387", "fee": 0.0100},
}

POOL_ABI = [
    {
        "inputs": [],
        "name": "slot0",
        "outputs": [
            {"internalType": "uint160", "name": "sqrtPriceX96", "type": "uint160"},
            {"internalType": "int24", "name": "tick", "type": "int24"},
            {"internalType": "uint16", "name": "observationIndex", "type": "uint16"},
            {"internalType": "uint16", "name": "observationCardinality", "type": "uint16"},
            {"internalType": "uint16", "name": "observationCardinalityNext", "type": "uint16"},
            {"internalType": "uint8", "name": "feeProtocol", "type": "uint8"},
            {"internalType": "bool", "name": "unlocked", "type": "bool"},
        ],
        "stateMutability": "view",
        "type": "function",
    }
]

console = Console()

class PoolStats:
    def __init__(self, name, fee):
        self.name = name
        self.fee = fee
        self.total_lvr_lost_usd = 0.0
        self.toxic_event_count = 0
        self.last_price = None
        self.stale_count = 0
        self.start_time = time.time()
        
    @property
    def uptime(self):
        return time.time() - self.start_time

    def update(self, current_price, net_profit):
        if self.last_price is not None:
            if abs(current_price - self.last_price) < 0.01:
                self.stale_count += 1
            else:
                self.stale_count = 0
        
        self.last_price = current_price
        
        if net_profit > 0:
            self.total_lvr_lost_usd += net_profit
            self.toxic_event_count += 1

class LVRTracker:
    def __init__(self):
        self.binance = ccxt.binance()
        self.w3 = AsyncWeb3(AsyncHTTPProvider(RPC_URL))
        self.binance_prices = collections.deque(maxlen=50)
        self.pool_stats = {name: PoolStats(name, data['fee']) for name, data in POOLS.items()}
        self.pool_contracts = {
            name: self.w3.eth.contract(address=AsyncWeb3.to_checksum_address(data['address']), abi=POOL_ABI)
            for name, data in POOLS.items()
        }
        self.adverse_score = 0

    async def get_binance_price(self):
        try:
            ticker = await self.binance.fetch_ticker(BINANCE_SYMBOL)
            price = (ticker['bid'] + ticker['ask']) / 2
            self.binance_prices.append(price)
            return price
        except Exception:
            return None

    async def get_pool_price(self, name):
        try:
            slot0 = await self.pool_contracts[name].functions.slot0().call()
            sqrtPriceX96 = slot0[0]
            price_ratio = (Decimal(sqrtPriceX96) / Decimal(2**96)) ** 2
            # ETH/USDC 0.05% has token0=USDC, token1=WETH. Standardizing for others:
            # Most ETH/USDC pools on Mainnet follow token0=USDC, token1=WETH
            price_eth_usdc = Decimal(10**12) / price_ratio
            return float(price_eth_usdc)
        except Exception:
            return None

    async def get_gas_price_gwei(self):
        try:
            gas_price = await self.w3.eth.gas_price
            return float(self.w3.from_wei(gas_price, 'gwei'))
        except Exception:
            return 20.0

    def calculate_volatility(self):
        if len(self.binance_prices) < 2: return 0.0
        log_returns = [math.log(self.binance_prices[i]/self.binance_prices[i-1]) for i in range(1, len(self.binance_prices))]
        if len(log_returns) < 2: return 0.0
        return statistics.stdev(log_returns) * math.sqrt(15768000)

    def make_live_table(self, results):
        table = Table(title=f"Researcher's Suite - Live Multi-Pool Monitor ({datetime.now().strftime('%H:%M:%S')})", expand=True)
        table.add_column("Pool Tier", style="cyan")
        table.add_column("CEX Price", justify="right")
        table.add_column("DEX Price", justify="right")
        table.add_column("Spread %", justify="right")
        table.add_column("Net Profit (1E)", justify="right")
        table.add_column("σ (Ann.)", justify="right", style="magenta")
        table.add_column("Status", justify="center")

        vola = self.calculate_volatility()
        for name, r in results.items():
            if not r['dex_price']: continue
            
            lvr_type = "NORMAL"
            style = "green"
            if r['spread'] > r['fee']:
                lvr_type = "POTENTIAL"
                style = "yellow"
                if r['net_profit'] > 0:
                    lvr_type = "⚠️ CRITICAL"
                    style = "bold red"

            table.add_row(
                name,
                f"{r['cex_price']:.2f}",
                f"{r['dex_price']:.2f}",
                f"{r['spread']*100:.4f}%",
                f"${r['net_profit']:+.2f}",
                f"{vola*100:.1f}%",
                f"[{style}]{lvr_type}[/{style}]"
            )
        return table

    def make_stats_table(self):
        table = Table(title="Live Backtest Summary (Akkumulierte Metriken)", expand=True)
        table.add_column("Pool Tier")
        table.add_column("Toxic Events", justify="right")
        table.add_column("Total LVR Lost (1E)", justify="right", style="red")
        table.add_column("Est. LP Loss ($100k TVL)", justify="right", style="bold red")
        table.add_column("LVR-Resistance", justify="right", style="bold green")

        vola = self.calculate_volatility()
        for name, stats in self.pool_stats.items():
            # Theoretical LP Loss: (Lost per 1ETH / CEX_PRICE) * TVL
            # Simplification: Assume lost reflects direct shrinkage of position
            lp_loss = (stats.total_lvr_lost_usd / (self.binance_prices[-1] if self.binance_prices else 2500)) * (LP_TVL_REFERENCE / 10) # Scoped estimate
            
            resistance = "N/A"
            if vola > 0:
                # Resistance = 1 / (Events / Volatility) -> Higher is better
                resistance = f"{vola / (stats.toxic_event_count + 1):.4f}"

            table.add_row(
                name,
                str(stats.toxic_event_count),
                f"${stats.total_lvr_lost_usd:.2f}",
                f"${lp_loss:.2f}",
                resistance
            )
        return table

    async def run(self):
        layout = Layout()
        layout.split(Layout(name="upper"), Layout(name="lower"))
        
        try:
            with Live(layout, refresh_per_second=1) as live:
                while True:
                    b_price = await self.get_binance_price()
                    gas_gwei = await self.get_gas_price_gwei()
                    
                    # Parallel fetching
                    pool_names = list(POOLS.keys())
                    dex_prices = await asyncio.gather(*(self.get_pool_price(name) for name in pool_names))
                    
                    results = {}
                    for i, name in enumerate(pool_names):
                        u_price = dex_prices[i]
                        if b_price and u_price:
                            spread = abs(b_price - u_price) / b_price
                            gas_usd = gas_gwei * 1e-9 * GAS_ESTIMATE_SWAP * b_price
                            fee_tier = POOLS[name]['fee']
                            gross_profit = (abs(b_price - u_price) * REFERENZ_AMOUNT_ETH) - (u_price * REFERENZ_AMOUNT_ETH * fee_tier)
                            net_profit = gross_profit - gas_usd
                            
                            self.pool_stats[name].update(u_price, net_profit)
                            
                            results[name] = {
                                "cex_price": b_price, "dex_price": u_price, "spread": spread,
                                "net_profit": net_profit, "fee": fee_tier
                            }

                    layout["upper"].update(Panel(self.make_live_table(results)))
                    layout["lower"].update(Panel(self.make_stats_table()))
                    
                    await asyncio.sleep(2)
        finally:
            await self.binance.close()
            self.print_final_report()

    def print_final_report(self):
        console.print("\n[bold reverse blue] RESEARCHER'S SUITE - FINAL PITCH REPORT [/]\n")
        table = Table(show_header=True, header_style="bold magenta")
        table.add_column("Tier", style="cyan")
        table.add_column("Uptime", justify="right")
        table.add_column("Toxic Events", justify="right")
        table.add_column("Total LVR Lost", justify="right")
        table.add_column("LP Exposure Index", justify="right")

        best_tier = None
        min_loss = float('inf')

        for name, stats in self.pool_stats.items():
            if stats.total_lvr_lost_usd < min_loss:
                min_loss = stats.total_lvr_lost_usd
                best_tier = name
                
            table.add_row(
                name,
                f"{stats.uptime:.1f}s",
                str(stats.toxic_event_count),
                f"${stats.total_lvr_lost_usd:.4f}",
                f"{(stats.total_lvr_lost_usd / stats.uptime if stats.uptime > 0 else 0):.6f}"
            )
        
        console.print(table)
        console.print(f"\n[bold green]RESULT:[/] Der [bold]{best_tier}[/] Tier war in diesem Zeitraum am resistentesten gegen LVR.")
        console.print(f"[dim]Passive LPs in volatilen Phasen sollten diesen Tier bevorzugen.[/]\n")

if __name__ == "__main__":
    tracker = LVRTracker()
    try:
        asyncio.run(tracker.run())
    except KeyboardInterrupt:
        pass
