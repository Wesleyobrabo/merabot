// language: Rust, file: src/lateral_scalper.rs, target: Windows 11 / Linux, MSVC
// =========================================================================================
//  LATERAL COMPRESSION & PERSISTENT FAKEOUT HARVESTER (LCP-HARVESTER)
// =========================================================================================
//  Motor Matemático desenvolvido especificamente para operar em Regimes de Compressão
//  Lateral e Falsos Rompimentos Persistentes onde o fluxo macro NÃO está validado.
//
//  Pilares Analíticos:
//  1. Filtro Macro de Não-Validação:
//     - KER 1h <= 0.35: Ausência de deslocamento líquido direcional (mercado em ruído/faixa).
//     - EMA 50 1h Flat (|slope| <= 0.08%): Sem inclinação direcional macro.
//
//  2. Detecção de Armadilha (Bull/Bear Trap) no 5m:
//     - Varredura de liquidez além da banda de Nadaraya-Watson (1.9x ATR).
//     - Rejeição mecânica por pavio (wick ratio >= 22%).
//     - Fechamento de reentrada dentro do envelope de volatilidade.
//     - Exaustão estocástica: RSI(14) sobrecomprado (> 58) para Short / sobrevendido (< 42) para Long.
//     - Volume filtrado: sem ignição explosiva institucional (entre 0.7x e 2.2x SMA).
//
//  3. Gestão Cirúrgica de Pequenos Lucros (Chop Harvesting):
//     - TP1 (Micro-Scalp 70%): +1.05% (ou retorno ao centro da regressão).
//     - Breakeven Travado: +0.20% acima do preço de entrada, cobrindo taxas e slippage integralmente.
//     - TP2 (Final Runner 30%): +1.75% (extensão até a banda oposta da consolidação).
//     - Stop Loss Cirúrgico: 1.15% (proteção com respiro adequado para ruído de 5m).
// =========================================================================================

use serde::{Deserialize, Serialize};
use crate::indicators::{Atr, Ema, NadarayaWatson, Rsi, VolumeSma};
use crate::types::{Candle, PositionSide, SignalType};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketChopRegime {
    LateralCompression,
    PersistentFakeout,
    ValidatedMacroTrend,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LateralScalpConfig {
    pub enabled: bool,
    /// Limiar de KER macro de 1h (padrão: 0.35)
    pub max_ker_macro: f64,
    /// Limiar do slope da EMA 50 de 1h (padrão: 0.08%)
    pub max_ema_slope_pct: f64,
    /// Bandwidth da regressão de Nadaraya-Watson no 5m (padrão: 5.0)
    pub nw_bandwidth: f64,
    /// Multiplicador de desvio da banda NW 5m (padrão: 1.90)
    pub nw_multiplier: f64,
    /// Lookback de velas para cálculo das bandas 5m (padrão: 48)
    pub nw_lookback: usize,
    /// Proporção mínima de pavio na vela de rejeição (padrão: 0.22 = 22%)
    pub min_wick_ratio: f64,
    /// Limiar RSI sobrevendido para Long (padrão: 42.0)
    pub rsi_oversold: f64,
    /// Limiar RSI sobrecomprado para Short (padrão: 58.0)
    pub rsi_overbought: f64,
    /// Multiplicador de volume mínimo em relação à SMA 20 (padrão: 0.70)
    pub min_volume_mult: f64,
    /// Multiplicador de volume máximo para barrar breakouts verdadeiros (padrão: 2.20)
    pub max_volume_mult: f64,
    /// Take Profit 1 em percentual (padrão: 1.05%)
    pub tp_partial_pct: f64,
    /// Take Profit 2 final em percentual (padrão: 1.75%)
    pub tp_final_pct: f64,
    /// Fração a realizar no TP1 (padrão: 0.70 = 70%)
    pub partial_ratio: f64,
    /// Lucro travado no Breakeven após TP1 (padrão: 0.20%)
    pub be_offset_pct: f64,
    /// Stop Loss em percentual (padrão: 1.15%)
    pub sl_pct: f64,
    /// Máximo de velas de 5m antes do Time-Stop por estagnação (padrão: 12 velas = 60 min)
    pub max_hold_candles: usize,
    /// Cooldown de velas entre operações consecutivas (padrão: 4 velas)
    pub cooldown_candles: usize,
    /// Risco percentual da banca por trade de scalp (padrão: 2.0%)
    pub risk_per_trade_pct: f64,
}

impl Default for LateralScalpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_ker_macro: 0.35,
            max_ema_slope_pct: 0.08,
            nw_bandwidth: 5.0,
            nw_multiplier: 1.90,
            nw_lookback: 48,
            min_wick_ratio: 0.22,
            rsi_oversold: 42.0,
            rsi_overbought: 58.0,
            min_volume_mult: 0.70,
            max_volume_mult: 2.20,
            tp_partial_pct: 1.05,
            tp_final_pct: 1.75,
            partial_ratio: 0.70,
            be_offset_pct: 0.20,
            sl_pct: 1.15,
            max_hold_candles: 12,
            cooldown_candles: 4,
            risk_per_trade_pct: 2.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MacroZone {
    Overbought,
    Oversold,
    Equilibrium,
}

#[derive(Debug, Clone)]
pub struct MacroChopState {
    pub ker_1h: f64,
    pub ema50_slope_pct: f64,
    pub regime: MarketChopRegime,
    pub zone: MacroZone,
    pub is_scalp_eligible: bool,
    pub is_breakout_mode: bool,
}

#[derive(Debug, Clone)]
pub struct LateralScalpSignal {
    pub signal_type: SignalType,
    pub side: PositionSide,
    pub entry_price: f64,
    pub stop_loss: f64,
    pub tp_partial: f64,
    pub tp_final: f64,
    pub max_hold_candles: usize,
    pub reason: String,
    pub nw_central: f64,
    pub nw_band: f64,
    pub wick_ratio: f64,
}

#[derive(Debug, Clone)]
pub struct Macro1hAggregator {
    current_chunk: Vec<Candle>,
    history_1h_closes: Vec<f64>,
    history_ema50: Vec<f64>,
    ema50: Ema,
    nw_1h: NadarayaWatson,
    atr_1h: Atr,
    last_state: Option<MacroChopState>,
}

impl Macro1hAggregator {
    pub fn new() -> Self {
        Self {
            current_chunk: Vec::with_capacity(12),
            history_1h_closes: Vec::with_capacity(200),
            history_ema50: Vec::with_capacity(200),
            ema50: Ema::new(50),
            nw_1h: NadarayaWatson::new(5.0, 2.2, 48),
            atr_1h: Atr::new(14),
            last_state: None,
        }
    }

    pub fn process_5m(&mut self, candle: &Candle, max_ker: f64, max_slope: f64) -> Option<MacroChopState> {
        self.current_chunk.push(candle.clone());

        if self.current_chunk.len() == 12 {
            let high = self.current_chunk.iter().map(|k| k.high).fold(f64::NEG_INFINITY, f64::max);
            let low = self.current_chunk.iter().map(|k| k.low).fold(f64::INFINITY, f64::min);
            let close = self.current_chunk[11].close;
            self.current_chunk.clear();

            let atr_val = self.atr_1h.update(high, low, close).unwrap_or(close * 0.015);
            let (_nc, nu, nl, _ns) = self.nw_1h.update_with_atr(close, atr_val);

            self.history_1h_closes.push(close);
            if self.history_1h_closes.len() > 150 {
                self.history_1h_closes.remove(0);
            }

            let ema_val = self.ema50.update(close).unwrap_or(close);
            self.history_ema50.push(ema_val);
            if self.history_ema50.len() > 150 {
                self.history_ema50.remove(0);
            }

            // Kaufman Efficiency Ratio (KER 24h)
            let ker = if self.history_1h_closes.len() >= 24 {
                let n = self.history_1h_closes.len();
                let slice = &self.history_1h_closes[n - 24..];
                let net_change = (slice[23] - slice[0]).abs();
                let total_path: f64 = slice.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
                if total_path > 0.0 { net_change / total_path } else { 0.0 }
            } else {
                0.25
            };

            // Slope da EMA 50 nas últimas 6 horas
            let ema_slope_pct = if self.history_ema50.len() >= 6 {
                let n = self.history_ema50.len();
                let s_first = self.history_ema50[n - 6];
                let s_last = self.history_ema50[n - 1];
                if s_first > 0.0 { ((s_last - s_first) / s_first) * 100.0 } else { 0.0 }
            } else {
                0.0
            };

            let zone = if close >= nu * 0.995 || high >= nu {
                MacroZone::Overbought
            } else if close <= nl * 1.005 || low <= nl {
                MacroZone::Oversold
            } else {
                MacroZone::Equilibrium
            };

            let is_breakout_mode = ker > 0.45 && ema_slope_pct.abs() > 0.10;
            let is_lateral = ker <= max_ker && ema_slope_pct.abs() <= max_slope;
            let is_fakeout_prone = ker <= (max_ker * 1.15) && ema_slope_pct.abs() <= (max_slope * 1.25);

            let regime = if is_lateral {
                MarketChopRegime::LateralCompression
            } else if is_fakeout_prone {
                MarketChopRegime::PersistentFakeout
            } else {
                MarketChopRegime::ValidatedMacroTrend
            };

            let is_scalp_eligible = regime != MarketChopRegime::ValidatedMacroTrend && zone == MacroZone::Equilibrium;

            let state = MacroChopState {
                ker_1h: ker,
                ema50_slope_pct: ema_slope_pct,
                regime,
                zone,
                is_scalp_eligible,
                is_breakout_mode,
            };

            self.last_state = Some(state);
        }

        self.last_state.clone()
    }
}

#[derive(Debug, Clone)]
pub struct LateralScalpEngine {
    pub config: LateralScalpConfig,
    macro_aggregator: Macro1hAggregator,
    nw_5m: NadarayaWatson,
    atr_5m: Atr,
    rsi_5m: Rsi,
    vol_sma_5m: VolumeSma,
    cooldown_counter: usize,
    last_macro_state: Option<MacroChopState>,
}

impl LateralScalpEngine {
    pub fn new(config: LateralScalpConfig) -> Self {
        let nw = NadarayaWatson::new(config.nw_bandwidth, config.nw_multiplier, config.nw_lookback);
        Self {
            config,
            macro_aggregator: Macro1hAggregator::new(),
            nw_5m: nw,
            atr_5m: Atr::new(14),
            rsi_5m: Rsi::new(14),
            vol_sma_5m: VolumeSma::new(20),
            cooldown_counter: 0,
            last_macro_state: None,
        }
    }

    pub fn process_candle(&mut self, candle: &Candle) -> Option<LateralScalpSignal> {
        if self.cooldown_counter > 0 {
            self.cooldown_counter -= 1;
        }

        if let Some(m_state) = self.macro_aggregator.process_5m(
            candle,
            self.config.max_ker_macro,
            self.config.max_ema_slope_pct,
        ) {
            self.last_macro_state = Some(m_state);
        }

        let atr_val = self.atr_5m.update(candle.high, candle.low, candle.close).unwrap_or(candle.close * 0.008);
        let (nw_central, nw_upper, nw_lower, _nw_slope) = self.nw_5m.update_with_atr(candle.close, atr_val);
        let rsi_val = self.rsi_5m.update(candle.close).unwrap_or(50.0);
        let vol_sma = self.vol_sma_5m.update(candle.volume);

        if !self.config.enabled || self.cooldown_counter > 0 {
            return None;
        }

        let macro_state = self.last_macro_state.as_ref()?;
        if !macro_state.is_scalp_eligible {
            return None;
        }

        let candle_range = (candle.high - candle.low).max(0.01);
        let body_top = candle.close.max(candle.open);
        let body_bottom = candle.close.min(candle.open);
        let upper_wick = candle.high - body_top;
        let lower_wick = body_bottom - candle.low;
        let upper_wick_ratio = upper_wick / candle_range;
        let lower_wick_ratio = lower_wick / candle_range;

        let is_vol_balanced = vol_sma > 0.0
            && candle.volume >= (vol_sma * self.config.min_volume_mult)
            && candle.volume <= (vol_sma * self.config.max_volume_mult);

        if !is_vol_balanced {
            return None;
        }

        // GATILHO LONG: Bear Trap em Compressão Lateral
        let is_bear_trap = candle.low <= nw_lower
            && candle.close >= nw_lower
            && candle.close >= candle.open
            && lower_wick_ratio >= self.config.min_wick_ratio
            && rsi_val <= self.config.rsi_oversold;

        if is_bear_trap {
            let entry_price = candle.close;
            let stop_loss = entry_price * (1.0 - self.config.sl_pct / 100.0);
            let tp_partial = entry_price * (1.0 + self.config.tp_partial_pct / 100.0);
            let tp_final = entry_price * (1.0 + self.config.tp_final_pct / 100.0);

            self.cooldown_counter = self.config.cooldown_candles;

            return Some(LateralScalpSignal {
                signal_type: SignalType::OpenLong,
                side: PositionSide::Long,
                entry_price,
                stop_loss,
                tp_partial,
                tp_final,
                max_hold_candles: self.config.max_hold_candles,
                reason: format!(
                    "Chop Scalp Long [Regime: {:?} | KER 1h: {:.2} | RSI: {:.1}] Bear Trap (Wick {:.0}%)",
                    macro_state.regime, macro_state.ker_1h, rsi_val, lower_wick_ratio * 100.0
                ),
                nw_central,
                nw_band: nw_lower,
                wick_ratio: lower_wick_ratio,
            });
        }

        // GATILHO SHORT: Bull Trap em Compressão Lateral
        let is_bull_trap = candle.high >= nw_upper
            && candle.close <= nw_upper
            && candle.close <= candle.open
            && upper_wick_ratio >= self.config.min_wick_ratio
            && rsi_val >= self.config.rsi_overbought;

        if is_bull_trap {
            let entry_price = candle.close;
            let stop_loss = entry_price * (1.0 + self.config.sl_pct / 100.0);
            let tp_partial = entry_price * (1.0 - self.config.tp_partial_pct / 100.0);
            let tp_final = entry_price * (1.0 - self.config.tp_final_pct / 100.0);

            self.cooldown_counter = self.config.cooldown_candles;

            return Some(LateralScalpSignal {
                signal_type: SignalType::OpenShort,
                side: PositionSide::Short,
                entry_price,
                stop_loss,
                tp_partial,
                tp_final,
                max_hold_candles: self.config.max_hold_candles,
                reason: format!(
                    "Chop Scalp Short [Regime: {:?} | KER 1h: {:.2} | RSI: {:.1}] Bull Trap (Wick {:.0}%)",
                    macro_state.regime, macro_state.ker_1h, rsi_val, upper_wick_ratio * 100.0
                ),
                nw_central,
                nw_band: nw_upper,
                wick_ratio: upper_wick_ratio,
            });
        }

        None
    }
}
