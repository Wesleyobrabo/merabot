use crate::config::RiskConfig;
use crate::types::{AccountState, OrderFeasibility, Signal};
use chrono::{Datelike, Utc};
use tracing::{error, info, warn};

pub struct RiskManager {
    config: RiskConfig,
    is_halted: bool,
    pub symbol_rules: Option<crate::types::SymbolRules>,
}

impl RiskManager {
    pub fn new(config: RiskConfig) -> Self {
        Self {
            config,
            is_halted: false,
            symbol_rules: None,
        }
    }

    pub fn update_from_symbol_rules(&mut self, rules: &crate::types::SymbolRules) {
        self.symbol_rules = Some(rules.clone());
        self.config.min_order_qty = rules.min_qty;
        self.config.qty_precision = rules.qty_precision;
        self.config.price_precision = rules.price_precision;
        self.config.min_notional_usdt = rules.min_notional;
        if let Some(bracket_cap) = rules.max_notional_bracket {
            if bracket_cap > 0.0 {
                self.config.max_position_usdt = self.config.max_position_usdt.min(bracket_cap);
            }
        }
        info!(
            "[RISCO DINÂMICO BINANCE - {}] Lote Mín: {} | Step: {} | Precisão Qty: {} dec | Precisão Preço: {} dec | Nocional Mín: {:.2} USDT | Teto Posição: {:.2} USDT | Alavancagem Máx: {}x",
            rules.symbol,
            self.config.min_order_qty,
            rules.step_size,
            self.config.qty_precision,
            self.config.price_precision,
            self.config.min_notional_usdt,
            self.config.max_position_usdt,
            rules.max_leverage.unwrap_or(50)
        );
    }

    pub fn evaluate_order_feasibility(
        &self,
        account: &AccountState,
        current_price: f64,
        current_leverage: u32,
    ) -> OrderFeasibility {
        let avail = if account.available_balance > 0.0 {
            account.available_balance
        } else {
            account.balance
        };

        let min_notional = self.config.min_notional_usdt.max(5.0);
        let min_qty = self.config.min_order_qty;
        let qty_cost = if current_price > 0.0 { min_qty * current_price } else { 0.0 };
        let min_cost_1x = min_notional.max(qty_cost);

        let lev = current_leverage.max(1);
        let req_margin = min_cost_1x / (lev as f64);
        let can_1x = avail >= min_cost_1x && min_cost_1x > 0.0;
        let can_current = avail >= req_margin && req_margin > 0.0;

        let max_lev = self.symbol_rules.as_ref()
            .and_then(|r| r.max_leverage)
            .unwrap_or(50);

        let min_lev_needed = if avail > 0.0 && min_cost_1x > 0.0 {
            let needed = (min_cost_1x / avail).ceil() as u32;
            needed.max(1)
        } else {
            1
        };

        let max_purchasing_power = avail * (max_lev as f64);

        let diagnosis = if can_1x {
            format!(
                "Saldo suficiente para operar Futuros USDⓈ-M SEM alavancagem (1x Perpétuo com margem 100%). Nocional mínimo: ${:.2} | Saldo disponível: ${:.2} USDT.",
                min_cost_1x, avail
            )
        } else if can_current {
            format!(
                "Saldo insuficiente para 1x, porém VIÁVEL na alavancagem atual ({}x). Margem necessária: ${:.2} | Saldo disponível: ${:.2} USDT (Mínimo exigido: {}x | Máximo permitido: {}x).",
                lev, req_margin, avail, min_lev_needed, max_lev
            )
        } else if min_lev_needed <= max_lev {
            format!(
                "Saldo insuficiente para alavancagem atual ({}x). Requer no mínimo {}x (margem de ${:.2}) ou depósito até ${:.2} USDT. Alavancagem máxima do par: {}x.",
                lev, min_lev_needed, min_cost_1x / (min_lev_needed as f64), min_cost_1x, max_lev
            )
        } else {
            format!(
                "Saldo insuficiente mesmo com alavancagem máxima ({}x). Saldo livre: ${:.2}. Deposite no mínimo ${:.2} USDT para viabilizar novas ordens.",
                max_lev, avail, min_cost_1x / (max_lev as f64)
            )
        };

        OrderFeasibility {
            can_open_unleveraged: can_1x,
            can_open_current_leverage: can_current,
            current_leverage: lev,
            min_notional,
            min_order_qty: min_qty,
            current_price,
            min_order_cost_unleveraged: min_cost_1x,
            required_margin_current: req_margin,
            available_balance: avail,
            min_leverage_required: min_lev_needed,
            max_leverage_allowed: max_lev,
            max_purchasing_power,
            diagnosis_message: diagnosis,
        }
    }

    pub fn config(&self) -> &RiskConfig {
        &self.config
    }

    pub fn check_circuit_breaker(&mut self, account: &mut AccountState) -> bool {
        // 1. Reseta metricas diarias as 00:00 UTC
        let current_day = Utc::now().ordinal();
        if account.last_daily_reset_day != current_day {
            account.last_daily_reset_day = current_day;
            account.daily_starting_balance = account.balance;
            account.daily_realized_pnl = 0.0;
            account.daily_drawdown = 0.0;
            if self.is_halted {
                info!("Novo dia UTC detectado. Circuit Breaker diario redefinido.");
                self.is_halted = false;
            }
        }

        // 2. Checagem de Drawdown Diario (Daily Loss Limit)
        if self.config.enable_daily_loss_limit && account.daily_starting_balance > 0.0 {
            let current_daily_loss = account.daily_starting_balance - account.equity;
            if current_daily_loss > 0.0 {
                let daily_dd_pct = (current_daily_loss / account.daily_starting_balance) * 100.0;
                account.daily_drawdown = daily_dd_pct;
                if daily_dd_pct >= self.config.max_daily_loss_percent {
                    if !self.is_halted {
                        error!(
                            "CIRCUIT BREAKER DIARIO ATIVADO! Perda diaria de {:.2}% atingiu limite de {:.2}%. Entradas bloqueadas ate 00:00 UTC.",
                            daily_dd_pct, self.config.max_daily_loss_percent
                        );
                        self.is_halted = true;
                    }
                    return true;
                }
            }
        }

        // 3. Checagem de Drawdown Historico Maximo (Global)
        if account.equity > account.peak_balance {
            account.peak_balance = account.equity;
        }

        if account.peak_balance > 0.0 {
            let current_dd = ((account.peak_balance - account.equity) / account.peak_balance) * 100.0;
            account.max_drawdown = account.max_drawdown.max(current_dd);

            if current_dd >= self.config.max_drawdown_percent {
                if !self.is_halted {
                    error!(
                        "CIRCUIT BREAKER GLOBAL ATIVADO! Drawdown de {:.2}% atingiu o limite maximo de {:.2}%. Novas ordens bloqueadas.",
                        current_dd, self.config.max_drawdown_percent
                    );
                    self.is_halted = true;
                }
                return true;
            }
        }

        self.is_halted = false;
        false
    }

    pub fn calculate_order_size(
        &self,
        account: &AccountState,
        signal: &Signal,
        leverage: u32,
    ) -> Option<f64> {
        if self.is_halted {
            warn!("Risco: Ordem rejeitada devido ao Circuit Breaker ativo.");
            return None;
        }

        let sl_price = signal.stop_loss?;
        let price_diff = (signal.price - sl_price).abs();

        if price_diff <= 0.0 {
            warn!("Risco: Distancia de Stop Loss invalida ou zero.");
            return None;
        }

        // Risco financeiro maximo tolerado por operacao (em USDT)
        let risk_usdt = account.equity * (self.config.risk_per_trade_percent / 100.0);
        
        // Quantidade teorica baseada na distancia do stop loss
        let mut target_qty = risk_usdt / price_diff;

        // Limita ao valor maximo nocional de posicao configurado
        let position_value = target_qty * signal.price;
        if position_value > self.config.max_position_usdt {
            target_qty = self.config.max_position_usdt / signal.price;
        }

        // Verifica capacidade de margem com a alavancagem
        let margin_required = (target_qty * signal.price) / (leverage as f64);
        if margin_required > (account.equity * 0.95) {
            // Evita consumir 100% da margem disponivel
            target_qty = (account.equity * 0.95 * leverage as f64) / signal.price;
        }

        // Arredonda para a precisao correta da exchange
        let factor = 10f64.powi(self.config.qty_precision as i32);
        let mut rounded_qty = (target_qty * factor).floor() / factor;

        // Regra de Nocional Minimo da Binance Futures com Buffer de Seguranca (padrao 5.50 USDT)
        let min_notional = self.config.min_notional_usdt.max(5.0);
        let notional_value = rounded_qty * signal.price;
        if notional_value < min_notional {
            // Calibra quantidade para atingir o valor nocional seguro configurado
            let min_qty_needed = (min_notional / signal.price * factor).ceil() / factor;
            let required_margin = (min_qty_needed * signal.price) / (leverage as f64);
            let meets_max_pos = (min_qty_needed * signal.price) <= self.config.max_position_usdt;
            let available_margin = account.available_balance.min(account.equity * 0.95);
            let meets_margin = required_margin <= available_margin;

            if meets_max_pos && meets_margin {
                info!(
                    "Risco: Quantidade calibrada de {:.4} para {:.4} para cumprir Buffer de Nocional de {:.2} USDT (Margem: {:.2} USDT)",
                    rounded_qty, min_qty_needed, min_notional, required_margin
                );
                rounded_qty = min_qty_needed;
            } else {
                warn!(
                    "Risco: Nocional ({:.2} USDT) abaixo do piso de {:.2} USDT e margem insuficiente ({:.2} USDT necessarios com saldo livre de {:.2} USDT).",
                    notional_value, min_notional, required_margin, available_margin
                );
                return None;
            }
        }

        if rounded_qty < self.config.min_order_qty {
            warn!(
                "Risco: Quantidade calculada ({:.4}) menor que o lote minimo ({:.4}).",
                rounded_qty, self.config.min_order_qty
            );
            return None;
        }

        Some(rounded_qty)
    }

    pub fn is_halted(&self) -> bool {
        self.is_halted
    }

    pub fn reset_circuit_breaker(&mut self) {
        self.is_halted = false;
    }

    pub fn set_min_notional(&mut self, notional: f64) {
        self.config.min_notional_usdt = notional.max(5.0);
        info!("Risco: Buffer Minimo Nocional ajustado para {:.2} USDT", self.config.min_notional_usdt);
    }

    pub fn set_risk_per_trade(&mut self, pct: f64) {
        self.config.risk_per_trade_percent = pct.clamp(0.5, 25.0);
        info!("Risco: Percentual de risco por trade ajustado para {:.2}%", self.config.risk_per_trade_percent);
    }

    pub fn min_notional(&self) -> f64 {
        self.config.min_notional_usdt
    }

    pub fn risk_per_trade(&self) -> f64 {
        self.config.risk_per_trade_percent
    }
}
