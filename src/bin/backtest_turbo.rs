#![allow(dead_code, unused_variables, unused_imports, unused_assignments)]
use anyhow::Result;
use binance_futures_bot::indicators::{calculate_atr_slice, calculate_linreg, ChoppinessIndex, Supertrend};
use binance_futures_bot::types::Candle;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::env;

#[derive(Debug, Clone)]
struct TradeLog {
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
    period_name: String,
    symbol: String,
    candles_count: usize,
    trades_count: usize,
    winning_trades: usize,
    losing_trades: usize,
    breakeven_trades: usize,
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

fn simulate_cascade_turbo(
    name: &str,
    period_name: &str,
    symbol: &str,
    candles: &[Candle],
    test_start_idx: usize,
    enable_partial_tp: bool,
    enable_breakeven: bool,
    enable_adaptive_bands: bool,
    enable_smart_ignition: bool,
    initial_balance: f64,
    risk_pct_trade: f64,
) -> (SimSummary, Vec<TradeLog>) {
    let mut balance = initial_balance;
    let mut peak_balance = initial_balance;
    let mut max_dd = 0.0;

    let mut in_pos = false;
    let mut pos_side = "";
    let mut pos_entry = 0.0;
    let mut pos_sl = 0.0;
    let mut pos_notional = 0.0;
    let mut pos_entry_time = 0;
    let mut be_locked = false;
    let mut partial_taken = false;
    let mut st_tracker = Supertrend::new(22, 3.0);

    let mut trades: Vec<TradeLog> = Vec::new();
    let mut long_count = 0;
    let mut short_count = 0;
    let mut be_count = 0;
    let mut cooldown = 0;

    for i in test_start_idx..candles.len() {
        let c = &candles[i];
        if cooldown > 0 {
            cooldown -= 1;
        }

        let slice = &candles[..=i];
        let closes: Vec<f64> = slice.iter().map(|k| k.close).collect();
        let baseline = calculate_linreg(&closes, 100);
        let atr = calculate_atr_slice(slice, 14);

        let mut chop_calc = ChoppinessIndex::new(14);
        let mut chop_val = 50.0;
        for k in &slice[slice.len().saturating_sub(25)..] {
            chop_val = chop_calc.update(k);
        }

        let st_opt = st_tracker.update(c.high, c.low, c.close).map(|(val, _)| val);

        // 1. GESTÃO ATIVA DA POSIÇÃO (INTRABAR & FECHAMENTO)
        if in_pos {
            let mut closed_full = false;
            let mut exit_p = 0.0;
            let mut exit_reason = "";

            if pos_side == "LONG" {
                // A. Realização Parcial de 50% a +2.5%
                if enable_partial_tp && !partial_taken && c.high >= pos_entry * 1.025 {
                    partial_taken = true;
                    let partial_exit = pos_entry * 1.025;
                    let pnl_pct = 2.5;
                    let closed_notional = pos_notional * 0.5;
                    pos_notional *= 0.5; // Resta 50%
                    let fee = closed_notional * 0.0008;
                    let pnl_usdt = (closed_notional * (pnl_pct / 100.0)) - fee;
                    balance += pnl_usdt;

                    // Ao realizar parcial, trava o restante no Breakeven
                    pos_sl = pos_entry * 1.0025;
                    be_locked = true;

                    trades.push(TradeLog {
                        side: "LONG (PARCIAL 50%)",
                        entry_price: pos_entry,
                        exit_price: partial_exit,
                        pnl_pct,
                        pnl_usdt,
                        is_win: true,
                        entry_time: pos_entry_time,
                        exit_time: c.close_time,
                        reason: "Take Profit Parcial (+2.5%)".to_string(),
                    });
                }

                // B. Trava de Breakeven em +1.5%
                if enable_breakeven && !be_locked && c.high >= pos_entry * 1.015 {
                    be_locked = true;
                    pos_sl = pos_entry * 1.0025; // Trava +0.25% cobrindo taxas
                }

                // C. Ratchet Dinâmico do Stop Supertrend
                if let Some(st_val) = st_opt {
                    if st_val > pos_sl {
                        pos_sl = st_val;
                    }
                }

                // D. Checagem de Saída pelo Stop (ou Breakeven)
                if c.low <= pos_sl {
                    closed_full = true;
                    exit_p = pos_sl;
                    exit_reason = if be_locked {
                        be_count += 1;
                        if partial_taken { "Trailing Stop após Parcial" } else { "Breakeven Seguro (+0.25%)" }
                    } else {
                        "Stop Loss Supertrend"
                    };
                }
            } else if pos_side == "SHORT" {
                // A. Realização Parcial de 50% a +2.5%
                if enable_partial_tp && !partial_taken && c.low <= pos_entry * 0.975 {
                    partial_taken = true;
                    let partial_exit = pos_entry * 0.975;
                    let pnl_pct = 2.5;
                    let closed_notional = pos_notional * 0.5;
                    pos_notional *= 0.5;
                    let fee = closed_notional * 0.0008;
                    let pnl_usdt = (closed_notional * (pnl_pct / 100.0)) - fee;
                    balance += pnl_usdt;

                    pos_sl = pos_entry * 0.9975;
                    be_locked = true;

                    trades.push(TradeLog {
                        side: "SHORT (PARCIAL 50%)",
                        entry_price: pos_entry,
                        exit_price: partial_exit,
                        pnl_pct,
                        pnl_usdt,
                        is_win: true,
                        entry_time: pos_entry_time,
                        exit_time: c.close_time,
                        reason: "Take Profit Parcial (+2.5%)".to_string(),
                    });
                }

                // B. Trava de Breakeven em +1.5%
                if enable_breakeven && !be_locked && c.low <= pos_entry * 0.985 {
                    be_locked = true;
                    pos_sl = pos_entry * 0.9975;
                }

                // C. Ratchet Supertrend
                if let Some(st_val) = st_opt {
                    if st_val < pos_sl && st_val > 0.0 {
                        pos_sl = st_val;
                    }
                }

                // D. Checagem de Saída
                if c.high >= pos_sl {
                    closed_full = true;
                    exit_p = pos_sl;
                    exit_reason = if be_locked {
                        be_count += 1;
                        if partial_taken { "Trailing Stop após Parcial" } else { "Breakeven Seguro (+0.25%)" }
                    } else {
                        "Stop Loss Supertrend"
                    };
                }
            }

            if closed_full {
                let pnl_pct = if pos_side == "LONG" {
                    (exit_p - pos_entry) / pos_entry * 100.0
                } else {
                    (pos_entry - exit_p) / pos_entry * 100.0
                };
                let fee = pos_notional * 0.0008;
                let pnl_usdt = (pos_notional * (pnl_pct / 100.0)) - fee;
                balance += pnl_usdt;

                if balance > peak_balance {
                    peak_balance = balance;
                }
                let dd = if peak_balance > 0.0 { ((peak_balance - balance) / peak_balance) * 100.0 } else { 0.0 };
                if dd > max_dd {
                    max_dd = dd;
                }

                trades.push(TradeLog {
                    side: pos_side,
                    entry_price: pos_entry,
                    exit_price: exit_p,
                    pnl_pct,
                    pnl_usdt,
                    is_win: pnl_usdt > 0.0,
                    entry_time: pos_entry_time,
                    exit_time: c.close_time,
                    reason: exit_reason.to_string(),
                });

                in_pos = false;
                pos_side = "";
                be_locked = false;
                partial_taken = false;
                cooldown = 2;
            }
        }

        // 2. AVALIAÇÃO DE NOVA ENTRADA
        if !in_pos && cooldown == 0 && i >= 100 {
            // Multiplicador Adaptativo de Banda
            let sd_mult = if enable_adaptive_bands {
                if chop_val >= 50.0 { 2.2 } else { 1.8 }
            } else {
                2.0
            };

            let upper_band = baseline + (sd_mult * atr);
            let lower_band = baseline - (sd_mult * atr);

            // Slope OLS Normalizado
            let prev_base = calculate_linreg(&closes[..closes.len().saturating_sub(3)], 100);
            let norm_slope = if atr > 0.0 { (baseline - prev_base) / 3.0 / atr } else { 0.0 };

            let vol_sma20: f64 = slice.iter().rev().take(20).map(|k| k.volume).sum::<f64>() / 20.0;
            let vol_expansion = c.volume >= vol_sma20 * 1.35;

            // Smart Ignition
            let smart_ignition = if enable_smart_ignition {
                chop_val < 46.0 && vol_expansion && (c.high - c.low) > (1.5 * atr)
            } else {
                false
            };

            let notional = (balance * (risk_pct_trade / 100.0) / 0.015).min(balance * 5.0).max(10.0);

            // GATILHO LONG: Preço fecha acima da banda superior adaptativa
            if (c.close > upper_band && norm_slope >= 0.08 && c.close > c.open) || (smart_ignition && norm_slope > 0.05) {
                in_pos = true;
                pos_side = "LONG";
                pos_entry = c.close;
                pos_entry_time = c.close_time;
                pos_notional = notional;
                pos_sl = if let Some(st) = st_opt { st.min(pos_entry * 0.985) } else { pos_entry * 0.985 };
                be_locked = false;
                partial_taken = false;
                long_count += 1;
            }
            // GATILHO SHORT: Preço fecha abaixo da banda inferior adaptativa
            else if (c.close < lower_band && norm_slope <= -0.08 && c.close < c.open) || (smart_ignition && norm_slope < -0.05) {
                in_pos = true;
                pos_side = "SHORT";
                pos_entry = c.close;
                pos_entry_time = c.close_time;
                pos_notional = notional;
                pos_sl = if let Some(st) = st_opt { st.max(pos_entry * 1.015) } else { pos_entry * 1.015 };
                be_locked = false;
                partial_taken = false;
                short_count += 1;
            }
        }
    }

    // Fecha posição residual ao fim
    if in_pos {
        let last_c = candles.last().unwrap();
        let pnl_pct = if pos_side == "LONG" {
            (last_c.close - pos_entry) / pos_entry * 100.0
        } else {
            (pos_entry - last_c.close) / pos_entry * 100.0
        };
        let pnl_usdt = pos_notional * (pnl_pct / 100.0);
        balance += pnl_usdt;
        trades.push(TradeLog {
            side: pos_side,
            entry_price: pos_entry,
            exit_price: last_c.close,
            pnl_pct,
            pnl_usdt,
            is_win: pnl_usdt > 0.0,
            entry_time: pos_entry_time,
            exit_time: last_c.close_time,
            reason: "Fechamento Final do Período".to_string(),
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
            period_name: period_name.to_string(),
            symbol: symbol.to_string(),
            candles_count: candles.len() - test_start_idx,
            trades_count,
            winning_trades: wins,
            losing_trades: losses,
            breakeven_trades: be_count,
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
    println!("│ Ativo: {:<8} │ Período: {:<17} │ Buy & Hold: {:>+15.2}% │", res.symbol, res.period_name, res.buy_and_hold_pct);
    println!("│ Saldo Inicial: ${:>7.2} │ Saldo Final: ${:>7.2} │ Lucro Líquido: {:>+9.2} ({:>+6.2}%)│", res.initial_balance, res.final_balance, res.net_profit, res.net_profit_pct);
    println!("│ Trades: {:<3} (BE: {:<2})│ Vitórias / Derrotas: {:>2}W / {:<2}L │ Taxa de Acerto: {:>14.1}% │", res.trades_count, res.breakeven_trades, res.winning_trades, res.losing_trades, res.win_rate);
    println!("│ Profit Factor: {:>7.2} │ Max Drawdown: {:>7.2}% │ Trades Long / Short: {:>4} / {:<4} │", res.profit_factor, res.max_drawdown, res.long_trades, res.short_trades);
    println!("└────────────────────────────────────────────────────────────────────────┘");
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::new();
    let symbol = env::args().nth(1).unwrap_or_else(|| "SOLUSDT".to_string());
    let initial_balance = 100.0;

    println!("==========================================================================");
    println!("  BACKTEST QUANTITATIVO COMPARATIVO: CASCATA TURBO (PROJETO OTIMIZADO)");
    println!("  Ativo: {} | Binance Futures USDⓈ-M Perpétuo", symbol);
    println!("==========================================================================");

    // 1. DADOS DE ABRIL (MERCADO LATERALIZADO)
    let april_start_ms = 1775001600000i64; // 01/04/2026 00:00:00 UTC
    let april_end_ms = 1777593599999i64;   // 30/04/2026 23:59:59 UTC
    let warmup_april_ms = april_start_ms - (200 * 15 * 60 * 1000);

    let april_15m = fetch_range_klines(&client, &symbol, "15m", warmup_april_ms, april_end_ms).await?;
    let april_idx = april_15m.iter().position(|c| c.open_time >= april_start_ms).unwrap_or(200);

    // 2. DADOS DE OUTUBRO (MERCADO DE TENDÊNCIA DE QUEDA)
    let oct_start_ms = 1790812800000i64; // 01/10/2026 00:00:00 UTC
    let oct_now_ms = 1792022236580i64;
    let warmup_oct_ms = oct_start_ms - (200 * 15 * 60 * 1000);

    let oct_15m = fetch_range_klines(&client, &symbol, "15m", warmup_oct_ms, oct_now_ms).await?;
    let oct_idx = oct_15m.iter().position(|c| c.open_time >= oct_start_ms).unwrap_or(200);

    println!("Dados carregados:");
    println!("  • Mês de Abril:   {} candles de 15m (Consolidação em Faixa)", april_15m.len() - april_idx);
    println!("  • Mês de Outubro: {} candles de 15m (Tendência Direcional)", oct_15m.len() - oct_idx);

    // ==========================================
    // TESTE NO MÊS LATERAL (ABRIL)
    // ==========================================
    println!("\n==========================================================================");
    println!("                 TESTE 1: MÊS LATERAL (ABRIL/2026 • SOLUSDT)              ");
    println!("==========================================================================");

    // Cenário A: Cascata Turbo Completa (Breakeven + Parcial 50% + Bandas Adaptativas + Smart Ignition)
    let (res_turbo_apr, trades_turbo_apr) = simulate_cascade_turbo(
        "Cascata Turbo (Breakeven + TP Parcial 50% + Bandas Adapt)",
        "Abril/2026",
        &symbol,
        &april_15m,
        april_idx,
        true,  // TP parcial 50% em +2.5%
        true,  // Trava Breakeven em +1.5%
        true,  // Bandas adaptativas por Choppiness
        true,  // Smart ignition
        initial_balance,
        2.0,
    );

    // Cenário B: Cascata Clássica (Versão Anterior sem Breakeven nem Parcial)
    let (res_classic_apr, _) = simulate_cascade_turbo(
        "Cascata MTF Anterior (Sem Breakeven / Sem Parcial)",
        "Abril/2026",
        &symbol,
        &april_15m,
        april_idx,
        false,
        false,
        false,
        false,
        initial_balance,
        2.0,
    );

    print_card(&res_turbo_apr);
    print_card(&res_classic_apr);

    println!("\nExtrato de Operações da Cascata Turbo em Abril:");
    for (idx, t) in trades_turbo_apr.iter().enumerate() {
        let t_in = DateTime::<Utc>::from_timestamp_millis(t.entry_time).unwrap_or_default();
        let t_out = DateTime::<Utc>::from_timestamp_millis(t.exit_time).unwrap_or_default();
        println!(
            "  #{:02} [{}] {:<20} @ {:.2} -> [{}] @ {:.2} | PnL: {:>+6.2}% (${:>+5.2}) | {}",
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

    // ==========================================
    // TESTE NO MÊS DE TENDÊNCIA (OUTUBRO)
    // ==========================================
    println!("\n==========================================================================");
    println!("             TESTE 2: MÊS DE TENDÊNCIA (OUTUBRO/2026 • SOLUSDT)           ");
    println!("==========================================================================");

    let (res_turbo_oct, trades_turbo_oct) = simulate_cascade_turbo(
        "Cascata Turbo (Breakeven + TP Parcial 50% + Bandas Adapt)",
        "Outubro/2026",
        &symbol,
        &oct_15m,
        oct_idx,
        true,
        true,
        true,
        true,
        initial_balance,
        2.0,
    );

    let (res_classic_oct, _) = simulate_cascade_turbo(
        "Cascata MTF Anterior (Sem Breakeven / Sem Parcial)",
        "Outubro/2026",
        &symbol,
        &oct_15m,
        oct_idx,
        false,
        false,
        false,
        false,
        initial_balance,
        2.0,
    );

    print_card(&res_turbo_oct);
    print_card(&res_classic_oct);

    println!("\nExtrato de Operações da Cascata Turbo em Outubro:");
    for (idx, t) in trades_turbo_oct.iter().enumerate() {
        let t_in = DateTime::<Utc>::from_timestamp_millis(t.entry_time).unwrap_or_default();
        let t_out = DateTime::<Utc>::from_timestamp_millis(t.exit_time).unwrap_or_default();
        println!(
            "  #{:02} [{}] {:<20} @ {:.2} -> [{}] @ {:.2} | PnL: {:>+6.2}% (${:>+5.2}) | {}",
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

    Ok(())
}
