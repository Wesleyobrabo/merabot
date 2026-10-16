// language: Rust, file: src/mera.rs, target: Windows 11, MSVC
// Motor MERA: Scalping 5m Institucional (Session VWAP + Nadaraya-Watson Envelope + Rejeição de Pavio + Parcial Dinâmica)

use crate::config::MeraConfig;
use crate::drift_filter::{SlowDriftConfig, SlowDriftKillSwitch};
use crate::indicators::{Atr, Ema, MeraKineticTensor, NadarayaWatson, Rsi, SessionVwap, VolumeSma};
use crate::lateral_scalper::{Macro1hAggregator, MacroZone};
use crate::liquidity_trap::{LiquidityTrapConfig, PhantomLiquidityEngine};
use crate::types::{Candle, MeraTelemetry, Position, PositionSide, Signal, SignalType};
use tracing::info;

#[derive(Debug, Clone)]
pub struct MeraEngine {
    pub config: MeraConfig,
    pub vwap: SessionVwap,
    pub nw: NadarayaWatson,
    pub vol_sma: VolumeSma,
    pub atr: Atr,
    pub ema_macro: Ema,
    pub last_telemetry: MeraTelemetry,
    pub trade_timestamps: Vec<i64>,
    pub cooldown_remaining: usize,
    pub consecutive_losses: usize,
    pub kinetic: MeraKineticTensor,
    pub ple: PhantomLiquidityEngine,
    pub macro_1h: Macro1hAggregator,
    pub rsi: Rsi,
    pub drift_switch: SlowDriftKillSwitch,
}

impl MeraEngine {
    pub fn new(config: MeraConfig) -> Self {
        let vwap = SessionVwap::new();
        let nw = NadarayaWatson::new(config.bandwidth, config.mult, config.lookback);
        let vol_sma = VolumeSma::new(20);
        let atr = Atr::new(14);
        let ema_macro = Ema::new(config.ema_trend_period);
        let kinetic = MeraKineticTensor::new(576); // Janela macro de 48 horas (576 velas de 5m)
        let macro_1h = Macro1hAggregator::new();
        let rsi = Rsi::new(14);
        let drift_switch = SlowDriftKillSwitch::new(SlowDriftConfig {
            enabled: config.enabled && config.enable_drift_filter,
            slope_window_hours: config.drift_min_hours.round() as usize,
            ..SlowDriftConfig::default()
        });

        let ple_cfg = LiquidityTrapConfig {
            enabled: config.enabled && config.enable_ple,
            bandwidth: config.bandwidth,
            mult: config.mult,
            lookback: config.lookback,
            min_nw_spread: config.min_nw_spread,
            volume_mult: config.volume_mult,
            min_wick_ratio: config.min_wick_ratio,
            sl_offset_pct: 0.15,
            max_sl_pct: config.sl_percent,
            tp_center_ratio: 1.0,
            tp_partial_ratio: config.partial_ratio,
            tp_runner_mult: 1.8,
            be_offset_pct: config.be_offset_percent,
            min_target_pct: 0.50,
            use_trend_filter: config.use_htf_trend,
            cooldown_candles: config.cooldown_candles,
            max_trades_24h: config.max_trades_24h,
            enable_fade_traps: config.ple_fade_traps,
            enable_retest_trend: config.ple_retest_trend,
        };
        let ple = PhantomLiquidityEngine::new(ple_cfg);

        Self {
            config,
            vwap,
            nw,
            vol_sma,
            atr,
            ema_macro,
            kinetic,
            ple,
            macro_1h,
            rsi,
            drift_switch,
            last_telemetry: MeraTelemetry::default(),
            trade_timestamps: Vec::new(),
            cooldown_remaining: 0,
            consecutive_losses: 0,
        }
    }

    pub fn set_config(&mut self, config: MeraConfig) {
        self.nw = NadarayaWatson::new(config.bandwidth, config.mult, config.lookback);
        self.ema_macro = Ema::new(config.ema_trend_period);
        self.ple.config = LiquidityTrapConfig {
            enabled: config.enabled && config.enable_ple,
            bandwidth: config.bandwidth,
            mult: config.mult,
            lookback: config.lookback,
            min_nw_spread: config.min_nw_spread,
            volume_mult: config.volume_mult,
            min_wick_ratio: config.min_wick_ratio,
            sl_offset_pct: 0.15,
            max_sl_pct: config.sl_percent,
            tp_center_ratio: 1.0,
            tp_partial_ratio: config.partial_ratio,
            tp_runner_mult: 1.8,
            be_offset_pct: config.be_offset_percent,
            min_target_pct: 0.50,
            use_trend_filter: config.use_htf_trend,
            cooldown_candles: config.cooldown_candles,
            max_trades_24h: config.max_trades_24h,
            enable_fade_traps: config.ple_fade_traps,
            enable_retest_trend: config.ple_retest_trend,
        };
        self.drift_switch.config.enabled = config.enabled && config.enable_drift_filter;
        self.drift_switch.config.slope_window_hours = config.drift_min_hours.round() as usize;
        self.config = config;
    }

    pub fn warmup(&mut self, candles: &[Candle]) {
        if candles.is_empty() {
            return;
        }

        self.ple.warmup(candles);

        for c in candles {
            self.vol_sma.update(c.volume);
            let atr_val = self.atr.update(c.high, c.low, c.close).unwrap_or(c.close * 0.005);
            let (vw, _, _) = self.vwap.update(c);
            let (_nc, _, _, _) = self.nw.update_with_atr(c.close, atr_val);
            self.ema_macro.update(c.close);
            self.kinetic.update(c, vw);
            self.macro_1h.process_5m(c, 0.35, 0.08);
            self.rsi.update(c.close);
            self.drift_switch.update(c);
        }

        if let Some(last) = candles.last() {
            let atr_val = self.atr.update(last.high, last.low, last.close).unwrap_or(last.close * 0.005);
            let (vw, vu, vl) = self.vwap.update(last);
            let (nc, nu, nl, ns) = self.nw.update_with_atr(last.close, atr_val);
            let vsma = self.vol_sma.update(last.volume);

            self.last_telemetry = MeraTelemetry {
                enabled: self.config.enabled,
                session_vwap: vw,
                vwap_upper: vu,
                vwap_lower: vl,
                nw_central: nc,
                nw_upper: nu,
                nw_lower: nl,
                nw_slope_pct: ns,
                bias_bullish: last.close > vw,
                bias_bearish: last.close < vw,
                volume_ratio: if vsma > 0.0 { last.volume / vsma } else { 1.0 },
                is_volume_ok: last.volume >= vsma * self.config.volume_mult,
                setup_long: false,
                setup_short: false,
                signal_reason: "Aquecimento concluído".to_string(),
                active_stop: None,
                tp_partial: None,
                tp_final: None,
            };

            info!(
                "[MERA MOTOR AQUECIDO] {} candles de 5m processados | VWAP: {:.2} | NW Central: {:.2} | NW Bandas: [{:.2} - {:.2}] | Bias: {}",
                candles.len(),
                vw,
                nc,
                nl,
                nu,
                if last.close > vw { "ALTA (Preço > VWAP)" } else { "BAIXA (Preço < VWAP)" }
            );
        }
    }

    /// Processa o candle e avalia parciais, trailing stops e novos gatilhos de scalp
    pub fn update(&mut self, candle: &Candle, pos: &Position) -> (MeraTelemetry, Option<Signal>) {
        let is_in_pos = pos.side != PositionSide::None && pos.amount > 0.0;
        let pos_entry = pos.entry_price;
        let pos_side = pos.side;

        // 1. Atualiza indicadores
        let atr_val = self.atr.update(candle.high, candle.low, candle.close).unwrap_or(candle.close * 0.005);
        let (vw, vu, vl) = self.vwap.update(candle);
        let (nc, nu, nl, ns) = self.nw.update_with_atr(candle.close, atr_val);
        let vsma = self.vol_sma.update(candle.volume);
        let ema_val = self.ema_macro.update(candle.close).unwrap_or(candle.close);
        let k_out = self.kinetic.update(candle, vw);
        let macro_state = self.macro_1h.process_5m(candle, 0.35, 0.08);
        let rsi_val = self.rsi.update(candle.close).unwrap_or(50.0);
        let drift_telem = self.drift_switch.update(candle);

        let bias_bullish = candle.close > vw;
        let bias_bearish = candle.close < vw;
        let macro_bullish = !self.config.use_htf_trend || candle.close >= ema_val;
        let macro_bearish = !self.config.use_htf_trend || candle.close <= ema_val;
        let vol_ratio = if vsma > 0.0 { candle.volume / vsma } else { 1.0 };
        let is_volume_ok = candle.volume >= vsma * self.config.volume_mult && candle.volume <= vsma * self.config.max_volume_mult;

        // 2. Gerenciamento intrabar se já estiver posicionado
        if is_in_pos {
            let is_chop_trade = pos.tp_partial
                .map(|tp| ((tp - pos_entry).abs() / pos_entry) < 0.020)
                .unwrap_or(false);

            let (eff_tp_partial_pct, eff_tp_final_pct, eff_sl_pct, eff_be_offset_pct, win_cooldown) = if is_chop_trade {
                (
                    self.config.chop_tp_partial_percent,
                    self.config.chop_tp_final_percent,
                    self.config.chop_sl_percent,
                    self.config.chop_be_offset_percent,
                    3,
                )
            } else {
                (
                    self.config.tp_partial_percent,
                    self.config.tp_final_percent,
                    self.config.sl_percent,
                    self.config.be_offset_percent,
                    self.config.cooldown_candles,
                )
            };

            if pos_side == PositionSide::Long {
                let tp1 = pos.tp_partial.unwrap_or(pos_entry * (1.0 + eff_tp_partial_pct / 100.0));
                let tp2 = pos.take_profit.unwrap_or(pos_entry * (1.0 + eff_tp_final_pct / 100.0));

                // A. Take Profit Parcial
                if !pos.partial_taken && candle.high >= tp1 {
                    self.consecutive_losses = 0;
                    let be_stop = pos_entry * (1.0 + eff_be_offset_pct / 100.0);
                    info!("[MERA SCALP] Take Profit Parcial {:.0}% atingido para LONG @ {:.2} (BE travado em {:.2})", self.config.partial_ratio * 100.0, tp1, be_stop);
                    return (
                        self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, "Take Profit Parcial", Some(be_stop), Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::PartialCloseLong,
                            price: tp1,
                            timestamp: candle.close_time,
                            reason: format!("MERA TP Parcial (+{:.2}%)", eff_tp_partial_pct),
                            stop_loss: Some(be_stop),
                            take_profit: Some(tp2),
                            tp_partial: None,
                            partial_ratio: Some(self.config.partial_ratio),
                            size_factor: None,
                        }),
                    );
                }

                // B. Take Profit Final
                if candle.high >= tp2 {
                    self.consecutive_losses = 0;
                    self.cooldown_remaining = win_cooldown;
                    info!("[MERA SCALP] Take Profit Final 100% atingido para LONG @ {:.2}", tp2);
                    return (
                        self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, "Take Profit Final", None, Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::CloseLong,
                            price: tp2,
                            timestamp: candle.close_time,
                            reason: format!("MERA TP Final (+{:.2}%)", eff_tp_final_pct),
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
                    let be_floor = pos_entry * (1.0 + eff_be_offset_pct / 100.0);
                    pos.stop_loss.map(|s| s.max(be_floor)).unwrap_or(be_floor)
                } else {
                    pos.stop_loss.unwrap_or(pos_entry * (1.0 - eff_sl_pct / 100.0))
                };

                if candle.low <= stop {
                    let reason = if pos.be_locked || pos.partial_taken {
                        self.consecutive_losses = 0;
                        self.cooldown_remaining = win_cooldown;
                        "MERA Breakeven Seguro Garantido"
                    } else {
                        self.consecutive_losses += 1;
                        self.cooldown_remaining = match self.consecutive_losses {
                            1 => 6, // 30 min de pausa defensiva após stop no 5m
                            2 => 12, // 1 hora de pausa após 2 stops seguidos
                            _ => 24, // 2 horas de pausa após 3+ stops seguidos
                        };
                        "MERA Stop Loss"
                    };
                    return (
                        self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, reason, Some(stop), Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::CloseLong,
                            price: stop,
                            timestamp: candle.close_time,
                            reason: format!("{} em {:.2}", reason, stop),
                            stop_loss: None,
                            take_profit: None,
                            tp_partial: None,
                            partial_ratio: None,
                            size_factor: None,
                        }),
                    );
                }
            } else if pos_side == PositionSide::Short {
                let tp1 = pos.tp_partial.unwrap_or(pos_entry * (1.0 - eff_tp_partial_pct / 100.0));
                let tp2 = pos.take_profit.unwrap_or(pos_entry * (1.0 - eff_tp_final_pct / 100.0));

                // A. Take Profit Parcial
                if !pos.partial_taken && candle.low <= tp1 {
                    self.consecutive_losses = 0;
                    let be_stop = pos_entry * (1.0 - eff_be_offset_pct / 100.0);
                    info!("[MERA SCALP] Take Profit Parcial {:.0}% atingido para SHORT @ {:.2} (BE travado em {:.2})", self.config.partial_ratio * 100.0, tp1, be_stop);
                    return (
                        self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, "Take Profit Parcial", Some(be_stop), Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::PartialCloseShort,
                            price: tp1,
                            timestamp: candle.close_time,
                            reason: format!("MERA TP Parcial (+{:.2}%)", eff_tp_partial_pct),
                            stop_loss: Some(be_stop),
                            take_profit: Some(tp2),
                            tp_partial: None,
                            partial_ratio: Some(self.config.partial_ratio),
                            size_factor: None,
                        }),
                    );
                }

                // B. Take Profit Final
                if candle.low <= tp2 {
                    self.consecutive_losses = 0;
                    self.cooldown_remaining = win_cooldown;
                    info!("[MERA SCALP] Take Profit Final 100% atingido para SHORT @ {:.2}", tp2);
                    return (
                        self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, "Take Profit Final", None, Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::CloseShort,
                            price: tp2,
                            timestamp: candle.close_time,
                            reason: format!("MERA TP Final (+{:.2}%)", eff_tp_final_pct),
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
                    let be_floor = pos_entry * (1.0 - eff_be_offset_pct / 100.0);
                    pos.stop_loss.map(|s| s.min(be_floor)).unwrap_or(be_floor)
                } else {
                    pos.stop_loss.unwrap_or(pos_entry * (1.0 + eff_sl_pct / 100.0))
                };

                if candle.high >= stop {
                    let reason = if pos.be_locked || pos.partial_taken {
                        self.consecutive_losses = 0;
                        self.cooldown_remaining = win_cooldown;
                        "MERA Breakeven Seguro Garantido"
                    } else {
                        self.consecutive_losses += 1;
                        self.cooldown_remaining = match self.consecutive_losses {
                            1 => 6, // 30 min de pausa defensiva após stop no 5m
                            2 => 12, // 1 hora de pausa após 2 stops seguidos
                            _ => 24, // 2 horas de pausa após 3+ stops seguidos
                        };
                        "MERA Stop Loss"
                    };
                    return (
                        self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, reason, Some(stop), Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::CloseShort,
                            price: stop,
                            timestamp: candle.close_time,
                            reason: format!("{} em {:.2}", reason, stop),
                            stop_loss: None,
                            take_profit: None,
                            tp_partial: None,
                            partial_ratio: None,
                            size_factor: None,
                        }),
                    );
                }
            }

            // Atualiza o estado interno do PLE sincronizado
            let _ = self.ple.update(candle, pos);

            // Posição mantida
            let telem = self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, "Posição em andamento", pos.stop_loss, pos.take_profit, None);
            return (telem, None);
        }

        // 3. Checagem de novos gatilhos somente em fechamento de candle
        if candle.is_closed {
            if self.cooldown_remaining > 0 {
                self.cooldown_remaining -= 1;
            }
        } else {
            let telem = self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, "Aguardando fechamento da vela 5m", None, None, None);
            return (telem, None);
        }

        if self.cooldown_remaining > 0 {
            let msg = format!("Cooldown ativo após trade ({}/{} velas)", self.cooldown_remaining, self.config.cooldown_candles);
            let telem = self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, &msg, None, None, None);
            return (telem, None);
        }

        // Limite de operações nas últimas 24h
        let now_ms = candle.close_time;
        self.trade_timestamps.retain(|&t| now_ms - t < 86_400_000);
        if self.trade_timestamps.len() >= self.config.max_trades_24h {
            let telem = self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, "Limite de trades diários atingido", None, None, None);
            return (telem, None);
        }

        // --- PRIORIDADE MKT 3.0: PHANTOM LIQUIDITY ENGINE (FADE DE ARMADILHAS & RETESTES) ---
        if self.config.enable_ple {
            let (_ple_telem, ple_sig) = self.ple.update(candle, pos);
            if let Some(sig) = ple_sig {
                // VETO CINÉTICO MKT 3.0:
                // Proíbe Fade Trap contra momentum direcional forte (evita vender explosão ou comprar sangria)
                let is_fade_short = sig.signal_type == SignalType::OpenShort && sig.reason.contains("Fade");
                let is_fade_long = sig.signal_type == SignalType::OpenLong && sig.reason.contains("Fade");

                let veto_short = (is_fade_short && (k_out.tensor > 0.055 || k_out.is_volume_climax))
                    || (sig.signal_type == SignalType::OpenShort && drift_telem.veto_short);
                let veto_long = (is_fade_long && (k_out.tensor < -0.055 || k_out.is_volume_climax))
                    || (sig.signal_type == SignalType::OpenLong && drift_telem.veto_long);

                if !veto_short && !veto_long {
                    self.trade_timestamps.push(now_ms);
                    let telem = self.build_telem(
                        vw, vu, vl, nc, nu, nl, ns,
                        bias_bullish, bias_bearish, vol_ratio, is_volume_ok,
                        sig.signal_type == SignalType::OpenLong,
                        sig.signal_type == SignalType::OpenShort,
                        &sig.reason,
                        sig.stop_loss,
                        sig.tp_partial,
                        sig.take_profit,
                    );
                    return (telem, Some(sig));
                }
            }
        }

        // Análise da geometria do candle (pavios de rejeição e absorção)
        let candle_range = (candle.high - candle.low).max(0.01);
        let body_bottom = candle.close.min(candle.open);
        let body_top = candle.close.max(candle.open);
        let lower_wick = body_bottom - candle.low;
        let upper_wick = candle.high - body_top;
        let lower_wick_ratio = lower_wick / candle_range;
        let upper_wick_ratio = upper_wick / candle_range;

        let nw_spread = (nu - nl) / nc;
        let is_volatility_ok = nw_spread >= self.config.min_nw_spread && (atr_val / candle.close) >= 0.0022;

        // --- ATIVAÇÃO HÍBRIDA: SELETOR DE REGIME (MTF-1 TREND vs LCP-HARVESTER CHOP) ---
        if self.config.use_hybrid {
            let is_macro_trend = match macro_state {
                Some(ref s) => s.is_breakout_mode || s.ker_1h > 0.40 || s.zone != MacroZone::Equilibrium,
                None => true,
            };

            if !is_macro_trend {
                if let Some(ref s) = macro_state {
                    let is_lateral = s.ker_1h <= 0.35 && s.ema50_slope_pct.abs() <= 0.08;
                    if is_lateral {
                        // 1. Bear Trap em Compressão com RSI exausto (Gatilho Long)
                        let setup_chop_long = !drift_telem.veto_long
                            && candle.low <= nl
                            && candle.close >= nl
                            && candle.close >= candle.open
                            && lower_wick_ratio >= 0.25
                            && rsi_val <= 42.0;

                        if setup_chop_long && is_volume_ok && !k_out.is_volume_climax {
                            self.trade_timestamps.push(now_ms);
                            let sl_price = candle.close * (1.0 - self.config.chop_sl_percent / 100.0);
                            let tp1_price = candle.close * (1.0 + self.config.chop_tp_partial_percent / 100.0);
                            let tp2_price = candle.close * (1.0 + self.config.chop_tp_final_percent / 100.0);
                            let reason = format!(
                                "LCP-Harvester Chop Long [KER 1h: {:.2} | RSI: {:.1}] Bear Trap (Wick {:.0}%)",
                                s.ker_1h, rsi_val, lower_wick_ratio * 100.0
                            );
                            info!(
                                "[LCP CHOP LONG] Entrada @ {:.2} | Stop: {:.2} (-{:.2}%) | TP1: {:.2} (+{:.2}%) | TP2: {:.2} (+{:.2}%)",
                                candle.close, sl_price, self.config.chop_sl_percent, tp1_price, self.config.chop_tp_partial_percent, tp2_price, self.config.chop_tp_final_percent
                            );
                            let telem = self.build_telem(
                                vw, vu, vl, nc, nu, nl, ns,
                                bias_bullish, bias_bearish, vol_ratio, is_volume_ok,
                                true, false, "Entrada LCP CHOP LONG",
                                Some(sl_price), Some(tp1_price), Some(tp2_price)
                            );
                            return (
                                telem,
                                Some(Signal {
                                    signal_type: SignalType::OpenLong,
                                    price: candle.close,
                                    timestamp: candle.close_time,
                                    reason,
                                    stop_loss: Some(sl_price),
                                    take_profit: Some(tp2_price),
                                    tp_partial: Some(tp1_price),
                                    partial_ratio: Some(self.config.partial_ratio),
                                    size_factor: Some(1.0),
                                 }),
                            );
                        }

                        // 2. Bull Trap em Compressão com RSI exausto (Gatilho Short)
                        let setup_chop_short = !drift_telem.veto_short
                            && candle.high >= nu
                            && candle.close <= nu
                            && candle.close <= candle.open
                            && upper_wick_ratio >= 0.25
                            && rsi_val >= 58.0;

                        if setup_chop_short && is_volume_ok && !k_out.is_volume_climax {
                            self.trade_timestamps.push(now_ms);
                            let sl_price = candle.close * (1.0 + self.config.chop_sl_percent / 100.0);
                            let tp1_price = candle.close * (1.0 - self.config.chop_tp_partial_percent / 100.0);
                            let tp2_price = candle.close * (1.0 - self.config.chop_tp_final_percent / 100.0);
                            let reason = format!(
                                "LCP-Harvester Chop Short [KER 1h: {:.2} | RSI: {:.1}] Bull Trap (Wick {:.0}%)",
                                s.ker_1h, rsi_val, upper_wick_ratio * 100.0
                            );
                            info!(
                                "[LCP CHOP SHORT] Entrada @ {:.2} | Stop: {:.2} (-{:.2}%) | TP1: {:.2} (+{:.2}%) | TP2: {:.2} (+{:.2}%)",
                                candle.close, sl_price, self.config.chop_sl_percent, tp1_price, self.config.chop_tp_partial_percent, tp2_price, self.config.chop_tp_final_percent
                            );
                            let telem = self.build_telem(
                                vw, vu, vl, nc, nu, nl, ns,
                                bias_bullish, bias_bearish, vol_ratio, is_volume_ok,
                                false, true, "Entrada LCP CHOP SHORT",
                                Some(sl_price), Some(tp1_price), Some(tp2_price)
                            );
                            return (
                                telem,
                                Some(Signal {
                                    signal_type: SignalType::OpenShort,
                                    price: candle.close,
                                    timestamp: candle.close_time,
                                    reason,
                                    stop_loss: Some(sl_price),
                                    take_profit: Some(tp2_price),
                                    tp_partial: Some(tp1_price),
                                    partial_ratio: Some(self.config.partial_ratio),
                                    size_factor: Some(1.0),
                                }),
                            );
                        }
                    }
                }

                // Em compressão lateral sem armadilha confirmada: bloqueia expansão de tendência
                let telem = self.build_telem(
                    vw, vu, vl, nc, nu, nl, ns,
                    bias_bullish, bias_bearish, vol_ratio, is_volume_ok,
                    false, false, "Compressão Lateral (Aguardando Reversão LCP)",
                    None, None, None
                );
                return (telem, None);
            }
        }

        // --- SISTEMA DE CONFLUÊNCIA CINÉTICA PONDERADA (SCORING DINÂMICO 5M) ---
        // 1. Scoring para Long:
        let mut score_long: u32 = 0;
        let mut veto_long = drift_telem.veto_long;

        if k_out.is_trend_bear
            || k_out.is_volume_climax
            || candle.close > ema_val * 1.025
            || ns < -0.025
            || k_out.tensor < -0.025
            || k_out.tensor > 0.055
            || k_out.tensor_slow < -0.050
            || k_out.tensor_slow > 0.080
            || k_out.vwap_vel < -0.15
            || !k_out.is_long_allowed
        {
            veto_long = true;
        }

        if !veto_long {
            // A. Posição na Banda de Nadaraya (até 35 pts)
            if candle.low <= nl && candle.close >= nl {
                score_long += 35;
            } else if candle.close <= nl + (nu - nl) * 0.18 {
                score_long += 25;
            } else if candle.close <= nc && candle.low <= nl + (nu - nl) * 0.25 {
                score_long += 15;
            }

            // B. Rejeição e Geometria da Vela (até 25 pts)
            if lower_wick_ratio >= 0.25 {
                score_long += 20;
            } else if lower_wick_ratio >= 0.15 {
                score_long += 12;
            }
            if candle.close >= candle.open {
                score_long += 10;
            }

            // C. Momentum Cinético MKT 3.0 (até 25 pts)
            if k_out.tensor >= 0.0 && k_out.tensor <= 0.055 {
                score_long += 15;
            } else if k_out.tensor >= -0.015 {
                score_long += 8;
            }
            if k_out.vwap_vel >= 0.0 {
                score_long += 10;
            } else if k_out.vwap_vel >= -0.08 {
                score_long += 5;
            }

            // D. Macro e Volatilidade (até 15 pts)
            if macro_bullish {
                score_long += 10;
            }
            if is_volume_ok {
                score_long += 5;
            }
        }

        let macro_zone_allows_long = !self.config.use_hybrid
            || macro_state.as_ref().map(|s| s.zone == MacroZone::Oversold || s.is_breakout_mode).unwrap_or(true);
        let macro_zone_allows_short = !self.config.use_hybrid
            || macro_state.as_ref().map(|s| s.zone == MacroZone::Overbought || s.is_breakout_mode).unwrap_or(true);

        let setup_long_reversion = self.config.enabled && !veto_long && score_long >= 68 && macro_zone_allows_long;

        // 2. Scoring para Short:
        let mut score_short: u32 = 0;
        let mut veto_short = drift_telem.veto_short;

        if k_out.is_trend_bull
            || k_out.is_volume_climax
            || candle.close < ema_val * 0.975
            || ns > 0.025
            || k_out.tensor > 0.025
            || k_out.tensor < -0.055
            || k_out.tensor_slow > 0.050
            || k_out.tensor_slow < -0.080
            || k_out.vwap_vel > 0.15
            || !k_out.is_short_allowed
        {
            veto_short = true;
        }

        if !veto_short {
            // A. Posição na Banda de Nadaraya (até 35 pts)
            if candle.high >= nu && candle.close <= nu {
                score_short += 35;
            } else if candle.close >= nu - (nu - nl) * 0.18 {
                score_short += 25;
            } else if candle.close >= nc && candle.high >= nu - (nu - nl) * 0.25 {
                score_short += 15;
            }

            // B. Rejeição e Geometria da Vela (até 25 pts)
            if upper_wick_ratio >= 0.25 {
                score_short += 20;
            } else if upper_wick_ratio >= 0.15 {
                score_short += 12;
            }
            if candle.close <= candle.open {
                score_short += 10;
            }

            // C. Momentum Cinético MKT 3.0 (até 25 pts)
            if k_out.tensor <= 0.0 && k_out.tensor >= -0.055 {
                score_short += 15;
            } else if k_out.tensor <= 0.015 {
                score_short += 8;
            }
            if k_out.vwap_vel <= 0.0 {
                score_short += 10;
            } else if k_out.vwap_vel <= 0.08 {
                score_short += 5;
            }

            // D. Macro e Volatilidade (até 15 pts)
            if macro_bearish {
                score_short += 10;
            }
            if is_volume_ok {
                score_short += 5;
            }
        }

        let setup_short_reversion = self.config.enabled && !veto_short && score_short >= 68 && macro_zone_allows_short;

        // --- MODO 2: IGNIÇÃO CINÉTICA BIMODAL (BREAKOUT INSTITUCIONAL) ---
        let is_bimodal = self.config.use_bimodal;

        // Breakout Long: Rompimento da banda superior com aceleração cinética positiva
        let breakout_long_trigger = candle.close >= nu && candle.close > candle.open;
        let is_kinetic_thrust_bull = (k_out.is_trend_bull || (k_out.tensor >= 0.035 && k_out.vwap_vel >= 0.10))
            && k_out.is_long_allowed
            && !k_out.is_whipsaw;

        let setup_long_breakout = is_bimodal
            && self.config.enabled
            && !drift_telem.veto_long
            && macro_bullish
            && breakout_long_trigger
            && is_kinetic_thrust_bull
            && is_volume_ok
            && is_volatility_ok
            && macro_zone_allows_long;

        // Breakdown Short: Rompimento da banda inferior com aceleração cinética negativa
        let breakdown_short_trigger = candle.close <= nl && candle.close < candle.open;
        let is_kinetic_thrust_bear = (k_out.is_trend_bear || (k_out.tensor <= -0.035 && k_out.vwap_vel <= -0.10))
            && k_out.is_short_allowed
            && !k_out.is_whipsaw;

        let setup_short_breakdown = is_bimodal
            && self.config.enabled
            && !drift_telem.veto_short
            && macro_bearish
            && breakdown_short_trigger
            && is_kinetic_thrust_bear
            && is_volume_ok
            && is_volatility_ok
            && macro_zone_allows_short;

        let is_breakout_long = setup_long_breakout;
        let setup_long = setup_long_reversion || setup_long_breakout;

        if setup_long {
            self.trade_timestamps.push(now_ms);
            let eff_sl = self.config.sl_percent;
            let sl_price = candle.close * (1.0 - eff_sl / 100.0);
            let tp1_pct = self.config.tp_partial_percent;
            let tp2_pct = self.config.tp_final_percent;
            let tp1_price = candle.close * (1.0 + tp1_pct / 100.0);
            let tp2_price = candle.close * (1.0 + tp2_pct / 100.0);
            let size_factor = k_out.size_factor_long;

            let log_tag = if is_breakout_long { "MERA BREAKOUT LONG" } else { "MERA REVERSÃO LONG" };
            info!(
                "[{}] Entrada @ {:.2} | Stop: {:.2} (-{:.2}%) | TP1: {:.2} (+{:.2}%) | TP2: {:.2} (+{:.2}%) | Size: {:.0}%",
                log_tag,
                candle.close,
                sl_price,
                ((candle.close - sl_price) / candle.close) * 100.0,
                tp1_price,
                tp1_pct,
                tp2_price,
                tp2_pct,
                size_factor * 100.0
            );

            let reason = if is_breakout_long {
                format!("MERA Breakout Long [Kinetic Thrust: {:+.2} | VWAP Vel: {:+.2}]", k_out.tensor, k_out.vwap_vel)
            } else {
                format!("MERA Scalp Long [T {:+.2} | 24h {:+.2}]: Rejeição Nadaraya", k_out.tensor, k_out.tensor_slow)
            };

            let telem = self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, true, false, "Entrada LONG Confirmada", Some(sl_price), Some(tp1_price), Some(tp2_price));
            return (
                telem,
                Some(Signal {
                    signal_type: SignalType::OpenLong,
                    price: candle.close,
                    timestamp: candle.close_time,
                    reason,
                    stop_loss: Some(sl_price),
                    take_profit: Some(tp2_price),
                    tp_partial: Some(tp1_price),
                    partial_ratio: Some(self.config.partial_ratio),
                    size_factor: Some(size_factor),
                }),
            );
        }

        let is_breakdown_short = setup_short_breakdown;
        let setup_short = setup_short_reversion || setup_short_breakdown;

        if setup_short {
            self.trade_timestamps.push(now_ms);
            let eff_sl = self.config.sl_percent;
            let sl_price = candle.close * (1.0 + eff_sl / 100.0);
            let tp1_pct = self.config.tp_partial_percent;
            let tp2_pct = self.config.tp_final_percent;
            let tp1_price = candle.close * (1.0 - tp1_pct / 100.0);
            let tp2_price = candle.close * (1.0 - tp2_pct / 100.0);
            let size_factor = k_out.size_factor_short;

            let log_tag = if is_breakdown_short { "MERA BREAKDOWN SHORT" } else { "MERA REVERSÃO SHORT" };
            info!(
                "[{}] Entrada @ {:.2} | Stop: {:.2} (-{:.2}%) | TP1: {:.2} (+{:.2}%) | TP2: {:.2} (+{:.2}%) | Size: {:.0}%",
                log_tag,
                candle.close,
                sl_price,
                ((sl_price - candle.close) / candle.close) * 100.0,
                tp1_price,
                tp1_pct,
                tp2_price,
                tp2_pct,
                size_factor * 100.0
            );

            let reason = if is_breakdown_short {
                format!("MERA Breakdown Short [Kinetic Thrust: {:+.2} | VWAP Vel: {:+.2}]", k_out.tensor, k_out.vwap_vel)
            } else {
                format!("MERA Scalp Short [T {:+.2} | 24h {:+.2}]: Rejeição Nadaraya", k_out.tensor, k_out.tensor_slow)
            };

            let telem = self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, true, "Entrada SHORT Confirmada", Some(sl_price), Some(tp1_price), Some(tp2_price));
            return (
                telem,
                Some(Signal {
                    signal_type: SignalType::OpenShort,
                    price: candle.close,
                    timestamp: candle.close_time,
                    reason,
                    stop_loss: Some(sl_price),
                    take_profit: Some(tp2_price),
                    tp_partial: Some(tp1_price),
                    partial_ratio: Some(self.config.partial_ratio),
                    size_factor: Some(size_factor),
                }),
            );
        }

        let telem = self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, "Monitorando bandas Nadaraya vs VWAP", None, None, None);
        (telem, None)
    }

    fn build_telem(
        &mut self,
        vw: f64,
        vu: f64,
        vl: f64,
        nc: f64,
        nu: f64,
        nl: f64,
        ns: f64,
        bias_bullish: bool,
        bias_bearish: bool,
        volume_ratio: f64,
        is_volume_ok: bool,
        setup_long: bool,
        setup_short: bool,
        signal_reason: &str,
        active_stop: Option<f64>,
        tp_partial: Option<f64>,
        tp_final: Option<f64>,
    ) -> MeraTelemetry {
        let telem = MeraTelemetry {
            enabled: self.config.enabled,
            session_vwap: vw,
            vwap_upper: vu,
            vwap_lower: vl,
            nw_central: nc,
            nw_upper: nu,
            nw_lower: nl,
            nw_slope_pct: ns,
            bias_bullish,
            bias_bearish,
            volume_ratio,
            is_volume_ok,
            setup_long,
            setup_short,
            signal_reason: signal_reason.to_string(),
            active_stop,
            tp_partial,
            tp_final,
        };
        self.last_telemetry = telem.clone();
        telem
    }

    /// Computa as séries históricas para renderização sobreposta no gráfico nativo (VWAP, Nadaraya Central, Upper, Lower)
    pub fn compute_series_for_candles(
        &self,
        candles: &[Candle],
    ) -> (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) {
        let mut vwap_tracker = SessionVwap::new();
        let mut nw_tracker = NadarayaWatson::new(self.config.bandwidth, self.config.mult, self.config.lookback);

        let mut vwaps = Vec::with_capacity(candles.len());
        let mut centrals = Vec::with_capacity(candles.len());
        let mut uppers = Vec::with_capacity(candles.len());
        let mut lowers = Vec::with_capacity(candles.len());

        for c in candles {
            let (vw, _, _) = vwap_tracker.update(c);
            let (nc, nu, nl, _) = nw_tracker.update(c.close);
            vwaps.push(vw);
            centrals.push(nc);
            uppers.push(nu);
            lowers.push(nl);
        }

        (vwaps, centrals, uppers, lowers)
    }
}
