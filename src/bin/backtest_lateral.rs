#![allow(dead_code, unused_variables, unused_imports, unused_assignments)]
use anyhow::Result;
use binance_futures_bot::indicators::{calculate_atr_slice, calculate_linreg, ChoppinessIndex, Rsi};
use binance_futures_bot::types::Candle;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::env;

#[derive(Debug, Clone)]
struct Trade {
    side: &'static str,
    entry_price: f64,
    exit_price: f64,
    pnl_pct: f64,
    pnl_usdt: f64,
    is_win: bool,
    entry_time: i64,
    exit_time: i64,
    reason: String,
}

#[derive(Debug, Clone)]
struct SimSummary {
    name: String,
    symbol: String,
    trades_count: usize,
    winning_trades: usize,
    losing_trades: usize,
    win_rate: f64,
    initial_balance: f64,
    final_balance: f64,
    net_profit: f64,
    net_profit_pct: f64,
    profit_factor: f64,
    max_drawdown: f64,
    long_trades: usize,
    short_trades: usize,
    buy_and_hold_pct: f64,
}

async fn fetch_range_klines(
    client: &reqwest::Client,
    symbol: &str,
    interval: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<Candle>> {
    let mut all_candles: Vec<Candle> = Vec::new();
    let mut current_start = start_ms;

    while current_start < end_ms {
        let url = format!(
            "https://fapi.binance.com/fapi/v1/klines?symbol={}&interval={}&startTime={}&endTime={}&limit=1000",
            symbol, interval, current_start, end_ms
        );

        let resp: Vec<Vec<Value>> = client.get(&url).send().await?.json().await?;
        if resp.is_empty() {
            break;
        }

        let mut last_open = 0;
        for row in resp {
            let open_time = row[0].as_i64().unwrap_or(0);
            let open: f64 = row[1].as_str().unwrap().parse().unwrap();
            let high: f64 = row[2].as_str().unwrap().parse().unwrap();
            let low: f64 = row[3].as_str().unwrap().parse().unwrap();
            let close: f64 = row[4].as_str().unwrap().parse().unwrap();
            let volume: f64 = row[5].as_str().unwrap().parse().unwrap();
            let close_time = row[6].as_i64().unwrap_or(0);

            last_open = open_time;

            all_candles.push(Candle {
                open_time,
                close_time,
                open,
                high,
                low,
                close,
                volume,
                is_closed: true,
            });
        }

        if last_open >= end_ms || last_open <= current_start {
            break;
        }
        current_start = last_open + 1;
    }

    Ok(all_candles)
}

fn simulate_mean_reversion(
    name: &str,
    symbol: &str,
    candles: &[Candle],
    test_start_idx: usize,
    ols_len: usize,
    sd_mult: f64,
    tp_pct: f64,
    sl_pct: f64,
    use_chop_filter: bool,
    initial_balance: f64,
    risk_pct_trade: f64,
) -> (SimSummary, Vec<Trade>) {
    let mut balance = initial_balance;
    let mut peak_balance = initial_balance;
    let mut max_dd = 0.0;

    let mut in_pos = false;
    let mut pos_side = "";
    let mut pos_entry = 0.0;
    let mut pos_tp = 0.0;
    let mut pos_sl = 0.0;
    let mut pos_notional = 0.0;
    let mut pos_entry_time = 0;

    let mut trades: Vec<Trade> = Vec::new();
    let mut long_count = 0;
    let mut short_count = 0;
    let mut cooldown = 0;

    for i in test_start_idx..candles.len() {
        let c = &candles[i];
        if cooldown > 0 {
            cooldown -= 1;
        }

        // 1. Checagem de Saída Intrabar se estiver posicionado
        if in_pos {
            let mut exited = false;
            let mut exit_p = 0.0;
            let mut reason = "";

            if pos_side == "LONG" {
                if c.low <= pos_sl {
                    exited = true;
                    exit_p = pos_sl;
                    reason = "Stop Loss (-1.2% Limite)";
                } else if c.high >= pos_tp {
                    exited = true;
                    exit_p = pos_tp;
                    reason = "Take Profit (Reversão à Média)";
                }
            } else if pos_side == "SHORT" {
                if c.high >= pos_sl {
                    exited = true;
                    exit_p = pos_sl;
                    reason = "Stop Loss (-1.2% Limite)";
                } else if c.low <= pos_tp {
                    exited = true;
                    exit_p = pos_tp;
                    reason = "Take Profit (Reversão à Média)";
                }
            }

            if exited {
                let pnl_pct = if pos_side == "LONG" {
                    (exit_p - pos_entry) / pos_entry * 100.0
                } else {
                    (pos_entry - exit_p) / pos_entry * 100.0
                };
                let fee = pos_notional * 0.0008; // Taxa Binance taker 0.04% x 2
                let pnl_usdt = (pos_notional * (pnl_pct / 100.0)) - fee;

                balance += pnl_usdt;
                if balance > peak_balance {
                    peak_balance = balance;
                }
                let dd = if peak_balance > 0.0 { ((peak_balance - balance) / peak_balance) * 100.0 } else { 0.0 };
                if dd > max_dd {
                    max_dd = dd;
                }

                trades.push(Trade {
                    side: pos_side,
                    entry_price: pos_entry,
                    exit_price: exit_p,
                    pnl_pct,
                    pnl_usdt,
                    is_win: pnl_usdt > 0.0,
                    entry_time: pos_entry_time,
                    exit_time: c.close_time,
                    reason: reason.to_string(),
                });

                in_pos = false;
                pos_side = "";
                cooldown = 2; // Espera 2 velas para estabilizar
            }
        }

        // 2. Avaliação de Nova Entrada se estiver neutro e sem cooldown
        if !in_pos && cooldown == 0 && i >= ols_len {
            let slice = &candles[..=i];
            let closes: Vec<f64> = slice.iter().map(|k| k.close).collect();

            let baseline = calculate_linreg(&closes, ols_len);
            let atr = calculate_atr_slice(slice, 14);

            let mut rsi_calc = Rsi::new(14);
            let mut chop_calc = ChoppinessIndex::new(14);
            let mut chop_val = 50.0;
            for k in &slice[slice.len().saturating_sub(30)..] {
                rsi_calc.update(k.close);
                chop_val = chop_calc.update(k);
            }
            let rsi = rsi_calc.value().unwrap_or(50.0);
            let chop = chop_val;

            let upper_band = baseline + (sd_mult * atr);
            let lower_band = baseline - (sd_mult * atr);

            let is_lateral = if use_chop_filter { chop >= 48.0 } else { true };

            if is_lateral && baseline > 0.0 && atr > 0.0 {
                let notional = (balance * (risk_pct_trade / 100.0) / (sl_pct / 100.0)).min(balance * 5.0).max(10.0);

                // GATILHO LONG: Exaustão no Fundo (Preço bate ou fura banda inferior + RSI sobrevendido)
                if (c.low <= lower_band || c.close <= lower_band) && rsi <= 40.0 {
                    in_pos = true;
                    pos_side = "LONG";
                    pos_entry = c.close;
                    pos_entry_time = c.close_time;
                    pos_notional = notional;
                    // Alvo: Linha central da Regressão (Baseline) ou TP fixo
                    pos_tp = baseline.max(pos_entry * (1.0 + (tp_pct / 100.0)));
                    pos_sl = pos_entry * (1.0 - (sl_pct / 100.0));
                    long_count += 1;
                }
                // GATILHO SHORT: Exaustão no Topo (Preço bate ou fura banda superior + RSI sobrecomprado)
                else if (c.high >= upper_band || c.close >= upper_band) && rsi >= 60.0 {
                    in_pos = true;
                    pos_side = "SHORT";
                    pos_entry = c.close;
                    pos_entry_time = c.close_time;
                    pos_notional = notional;
                    // Alvo: Linha central da Regressão (Baseline) ou TP fixo
                    pos_tp = baseline.min(pos_entry * (1.0 - (tp_pct / 100.0)));
                    pos_sl = pos_entry * (1.0 + (sl_pct / 100.0));
                    short_count += 1;
                }
            }
        }
    }

    // Fecha posição residual
    if in_pos {
        let last_c = candles.last().unwrap();
        let pnl_pct = if pos_side == "LONG" {
            (last_c.close - pos_entry) / pos_entry * 100.0
        } else {
            (pos_entry - last_c.close) / pos_entry * 100.0
        };
        let pnl_usdt = pos_notional * (pnl_pct / 100.0);
        balance += pnl_usdt;
        trades.push(Trade {
            side: pos_side,
            entry_price: pos_entry,
            exit_price: last_c.close,
            pnl_pct,
            pnl_usdt,
            is_win: pnl_usdt > 0.0,
            entry_time: pos_entry_time,
            exit_time: last_c.close_time,
            reason: "Fechamento Final do Mês".to_string(),
        });
    }

    let trades_count = trades.len();
    let wins = trades.iter().filter(|t| t.is_win).count();
    let losses = trades_count - wins;
    let win_rate = if trades_count > 0 { (wins as f64 / trades_count as f64) * 100.0 } else { 0.0 };

    let mut gross_profit = 0.0;
    let mut gross_loss = 0.0;
    for t in &trades {
        if t.pnl_usdt > 0.0 {
            gross_profit += t.pnl_usdt;
        } else {
            gross_loss += t.pnl_usdt.abs();
        }
    }
    let profit_factor = if gross_loss > 0.0 { gross_profit / gross_loss } else if gross_profit > 0.0 { 99.9 } else { 0.0 };
    let net_profit = balance - initial_balance;
    let net_profit_pct = (net_profit / initial_balance) * 100.0;

    let p_first = candles[test_start_idx].open;
    let p_last = candles.last().unwrap().close;
    let buy_and_hold_pct = ((p_last - p_first) / p_first) * 100.0;

    (
        SimSummary {
            name: name.to_string(),
            symbol: symbol.to_string(),
            trades_count,
            winning_trades: wins,
            losing_trades: losses,
            win_rate,
            initial_balance,
            final_balance: balance,
            net_profit,
            net_profit_pct,
            profit_factor,
            max_drawdown: max_dd,
            long_trades: long_count,
            short_trades: short_count,
            buy_and_hold_pct,
        },
        trades,
    )
}

fn print_card(res: &SimSummary) {
    println!("┌────────────────────────────────────────────────────────────────────────┐");
    println!("│ CENÁRIO: {:<61} │", res.name);
    println!("├────────────────────────────────────────────────────────────────────────┤");
    println!("│ Ativo: {:<12} │ Mês: Abril/2026 (2880 velas) │ Buy & Hold: {:>+11.2}% │", res.symbol, res.buy_and_hold_pct);
    println!("│ Saldo Inicial: ${:>7.2} │ Saldo Final: ${:>7.2} │ Lucro Líquido: {:>+9.2} ({:>+6.2}%)│", res.initial_balance, res.final_balance, res.net_profit, res.net_profit_pct);
    println!("│ Trades Executados: {:<4}│ Vitórias / Derrotas: {:>2}W / {:<2}L │ Taxa de Acerto: {:>14.1}% │", res.trades_count, res.winning_trades, res.losing_trades, res.win_rate);
    println!("│ Profit Factor: {:>7.2} │ Max Drawdown: {:>7.2}% │ Trades Long / Short: {:>4} / {:<4} │", res.profit_factor, res.max_drawdown, res.long_trades, res.short_trades);
    println!("└────────────────────────────────────────────────────────────────────────┘");
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::new();
    let symbol = env::args().nth(1).unwrap_or_else(|| "SOLUSDT".to_string());
    let initial_balance = 100.0;

    let april_start_ms = 1775001600000i64; // 01/04/2026 00:00:00 UTC
    let april_end_ms = 1777593599999i64;   // 30/04/2026 23:59:59 UTC
    let warmup_start_ms = april_start_ms - (200 * 15 * 60 * 1000);

    println!("==========================================================================");
    println!("  EXPERIMENTO QUANTITATIVO: OPERANDO REVERSÃO À MÉDIA EM MERCADO LATERAL");
    println!("  Ativo: {} | Mês: Abril/2026 (Consolidação em Faixa)", symbol);
    println!("==========================================================================");

    let all_15m = fetch_range_klines(&client, &symbol, "15m", warmup_start_ms, april_end_ms).await?;
    let test_start_idx = all_15m.iter().position(|c| c.open_time >= april_start_ms).unwrap_or(200);

    println!("Dados carregados: {} candles de 15m no total ({} velas no mês de Abril)", 
        all_15m.len(), 
        all_15m.len() - test_start_idx
    );

    // CENÁRIO A: Reversão à Média Calibrada (Bandas 1.8x ATR + TP na Linha Central + RSI)
    let (res_a, trades_a) = simulate_mean_reversion(
        "Reversão à Média (Fade the Extremes • TP OLS Baseline)",
        &symbol,
        &all_15m,
        test_start_idx,
        100,  // OLS len 100
        1.8,  // Banda 1.8 ATR
        1.6,  // TP 1.6% (ou OLS)
        1.2,  // SL 1.2%
        true, // Filtro Choppiness ativo
        initial_balance,
        2.0,  // Risco 2%
    );

    // CENÁRIO B: Reversão à Média Ágil (Bandas 1.5x ATR + TP Rápido 1.2% + SL 1.0%)
    let (res_b, trades_b) = simulate_mean_reversion(
        "Reversão à Média Ágil (Bandas 1.5x • TP 1.2% • SL 1.0%)",
        &symbol,
        &all_15m,
        test_start_idx,
        100,
        1.5,
        1.2,
        1.0,
        false,
        initial_balance,
        2.0,
    );

    println!("\n==========================================================================");
    println!("                 RESULTADOS COMPARATIVOS EM MERCADO LATERAL               ");
    println!("==========================================================================");
    print_card(&res_a);
    print_card(&res_b);

    println!("\n==========================================================================");
    println!("    EXTRATO DETALHADO: REVERSÃO À MÉDIA EM ABRIL (CENÁRIO A)");
    println!("==========================================================================");
    if trades_a.is_empty() {
        println!("Nenhum trade realizado.");
    } else {
        for (idx, t) in trades_a.iter().enumerate() {
            let t_in = DateTime::<Utc>::from_timestamp_millis(t.entry_time).unwrap_or_default();
            let t_out = DateTime::<Utc>::from_timestamp_millis(t.exit_time).unwrap_or_default();
            println!(
                "  #{:02} [{}] {:<5} @ {:.2} -> [{}] @ {:.2} | PnL: {:>+6.2}% (${:>+5.2}) | {}",
                idx + 1,
                t_in.format("%d/%m %H:%M"),
                t.side,
                t.entry_price,
                t_out.format("%d/%m %H:%M"),
                t.exit_price,
                t.pnl_pct,
                t.pnl_usdt,
                t.reason
            );
        }
    }

    // Avaliação cruzada com ETHUSDT
    if symbol != "ETHUSDT" {
        println!("\n==========================================================================");
        println!("  AVALIAÇÃO CRUZADA: REVERSÃO À MÉDIA NO ETHUSDT EM ABRIL");
        println!("==========================================================================");
        let eth_15m = fetch_range_klines(&client, "ETHUSDT", "15m", warmup_start_ms, april_end_ms).await?;
        let eth_idx = eth_15m.iter().position(|c| c.open_time >= april_start_ms).unwrap_or(200);

        let (eth_res, eth_trades) = simulate_mean_reversion(
            "ETHUSDT - Reversão à Média (Fade the Extremes)",
            "ETHUSDT",
            &eth_15m,
            eth_idx,
            100,
            1.8,
            1.6,
            1.2,
            true,
            initial_balance,
            2.0,
        );
        print_card(&eth_res);

        println!("\nExtrato ETHUSDT em Abril:");
        for (idx, t) in eth_trades.iter().enumerate() {
            let t_in = DateTime::<Utc>::from_timestamp_millis(t.entry_time).unwrap_or_default();
            let t_out = DateTime::<Utc>::from_timestamp_millis(t.exit_time).unwrap_or_default();
            println!(
                "  #{:02} [{}] {:<5} @ {:.2} -> [{}] @ {:.2} | PnL: {:>+6.2}% (${:>+5.2}) | {}",
                idx + 1,
                t_in.format("%d/%m %H:%M"),
                t.side,
                t.entry_price,
                t_out.format("%d/%m %H:%M"),
                t.exit_price,
                t.pnl_pct,
                t.pnl_usdt,
                t.reason
            );
        }
    }

    Ok(())
}
