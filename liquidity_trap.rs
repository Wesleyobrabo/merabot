// language: Rust, file: src/liquidity_trap.rs, target: Windows 11 / Ubuntu VPS, MSVC
// =========================================================================================
//  PHANTOM LIQUIDITY ENGINE (PLE) - Caçador de Armadilhas de Liquidez & Retestes de Banda
//  Desenvolvido do Absoluto Zero para Explorar a Assimetria Estrutural de 5m em Cripto:
//  1. 70% das violações de banda externa (close >= nu / close <= nl) são armadilhas (Bull/Bear Traps).
//     -> O motor caça o fechamento de reentrada e abre FADE (Short no Bull Trap / Long no Bear Trap)
//        com Stop Loss cirúrgico acima da máxima do pavio e Alvo na Regressão Central.
//  2. 30% são tendências verdadeiras.
//     -> O motor NUNCA compra a vela de rompimento; ele espera o pullback de 5m testar a banda
//        como suporte/resistência e entra no FUNDO da retração a favor do fluxo institucional.
// =========================================================================================

use serde::{Deserialize, Serialize};
use tracing::info;
use crate::indicators::{Atr, Ema, MeraKineticTensor, NadarayaWatson, SessionVwap, VolumeSma};
use crate::types::{Candle, Position, PositionSide, Signal, SignalType};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TrapState {
    Idle,
    UpperBreach {
        candles_since: usize,
        peak_high: f64,
        breach_volume: f64,
    },
    LowerBreach {
        candles_since: usize,
        trough_low: f64,
        breach_volume: f64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidityTrapConfig {
    pub enabled: bool,
    pub bandwidth: f64,
    pub mult: f64,
    pub lookback: usize,
    pub min_nw_spread: f64,
    pub volume_mult: f64,
    pub min_wick_ratio: f64,
    pub sl_offset_pct: f64,      // Folga além da máxima/mínima do pavio (ex: 0.12%)
    pub max_sl_pct: f64,         // Teto máximo de stop loss (ex: 0.70%)
    pub tp_center_ratio: f64,    // Alvo TP1 na linha central (1.0 = 100% até a central)
    pub tp_partial_ratio: f64,   // Fração a realizar no TP1 (ex: 0.70 = 70%)
    pub tp_runner_mult: f64,     // Multiplicador da banda oposta para TP Final
    pub be_offset_pct: f64,      // Lucro travado no Breakeven após TP1
    pub min_target_pct: f64,     // Distância mínima até a central para valer a pena o trade (ex: 0.65%)
    pub use_trend_filter: bool,  // Alinha os Fades com a tendência macro (EMA 200)
    pub cooldown_candles: usize,
    pub max_trades_24h: usize,
    pub enable_fade_traps: bool, // 70% das vezes: Fade fakeout (Short Bull Trap / Long Bear Trap)
    pub enable_retest_trend: bool, // 30% das vezes: Comprar o pullback no reteste da banda
}

impl Default for LiquidityTrapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bandwidth: 8.0,
            mult: 2.2,
            lookback: 150,
            min_nw_spread: 0.0030,
            volume_mult: 0.80,
            min_wick_ratio: 0.20,
            sl_offset_pct: 0.15,
            max_sl_pct: 0.90,
            tp_center_ratio: 1.0,
            tp_partial_ratio: 0.70,
            tp_runner_mult: 1.85,
            be_offset_pct: 0.12,
            min_target_pct: 0.35,
            use_trend_filter: true,
            cooldown_candles: 2,
            max_trades_24h: 16,
            enable_fade_traps: true,
            enable_retest_trend: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidityTrapTelemetry {
    pub state_name: String,
    pub session_vwap: f64,
    pub nw_central: f64,
    pub nw_upper: f64,
    pub nw_lower: f64,
    pub nw_spread_pct: f64,
    pub volume_ratio: f64,
    pub candle_wick_upper_pct: f64,
    pub candle_wick_lower_pct: f64,
    pub active_action: String,
    pub active_stop: Option<f64>,
    pub tp_partial: Option<f64>,
    pub tp_final: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct PhantomLiquidityEngine {
    pub config: LiquidityTrapConfig,
    pub nw: NadarayaWatson,
    pub vwap: SessionVwap,
    pub vol_sma: VolumeSma,
    pub atr: Atr,
    pub ema_macro: Ema,
    pub kinetic: MeraKineticTensor,
    pub trap_state: TrapState,
    pub cooldown_remaining: usize,
    pub trade_timestamps: Vec<i64>,
    pub consecutive_losses: usize,
    pub last_telemetry: LiquidityTrapTelemetry,
}

impl PhantomLiquidityEngine {
    pub fn new(config: LiquidityTrapConfig) -> Self {
        let nw = NadarayaWatson::new(config.bandwidth, config.mult, config.lookback);
        let vwap = SessionVwap::new();
        let vol_sma = VolumeSma::new(24);
        let atr = Atr::new(14);
        let ema_macro = Ema::new(200);
        let kinetic = MeraKineticTensor::new(576);

        Self {
            config,
            nw,
            vwap,
            vol_sma,
            atr,
            ema_macro,
            kinetic,
            trap_state: TrapState::Idle,
            cooldown_remaining: 0,
            trade_timestamps: Vec::with_capacity(30),
            consecutive_losses: 0,
            last_telemetry: LiquidityTrapTelemetry {
                state_name: "Inicializando".to_string(),
                session_vwap: 0.0,
                nw_central: 0.0,
                nw_upper: 0.0,
                nw_lower: 0.0,
                nw_spread_pct: 0.0,
                volume_ratio: 1.0,
                candle_wick_upper_pct: 0.0,
                candle_wick_lower_pct: 0.0,
                active_action: "Aguardando velas".to_string(),
                active_stop: None,
                tp_partial: None,
                tp_final: None,
            },
        }
    }

    pub fn warmup(&mut self, candles: &[Candle]) {
        if candles.is_empty() {
            return;
        }
        for c in candles {
            self.vol_sma.update(c.volume);
            let atr_val = self.atr.update(c.high, c.low, c.close).unwrap_or(c.close * 0.005);
            let (vw, _, _) = self.vwap.update(c);
            self.nw.update_with_atr(c.close, atr_val);
            self.ema_macro.update(c.close);
            self.kinetic.update(c, vw);
        }
    }

    pub fn update(&mut self, candle: &Candle, pos: &Position) -> (LiquidityTrapTelemetry, Option<Signal>) {
        let is_in_pos = pos.side != PositionSide::None && pos.amount > 0.0;
        let pos_entry = pos.entry_price;
        let pos_side = pos.side;

        // 1. Atualização dos Cálculos Matemáticos Fundamentais
        let atr_val = self.atr.update(candle.high, candle.low, candle.close).unwrap_or(candle.close * 0.005);
        let (vw, _vu, _vl) = self.vwap.update(candle);
        let (nc, nu, nl, ns) = self.nw.update_with_atr(candle.close, atr_val);
        let vsma = self.vol_sma.update(candle.volume);
        let ema_val = self.ema_macro.update(candle.close).unwrap_or(candle.close);
        let k_out = self.kinetic.update(candle, vw);
        let is_uptrend = candle.close >= ema_val;
        let _is_downtrend = candle.close <= ema_val;
        let vol_ratio = if vsma > 0.0 { candle.volume / vsma } else { 1.0 };
        let is_vol_ok = vol_ratio >= self.config.volume_mult;
        let nw_spread = (nu - nl) / nc.max(1.0);
        let is_spread_ok = nw_spread >= self.config.min_nw_spread;

        let candle_range = (candle.high - candle.low).max(0.01);
        let body_top = candle.close.max(candle.open);
        let body_bottom = candle.close.min(candle.open);
        let upper_wick = candle.high - body_top;
        let lower_wick = body_bottom - candle.low;
        let upper_wick_ratio = upper_wick / candle_range;
        let lower_wick_ratio = lower_wick / candle_range;

        // 2. Gestão Intrabar para Posição em Aberto
        if is_in_pos {
            if pos_side == PositionSide::Long {
                let tp1 = pos.tp_partial.unwrap_or(pos_entry * 1.01);
                let tp2 = pos.take_profit.unwrap_or(pos_entry * 1.02);

                // A. Take Profit Parcial (Retorno à Média ou Reteste)
                if !pos.partial_taken && candle.high >= tp1 {
                    self.consecutive_losses = 0;
                    let be_stop = pos_entry * (1.0 + self.config.be_offset_pct / 100.0);
                    info!("[PLE] TP Parcial {:.0}% atingido para LONG @ {:.2} (BE @ {:.2})", self.config.tp_partial_ratio * 100.0, tp1, be_stop);
                    return (
                        self.build_telem("TP Parcial Long", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, "TP1 Atingido", Some(be_stop), Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::PartialCloseLong,
                            price: tp1,
                            timestamp: candle.close_time,
                            reason: format!("PLE TP Parcial (+{:.2}%)", ((tp1 - pos_entry) / pos_entry) * 100.0),
                            stop_loss: Some(be_stop),
                            take_profit: Some(tp2),
                            tp_partial: None,
                            partial_ratio: Some(self.config.tp_partial_ratio),
                            size_factor: None,
                        }),
                    );
                }

                // B. Take Profit Final
                if candle.high >= tp2 {
                    self.consecutive_losses = 0;
                    self.cooldown_remaining = self.config.cooldown_candles;
                    info!("[PLE] TP Final atingido para LONG @ {:.2}", tp2);
                    return (
                        self.build_telem("TP Final Long", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, "TP Final 100%", None, Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::CloseLong,
                            price: tp2,
                            timestamp: candle.close_time,
                            reason: format!("PLE TP Final (+{:.2}%)", ((tp2 - pos_entry) / pos_entry) * 100.0),
                            stop_loss: None,
                            take_profit: None,
                            tp_partial: None,
                            partial_ratio: None,
                            size_factor: None,
                        }),
                    );
                }

                // C. Stop Loss / Breakeven
                let stop = if pos.be_locked || pos.partial_taken {
                    let be_floor = pos_entry * (1.0 + self.config.be_offset_pct / 100.0);
                    pos.stop_loss.map(|s| s.max(be_floor)).unwrap_or(be_floor)
                } else {
                    pos.stop_loss.unwrap_or(pos_entry * (1.0 - self.config.max_sl_pct / 100.0))
                };

                if candle.low <= stop {
                    let is_be = stop >= pos_entry;
                    if !is_be {
                        self.consecutive_losses += 1;
                        self.cooldown_remaining = if self.consecutive_losses >= 2 { 12 } else { 6 };
                    }
                    let reason = if is_be { "Breakeven Garantido" } else { "Stop Loss" };
                    info!("[PLE] {} para LONG @ {:.2}", reason, stop);
                    return (
                        self.build_telem("Stop Long", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, reason, None, None, None),
                        Some(Signal {
                            signal_type: SignalType::CloseLong,
                            price: stop,
                            timestamp: candle.close_time,
                            reason: format!("PLE {} em {:.2}", reason, stop),
                            stop_loss: None,
                            take_profit: None,
                            tp_partial: None,
                            partial_ratio: None,
                            size_factor: None,
                        }),
                    );
                }
            } else if pos_side == PositionSide::Short {
                let tp1 = pos.tp_partial.unwrap_or(pos_entry * 0.99);
                let tp2 = pos.take_profit.unwrap_or(pos_entry * 0.98);

                // A. Take Profit Parcial (Retorno à Média ou Reteste)
                if !pos.partial_taken && candle.low <= tp1 {
                    self.consecutive_losses = 0;
                    let be_stop = pos_entry * (1.0 - self.config.be_offset_pct / 100.0);
                    info!("[PLE] TP Parcial {:.0}% atingido para SHORT @ {:.2} (BE @ {:.2})", self.config.tp_partial_ratio * 100.0, tp1, be_stop);
                    return (
                        self.build_telem("TP Parcial Short", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, "TP1 Atingido", Some(be_stop), Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::PartialCloseShort,
                            price: tp1,
                            timestamp: candle.close_time,
                            reason: format!("PLE TP Parcial (+{:.2}%)", ((pos_entry - tp1) / pos_entry) * 100.0),
                            stop_loss: Some(be_stop),
                            take_profit: Some(tp2),
                            tp_partial: None,
                            partial_ratio: Some(self.config.tp_partial_ratio),
                            size_factor: None,
                        }),
                    );
                }

                // B. Take Profit Final
                if candle.low <= tp2 {
                    self.consecutive_losses = 0;
                    self.cooldown_remaining = self.config.cooldown_candles;
                    info!("[PLE] TP Final atingido para SHORT @ {:.2}", tp2);
                    return (
                        self.build_telem("TP Final Short", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, "TP Final 100%", None, Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::CloseShort,
                            price: tp2,
                            timestamp: candle.close_time,
                            reason: format!("PLE TP Final (+{:.2}%)", ((pos_entry - tp2) / pos_entry) * 100.0),
                            stop_loss: None,
                            take_profit: None,
                            tp_partial: None,
                            partial_ratio: None,
                            size_factor: None,
                        }),
                    );
                }

                // C. Stop Loss / Breakeven
                let stop = if pos.be_locked || pos.partial_taken {
                    let be_floor = pos_entry * (1.0 - self.config.be_offset_pct / 100.0);
                    pos.stop_loss.map(|s| s.min(be_floor)).unwrap_or(be_floor)
                } else {
                    pos.stop_loss.unwrap_or(pos_entry * (1.0 + self.config.max_sl_pct / 100.0))
                };

                if candle.high >= stop {
                    let is_be = stop <= pos_entry;
                    if !is_be {
                        self.consecutive_losses += 1;
                        self.cooldown_remaining = if self.consecutive_losses >= 2 { 12 } else { 6 };
                    }
                    let reason = if is_be { "Breakeven Garantido" } else { "Stop Loss" };
                    info!("[PLE] {} para SHORT @ {:.2}", reason, stop);
                    return (
                        self.build_telem("Stop Short", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, reason, None, None, None),
                        Some(Signal {
                            signal_type: SignalType::CloseShort,
                            price: stop,
                            timestamp: candle.close_time,
                            reason: format!("PLE {} em {:.2}", reason, stop),
                            stop_loss: None,
                            take_profit: None,
                            tp_partial: None,
                            partial_ratio: None,
                            size_factor: None,
                        }),
                    );
                }
            }
        }

        // 3. Avaliação de Gatilhos Somente no Fechamento da Vela 5m
        if candle.is_closed {
            if self.cooldown_remaining > 0 {
                self.cooldown_remaining -= 1;
            }
        } else {
            let telem = self.build_telem("Aguardando vela", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, "Vela aberta", None, None, None);
            return (telem, None);
        }

        if self.cooldown_remaining > 0 {
            let telem = self.build_telem("Cooldown", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, "Cooldown ativo", None, None, None);
            return (telem, None);
        }

        let now_ms = candle.close_time;
        self.trade_timestamps.retain(|&t| now_ms - t < 86_400_000);
        if self.trade_timestamps.len() >= self.config.max_trades_24h {
            let telem = self.build_telem("Limite 24h", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, "Limite diário atingido", None, None, None);
            return (telem, None);
        }

        // 4. MÁQUINA DE ESTADOS: Detecção de Violação e Armadilha
        let was_upper_breach = candle.high >= nu || candle.close >= nu;
        let was_lower_breach = candle.low <= nl || candle.close <= nl;

        // Atualiza a memória de violação
        self.trap_state = match self.trap_state {
            TrapState::Idle => {
                if was_upper_breach {
                    TrapState::UpperBreach {
                        candles_since: 1,
                        peak_high: candle.high,
                        breach_volume: candle.volume,
                    }
                } else if was_lower_breach {
                    TrapState::LowerBreach {
                        candles_since: 1,
                        trough_low: candle.low,
                        breach_volume: candle.volume,
                    }
                } else {
                    TrapState::Idle
                }
            }
            TrapState::UpperBreach { candles_since, peak_high, breach_volume } => {
                let new_peak = peak_high.max(candle.high);
                if candle.close > nu && candle.close > candle.open && candles_since < 5 {
                    TrapState::UpperBreach { candles_since: candles_since + 1, peak_high: new_peak, breach_volume }
                } else if candles_since >= 5 {
                    TrapState::Idle
                } else {
                    TrapState::UpperBreach { candles_since: candles_since + 1, peak_high: new_peak, breach_volume }
                }
            }
            TrapState::LowerBreach { candles_since, trough_low, breach_volume } => {
                let new_trough = trough_low.min(candle.low);
                if candle.close < nl && candle.close < candle.open && candles_since < 5 {
                    TrapState::LowerBreach { candles_since: candles_since + 1, trough_low: new_trough, breach_volume }
                } else if candles_since >= 5 {
                    TrapState::Idle
                } else {
                    TrapState::LowerBreach { candles_since: candles_since + 1, trough_low: new_trough, breach_volume }
                }
            }
        };

        if !self.config.enabled || !is_spread_ok {
            let telem = self.build_telem("Standby", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, "Filtro spread/volatilidade", None, None, None);
            return (telem, None);
        }

        // =========================================================================================
        //  ESTRATÉGIA 1 (70%): FADE THE BULL TRAP (Vender a Descoberto na Armadilha de Topo)
        //  Condições:
        //  1. O preço violou nu (UpperBreach).
        //  2. A vela atual FECHA DENTRO da banda (close < nu).
        //  3. Apresenta pavio de rejeição superior OU vela vermelha de confirmação (close <= open).
        // =========================================================================================
        if self.config.enable_fade_traps {
            if let TrapState::UpperBreach { peak_high, .. } = self.trap_state {
                let reentered_band = candle.close < nu;
                let rejection_confirmed = candle.close <= candle.open || upper_wick_ratio >= self.config.min_wick_ratio;
                let dist_to_center_pct = ((candle.close - nc) / candle.close) * 100.0;
                let trend_allows_short = (!self.config.use_trend_filter || ns <= 0.02) && candle.close >= ema_val * 0.975;

                if reentered_band && rejection_confirmed && is_vol_ok && trend_allows_short && dist_to_center_pct >= self.config.min_target_pct {
                    self.trap_state = TrapState::Idle; // Consome a armadilha
                    self.trade_timestamps.push(now_ms);

                    // Stop cirúrgico: logo acima da máxima do pico com piso de respiro
                    let sl_price = peak_high * (1.0 + self.config.sl_offset_pct / 100.0);
                    let min_sl = candle.close * 1.0070;
                    let max_sl_price = candle.close * (1.0 + self.config.max_sl_pct / 100.0);
                    let eff_sl = sl_price.max(min_sl).min(max_sl_price);

                    // TP1 Assimétrico: 1.5x o risco do Stop Loss
                    let r_dist = (eff_sl - candle.close).abs() * 1.5;
                    let tp1_price = candle.close - r_dist;
                    // TP2 Runner: 2.7x o risco do Stop Loss
                    let tp2_price = candle.close - r_dist * 1.8;

                    info!(
                        "[PLE FADE BULL TRAP] Entrada SHORT @ {:.2} | Stop Cirúrgico: {:.2} (-{:.2}%) | TP1 Central: {:.2} (+{:.2}%) | TP2: {:.2} (+{:.2}%)",
                        candle.close,
                        eff_sl,
                        ((eff_sl - candle.close) / candle.close) * 100.0,
                        tp1_price,
                        ((candle.close - tp1_price) / candle.close) * 100.0,
                        tp2_price,
                        ((candle.close - tp2_price) / candle.close) * 100.0
                    );

                    let telem = self.build_telem("Fade Bull Trap", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, "Fade Bull Trap SHORT Confirmado", Some(eff_sl), Some(tp1_price), Some(tp2_price));
                    return (
                        telem,
                        Some(Signal {
                            signal_type: SignalType::OpenShort,
                            price: candle.close,
                            timestamp: candle.close_time,
                            reason: format!("PLE Fade Bull Trap: Reentrada de Banda após Pico em {:.2}", peak_high),
                            stop_loss: Some(eff_sl),
                            take_profit: Some(tp2_price),
                            tp_partial: Some(tp1_price),
                            partial_ratio: Some(self.config.tp_partial_ratio),
                            size_factor: Some(1.0),
                        }),
                    );
                }
            }

            // =========================================================================================
            //  ESTRATÉGIA 2 (70%): FADE THE BEAR TRAP (Comprar no Falso Rompimento de Fundo)
            //  Condições:
            //  1. O preço violou nl (LowerBreach).
            //  2. A vela atual FECHA DENTRO da banda (close > nl).
            //  3. Apresenta pavio de rejeição inferior OU vela verde de confirmação (close >= open).
            // =========================================================================================
            if let TrapState::LowerBreach { trough_low, .. } = self.trap_state {
                let reentered_band = candle.close > nl;
                let rejection_confirmed = candle.close >= candle.open || lower_wick_ratio >= self.config.min_wick_ratio;
                let dist_to_center_pct = ((nc - candle.close) / candle.close) * 100.0;
                let trend_allows_long = (!self.config.use_trend_filter || ns >= -0.02) && candle.close <= ema_val * 1.025;

                if reentered_band && rejection_confirmed && is_vol_ok && trend_allows_long && dist_to_center_pct >= self.config.min_target_pct {
                    self.trap_state = TrapState::Idle;
                    self.trade_timestamps.push(now_ms);

                    // Stop cirúrgico: logo abaixo da mínima do falso rompimento com piso de respiro
                    let sl_price = trough_low * (1.0 - self.config.sl_offset_pct / 100.0);
                    let max_sl = candle.close * 0.9930;
                    let min_sl_price = candle.close * (1.0 - self.config.max_sl_pct / 100.0);
                    let eff_sl = sl_price.min(max_sl).max(min_sl_price);

                    // TP1 Assimétrico: 1.5x o risco do Stop Loss
                    let r_dist = (candle.close - eff_sl).abs() * 1.5;
                    let tp1_price = candle.close + r_dist;
                    // TP2 Runner: 2.7x o risco do Stop Loss
                    let tp2_price = candle.close + r_dist * 1.8;

                    info!(
                        "[PLE FADE BEAR TRAP] Entrada LONG @ {:.2} | Stop Cirúrgico: {:.2} (-{:.2}%) | TP1 Central: {:.2} (+{:.2}%) | TP2: {:.2} (+{:.2}%)",
                        candle.close,
                        eff_sl,
                        ((candle.close - eff_sl) / candle.close) * 100.0,
                        tp1_price,
                        ((tp1_price - candle.close) / candle.close) * 100.0,
                        tp2_price,
                        ((tp2_price - candle.close) / candle.close) * 100.0
                    );

                    let telem = self.build_telem("Fade Bear Trap", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, "Fade Bear Trap LONG Confirmado", Some(eff_sl), Some(tp1_price), Some(tp2_price));
                    return (
                        telem,
                        Some(Signal {
                            signal_type: SignalType::OpenLong,
                            price: candle.close,
                            timestamp: candle.close_time,
                            reason: format!("PLE Fade Bear Trap: Reentrada de Banda após Fundo em {:.2}", trough_low),
                            stop_loss: Some(eff_sl),
                            take_profit: Some(tp2_price),
                            tp_partial: Some(tp1_price),
                            partial_ratio: Some(self.config.tp_partial_ratio),
                            size_factor: Some(1.0),
                        }),
                    );
                }
            }
        }

        // =========================================================================================
        //  ESTRATÉGIA 3 (30%): RETESTE INSTITUCIONAL DE TENDÊNCIA (Pullback Breakout)
        //  Regra de Ouro: NUNCA comprar a vela que rompe. Esperar o pullback testar a banda.
        //  1. O preço rompeu nu no candle anterior (UpperBreach com candle_count >= 1).
        //  2. A vela atual recua (low <= nu * 1.002) testando nu como suporte.
        //  3. A vela fecha verde acima de nu (close >= nu e close > open).
        // =========================================================================================
        if self.config.enable_retest_trend {
            if let TrapState::UpperBreach { candles_since, peak_high, .. } = self.trap_state {
                if candles_since >= 1 {
                    let tested_band_as_support = candle.low <= nu * 1.002;
                    let closed_strong = candle.close >= nu && candle.close > candle.open;
                    let trend_allows_long = (!self.config.use_trend_filter || is_uptrend)
                        && k_out.is_long_allowed
                        && !k_out.is_volume_climax
                        && !k_out.is_whipsaw;

                    if tested_band_as_support && closed_strong && is_vol_ok && trend_allows_long && candle.close > vw {
                        self.trap_state = TrapState::Idle;
                        self.trade_timestamps.push(now_ms);

                        // Stop posicionado de forma segura logo abaixo da banda que virou suporte
                        let sl_price = nu * (1.0 - self.config.sl_offset_pct / 100.0);
                        let eff_sl = sl_price.max(candle.close * (1.0 - self.config.max_sl_pct / 100.0));

                        let tp1_price = candle.close * 1.012; // +1.20%
                        let tp2_price = candle.close * 1.030; // +3.00% Runner

                        info!(
                            "[PLE RETEST BREAKOUT LONG] Entrada @ {:.2} (Fundo do Pullback) | Stop: {:.2} | TP1: {:.2} | TP2: {:.2}",
                            candle.close, eff_sl, tp1_price, tp2_price
                        );

                        let telem = self.build_telem("Retest Long", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, "Reteste de Rompimento LONG", Some(eff_sl), Some(tp1_price), Some(tp2_price));
                        return (
                            telem,
                            Some(Signal {
                                signal_type: SignalType::OpenLong,
                                price: candle.close,
                                timestamp: candle.close_time,
                                reason: format!("PLE Retest Long: Suporte Confirmado em {:.2} pós-pico {:.2}", nu, peak_high),
                                stop_loss: Some(eff_sl),
                                take_profit: Some(tp2_price),
                                tp_partial: Some(tp1_price),
                                partial_ratio: Some(self.config.tp_partial_ratio),
                                size_factor: Some(1.0),
                            }),
                        );
                    }
                }
            }
        }

        let telem = self.build_telem("Monitorando", vw, nc, nu, nl, nw_spread, vol_ratio, upper_wick_ratio, lower_wick_ratio, "Caçando armadilhas nas bandas", None, None, None);
        (telem, None)
    }

    fn build_telem(
        &mut self,
        state_name: &str,
        vw: f64,
        nc: f64,
        nu: f64,
        nl: f64,
        nw_spread: f64,
        vol_ratio: f64,
        upper_wick_ratio: f64,
        lower_wick_ratio: f64,
        action: &str,
        stop: Option<f64>,
        tp1: Option<f64>,
        tp2: Option<f64>,
    ) -> LiquidityTrapTelemetry {
        self.last_telemetry = LiquidityTrapTelemetry {
            state_name: state_name.to_string(),
            session_vwap: vw,
            nw_central: nc,
            nw_upper: nu,
            nw_lower: nl,
            nw_spread_pct: nw_spread * 100.0,
            volume_ratio: vol_ratio,
            candle_wick_upper_pct: upper_wick_ratio * 100.0,
            candle_wick_lower_pct: lower_wick_ratio * 100.0,
            active_action: action.to_string(),
            active_stop: stop,
            tp_partial: tp1,
            tp_final: tp2,
        };
        self.last_telemetry.clone()
    }
}
