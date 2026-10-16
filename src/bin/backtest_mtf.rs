#![allow(dead_code, unused_variables, unused_imports, unused_assignments)]
// language: Rust, file: src/bin/backtest_mtf.rs, target: Windows 11, MSVC
// =========================================================================================
//  BACKTEST OFICIAL MULTI-TIMEFRAME (MTF): 1H MACRO BIAS + 5M GATILHO CIRÚRGICO
//  Trimestre: Agosto, Setembro e Outubro (2026)
//  Arquitetura:
//    - 1h: Envelope Nadaraya-Watson + EMA 20/50 + ATR (Congelado sem Lookahead Bias)
//    - 5m: MERA Scalp + PLE Traps (Execução, Rejeição de Pavio e Trailing Stop)
// =========================================================================================

use anyhow::Result;
use binance_futures_bot::indicators::{Atr, Ema, MeraKineticTensor, NadarayaWatson, SessionVwap, VolumeSma};
use binance_futures_bot::liquidity_trap::{LiquidityTrapConfig, PhantomLiquidityEngine};
use binance_futures_bot::types::{Candle, Position, PositionSide, SignalType};
use serde_json::Value;

#[derive(Debug, Clone)]
struct TradeLog {
    timestamp: i64,
    side: String,
    entry_price: f64,
    exit_price: f64,
    quantity: f64,
    gross_pnl_eur: f64,
    fee_eur: f64,
    net_pnl_eur: f64,
    return_pct: f64,
    balance_after_eur: f64,
    reason: String,
}

#[derive(Debug, Clone)]
struct BacktestResult {
    period_name: String,
    leverage: u32,
    initial_eur: f64,
    final_eur: f64,
    profit_eur: f64,
    total_return_pct: f64,
    total_trades: usize,
    wins: usize,
    losses: usize,
    win_rate: f64,
    max_drawdown_pct: f64,
    profit_factor: f64,
    tp_partials: usize,
    tp_finals: usize,
    breakevens: usize,
    stop_losses: usize,
    trades: Vec<TradeLog>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MacroBias {
    Bullish,
    Bearish,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MacroZone {
    Overbought, // Próximo ou acima do teto de 1h
    Oversold,   // Próximo ou abaixo do piso de 1h
    Equilibrium,// No centro da regressão
}

#[derive(Debug, Clone)]
struct MacroState1h {
    bias: MacroBias,
    zone: MacroZone,
    nw_central: f64,
    nw_upper: f64,
    nw_lower: f64,
    ema_fast: f64,
    ema_slow: f64,
    atr_1h: f64,
    ker_1h: f64,
    ema_slope_pct: f64,
    is_breakout_mode: bool,
}

struct Mtf1hEngine {
    nw: NadarayaWatson,
    ema_fast: Ema,
    ema_slow: Ema,
    atr: Atr,
    vol_sma: VolumeSma,
    current_chunk: Vec<Candle>,
    history_1h_closes: Vec<f64>,
    history_ema_slow: Vec<f64>,
    last_state: Option<MacroState1h>,
}

impl Mtf1hEngine {
    pub fn new() -> Self {
        Self {
            nw: NadarayaWatson::new(5.0, 2.2, 48), // 48 horas de lookback
            ema_fast: Ema::new(20),                // 20 horas
            ema_slow: Ema::new(50),                // 50 horas
            atr: Atr::new(14),
            vol_sma: VolumeSma::new(20),
            current_chunk: Vec::with_capacity(12),
            history_1h_closes: Vec::with_capacity(200),
            history_ema_slow: Vec::with_capacity(200),
            last_state: None,
        }
    }

    /// Recebe cada candle de 5m. Quando completa 12 velas de 5m, fecha a vela de 1h
    /// e recalcula o estado macro de forma estritamente causal (zero lookahead).
    pub fn process_5m(&mut self, c_5m: &Candle) -> Option<MacroState1h> {
        self.current_chunk.push(c_5m.clone());

        if self.current_chunk.len() == 12 {
            let open = self.current_chunk[0].open;
            let high = self.current_chunk.iter().map(|k| k.high).fold(f64::NEG_INFINITY, f64::max);
            let low = self.current_chunk.iter().map(|k| k.low).fold(f64::INFINITY, f64::min);
            let close = self.current_chunk[11].close;
            let volume = self.current_chunk.iter().map(|k| k.volume).sum();
            self.current_chunk.clear();

            let atr_val = self.atr.update(high, low, close).unwrap_or(close * 0.015);
            let (nc, nu, nl, _ns) = self.nw.update_with_atr(close, atr_val);
            let e_fast = self.ema_fast.update(close).unwrap_or(close);
            let e_slow = self.ema_slow.update(close).unwrap_or(close);
            let _ = self.vol_sma.update(volume);

            self.history_1h_closes.push(close);
            self.history_ema_slow.push(e_slow);
            if self.history_1h_closes.len() > 100 {
                self.history_1h_closes.remove(0);
                self.history_ema_slow.remove(0);
            }

            // 1. Kaufman Efficiency Ratio de 1h (últimas 12 horas):
            // KER = Deslocamento líquido / Caminho total percorrido
            let n_h = self.history_1h_closes.len();
            let ker_window = 12.min(n_h);
            let ker_1h = if ker_window >= 2 {
                let slice = &self.history_1h_closes[n_h - ker_window..];
                let net = (slice.last().unwrap() - slice.first().unwrap()).abs();
                let mut path = 0.0;
                for i in 1..slice.len() {
                    path += (slice[i] - slice[i - 1]).abs();
                }
                if path > 1e-9 { (net / path).clamp(0.0, 1.0) } else { 0.0 }
            } else {
                0.0
            };

            // 2. Inclinação (Slope) da EMA 50 de 1h nas últimas 6 horas:
            let slope_window = 6.min(self.history_ema_slow.len());
            let ema_slope_pct = if slope_window >= 2 {
                let s_first = self.history_ema_slow[self.history_ema_slow.len() - slope_window];
                let s_last = *self.history_ema_slow.last().unwrap();
                if s_first > 0.0 { ((s_last - s_first) / s_first) * 100.0 } else { 0.0 }
            } else {
                0.0
            };

            // 3. Classificação de Regime Macro:
            // Breakout Mode: Se KER > 0.50 E a inclinação da EMA for forte (|slope| > 0.12%)
            // Cíclico / Range: KER <= 0.50 (preço oscila e volta para a média)
            let is_breakout_mode = ker_1h > 0.50 && ema_slope_pct.abs() > 0.12;

            let bias = if close >= e_slow && e_fast >= e_slow {
                MacroBias::Bullish
            } else if close <= e_slow && e_fast <= e_slow {
                MacroBias::Bearish
            } else if close > nc {
                MacroBias::Bullish
            } else if close < nc {
                MacroBias::Bearish
            } else {
                MacroBias::Neutral
            };

            let zone = if close >= nu * 0.995 || high >= nu {
                MacroZone::Overbought
            } else if close <= nl * 1.005 || low <= nl {
                MacroZone::Oversold
            } else {
                MacroZone::Equilibrium
            };

            let state = MacroState1h {
                bias,
                zone,
                nw_central: nc,
                nw_upper: nu,
                nw_lower: nl,
                ema_fast: e_fast,
                ema_slow: e_slow,
                atr_1h: atr_val,
                ker_1h,
                ema_slope_pct,
                is_breakout_mode,
            };
            self.last_state = Some(state);
        }

        self.last_state.clone()
    }
}

async fn fetch_cached_candles(
    client: &reqwest::Client,
    symbol: &str,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<Candle>> {
    let cache_dir = std::path::Path::new("data");
    let cache_file = cache_dir.join(format!("cache_{}_{}_{}.json", symbol, start_time, end_time));
    if cache_file.exists() {
        if let Ok(data) = std::fs::read_to_string(&cache_file) {
            if let Ok(cached) = serde_json::from_str::<Vec<Candle>>(&data) {
                if !cached.is_empty() {
                    return Ok(cached);
                }
            }
        }
    }

    let mut current_start = start_time;
    let mut candles = Vec::new();

    while current_start < end_time {
        let url = format!(
            "https://fapi.binance.com/fapi/v1/klines?symbol={}&interval=5m&startTime={}&endTime={}&limit=1000",
            symbol, current_start, end_time
        );
        let resp = client
            .get(&url)
            .header("User-Agent", "Mozilla/5.0")
            .send()
            .await?;
        let raw: Vec<Vec<Value>> = resp.json().await?;
        if raw.is_empty() {
            break;
        }

        let mut last_close = current_start;
        for item in raw {
            if item.len() < 7 {
                continue;
            }
            let open_time = item[0].as_i64().unwrap_or(0);
            let open = item[1].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
            let high = item[2].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
            let low = item[3].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
            let close = item[4].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
            let volume = item[5].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
            let close_time = item[6].as_i64().unwrap_or(0);
            last_close = close_time;

            candles.push(Candle {
                open_time,
                open,
                high,
                low,
                close,
                volume,
                close_time,
                is_closed: true,
            });
        }

        if last_close <= current_start {
            break;
        }
        current_start = last_close + 1;
        tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;
    }

    let _ = std::fs::create_dir_all(&cache_dir);
    if let Ok(serialized) = serde_json::to_string(&candles) {
        let _ = std::fs::write(&cache_file, serialized);
    }

    Ok(candles)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum MtfMode {
    ExtremesOnly,     // SÓ opera nos extremos de 1h (Oversold Long / Overbought Short)
    TrendPullback,    // SÓ opera a favor da tendência de 1h em retrações na VWAP de 5m
    BalancedConfluent,// Opera ambos com confirmação estrita
}

#[derive(Debug, Clone)]
struct MtfConfig {
    mode: MtfMode,
    sl_percent: f64,         // Ex: 1.15%
    tp_partial_percent: f64, // Ex: 2.60% (70%)
    tp_final_percent: f64,   // Ex: 6.20% (30% runner)
    be_offset_percent: f64,  // Ex: 0.25% (lucro garantido pós-taxas)
    partial_ratio: f64,      // 0.70 (70%)
    cooldown_5m_candles: usize, // Ex: 4 velas (20 minutos)
    min_wick_ratio: f64,     // 0.20
    volume_mult: f64,        // 0.85
}

fn simulate_mtf(
    period_name: &str,
    candles_5m: &[Candle],
    cfg: MtfConfig,
    initial_eur: f64,
    leverage: u32,
    eur_to_usdt: f64,
    warmup_candles: usize,
) -> BacktestResult {
    let mut macro_1h = Mtf1hEngine::new();
    let mut vwap_5m = SessionVwap::new();
    let mut nw_5m = NadarayaWatson::new(8.0, 2.2, 150);
    let mut vol_sma_5m = VolumeSma::new(20);
    let mut atr_5m = Atr::new(14);
    let mut kinetic = MeraKineticTensor::new(576);

    let ple_cfg = LiquidityTrapConfig {
        enabled: true,
        bandwidth: 8.0,
        mult: 2.2,
        lookback: 150,
        min_nw_spread: 0.0030,
        volume_mult: cfg.volume_mult,
        min_wick_ratio: cfg.min_wick_ratio,
        sl_offset_pct: 0.15,
        max_sl_pct: cfg.sl_percent,
        tp_center_ratio: 1.0,
        tp_partial_ratio: cfg.partial_ratio,
        tp_runner_mult: 1.85,
        be_offset_pct: cfg.be_offset_percent,
        min_target_pct: 0.40,
        use_trend_filter: true,
        cooldown_candles: cfg.cooldown_5m_candles,
        max_trades_24h: 16,
        enable_fade_traps: true,
        enable_retest_trend: false,
    };
    let mut ple = PhantomLiquidityEngine::new(ple_cfg);

    let mut balance_eur = initial_eur;
    let mut peak_eur = initial_eur;
    let mut max_drawdown_pct = 0.0;
    let mut trades = Vec::new();
    let mut current_pos = Position::new("ETHUSDT".to_string(), leverage);

    let fee_rate = 0.0005; // 0.05% Taker fee
    let slip_rate = 0.0005; // 0.05% Slippage

    let mut tp_partials = 0;
    let mut tp_finals = 0;
    let mut breakevens = 0;
    let mut stop_losses = 0;
    let mut entry_reason = String::new();
    let mut cooldown_remaining = 0;
    let mut consecutive_losses = 0;

    for (idx, candle) in candles_5m.iter().enumerate() {
        let macro_state = macro_1h.process_5m(candle);
        let (vw, _, _) = vwap_5m.update(candle);
        let atr_v = atr_5m.update(candle.high, candle.low, candle.close).unwrap_or(candle.close * 0.005);
        let (nc_5m, nu_5m, nl_5m, _ns_5m) = nw_5m.update_with_atr(candle.close, atr_v);
        let vsma = vol_sma_5m.update(candle.volume);
        let k_out = kinetic.update(candle, vw);
        let (_ple_telem, ple_sig_opt) = ple.update(candle, &current_pos);

        if idx < warmup_candles {
            continue;
        }

        let is_in_pos = current_pos.side != PositionSide::None && current_pos.amount > 0.0;
        let pos_entry = current_pos.entry_price;

        // 1. Gestão Intrabar da Posição Aberta
        if is_in_pos {
            if current_pos.side == PositionSide::Long {
                let tp1 = current_pos.tp_partial.unwrap_or(pos_entry * (1.0 + cfg.tp_partial_percent / 100.0));
                let tp2 = current_pos.take_profit.unwrap_or(pos_entry * (1.0 + cfg.tp_final_percent / 100.0));

                // A. TP Parcial 70%
                if !current_pos.partial_taken && candle.high >= tp1 {
                    let fill_price = tp1 * (1.0 - slip_rate);
                    let ratio = cfg.partial_ratio;
                    let mut close_qty = (current_pos.initial_amount * ratio * 1000.0).floor() / 1000.0;
                    close_qty = close_qty.min(current_pos.amount);
                    if close_qty >= 0.001 {
                        let gross_usdt = (fill_price - pos_entry) * close_qty;
                        let fee_usdt = (close_qty * fill_price) * fee_rate;
                        let net_usdt = gross_usdt - fee_usdt;
                        balance_eur += net_usdt / eur_to_usdt;

                        current_pos.amount = ((current_pos.amount - close_qty) * 1000.0).round() / 1000.0;
                        current_pos.partial_taken = true;
                        current_pos.be_locked = true;
                        let be_stop = pos_entry * (1.0 + cfg.be_offset_percent / 100.0);
                        current_pos.stop_loss = Some(be_stop);
                        tp_partials += 1;
                        consecutive_losses = 0;

                        trades.push(TradeLog {
                            timestamp: candle.close_time,
                            side: format!("LONG (PARCIAL {:.0}%)", ratio * 100.0),
                            entry_price: pos_entry,
                            exit_price: fill_price,
                            quantity: close_qty,
                            gross_pnl_eur: gross_usdt / eur_to_usdt,
                            fee_eur: fee_usdt / eur_to_usdt,
                            net_pnl_eur: net_usdt / eur_to_usdt,
                            return_pct: (gross_usdt / (close_qty * pos_entry)) * 100.0 * (leverage as f64),
                            balance_after_eur: balance_eur,
                            reason: format!("MTF TP1 Parcial (+{:.2}%)", cfg.tp_partial_percent),
                        });
                    }
                }

                // B. TP Final Runner 30%
                if candle.high >= tp2 && current_pos.amount >= 0.001 {
                    let fill_price = tp2 * (1.0 - slip_rate);
                    let gross_usdt = (fill_price - pos_entry) * current_pos.amount;
                    let fee_usdt = (current_pos.amount * fill_price) * fee_rate;
                    let net_usdt = gross_usdt - fee_usdt;
                    balance_eur += net_usdt / eur_to_usdt;
                    tp_finals += 1;
                    cooldown_remaining = cfg.cooldown_5m_candles;

                    trades.push(TradeLog {
                        timestamp: candle.close_time,
                        side: "LONG (FECHAMENTO TP2)".to_string(),
                        entry_price: pos_entry,
                        exit_price: fill_price,
                        quantity: current_pos.amount,
                        gross_pnl_eur: gross_usdt / eur_to_usdt,
                        fee_eur: fee_usdt / eur_to_usdt,
                        net_pnl_eur: net_usdt / eur_to_usdt,
                        return_pct: (gross_usdt / (current_pos.amount * pos_entry)) * 100.0 * (leverage as f64),
                        balance_after_eur: balance_eur,
                        reason: format!("[{}] -> [MTF TP2 Runner (+{:.2}%)]", entry_reason, cfg.tp_final_percent),
                    });
                    current_pos = Position::new("ETHUSDT".to_string(), leverage);
                }

                // C. Stop Loss / Breakeven
                let stop = if current_pos.be_locked || current_pos.partial_taken {
                    let be_floor = pos_entry * (1.0 + cfg.be_offset_percent / 100.0);
                    current_pos.stop_loss.map(|s| s.max(be_floor)).unwrap_or(be_floor)
                } else {
                    current_pos.stop_loss.unwrap_or(pos_entry * (1.0 - cfg.sl_percent / 100.0))
                };

                if current_pos.amount >= 0.001 && candle.low <= stop {
                    let fill_price = stop * (1.0 - slip_rate);
                    let gross_usdt = (fill_price - pos_entry) * current_pos.amount;
                    let fee_usdt = (current_pos.amount * fill_price) * fee_rate;
                    let net_usdt = gross_usdt - fee_usdt;
                    balance_eur += net_usdt / eur_to_usdt;

                    let is_be = current_pos.be_locked || current_pos.partial_taken;
                    if is_be {
                        breakevens += 1;
                        consecutive_losses = 0;
                        cooldown_remaining = cfg.cooldown_5m_candles;
                    } else {
                        stop_losses += 1;
                        consecutive_losses += 1;
                        cooldown_remaining = match consecutive_losses {
                            1 => cfg.cooldown_5m_candles,
                            2 => cfg.cooldown_5m_candles * 3, // 1 hora de pausa após 2 stops seguidos
                            _ => cfg.cooldown_5m_candles * 6, // 2 horas de pausa após 3 stops seguidos
                        };
                    }

                    trades.push(TradeLog {
                        timestamp: candle.close_time,
                        side: if is_be { "LONG (BREAKEVEN)".to_string() } else { "LONG (STOP LOSS)".to_string() },
                        entry_price: pos_entry,
                        exit_price: fill_price,
                        quantity: current_pos.amount,
                        gross_pnl_eur: gross_usdt / eur_to_usdt,
                        fee_eur: fee_usdt / eur_to_usdt,
                        net_pnl_eur: net_usdt / eur_to_usdt,
                        return_pct: (gross_usdt / (current_pos.amount * pos_entry)) * 100.0 * (leverage as f64),
                        balance_after_eur: balance_eur,
                        reason: format!("[{}] -> [{}]", entry_reason, if is_be { "Breakeven Seguro" } else { "Stop Loss Cirúrgico" }),
                    });
                    current_pos = Position::new("ETHUSDT".to_string(), leverage);
                }
            } else if current_pos.side == PositionSide::Short {
                let tp1 = current_pos.tp_partial.unwrap_or(pos_entry * (1.0 - cfg.tp_partial_percent / 100.0));
                let tp2 = current_pos.take_profit.unwrap_or(pos_entry * (1.0 - cfg.tp_final_percent / 100.0));

                // A. TP Parcial 70%
                if !current_pos.partial_taken && candle.low <= tp1 {
                    let fill_price = tp1 * (1.0 + slip_rate);
                    let ratio = cfg.partial_ratio;
                    let mut close_qty = (current_pos.initial_amount * ratio * 1000.0).floor() / 1000.0;
                    close_qty = close_qty.min(current_pos.amount);
                    if close_qty >= 0.001 {
                        let gross_usdt = (pos_entry - fill_price) * close_qty;
                        let fee_usdt = (close_qty * fill_price) * fee_rate;
                        let net_usdt = gross_usdt - fee_usdt;
                        balance_eur += net_usdt / eur_to_usdt;

                        current_pos.amount = ((current_pos.amount - close_qty) * 1000.0).round() / 1000.0;
                        current_pos.partial_taken = true;
                        current_pos.be_locked = true;
                        let be_stop = pos_entry * (1.0 - cfg.be_offset_percent / 100.0);
                        current_pos.stop_loss = Some(be_stop);
                        tp_partials += 1;
                        consecutive_losses = 0;

                        trades.push(TradeLog {
                            timestamp: candle.close_time,
                            side: format!("SHORT (PARCIAL {:.0}%)", ratio * 100.0),
                            entry_price: pos_entry,
                            exit_price: fill_price,
                            quantity: close_qty,
                            gross_pnl_eur: gross_usdt / eur_to_usdt,
                            fee_eur: fee_usdt / eur_to_usdt,
                            net_pnl_eur: net_usdt / eur_to_usdt,
                            return_pct: (gross_usdt / (close_qty * pos_entry)) * 100.0 * (leverage as f64),
                            balance_after_eur: balance_eur,
                            reason: format!("MTF TP1 Parcial (+{:.2}%)", cfg.tp_partial_percent),
                        });
                    }
                }

                // B. TP Final Runner 30%
                if candle.low <= tp2 && current_pos.amount >= 0.001 {
                    let fill_price = tp2 * (1.0 + slip_rate);
                    let gross_usdt = (pos_entry - fill_price) * current_pos.amount;
                    let fee_usdt = (current_pos.amount * fill_price) * fee_rate;
                    let net_usdt = gross_usdt - fee_usdt;
                    balance_eur += net_usdt / eur_to_usdt;
                    tp_finals += 1;
                    cooldown_remaining = cfg.cooldown_5m_candles;

                    trades.push(TradeLog {
                        timestamp: candle.close_time,
                        side: "SHORT (FECHAMENTO TP2)".to_string(),
                        entry_price: pos_entry,
                        exit_price: fill_price,
                        quantity: current_pos.amount,
                        gross_pnl_eur: gross_usdt / eur_to_usdt,
                        fee_eur: fee_usdt / eur_to_usdt,
                        net_pnl_eur: net_usdt / eur_to_usdt,
                        return_pct: (gross_usdt / (current_pos.amount * pos_entry)) * 100.0 * (leverage as f64),
                        balance_after_eur: balance_eur,
                        reason: format!("[{}] -> [MTF TP2 Runner (+{:.2}%)]", entry_reason, cfg.tp_final_percent),
                    });
                    current_pos = Position::new("ETHUSDT".to_string(), leverage);
                }

                // C. Stop Loss / Breakeven
                let stop = if current_pos.be_locked || current_pos.partial_taken {
                    let be_floor = pos_entry * (1.0 - cfg.be_offset_percent / 100.0);
                    current_pos.stop_loss.map(|s| s.min(be_floor)).unwrap_or(be_floor)
                } else {
                    current_pos.stop_loss.unwrap_or(pos_entry * (1.0 + cfg.sl_percent / 100.0))
                };

                if current_pos.amount >= 0.001 && candle.high >= stop {
                    let fill_price = stop * (1.0 + slip_rate);
                    let gross_usdt = (pos_entry - fill_price) * current_pos.amount;
                    let fee_usdt = (current_pos.amount * fill_price) * fee_rate;
                    let net_usdt = gross_usdt - fee_usdt;
                    balance_eur += net_usdt / eur_to_usdt;

                    let is_be = current_pos.be_locked || current_pos.partial_taken;
                    if is_be {
                        breakevens += 1;
                        consecutive_losses = 0;
                        cooldown_remaining = cfg.cooldown_5m_candles;
                    } else {
                        stop_losses += 1;
                        consecutive_losses += 1;
                        cooldown_remaining = match consecutive_losses {
                            1 => cfg.cooldown_5m_candles,
                            2 => cfg.cooldown_5m_candles * 3, // 1 hora de pausa após 2 stops seguidos
                            _ => cfg.cooldown_5m_candles * 6, // 2 horas de pausa após 3 stops seguidos
                        };
                    }

                    trades.push(TradeLog {
                        timestamp: candle.close_time,
                        side: if is_be { "SHORT (BREAKEVEN)".to_string() } else { "SHORT (STOP LOSS)".to_string() },
                        entry_price: pos_entry,
                        exit_price: fill_price,
                        quantity: current_pos.amount,
                        gross_pnl_eur: gross_usdt / eur_to_usdt,
                        fee_eur: fee_usdt / eur_to_usdt,
                        net_pnl_eur: net_usdt / eur_to_usdt,
                        return_pct: (gross_usdt / (current_pos.amount * pos_entry)) * 100.0 * (leverage as f64),
                        balance_after_eur: balance_eur,
                        reason: format!("[{}] -> [{}]", entry_reason, if is_be { "Breakeven Seguro" } else { "Stop Loss Cirúrgico" }),
                    });
                    current_pos = Position::new("ETHUSDT".to_string(), leverage);
                }
            }
        }

        // 2. Avaliação de Novos Gatilhos de Entrada em 5m
        if cooldown_remaining > 0 {
            cooldown_remaining -= 1;
            continue;
        }

        if current_pos.side != PositionSide::None {
            continue;
        }

        // Requer estado macro de 1h aquecido
        let m_state = match macro_state {
            Some(ref s) => s,
            None => continue,
        };

        let candle_range = (candle.high - candle.low).max(0.01);
        let body_top = candle.close.max(candle.open);
        let body_bottom = candle.close.min(candle.open);
        let upper_wick = candle.high - body_top;
        let lower_wick = body_bottom - candle.low;
        let upper_wick_ratio = upper_wick / candle_range;
        let lower_wick_ratio = lower_wick / candle_range;
        let is_vol_ok = vsma > 0.0 && candle.volume >= vsma * cfg.volume_mult;

        // --- FILTRO DE ENTRADA MULTI-TIMEFRAME (CONFORME MtfMode) ---
        let (macro_allows_long, macro_allows_short) = match cfg.mode {
            MtfMode::ExtremesOnly => {
                // SÓ entra se 1h estiver tocando as bandas externas de liquidez (Armadilhas Reais)
                let l = m_state.zone == MacroZone::Oversold;
                let s = m_state.zone == MacroZone::Overbought;
                (l, s)
            }
            MtfMode::TrendPullback => {
                // SÓ entra a favor da tendência de 1h quando não estiver esticado
                let l = m_state.bias == MacroBias::Bullish && m_state.zone != MacroZone::Overbought;
                let s = m_state.bias == MacroBias::Bearish && m_state.zone != MacroZone::Oversold;
                (l, s)
            }
            MtfMode::BalancedConfluent => {
                let l = (m_state.bias == MacroBias::Bullish && m_state.zone != MacroZone::Overbought)
                    || (m_state.zone == MacroZone::Oversold && m_state.bias != MacroBias::Bearish);
                let s = (m_state.bias == MacroBias::Bearish && m_state.zone != MacroZone::Oversold)
                    || (m_state.zone == MacroZone::Overbought && m_state.bias != MacroBias::Bullish);
                (l, s)
            }
        };

        // --- CHAVEAMENTO DE REGIME ADAPTATIVO BIMODAL ---
        // Se m_state.is_breakout_mode for verdadeiro (Ruptura Explosiva em 1h, como em Outubro):
        // -> Ativa o Escudo Cinético MKT 3.0: Veta compra em sangria e bloqueia Long contra EMA inclinada para baixo
        // Se m_state.is_breakout_mode for falso (Regime Cíclico/Range, como em Agosto e Setembro):
        // -> Dá rédea solta ao PLE nos extremos de 1h para capturar as reversões à média!
        let mkt_veto_long = if m_state.is_breakout_mode {
            // Em breakout de 1h para baixo: NUNCA compra fundo!
            m_state.ema_slope_pct < -0.10 || k_out.tensor < -0.040 || k_out.is_volume_climax || !k_out.is_long_allowed
        } else {
            // Em range cíclico de 1h: Veta apenas se houver clímax de volume extremo
            k_out.is_volume_climax && k_out.tensor < -0.080
        };

        let mkt_veto_short = if m_state.is_breakout_mode {
            // Em breakout de 1h para cima: NUNCA vende topo!
            m_state.ema_slope_pct > 0.10 || k_out.tensor > 0.040 || k_out.is_volume_climax || !k_out.is_short_allowed
        } else {
            // Em range cíclico de 1h: Veta apenas se houver clímax de volume extremo
            k_out.is_volume_climax && k_out.tensor > 0.080
        };

        // A. Gatilho Long com Reentrada Confirmada (Bear Trap Fade):
        // 1. Violou a banda inferior (low <= nl_5m), mas fecha DENTRO dela (close >= nl_5m)
        // 2. Vela verde (close >= open) com pavio de rejeição inferior significativo
        // 3. Veto MKT 3.0 inativo
        let setup_5m_long = candle.low <= nl_5m
            && candle.close >= nl_5m
            && candle.close >= candle.open
            && lower_wick_ratio >= cfg.min_wick_ratio;

        if setup_5m_long && macro_allows_long && is_vol_ok && !mkt_veto_long {
            let fill_price = candle.close * (1.0 + slip_rate);
            let balance_usdt = balance_eur * eur_to_usdt;
            // Dimensionamento de Risco: Arriscar exatamente 2.0% da banca no Stop Loss
            let risk_eur = balance_eur * 0.020;
            let target_notional = ((risk_eur * eur_to_usdt) / (cfg.sl_percent / 100.0))
                .min(balance_usdt * (leverage as f64) * 0.90)
                .min(800.0);
            let mut qty = target_notional / fill_price;
            qty = (qty * 1000.0).floor() / 1000.0;

            if qty >= 0.001 {
                let open_fee_usdt = (qty * fill_price) * fee_rate;
                balance_eur -= open_fee_usdt / eur_to_usdt;

                current_pos.side = PositionSide::Long;
                current_pos.entry_price = fill_price;
                current_pos.amount = qty;
                current_pos.initial_amount = qty;
                current_pos.stop_loss = Some(fill_price * (1.0 - cfg.sl_percent / 100.0));
                current_pos.take_profit = Some(fill_price * (1.0 + cfg.tp_final_percent / 100.0));
                current_pos.tp_partial = Some(fill_price * (1.0 + cfg.tp_partial_percent / 100.0));
                current_pos.partial_taken = false;
                current_pos.be_locked = false;

                entry_reason = format!(
                    "MTF Long [1h: {:?}/{:?}] [MKT T:{:+.2}] Bear Trap Reentrada (Wick {:.0}%)",
                    m_state.bias, m_state.zone, k_out.tensor, lower_wick_ratio * 100.0
                );
                continue;
            }
        }

        // B. Gatilho Short com Reentrada Confirmada (Bull Trap Fade):
        // 1. Violou a banda superior (high >= nu_5m), mas fecha DENTRO dela (close <= nu_5m)
        // 2. Vela vermelha (close <= open) com pavio de rejeição superior significativo
        // 3. Veto MKT 3.0 inativo (sem explosão compradora nem clímax de volume)
        let setup_5m_short = candle.high >= nu_5m
            && candle.close <= nu_5m
            && candle.close <= candle.open
            && upper_wick_ratio >= cfg.min_wick_ratio;

        if setup_5m_short && macro_allows_short && is_vol_ok && !mkt_veto_short {
            let fill_price = candle.close * (1.0 - slip_rate);
            let balance_usdt = balance_eur * eur_to_usdt;
            // Dimensionamento de Risco: Arriscar exatamente 2.0% da banca no Stop Loss
            let risk_eur = balance_eur * 0.020;
            let target_notional = ((risk_eur * eur_to_usdt) / (cfg.sl_percent / 100.0))
                .min(balance_usdt * (leverage as f64) * 0.90)
                .min(800.0);
            let mut qty = target_notional / fill_price;
            qty = (qty * 1000.0).floor() / 1000.0;

            if qty >= 0.001 {
                let open_fee_usdt = (qty * fill_price) * fee_rate;
                balance_eur -= open_fee_usdt / eur_to_usdt;

                current_pos.side = PositionSide::Short;
                current_pos.entry_price = fill_price;
                current_pos.amount = qty;
                current_pos.initial_amount = qty;
                current_pos.stop_loss = Some(fill_price * (1.0 + cfg.sl_percent / 100.0));
                current_pos.take_profit = Some(fill_price * (1.0 - cfg.tp_final_percent / 100.0));
                current_pos.tp_partial = Some(fill_price * (1.0 - cfg.tp_partial_percent / 100.0));
                current_pos.partial_taken = false;
                current_pos.be_locked = false;

                entry_reason = format!(
                    "MTF Short [1h: {:?}/{:?}] Bull Trap Reentrada (Wick {:.0}%)",
                    m_state.bias, m_state.zone, upper_wick_ratio * 100.0
                );
                continue;
            }
        }


        if balance_eur > peak_eur {
            peak_eur = balance_eur;
        }
        let dd = if peak_eur > 0.0 {
            ((peak_eur - balance_eur) / peak_eur) * 100.0
        } else {
            0.0
        };
        if dd > max_drawdown_pct {
            max_drawdown_pct = dd;
        }
    }

    let total_trades = trades.len();
    let wins = trades.iter().filter(|t| t.net_pnl_eur > 0.0).count();
    let losses = trades.iter().filter(|t| t.net_pnl_eur <= 0.0).count();
    let win_rate = if total_trades > 0 {
        (wins as f64 / total_trades as f64) * 100.0
    } else {
        0.0
    };

    let gross_wins: f64 = trades.iter().filter(|t| t.net_pnl_eur > 0.0).map(|t| t.net_pnl_eur).sum();
    let gross_losses: f64 = trades.iter().filter(|t| t.net_pnl_eur < 0.0).map(|t| t.net_pnl_eur.abs()).sum();
    let profit_factor = if gross_losses > 0.0 {
        gross_wins / gross_losses
    } else if gross_wins > 0.0 {
        99.9
    } else {
        0.0
    };

    let profit_eur = balance_eur - initial_eur;
    let total_return_pct = (profit_eur / initial_eur) * 100.0;

    BacktestResult {
        period_name: period_name.to_string(),
        leverage,
        initial_eur,
        final_eur: balance_eur,
        profit_eur,
        total_return_pct,
        total_trades,
        wins,
        losses,
        win_rate,
        max_drawdown_pct,
        profit_factor,
        tp_partials,
        tp_finals,
        breakevens,
        stop_losses,
        trades,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .danger_accept_invalid_certs(true)
        .build()?;

    let symbol = "ETHUSDT";
    let initial_eur = 200.00;
    let leverage = 5;
    let eur_to_usdt = 1.08;
    let warmup_candles_5m = 576; // 48 horas de velas de 5m
    let warmup_offset_ms = (warmup_candles_5m as i64) * 5 * 60 * 1000;

    // 1. Mês de Agosto
    let aug_start = 1785542400000i64;
    let aug_end = 1788220799999i64;
    let aug_candles_5m = fetch_cached_candles(&client, symbol, aug_start - warmup_offset_ms, aug_end).await?;

    // 2. Mês de Setembro
    let sep_start = 1788220800000i64;
    let sep_end = 1790812799999i64;
    let sep_candles_5m = fetch_cached_candles(&client, symbol, sep_start - warmup_offset_ms, sep_end).await?;

    // 3. Mês de Outubro
    let oct_start = 1790812800000i64;
    let oct_end = 1792176684688i64;
    let oct_candles_5m = fetch_cached_candles(&client, symbol, oct_start - warmup_offset_ms, oct_end).await?;

    println!("================================================================================");
    println!("   BACKTEST MULTI-TIMEFRAME (MTF): 1H MACRO BIAS + 5M GATILHO CIRÚRGICO");
    println!("   Frequência Ativa de Trades + Alvos Amplos + Proteção de Tendência");
    println!("================================================================================");
    println!("  PAR:              ETHUSDT Perpétuo (USDⓈ-M MTF)");
    println!("  BANCA BASE:       €{:.2} (${:.2} USDT) em {}x Alavancagem", initial_eur, initial_eur * eur_to_usdt, leverage);
    println!("  CUSTOS REAIS:     Taker Fee 0.05% + Slippage 0.05% por ponta de execução\n");

    let configs = vec![
        ("MTF-1 [EXTREMOS 1H APENAS]: Fade Traps nos Extremos de 1h | SL 1.25% | TP1 2.80% | TP2 6.50%", MtfConfig {
            mode: MtfMode::ExtremesOnly,
            sl_percent: 1.25,
            tp_partial_percent: 2.80,
            tp_final_percent: 6.50,
            be_offset_percent: 0.25,
            partial_ratio: 0.70,
            cooldown_5m_candles: 6,
            min_wick_ratio: 0.20,
            volume_mult: 0.85,
        }),
        ("MTF-2 [TREND PULLBACK]: A Favor da Tendência de 1h em Retrações | SL 1.15% | TP1 2.50% | TP2 5.80%", MtfConfig {
            mode: MtfMode::TrendPullback,
            sl_percent: 1.15,
            tp_partial_percent: 2.50,
            tp_final_percent: 5.80,
            be_offset_percent: 0.25,
            partial_ratio: 0.70,
            cooldown_5m_candles: 4,
            min_wick_ratio: 0.20,
            volume_mult: 0.85,
        }),
        ("MTF-3 [HÍBRIDO CONFLUENTE]: Extremos + Pullback | SL 1.20% | TP1 2.60% | TP2 6.00%", MtfConfig {
            mode: MtfMode::BalancedConfluent,
            sl_percent: 1.20,
            tp_partial_percent: 2.60,
            tp_final_percent: 6.00,
            be_offset_percent: 0.25,
            partial_ratio: 0.70,
            cooldown_5m_candles: 4,
            min_wick_ratio: 0.20,
            volume_mult: 0.85,
        }),
    ];

    for (name, cfg) in configs {
        let r_aug = simulate_mtf("Agosto (MTF)", &aug_candles_5m, cfg.clone(), initial_eur, leverage, eur_to_usdt, warmup_candles_5m);
        let r_sep = simulate_mtf("Setembro (MTF)", &sep_candles_5m, cfg.clone(), initial_eur, leverage, eur_to_usdt, warmup_candles_5m);
        let r_oct = simulate_mtf("Outubro (MTF)", &oct_candles_5m, cfg.clone(), initial_eur, leverage, eur_to_usdt, warmup_candles_5m);

        let tot_trades = r_aug.total_trades + r_sep.total_trades + r_oct.total_trades;
        let tot_wins = r_aug.wins + r_sep.wins + r_oct.wins;
        let tot_losses = r_aug.losses + r_sep.losses + r_oct.losses;
        let tot_pnl = r_aug.profit_eur + r_sep.profit_eur + r_oct.profit_eur;
        let wr = if tot_trades > 0 { (tot_wins as f64 / tot_trades as f64) * 100.0 } else { 0.0 };

        println!("\n>>> {}", name);
        println!("    Agosto:   {:+6.2}€ ({:2} trades | WR {:4.1}% | PF {:4.2})", r_aug.profit_eur, r_aug.total_trades, r_aug.win_rate, r_aug.profit_factor);
        println!("    Setembro: {:+6.2}€ ({:2} trades | WR {:4.1}% | PF {:4.2})", r_sep.profit_eur, r_sep.total_trades, r_sep.win_rate, r_sep.profit_factor);
        println!("    Outubro:  {:+6.2}€ ({:2} trades | WR {:4.1}% | PF {:4.2})", r_oct.profit_eur, r_oct.total_trades, r_oct.win_rate, r_oct.profit_factor);
        println!("    TOTAL:    {:+6.2}€ ({:+5.2}%) | {} trades ({}W/{}L) | WR: {:.1}%", tot_pnl, (tot_pnl/initial_eur)*100.0, tot_trades, tot_wins, tot_losses, wr);

        if tot_pnl > 0.0 || name.contains("MTF-A") {
            println!("    --- Amostra de Trades Executados ---");
            for (i, t) in r_aug.trades.iter().take(3).chain(r_sep.trades.iter().take(3)).enumerate() {
                println!("      #{:02}: {:<22} | ${:<7.2} -> ${:<7.2} | {:+6.2}€ | {}",
                    i+1, t.side, t.entry_price, t.exit_price, t.net_pnl_eur, t.reason
                );
            }
        }
    }

    Ok(())
}
