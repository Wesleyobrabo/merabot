use anyhow::Result;
use binance_futures_bot::indicators::{Ema, IndicatorEngine};
use binance_futures_bot::types::{Candle, PositionSide};
use serde_json::Value;

#[derive(Debug, Clone)]
struct TradeRecord {
    trade_num: usize,
    side: PositionSide,
    entry_time: i64,
    exit_time: i64,
    entry_price: f64,
    exit_price: f64,
    notional_usdt: f64,
    margin_usdt: f64,
    pnl_usdt: f64,
    pnl_eur: f64,
    balance_eur_after: f64,
    return_pct: f64,
    exit_reason: &'static str,
}

struct SimSummary {
    mode_name: String,
    initial_eur: f64,
    final_eur: f64,
    net_profit_eur: f64,
    total_return_pct: f64,
    total_trades: usize,
    wins: usize,
    losses: usize,
    win_rate: f64,
    profit_factor: f64,
    max_drawdown_pct: f64,
    trades: Vec<TradeRecord>,
}

async fn fetch_candles(client: &reqwest::Client, symbol: &str, total_needed: usize) -> Result<Vec<Candle>> {
    let mut all_candles = Vec::new();
    let mut end_time: Option<i64> = None;

    while all_candles.len() < total_needed {
        let chunk_limit = (total_needed - all_candles.len()).min(1000);
        let url = if let Some(et) = end_time {
            format!(
                "https://fapi.binance.com/fapi/v1/klines?symbol={}&interval=15m&limit={}&endTime={}",
                symbol, chunk_limit, et
            )
        } else {
            format!(
                "https://fapi.binance.com/fapi/v1/klines?symbol={}&interval=15m&limit={}",
                symbol, chunk_limit
            )
        };

        let res = client.get(&url)
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
            .send().await?;

        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            anyhow::bail!("Binance API HTTP {} em {}: {}", status, url, body);
        }

        let resp: Vec<Vec<Value>> = res.json().await?;
        if resp.is_empty() {
            break;
        }

        let mut chunk = Vec::with_capacity(resp.len());
        for row in &resp {
            let open_time = row[0].as_i64().unwrap_or(0);
            let open: f64 = row[1].as_str().unwrap().parse().unwrap();
            let high: f64 = row[2].as_str().unwrap().parse().unwrap();
            let low: f64 = row[3].as_str().unwrap().parse().unwrap();
            let close: f64 = row[4].as_str().unwrap().parse().unwrap();
            let volume: f64 = row[5].as_str().unwrap().parse().unwrap();
            let close_time = row[6].as_i64().unwrap_or(0);

            chunk.push(Candle {
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

        let oldest_in_chunk = chunk[0].open_time;
        chunk.append(&mut all_candles);
        all_candles = chunk;

        end_time = Some(oldest_in_chunk - 1);
        if resp.len() < chunk_limit {
            break;
        }
    }

    Ok(all_candles)
}

fn simulate(
    candles: &[Candle],
    initial_eur: f64,
    eur_to_usdt: f64,
    leverage: u32,
    use_full_balance: bool,
    tp_pct: f64,
    sl_pct: f64,
    mode_name: &str,
) -> SimSummary {
    let mut engine = IndicatorEngine::new(10, 2.2, 9, 21, 14, 20);
    engine.set_rolling_lookback(48); // 12 horas
    let mut ema_200 = Ema::new(200);

    let warmup_len = 200.min(candles.len() / 3);
    for c in &candles[..warmup_len] {
        engine.update(c);
        ema_200.update(c.close);
    }

    let mut balance_usdt = initial_eur * eur_to_usdt;
    let mut peak_balance = balance_usdt;
    let mut max_dd_pct = 0.0;

    let mut in_pos = false;
    let mut pos_side = PositionSide::None;
    let mut entry_price = 0.0;
    let mut sl_price = 0.0;
    let mut tp_price = 0.0;
    let mut entry_time = 0;
    let mut position_notional = 0.0;
    let mut margin_used = 0.0;

    let mut completed_trades = Vec::new();
    let mut trade_timestamps: Vec<i64> = Vec::new();

    for candle in &candles[warmup_len..] {
        trade_timestamps.retain(|&ts| candle.close_time - ts < 86_400_000);
        let limit_reached = trade_timestamps.len() >= 3;

        // 1. Checagem intrabar de saída
        if in_pos {
            let mut exited = false;
            let mut exit_p = 0.0;
            let mut reason = "";

            if pos_side == PositionSide::Long {
                if candle.low <= sl_price {
                    exited = true;
                    exit_p = sl_price;
                    reason = "Stop Loss (-1.2%)";
                } else if candle.high >= tp_price {
                    exited = true;
                    exit_p = tp_price;
                    reason = "Take Profit (+2.5%)";
                }
            } else if pos_side == PositionSide::Short {
                if candle.high >= sl_price {
                    exited = true;
                    exit_p = sl_price;
                    reason = "Stop Loss (-1.2%)";
                } else if candle.low <= tp_price {
                    exited = true;
                    exit_p = tp_price;
                    reason = "Take Profit (+2.5%)";
                }
            }

            if exited {
                let gross_price_pct = match pos_side {
                    PositionSide::Long => (exit_p - entry_price) / entry_price,
                    PositionSide::Short => (entry_price - exit_p) / entry_price,
                    _ => 0.0,
                };

                // Taxas Binance Futures VIP0 (0.05% taker entrada + 0.05% taker saída = 0.10%)
                let fee_pct = 0.0010;
                let net_return_factor = gross_price_pct - fee_pct;
                let trade_pnl_usdt = position_notional * net_return_factor;
                let trade_pnl_eur = trade_pnl_usdt / eur_to_usdt;

                balance_usdt += trade_pnl_usdt;
                let balance_eur = balance_usdt / eur_to_usdt;

                if balance_usdt > peak_balance {
                    peak_balance = balance_usdt;
                }
                let current_dd = if peak_balance > 0.0 {
                    ((peak_balance - balance_usdt) / peak_balance) * 100.0
                } else {
                    0.0
                };
                if current_dd > max_dd_pct {
                    max_dd_pct = current_dd;
                }

                completed_trades.push(TradeRecord {
                    trade_num: completed_trades.len() + 1,
                    side: pos_side,
                    entry_time,
                    exit_time: candle.close_time,
                    entry_price,
                    exit_price: exit_p,
                    notional_usdt: position_notional,
                    margin_usdt: margin_used,
                    pnl_usdt: trade_pnl_usdt,
                    pnl_eur: trade_pnl_eur,
                    balance_eur_after: balance_eur,
                    return_pct: (trade_pnl_usdt / margin_used) * 100.0,
                    exit_reason: reason,
                });

                in_pos = false;
                pos_side = PositionSide::None;
            }
        }

        // 2. Atualização dos indicadores
        let ind = engine.update(candle);
        let ema_val = ema_200.update(candle.close).unwrap_or(candle.close);

        // 3. Avaliação de Nova Entrada com Range de 12 Horas (48 candles)
        let r_high = ind.rolling_high_96;
        let r_low = ind.rolling_low_96;
        let range_ready = r_high > 0.0 && r_low > 0.0 && r_high > r_low;

        if !in_pos && !limit_reached && range_ready {
            let long_breakout = candle.close > r_high;
            let short_breakout = candle.close < r_low;
            let vol_ok = candle.volume >= ind.volume_sma;

            let allow_long = candle.close >= ema_val && vol_ok;
            let allow_short = candle.close < ema_val && vol_ok;

            let is_long_valid = long_breakout && !short_breakout && allow_long;
            let is_short_valid = short_breakout && !long_breakout && allow_short;

            if is_long_valid || is_short_valid {
                let is_long = is_long_valid;
                let side = if is_long { PositionSide::Long } else { PositionSide::Short };

                let slip = if is_long { 1.0005 } else { 0.9995 };
                let ep = candle.close * slip;

                // Dimensionamento de Posição
                let lev_f = leverage.max(1) as f64;
                let notional = if use_full_balance {
                    // Utiliza margem total disponível * alavancagem
                    balance_usdt * lev_f
                } else {
                    // Risco de 16% com alavancagem configurada (mínimo de 5.50 USDT piso Binance)
                    let risk_usdt = balance_usdt * 0.16;
                    let target_notional = risk_usdt / (sl_pct / 100.0);
                    target_notional.max(5.50).min(balance_usdt * lev_f)
                };

                let margin = notional / lev_f;

                // Verifica se tem margem suficiente na conta (com tolerância de precisão float)
                if margin <= (balance_usdt * 1.001) && notional >= 5.0 {
                    in_pos = true;
                    pos_side = side;
                    entry_price = ep;
                    entry_time = candle.close_time;
                    position_notional = notional;
                    margin_used = margin;

                    if is_long {
                        sl_price = ep * (1.0 - (sl_pct / 100.0));
                        tp_price = ep * (1.0 + (tp_pct / 100.0));
                    } else {
                        sl_price = ep * (1.0 + (sl_pct / 100.0));
                        tp_price = ep * (1.0 - (tp_pct / 100.0));
                    }

                    trade_timestamps.push(candle.close_time);
                }
            }
        }
    }

    let final_eur = balance_usdt / eur_to_usdt;
    let net_eur = final_eur - initial_eur;
    let tot_return = (net_eur / initial_eur) * 100.0;

    let wins = completed_trades.iter().filter(|t| t.pnl_eur > 0.0).count();
    let losses = completed_trades.iter().filter(|t| t.pnl_eur <= 0.0).count();
    let total_trades = completed_trades.len();
    let win_rate = if total_trades > 0 { (wins as f64 / total_trades as f64) * 100.0 } else { 0.0 };

    let gross_win_eur: f64 = completed_trades.iter().filter(|t| t.pnl_eur > 0.0).map(|t| t.pnl_eur).sum();
    let gross_loss_eur: f64 = completed_trades.iter().filter(|t| t.pnl_eur < 0.0).map(|t| t.pnl_eur.abs()).sum();
    let pf = if gross_loss_eur > 0.0 { gross_win_eur / gross_loss_eur } else { gross_win_eur };

    SimSummary {
        mode_name: mode_name.to_string(),
        initial_eur,
        final_eur,
        net_profit_eur: net_eur,
        total_return_pct: tot_return,
        total_trades,
        wins,
        losses,
        win_rate,
        profit_factor: pf,
        max_drawdown_pct: max_dd_pct,
        trades: completed_trades,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;
    let symbol = "ETHUSDT";
    let initial_eur = 20.00;
    let eur_to_usdt = 1.08; // Taxa de câmbio EUR/USDT aproximada

    println!("================================================================================");
    println!("  BACKTEST REAL: BINANCE FUTURES USD-M (ETHUSDT PERPETUAL)");
    println!("  BANCA INICIAL: €20,00 (~$21,60 USDT) | TIMEFRAME: 15M");
    println!("  CONFIGURAÇÃO ATUAL: Range 12h (48 candles) + EMA 200 + Volume SMA 20");
    println!("  REGRAS: TP +2.5% | SL -1.2% | Taker 0.05% + Slippage 0.05% por ponta");
    println!("================================================================================\n");

    let candles = fetch_candles(&client, symbol, 1500).await?;
    println!("Carregados {} candles de 15m da Binance Futures (~15.6 dias de dados reais).\n", candles.len());

    // 1. Simulação Oficial: 1x (Sem Alavancagem) - Margem Total da Banca (€20 por trade)
    let sim_1x_full = simulate(&candles, initial_eur, eur_to_usdt, 1, true, 2.5, 1.2, "1x (Sem Alavancagem) - 100% Banca (€20)");
    
    // 2. Simulação Risco Razoável: 1x com Nocional Mínimo Binance (~$5.50 USDT / ~€5,09)
    let sim_1x_min = simulate(&candles, initial_eur, eur_to_usdt, 1, false, 2.5, 1.2, "1x (Sem Alavancagem) - Nocional Seguro ($5.50)");

    // 3. Simulação Comparativa: 5x Alavancagem (Risco 16% da banca conforme config original)
    let sim_5x_risk = simulate(&candles, initial_eur, eur_to_usdt, 5, false, 2.5, 1.2, "5x Alavancagem (Risco 16% com SL 1.2%)");

    // 4. Simulação Comparativa: 3x Alavancagem Moderada
    let sim_3x = simulate(&candles, initial_eur, eur_to_usdt, 3, false, 2.5, 1.2, "3x Alavancagem Moderada");

    println!("----------------------------------------------------------------------------------------------------------------");
    println!("{:<36} | {:<7} | {:<7} | {:<8} | {:<12} | {:<10} | {:<7}", 
        "Configuração", "Trades", "WinRate", "Lucro €", "Retorno %", "Max DD %", "PF");
    println!("----------------------------------------------------------------------------------------------------------------");

    for sim in &[&sim_1x_full, &sim_1x_min, &sim_3x, &sim_5x_risk] {
        println!("{:<36} | {:>2} ({:>1}W/{:>1}L) | {:>6.1}% | {:>+9.2}€ | {:>+10.2}% | {:>8.2}% | {:>6.2}",
            sim.mode_name,
            sim.total_trades, sim.wins, sim.losses,
            sim.win_rate,
            sim.net_profit_eur,
            sim.total_return_pct,
            sim.max_drawdown_pct,
            sim.profit_factor
        );
    }
    println!("----------------------------------------------------------------------------------------------------------------\n");

    // Extrato Detalhado da Operação sem Alavancagem (1x 100% Banca)
    println!("EXTRATO TRADE A TRADE - OPERAÇÃO 1X SEM ALAVANCAGEM (€20 INICIAL):");
    println!("----------------------------------------------------------------------------------------------------------------");
    println!("{:<3} | {:<5} | {:<10} | {:<10} | {:<10} | {:<10} | {:<20} | {:<10}",
        "#", "Lado", "Entrada", "Saída", "PnL (€)", "Retorno", "Motivo Saída", "Saldo (€)");
    println!("----------------------------------------------------------------------------------------------------------------");

    for t in &sim_1x_full.trades {
        let side_str = match t.side {
            PositionSide::Long => "LONG",
            PositionSide::Short => "SHORT",
            _ => "NONE",
        };
        println!("{:<3} | {:<5} | ${:>9.2} | ${:>9.2} | {:>+8.2}€ | {:>+8.2}% | {:<20} | €{:>8.2}",
            t.trade_num,
            side_str,
            t.entry_price,
            t.exit_price,
            t.pnl_eur,
            t.return_pct,
            t.exit_reason,
            t.balance_eur_after
        );
    }
    println!("----------------------------------------------------------------------------------------------------------------\n");

    // Simulação no par ETHUSDT para comparação com a mesma banca de 20 EUR
    println!("================================================================================");
    println!("  COMPARATIVO NO ATIVO ETHUSDT (MESMA BANCA DE €20 E MESMAS REGRAS 12H)");
    println!("================================================================================\n");

    let candles_eth = fetch_candles(&client, "ETHUSDT", 1500).await?;
    let sim_eth_1x = simulate(&candles_eth, initial_eur, eur_to_usdt, 1, true, 2.5, 1.2, "ETHUSDT - 1x (Sem Alavancagem)");
    let sim_eth_3x = simulate(&candles_eth, initial_eur, eur_to_usdt, 3, false, 2.5, 1.2, "ETHUSDT - 3x Alavancagem");
    let sim_eth_5x = simulate(&candles_eth, initial_eur, eur_to_usdt, 5, false, 2.5, 1.2, "ETHUSDT - 5x Alavancagem");

    println!("----------------------------------------------------------------------------------------------------------------");
    println!("{:<36} | {:<7} | {:<7} | {:<8} | {:<12} | {:<10} | {:<7}", 
        "Configuração", "Trades", "WinRate", "Lucro €", "Retorno %", "Max DD %", "PF");
    println!("----------------------------------------------------------------------------------------------------------------");

    for sim in &[&sim_eth_1x, &sim_eth_3x, &sim_eth_5x] {
        println!("{:<36} | {:>2} ({:>1}W/{:>1}L) | {:>6.1}% | {:>+9.2}€ | {:>+10.2}% | {:>8.2}% | {:>6.2}",
            sim.mode_name,
            sim.total_trades, sim.wins, sim.losses,
            sim.win_rate,
            sim.net_profit_eur,
            sim.total_return_pct,
            sim.max_drawdown_pct,
            sim.profit_factor
        );
    }
    println!("----------------------------------------------------------------------------------------------------------------\n");

    Ok(())
}
