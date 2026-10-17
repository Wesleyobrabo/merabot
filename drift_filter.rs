// language: Rust, file: src/drift_filter.rs, target: Windows 11, MSVC
// =========================================================================================
//  KILL-SWITCH DE DERIVA LENTA & PROJETOR DE LATERALIZAÇÃO REAL (SLOW DRIFT GATE)
// =========================================================================================
//  Motor Matemático desenvolvido do absoluto zero para diferenciar:
//  1. Lateralização Real (Mean-Reverting Equilibrium):
//     Preço orbita a VWAP diária dentro de uma faixa de oscilação saudável (|dist| <= 0.50%),
//     com EMA 20 macro de 1h neutra/estável (|Slope 12h| < 0.05%).
//     Scalps de reversão (LCP-Harvester e MERA Reversion) pagam alvos consistentemente.
//
//  2. Deriva Lenta / Sangria Persistente (Slow Bleed Grind):
//     Preço desancorado abaixo da VWAP diária por horas, com EMA 20 de 1h inclinada para baixo
//     (Slope 12h <= -0.05%) ou preço esticado além de 0.50% abaixo da VWAP.
//     Pavios inferiores são armadilhas de continuação de queda (Bear Traps falsos).
//     AÇÃO DO GATE: Veto absoluto a COMPRAS (Longs cancelados).
//
//  3. Deriva Altista Grinding (Bullish Slow Drift):
//     Preço desancorado acima da VWAP diária com EMA 20 de 1h em ascensão (Slope 12h >= +0.05%).
//     AÇÃO DO GATE: Veto absoluto a VENDAS (Shorts cancelados).
// =========================================================================================

use serde::{Deserialize, Serialize};
use crate::indicators::{Ema, SessionVwap};
use crate::types::Candle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketDriftRegime {
    /// Lateralização Genuína: Preço em equilíbrio em torno da VWAP. Scalps 100% autorizados.
    TrueEquilibrium,
    /// Sangria Baixista Lenta: EMA 20 1h em declínio e preço abaixo da VWAP. LONGS VETADOS.
    BearishSlowDrift,
    /// Deriva Altista Lenta: EMA 20 1h em ascensão e preço acima da VWAP. SHORTS VETADOS.
    BullishSlowDrift,
    /// Expansão Dinâmica Esticada além do equilíbrio.
    StretchedExpansion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlowDriftConfig {
    pub enabled: bool,
    /// Janela de lookback em horas para o cálculo da inclinação da EMA 20 de 1h (padrão: 12 horas)
    pub slope_window_hours: usize,
    /// Inclinação mínima percentual da EMA 20 de 1h nas últimas 12h para confirmar deriva (padrão: 0.05%)
    pub min_ema20_slope_pct: f64,
    /// Distância máxima percentual da VWAP diária para considerar o mercado em lateralização (padrão: 0.50%)
    pub max_vwap_distance_pct: f64,
}

impl Default for SlowDriftConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            slope_window_hours: 12,
            min_ema20_slope_pct: 0.05,
            max_vwap_distance_pct: 0.50,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftTelemetry {
    pub regime: MarketDriftRegime,
    pub session_vwap: f64,
    pub vwap_dist_pct: f64,
    pub ema20_1h: f64,
    pub ema20_1h_slope_pct: f64,
    pub lateral_projector_score: f64, // 0.0 (sangria direcional pura) a 100.0 (lateralização pura)
    pub veto_long: bool,
    pub veto_short: bool,
    pub description: String,
}

impl Default for DriftTelemetry {
    fn default() -> Self {
        Self {
            regime: MarketDriftRegime::TrueEquilibrium,
            session_vwap: 0.0,
            vwap_dist_pct: 0.0,
            ema20_1h: 0.0,
            ema20_1h_slope_pct: 0.0,
            lateral_projector_score: 50.0,
            veto_long: false,
            veto_short: false,
            description: "Aguardando aquecimento do filtro de deriva".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SlowDriftKillSwitch {
    pub config: SlowDriftConfig,
    pub vwap: SessionVwap,
    ema20_1h: Ema,
    current_1h_chunk: Vec<Candle>,
    history_1h_ema20: Vec<f64>,
    last_telemetry: DriftTelemetry,
}

impl SlowDriftKillSwitch {
    pub fn new(config: SlowDriftConfig) -> Self {
        Self {
            config,
            vwap: SessionVwap::new(),
            ema20_1h: Ema::new(20),
            current_1h_chunk: Vec::with_capacity(12),
            history_1h_ema20: Vec::with_capacity(120),
            last_telemetry: DriftTelemetry::default(),
        }
    }

    pub fn reset(&mut self) {
        self.vwap.reset();
        self.current_1h_chunk.clear();
        self.history_1h_ema20.clear();
        self.last_telemetry = DriftTelemetry::default();
    }

    /// Processa o candle de 5m em tempo real e calcula o Projetor de Lateralização & Kill Switch
    pub fn update(&mut self, candle: &Candle) -> DriftTelemetry {
        // 1. Atualiza Session VWAP diária ancorada às 00:00 UTC
        let (vwap_val, _, _) = self.vwap.update(candle);

        // 2. Agregação horária para cálculo da EMA 20 Macro
        self.current_1h_chunk.push(candle.clone());
        if self.current_1h_chunk.len() == 12 {
            let close_1h = self.current_1h_chunk[11].close;
            self.current_1h_chunk.clear();

            if let Some(ema_val) = self.ema20_1h.update(close_1h) {
                self.history_1h_ema20.push(ema_val);
                if self.history_1h_ema20.len() > 100 {
                    self.history_1h_ema20.remove(0);
                }
            }
        }

        // 3. Gradiente Angular da EMA 20 de 1h nas últimas lookback horas (12h)
        let n_ema = self.history_1h_ema20.len();
        let lookback = self.config.slope_window_hours;
        let ema20_1h_slope_pct = if n_ema >= lookback {
            let past_ema = self.history_1h_ema20[n_ema - lookback];
            let curr_ema = *self.history_1h_ema20.last().unwrap();
            if past_ema > 0.0 {
                ((curr_ema - past_ema) / past_ema) * 100.0
            } else {
                0.0
            }
        } else if n_ema >= 4 {
            let past_ema = self.history_1h_ema20[0];
            let curr_ema = *self.history_1h_ema20.last().unwrap();
            if past_ema > 0.0 {
                ((curr_ema - past_ema) / past_ema) * 100.0
            } else {
                0.0
            }
        } else {
            0.0
        };

        let curr_ema20 = self.history_1h_ema20.last().copied().unwrap_or(candle.close);

        // 4. Distância percentual do preço em relação à VWAP diária
        let vwap_dist_pct = if vwap_val > 0.0 {
            ((candle.close - vwap_val) / vwap_val) * 100.0
        } else {
            0.0
        };

        // 5. Motor de Projeção de Lateralização (LCP Score de 0 a 100)
        // Quanto mais próximo da VWAP e mais flat o slope macro, maior a certeza de lateralização
        let mut score: f64 = 50.0;
        let dist_abs = vwap_dist_pct.abs();

        if dist_abs <= 0.20 {
            score += 30.0; // Perfeito equilíbrio em torno da VWAP
        } else if dist_abs <= self.config.max_vwap_distance_pct {
            score += 15.0; // Dentro da faixa elástica de consolidação
        } else if dist_abs > 1.00 {
            score -= 35.0; // Desancoragem severa
        } else {
            score -= 20.0; // Rompimento da faixa de consolidação
        }

        if ema20_1h_slope_pct.abs() <= 0.03 {
            score += 20.0; // EMA 20 1h perfeitamente flat
        } else if ema20_1h_slope_pct.abs() >= self.config.min_ema20_slope_pct {
            score -= 25.0; // Tendência macro evidente
        }

        let lateral_projector_score = score.clamp(0.0, 100.0);

        // 6. Determinação dos Vetos do Kill-Switch de Deriva Lenta
        let slope_threshold = self.config.min_ema20_slope_pct;
        let max_dist = self.config.max_vwap_distance_pct;

        // Veto Long:
        // Se a EMA 20 de 1h estiver inclinada para baixo por 12h e o preço abaixo da VWAP
        // OU o preço já estiver esticado abaixo da VWAP além do limite de dispersão
        let is_bearish_slow_drift = self.config.enabled
            && ((ema20_1h_slope_pct <= -slope_threshold && candle.close <= vwap_val)
                || (vwap_dist_pct < -max_dist));

        // Veto Short:
        // Se a EMA 20 de 1h estiver inclinada para cima por 12h e o preço acima da VWAP
        // OU o preço já estiver esticado acima da VWAP além do limite de dispersão
        let is_bullish_slow_drift = self.config.enabled
            && ((ema20_1h_slope_pct >= slope_threshold && candle.close >= vwap_val)
                || (vwap_dist_pct > max_dist));

        let regime = if is_bearish_slow_drift {
            MarketDriftRegime::BearishSlowDrift
        } else if is_bullish_slow_drift {
            MarketDriftRegime::BullishSlowDrift
        } else if lateral_projector_score >= 60.0 {
            MarketDriftRegime::TrueEquilibrium
        } else {
            MarketDriftRegime::StretchedExpansion
        };

        let veto_long = is_bearish_slow_drift;
        let veto_short = is_bullish_slow_drift;

        let description = match regime {
            MarketDriftRegime::BearishSlowDrift => format!(
                "KILL-SWITCH ATIVO: Sangria Baixista (Slope 12h: {:+.2}% | VWAP Dist: {:+.2}%) -> LONGS VETADOS",
                ema20_1h_slope_pct, vwap_dist_pct
            ),
            MarketDriftRegime::BullishSlowDrift => format!(
                "KILL-SWITCH ATIVO: Deriva Altista (Slope 12h: {:+.2}% | VWAP Dist: {:+.2}%) -> SHORTS VETADOS",
                ema20_1h_slope_pct, vwap_dist_pct
            ),
            MarketDriftRegime::TrueEquilibrium => format!(
                "LATERALIZAÇÃO REAL CONFIRMADA: Score {:.0}/100 (VWAP Dist: {:+.2}% | Slope: {:+.2}%) -> SCALPS LIBERADOS",
                lateral_projector_score, vwap_dist_pct, ema20_1h_slope_pct
            ),
            MarketDriftRegime::StretchedExpansion => format!(
                "EXPANSÃO DIRECIONAL: Score {:.0}/100 (VWAP Dist: {:+.2}% | Slope: {:+.2}%)",
                lateral_projector_score, vwap_dist_pct, ema20_1h_slope_pct
            ),
        };

        let telem = DriftTelemetry {
            regime,
            session_vwap: vwap_val,
            vwap_dist_pct,
            ema20_1h: curr_ema20,
            ema20_1h_slope_pct,
            lateral_projector_score,
            veto_long,
            veto_short,
            description,
        };

        self.last_telemetry = telem.clone();
        telem
    }

    pub fn last_telemetry(&self) -> &DriftTelemetry {
        &self.last_telemetry
    }
}
