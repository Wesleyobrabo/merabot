// diagnose.rs - Simulador e Diagnóstico da Estratégia Gold / Session Breakout 15M (WillyAlgoTrader)
use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Timelike};
use serde_json::Value;

#[derive(Debug, Clone)]
struct Candle {
    open_time: i64,
    close_time: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PositionSide {
    Long,
    Short,
    None,
}

struct TradeLog {
    side: PositionSide,
    entry_time: i64,
    exit_time: i64,
    entry_price: f64,
    exit_price: f64,
    notional: f64,
    margin: f64,
    gross_pnl: f64,
    fees: f64,
    net_pnl: f64,
    reason: String,
    balance_after: f64,
}

const TAKER_FEE_PCT: f64 = 0.0005; // 0.05% taker

async fn fetch_klines(client: &reqwest::Client, symbol: &str, interval: &str, limit: usize) -> Result<Vec<Candle>> {
    let url = format!("https://fapi.binance.com/fapi/v1/klines?symbol={}&interval={}&limit={}", symbol, interval, limit);
    let resp = client.get(&url).send().await?.json::<Value>().await?;
    let arr = resp.as_array().context("Array esperado")?;

    let mut candles = Vec::new();
    for row in arr {
        let r = row.as_array().unwrap();
        candles.push(Candle {
            open_time: r[0].as_i64().unwrap(),
            close_time: r[6].as_i64().unwrap(),
            open: r[1].as_str().unwrap().parse().unwrap(),
            high: r[2].as_str().unwrap().parse().unwrap(),
            low: r[3].as_str().unwrap().parse().unwrap(),
            close: r[4].as_str().unwrap().parse().unwrap(),
            volume: r[5].as_str().unwrap().parse().unwrap(),
        });
    }
    Ok(candles)
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;
    let symbol = "SOLUSDT";
    let interval = "15m";

    // Parâmetros da Estratégia Asian Range Breakout 15M (Otimizada para Solana)
    let tz_offset_hours: i32 = 0; // UTC Puro
    let range_start_hour: u32 = 0;
    let range_end_hour: u32 = 7;
    let close_hour: u32 = 21;
    let sl_pct: f64 = 0.01; // 1.0%
    let leverage: f64 = 5.0;
    let risk_pct: f64 = 16.0; // % de risco por trade
    let initial_usdt: f64 = 9.81;
    let max_trades_per_day: usize = 3;

    println!("==========================================================================================");
    println!("   SIMULAÇÃO ASIAN RANGE BREAKOUT 15M (OTIMIZADA PARA SOLANA - ATÉ 3 TRADES/DIA)          ");
    println!("   PARÂMETROS: Range 00:00–07:00 UTC | Breakout 07:00–21:00 UTC | Time Exit 21:00 UTC     ");
    println!("==========================================================================================");
    println!("Configuração Oficial Ativa:");
    println!("    Par: {} | Timeframe: {} | Fuso: UTC | Alavancagem: {:.0}x ISOLATED", symbol, interval, leverage);
    println!("    Saldo Inicial: ${:.2} USDT | Risco por Trade: {:.1}% | Stop Loss: {:.1}%", initial_usdt, risk_pct, sl_pct * 100.0);
    println!("    Horário Range: {:02}:00 - {:02}:00 UTC | Janela Operacional: {:02}:00 - {:02}:00 UTC", range_start_hour, range_end_hour, range_end_hour, close_hour);
    println!("    Regra diária: Até {} operações por dia | Saída: Time Exit às {:02}:00 UTC ou Stop Loss", max_trades_per_day, close_hour);
    println!("==========================================================================================\n");

    let total_candles = fetch_klines(&client, symbol, interval, 1000).await?;
    if total_candles.len() < 40 {
        println!("Dados insuficientes.");
        return Ok(());
    }

    let dt_start = DateTime::from_timestamp_millis(total_candles.first().unwrap().open_time).unwrap_or_default();
    let dt_end = DateTime::from_timestamp_millis(total_candles.last().unwrap().close_time).unwrap_or_default();

    println!("Janela Avaliada:");
    println!("    Início: {} UTC", dt_start.format("%d/%m/%Y %H:%M"));
    println!("    Fim:    {} UTC", dt_end.format("%d/%m/%Y %H:%M"));
    println!("    Total de Velas Fechadas: {} velas de 15m (~{:.1} dias)\n", total_candles.len(), total_candles.len() as f64 / 96.0);

    let mut balance = initial_usdt;
    let mut peak_balance = balance;
    let mut max_drawdown_pct = 0.0;
    let mut trades: Vec<TradeLog> = Vec::new();

    // Estado da Sessão e Posição
    let mut in_position = false;
    let mut pos_side = PositionSide::None;
    let mut pos_entry_price = 0.0;
    let mut pos_qty = 0.0;
    let mut pos_notional = 0.0;
    let mut pos_margin = 0.0;
    let mut pos_sl = 0.0;
    let mut pos_entry_time = 0i64;

    let mut current_day_key = -1;
    let mut range_high: Option<f64> = None;
    let mut range_low: Option<f64> = None;
    let mut trades_today: usize = 0;

    println!("{:<16} | {:<7} | {:<7} | {:<7} | {:<7} | {:<7} | {:<7} | Status / Evento",
        "Hora (Sessão)", "Open", "High", "Low", "Close", "R-High", "R-Low");
    println!("-----------------------------------------------------------------------------------------------------------------");

    for c in &total_candles {
        let total_seconds = (c.open_time / 1000) + (tz_offset_hours as i64 * 3600);
        let dt = DateTime::from_timestamp(total_seconds, 0)
            .map(|d| d.naive_utc())
            .unwrap_or_default();

        let bar_hour = dt.hour();
        let _bar_minute = dt.minute();
        let day_key = dt.year() * 10000 + dt.month() as i32 * 100 + dt.day() as i32;

        // 1. Transição de dia
        if day_key != current_day_key {
            current_day_key = day_key;
            range_high = None;
            range_low = None;
            trades_today = 0;
        }

        // 2. Construção do Range (00:00 - 06:59)
        let in_range = bar_hour >= range_start_hour && bar_hour < range_end_hour;
        if in_range {
            range_high = Some(range_high.map_or(c.high, |h| h.max(c.high)));
            range_low = Some(range_low.map_or(c.low, |l| l.min(c.low)));
        }

        let range_ready = range_high.is_some() && range_low.is_some() && range_high.unwrap() > range_low.unwrap();
        let in_trading_window = bar_hour >= range_end_hour && bar_hour < close_hour;
        let past_close_hour = bar_hour >= close_hour || bar_hour < range_start_hour;

        let mut event_str = String::new();

        if in_position {
            let mut closed = false;
            let mut exit_price = 0.0;
            let mut reason = String::new();

            // A. Time Exit
            if past_close_hour {
                exit_price = c.close;
                reason = format!("Time Exit {:02}:00", close_hour);
                closed = true;
            } else {
                // B. Stop Loss
                match pos_side {
                    PositionSide::Long => {
                        if c.low <= pos_sl {
                            exit_price = pos_sl;
                            reason = format!("Stop Loss atingido (${:.2})", pos_sl);
                            closed = true;
                        }
                    }
                    PositionSide::Short => {
                        if c.high >= pos_sl {
                            exit_price = pos_sl;
                            reason = format!("Stop Loss atingido (${:.2})", pos_sl);
                            closed = true;
                        }
                    }
                    PositionSide::None => {}
                }
            }

            if closed {
                let gross_pnl = match pos_side {
                    PositionSide::Long => (exit_price - pos_entry_price) * pos_qty,
                    PositionSide::Short => (pos_entry_price - exit_price) * pos_qty,
                    PositionSide::None => 0.0,
                };
                let entry_fee = pos_notional * TAKER_FEE_PCT;
                let exit_fee = (pos_qty * exit_price) * TAKER_FEE_PCT;
                let total_fees = entry_fee + exit_fee;
                let net_pnl = gross_pnl - total_fees;

                balance += net_pnl;
                if balance > peak_balance {
                    peak_balance = balance;
                }
                let current_dd = ((peak_balance - balance) / peak_balance) * 100.0;
                if current_dd > max_drawdown_pct {
                    max_drawdown_pct = current_dd;
                }

                let pnl_pct_on_margin = if pos_margin > 0.0 { (net_pnl / pos_margin) * 100.0 } else { 0.0 };

                trades.push(TradeLog {
                    side: pos_side,
                    entry_time: pos_entry_time,
                    exit_time: c.close_time,
                    entry_price: pos_entry_price,
                    exit_price,
                    notional: pos_notional,
                    margin: pos_margin,
                    gross_pnl,
                    fees: total_fees,
                    net_pnl,
                    reason: reason.clone(),
                    balance_after: balance,
                });

                event_str = format!("SAÍDA {:?}: Preço ${:.2} | PnL: {:+>.2}$ ({:+>.1}%) | Saldo: ${:.2} | {}",
                    pos_side, exit_price, net_pnl, pnl_pct_on_margin, balance, reason);

                in_position = false;
                pos_side = PositionSide::None;
            } else {
                let un_pnl = match pos_side {
                    PositionSide::Long => (c.close - pos_entry_price) * pos_qty,
                    PositionSide::Short => (pos_entry_price - c.close) * pos_qty,
                    PositionSide::None => 0.0,
                };
                event_str = format!("EM POSIÇÃO {:?}: Preço ${:.2} | PnL Flutuante: {:+>.2}$ | SL: ${:.2}", pos_side, c.close, un_pnl, pos_sl);
            }
        } else {
            // Avalia nova entrada de Breakout
            let limit_reached = trades_today >= max_trades_per_day;

            if in_trading_window && range_ready && !limit_reached {
                let r_high = range_high.unwrap();
                let r_low = range_low.unwrap();

                let is_break_long = c.close > r_high;
                let is_break_short = c.close < r_low;

                if is_break_long || is_break_short {
                    let target_side = if is_break_long { PositionSide::Long } else { PositionSide::Short };
                    let entry_price = c.close;

                    let target_risk_usdt = balance * (risk_pct / 100.0);
                    let mut calc_notional = target_risk_usdt / sl_pct;

                    let max_pos_usdt = 50.0;
                    if calc_notional > max_pos_usdt {
                        calc_notional = max_pos_usdt;
                    }
                    let max_leverage_notional = balance * leverage * 0.92;
                    if calc_notional > max_leverage_notional {
                        calc_notional = max_leverage_notional;
                    }
                    if calc_notional < 5.50 {
                        calc_notional = 5.50;
                    }

                    let mut qty = (calc_notional / entry_price * 100.0).round() / 100.0;
                    if qty < 0.01 {
                        qty = 0.01;
                    }
                    let actual_notional = qty * entry_price;
                    let margin_req = actual_notional / leverage;

                    if margin_req <= balance * 0.95 {
                        in_position = true;
                        pos_side = target_side;
                        pos_entry_price = entry_price;
                        pos_qty = qty;
                        pos_notional = actual_notional;
                        pos_margin = margin_req;
                        pos_entry_time = c.close_time;
                        pos_sl = if target_side == PositionSide::Long {
                            entry_price * (1.0 - sl_pct)
                        } else {
                            entry_price * (1.0 + sl_pct)
                        };
                        trades_today += 1;

                        event_str = format!("ENTRADA #{}/{} {:?}: Breakout @ ${:.2} | Qtd: {:.2} SOL (${:.2}) | Margem: ${:.2} | SL: ${:.2}",
                            trades_today, max_trades_per_day, pos_side, entry_price, qty, actual_notional, margin_req, pos_sl);
                    } else {
                        event_str = format!("SINAL {:?} VETADO: Margem insuficiente", target_side);
                    }
                } else {
                    event_str = "Aguardando rompimento da sessão".to_string();
                }
            } else if in_range {
                event_str = format!("Construindo Range [{:02}h-{:02}h]", range_start_hour, range_end_hour);
            } else if limit_reached {
                event_str = format!("Limite diário atingido ({}/{} trades)", trades_today, max_trades_per_day);
            } else {
                event_str = "Fora da sessão operacional".to_string();
            }
        }

        let rh_str = range_high.map_or("-".to_string(), |v| format!("{:.2}", v));
        let rl_str = range_low.map_or("-".to_string(), |v| format!("{:.2}", v));

        println!("{:<16} | {:>7.2} | {:>7.2} | {:>7.2} | {:>7.2} | {:>7} | {:>7} | {}",
            dt.format("%d/%m %H:%M"), c.open, c.high, c.low, c.close,
            rh_str, rl_str, event_str);
    }

    println!("-----------------------------------------------------------------------------------------------------------------\n");

    let total_trades = trades.len();
    let winning_trades = trades.iter().filter(|t| t.net_pnl > 0.0).count();
    let losing_trades = trades.iter().filter(|t| t.net_pnl < 0.0).count();
    let gross_wins: f64 = trades.iter().filter(|t| t.net_pnl > 0.0).map(|t| t.net_pnl).sum();
    let gross_losses: f64 = trades.iter().filter(|t| t.net_pnl < 0.0).map(|t| t.net_pnl.abs()).sum();
    let net_profit = balance - initial_usdt;
    let net_profit_pct = (net_profit / initial_usdt) * 100.0;
    let win_rate = if total_trades > 0 { (winning_trades as f64 / total_trades as f64) * 100.0 } else { 0.0 };
    let profit_factor = if gross_losses > 0.0 { gross_wins / gross_losses } else { 0.0 };

    println!("==========================================================================================");
    println!("             RELATÓRIO FINANCEIRO DA ESTRATÉGIA GOLD / SESSION BREAKOUT                   ");
    println!("==========================================================================================");
    println!("  • Saldo Inicial:       ${:.2} USDT", initial_usdt);
    println!("  • Saldo Final:         ${:.2} USDT", balance);
    println!("  • Lucro Líquido:       {:+.2}$ ({:+.2}%)", net_profit, net_profit_pct);
    println!("  • Total de Operações:  {} ({} Vitórias / {} Derrotas)", total_trades, winning_trades, losing_trades);
    println!("  • Taxa de Acerto:      {:.1}%", win_rate);
    println!("  • Profit Factor:       {:.2}", profit_factor);
    println!("  • Max Drawdown:        {:.2}%", max_drawdown_pct);
    println!("==========================================================================================");

    if !trades.is_empty() {
        println!("\n--- EXTRATO DETALHADO DE OPERAÇÕES ---");
        for (i, t) in trades.iter().enumerate() {
            let dt_in = DateTime::from_timestamp_millis(t.entry_time).unwrap_or_default();
            let dt_out = DateTime::from_timestamp_millis(t.exit_time).unwrap_or_default();
            println!("#{} | {:?} | Entrada: {} @ ${:.2} | Saída: {} @ ${:.2}",
                i + 1, t.side, dt_in.format("%d/%m %H:%M"), t.entry_price, dt_out.format("%d/%m %H:%M"), t.exit_price);
            println!("    Nocional: ${:.2} (Margem: ${:.2}) | PnL Bruto: {:+.2}$ | Taxas: -${:.4} | PnL Líquido: {:+.2}$",
                t.notional, t.margin, t.gross_pnl, t.fees, t.net_pnl);
            println!("    Saldo Pós-Trade: ${:.2} | Motivo: {}", t.balance_after, t.reason);
        }
        println!("==========================================================================================");
    }

    Ok(())
}
