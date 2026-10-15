// language: Rust, file: src/mera.rs, target: Windows 11, MSVC
// Motor MERA: Scalping 5m Institucional (Session VWAP + Nadaraya-Watson Envelope + Rejeição de Pavio + Parcial Dinâmica)

use crate::config::MeraConfig;
use crate::indicators::{Atr, Ema, MeraKineticTensor, NadarayaWatson, SessionVwap, VolumeSma};
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
}

impl MeraEngine {
    pub fn new(config: MeraConfig) -> Self {
        let vwap = SessionVwap::new();
        let nw = NadarayaWatson::new(config.bandwidth, config.mult, config.lookback);
        let vol_sma = VolumeSma::new(20);
        let atr = Atr::new(14);
        let ema_macro = Ema::new(config.ema_trend_period);
        let kinetic = MeraKineticTensor::new(576); // Janela macro de 48 horas (576 velas de 5m)

        Self {
            config,
            vwap,
            nw,
            vol_sma,
            atr,
            ema_macro,
            kinetic,
            last_telemetry: MeraTelemetry::default(),
            trade_timestamps: Vec::new(),
            cooldown_remaining: 0,
            consecutive_losses: 0,
        }
    }

    pub fn set_config(&mut self, config: MeraConfig) {
        self.nw = NadarayaWatson::new(config.bandwidth, config.mult, config.lookback);
        self.ema_macro = Ema::new(config.ema_trend_period);
        self.config = config;
    }

    pub fn warmup(&mut self, candles: &[Candle]) {
        if candles.is_empty() {
            return;
        }

        for c in candles {
            self.vol_sma.update(c.volume);
            self.atr.update(c.high, c.low, c.close);
            let (vw, _, _) = self.vwap.update(c);
            self.nw.update(c.close);
            self.ema_macro.update(c.close);
            self.kinetic.update(c, vw);
        }

        if let Some(last) = candles.last() {
            let (vw, vu, vl) = self.vwap.update(last);
            let (nc, nu, nl, ns) = self.nw.update(last.close);
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
        let (vw, vu, vl) = self.vwap.update(candle);
        let (nc, nu, nl, ns) = self.nw.update(candle.close);
        let vsma = self.vol_sma.update(candle.volume);
        let atr_val = self.atr.update(candle.high, candle.low, candle.close).unwrap_or(candle.close * 0.005);
        let ema_val = self.ema_macro.update(candle.close).unwrap_or(candle.close);
        let k_out = self.kinetic.update(candle, vw);

        let bias_bullish = candle.close > vw;
        let bias_bearish = candle.close < vw;
        let macro_bullish = !self.config.use_htf_trend || candle.close >= ema_val;
        let macro_bearish = !self.config.use_htf_trend || candle.close <= ema_val;
        let vol_ratio = if vsma > 0.0 { candle.volume / vsma } else { 1.0 };
        let is_volume_ok = candle.volume >= vsma * self.config.volume_mult && candle.volume <= vsma * self.config.max_volume_mult;

        // 2. Gerenciamento intrabar se já estiver posicionado
        if is_in_pos {
            if pos_side == PositionSide::Long {
                let tp1 = pos.tp_partial.unwrap_or(pos_entry * (1.0 + self.config.tp_partial_percent / 100.0));
                let tp2 = pos.take_profit.unwrap_or(pos_entry * (1.0 + self.config.tp_final_percent / 100.0));

                // A. Take Profit Parcial
                if !pos.partial_taken && candle.high >= tp1 {
                    self.consecutive_losses = 0;
                    let be_stop = pos_entry * (1.0 + self.config.be_offset_percent / 100.0);
                    info!("[MERA SCALP] Take Profit Parcial {:.0}% atingido para LONG @ {:.2} (BE travado em {:.2})", self.config.partial_ratio * 100.0, tp1, be_stop);
                    return (
                        self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, "Take Profit Parcial", Some(be_stop), Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::PartialCloseLong,
                            price: tp1,
                            timestamp: candle.close_time,
                            reason: format!("MERA TP Parcial (+{:.2}%)", self.config.tp_partial_percent),
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
                    self.cooldown_remaining = self.config.cooldown_candles;
                    info!("[MERA SCALP] Take Profit Final 100% atingido para LONG @ {:.2}", tp2);
                    return (
                        self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, "Take Profit Final", None, Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::CloseLong,
                            price: tp2,
                            timestamp: candle.close_time,
                            reason: format!("MERA TP Final (+{:.2}%)", self.config.tp_final_percent),
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
                    let be_floor = pos_entry * (1.0 + self.config.be_offset_percent / 100.0);
                    pos.stop_loss.map(|s| s.max(be_floor)).unwrap_or(be_floor)
                } else {
                    pos.stop_loss.unwrap_or(pos_entry * (1.0 - self.config.sl_percent / 100.0))
                };

                if candle.low <= stop {
                    let reason = if pos.be_locked || pos.partial_taken {
                        self.consecutive_losses = 0;
                        self.cooldown_remaining = self.config.cooldown_candles;
                        "MERA Breakeven Seguro Garantido"
                    } else {
                        self.consecutive_losses += 1;
                        self.cooldown_remaining = match self.consecutive_losses {
                            1 => 12, // 1 hora de pausa defensiva após stop
                            2 => 24, // 2 horas de pausa após 2 stops seguidos
                            _ => 72, // 6 horas de pausa após 3+ stops seguidos
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
                let tp1 = pos.tp_partial.unwrap_or(pos_entry * (1.0 - self.config.tp_partial_percent / 100.0));
                let tp2 = pos.take_profit.unwrap_or(pos_entry * (1.0 - self.config.tp_final_percent / 100.0));

                // A. Take Profit Parcial
                if !pos.partial_taken && candle.low <= tp1 {
                    self.consecutive_losses = 0;
                    let be_stop = pos_entry * (1.0 - self.config.be_offset_percent / 100.0);
                    info!("[MERA SCALP] Take Profit Parcial {:.0}% atingido para SHORT @ {:.2} (BE travado em {:.2})", self.config.partial_ratio * 100.0, tp1, be_stop);
                    return (
                        self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, "Take Profit Parcial", Some(be_stop), Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::PartialCloseShort,
                            price: tp1,
                            timestamp: candle.close_time,
                            reason: format!("MERA TP Parcial (+{:.2}%)", self.config.tp_partial_percent),
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
                    self.cooldown_remaining = self.config.cooldown_candles;
                    info!("[MERA SCALP] Take Profit Final 100% atingido para SHORT @ {:.2}", tp2);
                    return (
                        self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, false, "Take Profit Final", None, Some(tp1), Some(tp2)),
                        Some(Signal {
                            signal_type: SignalType::CloseShort,
                            price: tp2,
                            timestamp: candle.close_time,
                            reason: format!("MERA TP Final (+{:.2}%)", self.config.tp_final_percent),
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
                    let be_floor = pos_entry * (1.0 - self.config.be_offset_percent / 100.0);
                    pos.stop_loss.map(|s| s.min(be_floor)).unwrap_or(be_floor)
                } else {
                    pos.stop_loss.unwrap_or(pos_entry * (1.0 + self.config.sl_percent / 100.0))
                };

                if candle.high >= stop {
                    let reason = if pos.be_locked || pos.partial_taken {
                        self.consecutive_losses = 0;
                        self.cooldown_remaining = self.config.cooldown_candles;
                        "MERA Breakeven Seguro Garantido"
                    } else {
                        self.consecutive_losses += 1;
                        self.cooldown_remaining = match self.consecutive_losses {
                            1 => 12, // 1 hora de pausa defensiva após stop
                            2 => 24, // 2 horas de pausa após 2 stops seguidos
                            _ => 72, // 6 horas de pausa após 3+ stops seguidos
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

        // Análise da geometria do candle (pavios de rejeição e absorção)
        let candle_range = (candle.high - candle.low).max(0.01);
        let body_bottom = candle.close.min(candle.open);
        let body_top = candle.close.max(candle.open);
        let lower_wick = body_bottom - candle.low;
        let upper_wick = candle.high - body_top;
        let lower_wick_ratio = lower_wick / candle_range;
        let upper_wick_ratio = upper_wick / candle_range;

        // Gatilho LONG:
        // 1. Tendência institucional favorável: Preço acima da VWAP
        // 2. Desconto extremo: Mínima do candle atinge/fura banda inferior, mas fecha DENTRO (rejeição de rompimento)
        let at_nw_discount = candle.low <= nl && candle.close >= nl;
        let long_rejection = candle.close >= candle.open && lower_wick_ratio >= self.config.min_wick_ratio;

        let nw_spread = (nu - nl) / nc;
        let is_volatility_ok = nw_spread >= self.config.min_nw_spread && (atr_val / candle.close) >= 0.0022;

        // Gatilho LONG (Extremo Desconto Nadaraya + Rejeição)
        let setup_long = self.config.enabled
            && bias_bullish
            && macro_bullish
            && at_nw_discount
            && long_rejection
            && is_volume_ok
            && is_volatility_ok
            && ns >= -0.04
            && k_out.is_long_allowed
            && !k_out.is_volume_climax;

        if setup_long {
            self.trade_timestamps.push(now_ms);
            let eff_sl = self.config.sl_percent.min(k_out.sl_pct);
            let sl_price = candle.close * (1.0 - eff_sl / 100.0);
            let tp1_price = candle.close * (1.0 + k_out.tp_partial_pct / 100.0);
            let tp2_price = candle.close * (1.0 + k_out.tp_final_pct / 100.0);

            info!(
                "[MERA SCALP GATILHO LONG] Entrada @ {:.2} | Stop: {:.2} (-{:.2}%) | TP1: {:.2} (+{:.2}%) | TP2: {:.2} (+{:.2}%) | Size: {:.0}%",
                candle.close,
                sl_price,
                ((candle.close - sl_price) / candle.close) * 100.0,
                tp1_price,
                k_out.tp_partial_pct,
                tp2_price,
                k_out.tp_final_pct,
                k_out.size_factor_long * 100.0
            );

            let telem = self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, true, false, "Entrada LONG Confirmada", Some(sl_price), Some(tp1_price), Some(tp2_price));
            return (
                telem,
                Some(Signal {
                    signal_type: SignalType::OpenLong,
                    price: candle.close,
                    timestamp: candle.close_time,
                    reason: format!("MERA Scalp Long [T {:+.2} | 24h {:+.2}]: Rejeição Nadaraya", k_out.tensor, k_out.tensor_slow),
                    stop_loss: Some(sl_price),
                    take_profit: Some(tp2_price),
                    tp_partial: Some(tp1_price),
                    partial_ratio: Some(self.config.partial_ratio),
                    size_factor: Some(k_out.size_factor_long),
                }),
            );
        }

        // Gatilho SHORT:
        let at_nw_premium = candle.high >= nu && candle.close <= nu;
        let short_rejection = candle.close <= candle.open && upper_wick_ratio >= self.config.min_wick_ratio;

        let setup_short = self.config.enabled
            && bias_bearish
            && macro_bearish
            && at_nw_premium
            && short_rejection
            && is_volume_ok
            && is_volatility_ok
            && ns <= 0.04
            && k_out.is_short_allowed
            && !k_out.is_volume_climax;

        if setup_short {
            self.trade_timestamps.push(now_ms);
            let eff_sl = self.config.sl_percent.min(k_out.sl_pct);
            let sl_price = candle.close * (1.0 + eff_sl / 100.0);
            let tp1_price = candle.close * (1.0 - k_out.tp_partial_pct / 100.0);
            let tp2_price = candle.close * (1.0 - k_out.tp_final_pct / 100.0);

            info!(
                "[MERA SCALP GATILHO SHORT] Entrada @ {:.2} | Stop: {:.2} (-{:.2}%) | TP1: {:.2} (+{:.2}%) | TP2: {:.2} (+{:.2}%) | Size: {:.0}%",
                candle.close,
                sl_price,
                ((sl_price - candle.close) / candle.close) * 100.0,
                tp1_price,
                k_out.tp_partial_pct,
                tp2_price,
                k_out.tp_final_pct,
                k_out.size_factor_short * 100.0
            );

            let telem = self.build_telem(vw, vu, vl, nc, nu, nl, ns, bias_bullish, bias_bearish, vol_ratio, is_volume_ok, false, true, "Entrada SHORT Confirmada", Some(sl_price), Some(tp1_price), Some(tp2_price));
            return (
                telem,
                Some(Signal {
                    signal_type: SignalType::OpenShort,
                    price: candle.close,
                    timestamp: candle.close_time,
                    reason: format!("MERA Scalp Short [T {:+.2} | 24h {:+.2}]: Rejeição Nadaraya", k_out.tensor, k_out.tensor_slow),
                    stop_loss: Some(sl_price),
                    take_profit: Some(tp2_price),
                    tp_partial: Some(tp1_price),
                    partial_ratio: Some(self.config.partial_ratio),
                    size_factor: Some(k_out.size_factor_short),
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
