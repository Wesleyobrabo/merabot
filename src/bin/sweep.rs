use anyhow::Result;
use binance_futures_bot::types::Candle;
use chrono::DateTime;
use serde_json::Value;

const TAKER_FEE_PCT: f64 = 0.0005; // 0.05% por ordem na Binance Futures
const EUR_USD_RATE: f64 = 1.09;   // 1 EUR = 1.09 USDT (9 EUR = ~9.81 USDT)

async fn fetch_klines(symbol: &str, interval: &str, target_count: usize) -> Result<Vec<Candle>> {
    let client = reqwest::Client::new();
    let mut all: Vec<Candle> = Vec::new();
    let mut end_time: Option<i64> = None;

    while all.len() < target_count {
        let mut url = format!(
            "https://fapi.binance.com/fapi/v1/klines?symbol={}&interval={}&limit=1000",
            symbol, interval
        );
        if let Some(et) = end_time {
            url.push_str(&format!("&endTime={}", et));
        }
        let resp: Vec<Vec<Value>> = client.get(&url).send().await?.json().await?;
        if resp.is_empty() {
            break;
        }
        end_time = Some(resp[0][0].as_i64().unwrap_or(0) - 1);
        let mut chunk = Vec::new();
        for it in resp {
            chunk.push(Candle {
                open_time: it[0].as_i64().unwrap_or(0),
                close_time: it[6].as_i64().unwrap_or(0),
                open: it[1].as_str().unwrap().parse().unwrap(),
                high: it[2].as_str().unwrap().parse().unwrap(),
                low: it[3].as_str().unwrap().parse().unwrap(),
                close: it[4].as_str().unwrap().parse().unwrap(),
                volume: it[5].as_str().unwrap().parse().unwrap(),
                is_closed: true,
            });
        }
        chunk.extend(all);
        all = chunk;
    }
    all.pop();
    Ok(all)
}

#[derive(Debug, Clone)]
struct TradeRecordSim {
    trade_num: usize,
    side: &'static str,
    open_time: i64,
    close_time: i64,
    entry_price: f64,
    exit_price: f64,
    qty_sol: f64,
    notional_usdt: f64,
    margin_used: f64,
    fees_paid: f64,
    net_pnl_usdt: f64,
    balance_after: f64,
    pnl_pct_on_margin: f64,
    reason: &'static str,
}

fn simulate_portfolio_monthly(
    candles: &[Candle],
    timeframe: &'static str,
    initial_eur: f64,
    leverage: u32,
    risk_per_trade_pct: f64, // ex: 4.0%
) -> (f64, Vec<TradeRecordSim>, f64) {
    let entry_len = 20;
    let exit_len = 10;
    let initial_balance_usdt = initial_eur * EUR_USD_RATE;

    let mut balance = initial_balance_usdt;
    let mut peak_balance = balance;
    let mut max_drawdown_pct = 0.0;
    let mut trade_history = Vec::new();
    let mut busy_until = 0usize;

    for i in entry_len..candles.len() {
        if i < busy_until {
            continue;
        }

        // Calcula Donchian 20 sobre as velas anteriores
        let mut high_20 = f64::MIN;
        let mut low_20 = f64::MAX;
        for k in (i - entry_len)..i {
            if candles[k].high > high_20 { high_20 = candles[k].high; }
            if candles[k].low < low_20 { low_20 = candles[k].low; }
        }

        let curr = &candles[i];
        let is_long = curr.close > high_20;
        let is_short = curr.close < low_20;

        if !is_long && !is_short {
            continue;
        }

        let entry_price = curr.close;
        let sl_distance_pct = 0.035; // 3.5% Stop Loss

        // Dimensionamento de lote:
        // Risco financeiro tolerado = balance * risk_per_trade_pct (ex: 4% de $9.80 = $0.392)
        // Tamanho da posição (notional) = risco / sl_distance_pct = $0.392 / 0.035 = $11.20 USDT
        let target_risk_usdt = balance * (risk_per_trade_pct / 100.0);
        let mut position_notional = target_risk_usdt / sl_distance_pct;

        // Limita ao poder de compra máximo permitido pela margem e alavancagem
        let max_notional_by_leverage = balance * (leverage as f64) * 0.90; // 90% da margem disponível
        if position_notional > max_notional_by_leverage {
            position_notional = max_notional_by_leverage;
        }

        // Garante o mínimo nocional da Binance Futures ($5.50 USDT para evitar rejeição da exchange)
        if position_notional < 5.50 {
            position_notional = 5.50;
        }

        // Margem necessária
        let margin_required = position_notional / (leverage as f64);
        if margin_required > balance * 0.95 {
            // Saldo insuficiente para margem
            continue;
        }

        // Quantidade de SOL arredondada para 0.01 (lote mínimo da Binance)
        let mut qty_sol = (position_notional / entry_price * 100.0).round() / 100.0;
        if qty_sol < 0.01 {
            qty_sol = 0.01;
        }
        let actual_notional = qty_sol * entry_price;

        let entry_fee = actual_notional * TAKER_FEE_PCT;

        let sl_price = if is_long {
            entry_price * (1.0 - sl_distance_pct)
        } else {
            entry_price * (1.0 + sl_distance_pct)
        };

        // Rastreia saída vela a vela
        for j in (i + 1)..candles.len() {
            let k = &candles[j];

            // Donchian 10 Exit
            let start_exit = j.saturating_sub(exit_len);
            let mut low_10 = f64::MAX;
            let mut high_10 = f64::MIN;
            for m in start_exit..j {
                if candles[m].low < low_10 { low_10 = candles[m].low; }
                if candles[m].high > high_10 { high_10 = candles[m].high; }
            }

            let mut closed = false;
            let mut exit_price = 0.0;
            let mut reason = "";

            if is_long {
                if k.low <= sl_price {
                    exit_price = sl_price;
                    reason = "Stop Loss 3.5%";
                    closed = true;
                } else if k.close < low_10 {
                    exit_price = k.close;
                    reason = "Saída Donchian 10 (Quebra de Fundo)";
                    closed = true;
                }
            } else {
                if k.high >= sl_price {
                    exit_price = sl_price;
                    reason = "Stop Loss 3.5%";
                    closed = true;
                } else if k.close > high_10 {
                    exit_price = k.close;
                    reason = "Saída Donchian 10 (Quebra de Topo)";
                    closed = true;
                }
            }

            if closed || j == candles.len() - 1 {
                if !closed {
                    exit_price = k.close;
                    reason = "Fechamento de Período";
                }

                let exit_fee = (qty_sol * exit_price) * TAKER_FEE_PCT;
                let gross_pnl = if is_long {
                    (exit_price - entry_price) * qty_sol
                } else {
                    (entry_price - exit_price) * qty_sol
                };

                let net_pnl = gross_pnl - entry_fee - exit_fee;
                balance += net_pnl;

                if balance > peak_balance {
                    peak_balance = balance;
                }
                let current_dd = (peak_balance - balance) / peak_balance * 100.0;
                if current_dd > max_drawdown_pct {
                    max_drawdown_pct = current_dd;
                }

                let pnl_on_margin = (net_pnl / margin_required) * 100.0;

                trade_history.push(TradeRecordSim {
                    trade_num: trade_history.len() + 1,
                    side: if is_long { "LONG" } else { "SHORT" },
                    open_time: curr.close_time,
                    close_time: k.close_time,
                    entry_price,
                    exit_price,
                    qty_sol,
                    notional_usdt: actual_notional,
                    margin_used: margin_required,
                    fees_paid: entry_fee + exit_fee,
                    net_pnl_usdt: net_pnl,
                    balance_after: balance,
                    pnl_pct_on_margin: pnl_on_margin,
                    reason,
                });

                busy_until = j + 1;
                break;
            }
        }
    }

    (balance, trade_history, max_drawdown_pct)
}

#[tokio::main]
async fn main() -> Result<()> {
    let initial_eur = 9.00;
    let initial_usdt = initial_eur * EUR_USD_RATE; // ~9.81 USDT
    let leverage = 5;
    let risk_pct = 4.0; // 4% de risco por trade

    println!("==========================================================================================");
    println!("       SIMULAÇÃO DE BANCA REAL (9 EUROS / 5X ALAVANCAGEM ISOLADA) — PERÍODO DE 1 MÊS       ");
    println!("==========================================================================================");
    println!("Parâmetros da Conta:");
    println!("  • Banca Inicial: {:.2} EUR (~{:.2} USDT)", initial_eur, initial_usdt);
    println!("  • Alavancagem: {}x ISOLADA (Poder de compra total: ~{:.2} USDT)", leverage, initial_usdt * leverage as f64);
    println!("  • Risco por Trade: {:.1}% da banca (com buffer nocional mínimo Binance de $5.50 USDT)", risk_pct);
    println!("  • Modelo Operacional: OPÇÃO A (Donchian 20/10 Turtle Clássico)");
    println!("  • Taxas Taker Binance Futures Descontadas: 0.05% na entrada e 0.05% na saída (0.10% total)\n");

    // Testa em 30m e 15m exatamente os últimos 30 dias (288 * 30 = 8640 velas 5m = 2880 velas 15m = 1440 velas 30m)
    for tf in ["30m", "15m"] {
        let count = if tf == "30m" { 1500 } else { 3000 };
        print!(">>> Baixando histórico de 30 dias para {}... ", tf);
        let candles = fetch_klines("SOLUSDT", tf, count).await?;
        println!("OK ({} candles baixados).", candles.len());

        let start_dt = DateTime::from_timestamp_millis(candles.first().unwrap().open_time).unwrap_or_default();
        let end_dt = DateTime::from_timestamp_millis(candles.last().unwrap().close_time).unwrap_or_default();
        let days = (candles.last().unwrap().close_time - candles.first().unwrap().open_time) as f64 / 86_400_000.0;

        let (final_balance_usdt, trades, max_dd) = simulate_portfolio_monthly(
            &candles, tf, initial_eur, leverage, risk_pct
        );

        let final_eur = final_balance_usdt / EUR_USD_RATE;
        let net_pnl_usdt = final_balance_usdt - initial_usdt;
        let net_pnl_eur = final_eur - initial_eur;
        let net_return_pct = (net_pnl_usdt / initial_usdt) * 100.0;

        let total_trades = trades.len();
        let wins = trades.iter().filter(|t| t.net_pnl_usdt > 0.0).count();
        let losses = trades.iter().filter(|t| t.net_pnl_usdt < 0.0).count();
        let win_rate = if total_trades > 0 { (wins as f64 / total_trades as f64) * 100.0 } else { 0.0 };

        let total_fees: f64 = trades.iter().map(|t| t.fees_paid).sum();
        let gross_wins: f64 = trades.iter().filter(|t| t.net_pnl_usdt > 0.0).map(|t| t.net_pnl_usdt).sum();
        let gross_losses: f64 = trades.iter().filter(|t| t.net_pnl_usdt < 0.0).map(|t| -t.net_pnl_usdt).sum();
        let pf = if gross_losses > 0.0 { gross_wins / gross_losses } else { 99.9 };

        println!("\n------------------------------------------------------------------------------------------");
        println!("  RESULTADO MENSAL NO TIMEFRAME {}: (Período: {:.1} dias — {} a {})", 
            tf, days, start_dt.format("%d/%m"), end_dt.format("%d/%m"));
        println!("------------------------------------------------------------------------------------------");
        println!("  • Banca Inicial:       {:.2} EUR  (${:.2} USDT)", initial_eur, initial_usdt);
        println!("  • Banca Final:         {:.2} EUR  (${:.2} USDT)", final_eur, final_balance_usdt);
        println!("  • Lucro Líquido:      {:+6.2} EUR  ({:+6.2} USDT)", net_pnl_eur, net_pnl_usdt);
        println!("  • Rentabilidade:      {:>+6.2}% no mês", net_return_pct);
        println!("  • Total de Operações:  {} (~{:.1} trades por dia)", total_trades, total_trades as f64 / days);
        println!("  • Vitórias / Derrotas: {} V / {} D (Win Rate: {:.1}%)", wins, losses, win_rate);
        println!("  • Profit Factor:       {:.2}", pf);
        println!("  • Rebaixamento Máx:    {:.2}% da banca (${:.2} USDT)", max_dd, initial_usdt * (max_dd / 100.0));
        println!("  • Total Taxas Pagas:   ${:.2} USDT (~{:.2} EUR)", total_fees, total_fees / EUR_USD_RATE);
        println!("------------------------------------------------------------------------------------------\n");

        // Mostra o extrato das últimas 10 operações do período
        println!("--- ÚLTIMAS 10 OPERAÇÕES DO MÊS ({}) ---", tf);
        println!("{:<3} | {:<5} | {:<12} | {:<8} | {:<8} | {:<7} | {:<8} | {:<8} | Motivo",
            "#", "Lado", "Data Fech.", "Entrada", "Saída", "Nocional", "PnL ($)", "Saldo ($)");
        println!("-------------------------------------------------------------------------------------------------------");
        let start_slice = trades.len().saturating_sub(10);
        for t in &trades[start_slice..] {
            let dt = DateTime::from_timestamp_millis(t.close_time).unwrap_or_default();
            println!("{:<3} | {:<5} | {:<12} | ${:<7.2} | ${:<7.2} | ${:<6.2} | {:>+7.2} | ${:<7.2} | {}",
                t.trade_num,
                t.side,
                dt.format("%d/%m %H:%M"),
                t.entry_price,
                t.exit_price,
                t.notional_usdt,
                t.net_pnl_usdt,
                t.balance_after,
                t.reason
            );
        }
        println!("-------------------------------------------------------------------------------------------------------\n");
    }

    Ok(())
}
