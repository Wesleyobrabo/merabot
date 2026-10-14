use crate::binance::BinanceFuturesClient;
use crate::config::AppConfig;
use crate::notifier::Notifier;
use crate::risk::RiskManager;
use crate::storage::StorageManager;
use crate::types::{
    AccountState, Candle, OrderSide, Position, PositionSide, Signal, SignalType, TradeRecord,
};
use anyhow::Result;
use chrono::Utc;
use tracing::{info, warn};
use uuid::Uuid;

pub struct ExecutionEngine {
    pub config: AppConfig,
    client: Option<BinanceFuturesClient>,
    pub account: AccountState,
    pub position: Position,
    pub trade_history: Vec<TradeRecord>,
    pub last_candle: Option<Candle>,
    pub notifier: Notifier,
    pub storage: StorageManager,
}

impl ExecutionEngine {
    pub fn new(config: AppConfig) -> Self {
        let client = if !config.trading.paper_trading {
            Some(BinanceFuturesClient::new(
                config.exchange.api_key.clone(),
                config.exchange.api_secret.clone(),
                config.exchange.get_rest_url().to_string(),
            ))
        } else {
            None
        };

        let storage = StorageManager::new("data");
        let initial_balance = config.trading.initial_paper_balance;
        let position = Position::new(config.trading.symbol.clone(), config.trading.leverage);
        let notifier = Notifier::new(config.notifications.clone());

        // Recupera historico e estado salvo em disco se existirem
        let trade_history = storage.load_trades();
        let account = storage.load_account_state().unwrap_or_else(|| AccountState::new(initial_balance));

        Self {
            config,
            client,
            account,
            position,
            trade_history,
            last_candle: None,
            notifier,
            storage,
        }
    }

    pub fn new_with_storage(config: AppConfig, storage: crate::storage::StorageManager, initial_balance: f64) -> Self {
        let position = Position::new(config.trading.symbol.clone(), config.trading.leverage);
        let notifier = Notifier::new(config.notifications.clone());
        let account = AccountState::new(initial_balance);
        Self {
            config,
            client: None,
            account,
            position,
            trade_history: Vec::new(),
            last_candle: None,
            notifier,
            storage,
        }
    }

    pub async fn init(&mut self) -> Result<()> {
        if let Some(ref client) = self.client {
            info!("Inicializando conexao live com Binance Futures...");

            // Sincroniza alavancagem e margem
            let _ = client
                .set_margin_type(&self.config.trading.symbol, &self.config.trading.margin_type)
                .await;
            let _ = client
                .set_leverage(&self.config.trading.symbol, self.config.trading.leverage)
                .await;

            // Busca informacoes completas da carteira
            match client.get_account_wallet_info().await {
                Ok(info) => {
                    info!(
                        "Carteira Live conectada: Saldo Total: {:.2} USDT | Disponivel: {:.2} USDT | PnL Nao Realizado: {:.2} USDT",
                        info.total_balance, info.available_balance, info.unrealized_pnl
                    );
                    self.account.balance = info.total_balance;
                    self.account.available_balance = info.available_balance;
                    self.account.equity = info.total_balance + self.position.unrealized_pnl;
                    if self.account.equity > self.account.peak_balance {
                        self.account.peak_balance = self.account.equity;
                    }
                }
                Err(e) => {
                    warn!("Nao foi possivel carregar saldo live: {}. Mantendo saldo anterior.", e);
                }
            }

            // Verifica se ja ha posicao aberta
            if let Ok(Some(live_pos)) = client.get_position_risk(&self.config.trading.symbol).await {
                if live_pos.amount > 0.0 {
                    info!(
                        "Posicao aberta existente detectada: {:?} {} @ {:.2}",
                        live_pos.side, live_pos.amount, live_pos.entry_price
                    );
                    self.position = live_pos;
                    self.ensure_active_stop_loss();
                }
            }
        } else {
            // Modo Paper Trading
            if self.config.trading.fetch_real_balance_on_paper && !self.config.exchange.api_key.trim().is_empty() {
                info!("Consultando carteira real na Binance para calibrar saldo inicial do Paper Trading...");
                let query_client = BinanceFuturesClient::new(
                    self.config.exchange.api_key.clone(),
                    self.config.exchange.api_secret.clone(),
                    self.config.exchange.get_rest_url().to_string(),
                );
                match query_client.get_account_wallet_info().await {
                    Ok(info) => {
                        info!(
                            "Saldo real consultado com sucesso: {:.2} USDT (Disponivel: {:.2} USDT). Aplicando no Paper Trading.",
                            info.total_balance, info.available_balance
                        );
                        self.account.balance = info.total_balance;
                        self.account.available_balance = info.available_balance;
                        self.account.equity = info.total_balance;
                    }
                    Err(e) => {
                        warn!("Falha ao consultar saldo real na Binance: {}. Usando saldo configurado.", e);
                    }
                }
            } else {
                info!(
                    "Modo Paper Trading ativo com saldo virtual de {:.2} USDT",
                    self.account.balance
                );
            }
        }

        Ok(())
    }

    pub async fn reconcile_live_position(&mut self) -> Result<()> {
        let client = match self.client {
            Some(ref c) => c.clone(),
            None => return Ok(()),
        };

        if self.position.amount > 0.0 {
            if let Ok(Some(live_pos)) = client.get_position_risk(&self.config.trading.symbol).await {
                if live_pos.amount == 0.0 {
                    // Posicao foi fechada diretamente na Binance via Stop Loss ou Take Profit nativo
                    warn!(
                        "RECONCILIACAO: Posicao {:?} em {} fechou diretamente na Binance! Processando encerramento...",
                        self.position.side, self.config.trading.symbol
                    );
                    let exit_price = self.last_candle.as_ref().map(|c| c.close).unwrap_or(self.position.current_price);
                    let pnl = match self.position.side {
                        PositionSide::Long => (exit_price - self.position.entry_price) * self.position.amount,
                        PositionSide::Short => (self.position.entry_price - exit_price) * self.position.amount,
                        PositionSide::None => 0.0,
                    };
                    let side = match self.position.side {
                        PositionSide::Long => OrderSide::Sell,
                        PositionSide::Short => OrderSide::Buy,
                        PositionSide::None => OrderSide::Sell,
                    };

                    let entry_price = self.position.entry_price;
                    let qty = self.position.amount;

                    self.record_trade(
                        side,
                        entry_price,
                        exit_price,
                        qty,
                        pnl,
                        "Stop Loss / Take Profit Nativo Executado na Binance".to_string(),
                    ).await;

                    // Cancela quaisquer ordens residuais na corretora
                    let _ = client.cancel_all_open_orders(&self.config.trading.symbol).await;

                    self.position.side = PositionSide::None;
                    self.position.amount = 0.0;
                    self.position.entry_price = 0.0;
                    self.position.unrealized_pnl = 0.0;
                    self.position.stop_loss = None;
                    self.position.take_profit = None;
                } else {
                    let prev_sl = self.position.stop_loss;
                    self.position.current_price = live_pos.current_price;
                    self.position.unrealized_pnl = live_pos.unrealized_pnl;
                    if self.position.stop_loss.is_none() {
                        self.position.stop_loss = prev_sl;
                        self.ensure_active_stop_loss();
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn sync_live_balance(&mut self) -> Result<Option<crate::types::WalletInfo>> {
        // Primeiro reconcilia posicao aberta para evitar dessincronizacao de PnL
        let _ = self.reconcile_live_position().await;

        if let Some(ref client) = self.client {
            let info = client.get_account_wallet_info().await?;
            self.account.balance = info.total_balance;
            self.account.available_balance = info.available_balance;
            self.account.equity = info.total_balance + self.position.unrealized_pnl;
            if self.account.equity > self.account.peak_balance {
                self.account.peak_balance = self.account.equity;
            }
            return Ok(Some(info));
        } else if !self.config.exchange.api_key.trim().is_empty() {
            // Em paper trading, se as chaves da Binance foram informadas, consulta a carteira real
            let query_client = BinanceFuturesClient::new(
                self.config.exchange.api_key.clone(),
                self.config.exchange.api_secret.clone(),
                self.config.exchange.get_rest_url().to_string(),
            );
            if let Ok(info) = query_client.get_account_wallet_info().await {
                return Ok(Some(info));
            }
        }
        Ok(None)
    }

    pub fn ensure_active_stop_loss(&mut self) {
        if self.position.amount <= 0.0 || self.position.side == PositionSide::None {
            return;
        }

        let needs_sl = match self.position.stop_loss {
            Some(sl) => sl <= 0.0,
            None => true,
        };

        if needs_sl && self.position.entry_price > 0.0 {
            let sl_pct = self.config.risk.stop_loss_percent.max(0.5);
            let calculated_sl = match self.position.side {
                PositionSide::Long => self.position.entry_price * (1.0 - (sl_pct / 100.0)),
                PositionSide::Short => self.position.entry_price * (1.0 + (sl_pct / 100.0)),
                PositionSide::None => 0.0,
            };

            if calculated_sl > 0.0 {
                self.position.stop_loss = Some(calculated_sl);
                warn!(
                    "BLINDAGEM DE RISCO: Posição {:?} sem Stop Loss detectada! Stop Loss de proteção obrigatório ({:.1}%) ativado em {:.2}",
                    self.position.side, sl_pct, calculated_sl
                );

                if let Some(ref client) = self.client {
                    let client_clone = client.clone();
                    let symbol = self.config.trading.symbol.clone();
                    let side = match self.position.side {
                        PositionSide::Long => OrderSide::Sell,
                        PositionSide::Short => OrderSide::Buy,
                        PositionSide::None => OrderSide::Sell,
                    };
                    let price_precision = self.config.risk.price_precision;
                    tokio::spawn(async move {
                        let cid = format!("heal_sl_{}", Uuid::new_v4().simple());
                        let res = client_clone
                            .place_stop_market_order(&symbol, side, calculated_sl, Some(cid), price_precision)
                            .await;
                        match res {
                            Ok(_) => info!("Stop Loss obrigatório posicionado com sucesso na Binance: {:.2}", calculated_sl),
                            Err(e) => warn!("Falha ao registrar Stop Loss na Binance ({}). Monitor de emergência local ativo.", e),
                        }
                    });
                }
            }
        }
    }

    pub fn update_candle(&mut self, candle: &Candle, risk_mgr: &mut RiskManager) {
        self.last_candle = Some(candle.clone());
        self.position.update_pnl(candle.close);
        self.account.equity = self.account.balance + self.position.unrealized_pnl;

        // Auto-Heal: Garante que NENHUMA operação fique sem Stop Loss em nenhum momento
        self.ensure_active_stop_loss();

        // Atualiza pico de saldo e verifica circuit breaker
        let circuit_active = risk_mgr.check_circuit_breaker(&mut self.account);
        if circuit_active {
            self.notifier.notify_circuit_breaker(self.account.max_drawdown, self.config.risk.max_drawdown_percent);
        }

        // Breakeven Dinamico (Priority 3)
        if self.config.risk.enable_breakeven && self.position.amount > 0.0 {
            let trigger_distance = self.position.entry_price * (self.config.risk.breakeven_atr_multiplier * 0.008);
            match self.position.side {
                PositionSide::Long => {
                    let should_be = candle.close >= (self.position.entry_price + trigger_distance);
                    let current_sl = self.position.stop_loss.unwrap_or(0.0);
                    if should_be && current_sl < self.position.entry_price {
                        let new_sl = self.position.entry_price * 1.0005; // 0.05% acima para taxas
                        info!(
                            "[BREAKEVEN] Long atingiu alvo de protecao (+{:.2}%). Movendo SL para {:.2}",
                            (candle.close - self.position.entry_price) / self.position.entry_price * 100.0,
                            new_sl
                        );
                        self.position.stop_loss = Some(new_sl);
                        self.notifier.notify_breakeven(&self.config.trading.symbol, new_sl);

                        if let Some(ref client) = self.client {
                            let client_clone = client.clone();
                            let symbol = self.config.trading.symbol.clone();
                            let price_precision = self.config.risk.price_precision;
                            tokio::spawn(async move {
                                let _ = client_clone.cancel_all_open_orders(&symbol).await;
                                let cid = format!("be_{}", Uuid::new_v4().simple());
                                let _ = client_clone.place_stop_market_order(&symbol, OrderSide::Sell, new_sl, Some(cid), price_precision).await;
                            });
                        }
                    }
                }
                PositionSide::Short => {
                    let should_be = candle.close <= (self.position.entry_price - trigger_distance);
                    let current_sl = self.position.stop_loss.unwrap_or(f64::MAX);
                    if should_be && current_sl > self.position.entry_price {
                        let new_sl = self.position.entry_price * 0.9995; // 0.05% abaixo para taxas
                        info!(
                            "[BREAKEVEN] Short atingiu alvo de protecao (+{:.2}%). Movendo SL para {:.2}",
                            (self.position.entry_price - candle.close) / self.position.entry_price * 100.0,
                            new_sl
                        );
                        self.position.stop_loss = Some(new_sl);
                        self.notifier.notify_breakeven(&self.config.trading.symbol, new_sl);

                        if let Some(ref client) = self.client {
                            let client_clone = client.clone();
                            let symbol = self.config.trading.symbol.clone();
                            let price_precision = self.config.risk.price_precision;
                            tokio::spawn(async move {
                                let _ = client_clone.cancel_all_open_orders(&symbol).await;
                                let cid = format!("be_{}", Uuid::new_v4().simple());
                                let _ = client_clone.place_stop_market_order(&symbol, OrderSide::Buy, new_sl, Some(cid), price_precision).await;
                            });
                        }
                    }
                }
                PositionSide::None => {}
            }
        }
    }

    pub async fn process_signal(&mut self, signal: Signal, risk_mgr: &mut RiskManager) -> Result<()> {
        match signal.signal_type {
            SignalType::OpenLong => {
                if self.position.side != PositionSide::None {
                    return Ok(());
                }

                if let Some(qty) = risk_mgr.calculate_order_size(&self.account, &signal, self.config.trading.leverage) {
                    info!(
                        "[LONG] Abrindo posicao: Qty: {:.4} @ {:.2} | Motivo: {}",
                        qty, signal.price, signal.reason
                    );

                    let mut executed_price = signal.price;

                    if let Some(ref client) = self.client {
                        // Cancela qualquer ordem residual anterior
                        let _ = client.cancel_all_open_orders(&self.config.trading.symbol).await;

                        let cid = format!("bot_{}", Uuid::new_v4().simple());
                        let order_resp = client
                            .place_market_order(
                                &self.config.trading.symbol,
                                OrderSide::Buy,
                                qty,
                                false,
                                Some(cid),
                                self.config.risk.qty_precision,
                            )
                            .await?;

                        // Priority 1: Extrai preco medio real preenchido pela Binance
                        let avg_price: f64 = order_resp
                            .get("avgPrice")
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0.0);
                        if avg_price > 0.0 {
                            executed_price = avg_price;
                            info!("Ordem Long executada com preco medio real: {:.2}", executed_price);
                        }

                        // Blindagem de Risco: Stop Loss obrigatório na abertura
                        let sl_pct = self.config.risk.stop_loss_percent.max(0.5);
                        let sl_target = signal.stop_loss.unwrap_or_else(|| executed_price * (1.0 - (sl_pct / 100.0)));
                        let sl_cid = format!("sl_{}", Uuid::new_v4().simple());
                        let _ = client
                            .place_stop_market_order(
                                &self.config.trading.symbol,
                                OrderSide::Sell,
                                sl_target,
                                Some(sl_cid),
                                self.config.risk.price_precision,
                            )
                            .await;

                        // Priority 1: Posiciona Take Profit nativo TAKE_PROFIT_MARKET na exchange
                        if let Some(tp) = signal.take_profit {
                            let tp_cid = format!("tp_{}", Uuid::new_v4().simple());
                            let _ = client
                                .place_take_profit_market_order(
                                    &self.config.trading.symbol,
                                    OrderSide::Sell,
                                    tp,
                                    Some(tp_cid),
                                    self.config.risk.price_precision,
                                )
                                .await;
                        }
                    }

                    let sl_pct = self.config.risk.stop_loss_percent.max(0.5);
                    let final_sl = signal.stop_loss.unwrap_or_else(|| executed_price * (1.0 - (sl_pct / 100.0)));

                    self.position.side = PositionSide::Long;
                    self.position.amount = qty;
                    self.position.entry_price = executed_price;
                    self.position.current_price = executed_price;
                    self.position.stop_loss = Some(final_sl);
                    self.position.take_profit = signal.take_profit;
                    self.position.unrealized_pnl = 0.0;

                    let margin = (executed_price * qty) / (self.config.trading.leverage as f64);
                    self.account.available_balance = (self.account.balance - margin).max(0.0);

                    // Priority 2: Notifica abertura do trade
                    self.notifier.notify_trade_opened(
                        &self.config.trading.symbol,
                        "LONG",
                        executed_price,
                        qty,
                        signal.stop_loss,
                        signal.take_profit,
                        self.config.trading.leverage,
                        self.config.trading.paper_trading,
                    );
                }
            }
            SignalType::OpenShort => {
                if self.position.side != PositionSide::None {
                    return Ok(());
                }

                if let Some(qty) = risk_mgr.calculate_order_size(&self.account, &signal, self.config.trading.leverage) {
                    info!(
                        "[SHORT] Abrindo posicao: Qty: {:.4} @ {:.2} | Motivo: {}",
                        qty, signal.price, signal.reason
                    );

                    let mut executed_price = signal.price;

                    if let Some(ref client) = self.client {
                        // Cancela qualquer ordem residual anterior
                        let _ = client.cancel_all_open_orders(&self.config.trading.symbol).await;

                        let cid = format!("bot_{}", Uuid::new_v4().simple());
                        let order_resp = client
                            .place_market_order(
                                &self.config.trading.symbol,
                                OrderSide::Sell,
                                qty,
                                false,
                                Some(cid),
                                self.config.risk.qty_precision,
                            )
                            .await?;

                        // Priority 1: Extrai preco medio real preenchido pela Binance
                        let avg_price: f64 = order_resp
                            .get("avgPrice")
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0.0);
                        if avg_price > 0.0 {
                            executed_price = avg_price;
                            info!("Ordem Short executada com preco medio real: {:.2}", executed_price);
                        }

                        // Blindagem de Risco: Stop Loss obrigatório na abertura
                        let sl_pct = self.config.risk.stop_loss_percent.max(0.5);
                        let sl_target = signal.stop_loss.unwrap_or_else(|| executed_price * (1.0 + (sl_pct / 100.0)));
                        let sl_cid = format!("sl_{}", Uuid::new_v4().simple());
                        let _ = client
                            .place_stop_market_order(
                                &self.config.trading.symbol,
                                OrderSide::Buy,
                                sl_target,
                                Some(sl_cid),
                                self.config.risk.price_precision,
                            )
                            .await;

                        // Priority 1: Posiciona Take Profit nativo TAKE_PROFIT_MARKET na exchange
                        if let Some(tp) = signal.take_profit {
                            let tp_cid = format!("tp_{}", Uuid::new_v4().simple());
                            let _ = client
                                .place_take_profit_market_order(
                                    &self.config.trading.symbol,
                                    OrderSide::Buy,
                                    tp,
                                    Some(tp_cid),
                                    self.config.risk.price_precision,
                                )
                                .await;
                        }
                    }

                    let sl_pct = self.config.risk.stop_loss_percent.max(0.5);
                    let final_sl = signal.stop_loss.unwrap_or_else(|| executed_price * (1.0 + (sl_pct / 100.0)));

                    self.position.side = PositionSide::Short;
                    self.position.amount = qty;
                    self.position.entry_price = executed_price;
                    self.position.current_price = executed_price;
                    self.position.stop_loss = Some(final_sl);
                    self.position.take_profit = signal.take_profit;
                    self.position.unrealized_pnl = 0.0;

                    let margin = (executed_price * qty) / (self.config.trading.leverage as f64);
                    self.account.available_balance = (self.account.balance - margin).max(0.0);

                    // Priority 2: Notifica abertura do trade
                    self.notifier.notify_trade_opened(
                        &self.config.trading.symbol,
                        "SHORT",
                        executed_price,
                        qty,
                        signal.stop_loss,
                        signal.take_profit,
                        self.config.trading.leverage,
                        self.config.trading.paper_trading,
                    );
                }
            }
            SignalType::CloseLong => {
                if self.position.side == PositionSide::Long && self.position.amount > 0.0 {
                    let qty = self.position.amount;
                    let pnl = (signal.price - self.position.entry_price) * qty;

                    info!(
                        "[FECHAR LONG] Preco: {:.2} | PnL: {:.2} USDT | Motivo: {}",
                        signal.price, pnl, signal.reason
                    );

                    if let Some(ref client) = self.client {
                        let cid = format!("close_{}", Uuid::new_v4().simple());
                        let _ = client
                            .place_market_order(
                                &self.config.trading.symbol,
                                OrderSide::Sell,
                                qty,
                                true,
                                Some(cid),
                                self.config.risk.qty_precision,
                            )
                            .await;

                        // Priority 1: Cancela ordens pendentes de SL/TP na exchange
                        let _ = client.cancel_all_open_orders(&self.config.trading.symbol).await;
                    }

                    let entry_price = self.position.entry_price;
                    self.record_trade(OrderSide::Sell, entry_price, signal.price, qty, pnl, signal.reason).await;
                    self.position.side = PositionSide::None;
                    self.position.amount = 0.0;
                    self.position.entry_price = 0.0;
                    self.position.unrealized_pnl = 0.0;
                    self.position.stop_loss = None;
                    self.position.take_profit = None;
                }
            }
            SignalType::CloseShort => {
                if self.position.side == PositionSide::Short && self.position.amount > 0.0 {
                    let qty = self.position.amount;
                    let pnl = (self.position.entry_price - signal.price) * qty;

                    info!(
                        "[FECHAR SHORT] Preco: {:.2} | PnL: {:.2} USDT | Motivo: {}",
                        signal.price, pnl, signal.reason
                    );

                    if let Some(ref client) = self.client {
                        let cid = format!("close_{}", Uuid::new_v4().simple());
                        let _ = client
                            .place_market_order(
                                &self.config.trading.symbol,
                                OrderSide::Buy,
                                qty,
                                true,
                                Some(cid),
                                self.config.risk.qty_precision,
                            )
                            .await;

                        // Priority 1: Cancela ordens pendentes de SL/TP na exchange
                        let _ = client.cancel_all_open_orders(&self.config.trading.symbol).await;
                    }

                    let entry_price = self.position.entry_price;
                    self.record_trade(OrderSide::Buy, entry_price, signal.price, qty, pnl, signal.reason).await;
                    self.position.side = PositionSide::None;
                    self.position.amount = 0.0;
                    self.position.entry_price = 0.0;
                    self.position.unrealized_pnl = 0.0;
                    self.position.stop_loss = None;
                    self.position.take_profit = None;
                }
            }
            SignalType::Hold => {}
        }

        Ok(())
    }

    async fn record_trade(
        &mut self,
        side: OrderSide,
        entry_price: f64,
        exit_price: f64,
        quantity: f64,
        estimated_pnl: f64,
        reason: String,
    ) {
        let notional = quantity * exit_price;
        let margin = if self.position.leverage > 0 { notional / (self.position.leverage as f64) } else { notional / 5.0 };

        // Tenta conciliar valores oficiais de execução direto da Binance
        let mut final_exit_price = exit_price;
        let mut final_pnl = estimated_pnl;
        let mut commission = (entry_price * quantity + exit_price * quantity) * 0.0005; // 0.05% taker

        if let Some(ref client) = self.client {
            if !self.config.trading.paper_trading {
                tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
                if let Ok(trades) = client.get_user_trades(&self.config.trading.symbol, 5).await {
                    if let Some(last_trade) = trades.first() {
                        if let Some(p_str) = last_trade.get("price").and_then(|v| v.as_str()) {
                            if let Ok(p) = p_str.parse::<f64>() {
                                final_exit_price = p;
                            }
                        }
                        if let Some(comm_str) = last_trade.get("commission").and_then(|v| v.as_str()) {
                            if let Ok(c) = comm_str.parse::<f64>() {
                                commission = c;
                            }
                        }
                        if let Some(pnl_str) = last_trade.get("realizedPnl").and_then(|v| v.as_str()) {
                            if let Ok(rpnl) = pnl_str.parse::<f64>() {
                                if rpnl != 0.0 {
                                    final_pnl = rpnl;
                                }
                            }
                        }
                        info!(
                            "BINANCE TRADE CONCILIADO: Preço Real: ${:.2} | PnL Realizado: {:+.4}$ | Comissão Binance: -${:.4}",
                            final_exit_price, final_pnl, commission
                        );
                    }
                }
            }
        }

        let gross_pnl = match side {
            OrderSide::Sell => (final_exit_price - entry_price) * quantity,
            OrderSide::Buy => (entry_price - final_exit_price) * quantity,
        };

        // PnL Líquido = PnL oficial menos taxas
        let net_pnl = if final_pnl != estimated_pnl {
            final_pnl - commission
        } else {
            gross_pnl - commission
        };

        let pnl_pct = if margin > 0.0 { (net_pnl / margin) * 100.0 } else { 0.0 };

        self.account.balance += net_pnl;
        self.account.available_balance = self.account.balance;
        self.account.equity = self.account.balance;
        self.account.total_trades += 1;

        if net_pnl >= 0.0 {
            self.account.winning_trades += 1;
        } else {
            self.account.losing_trades += 1;
        }

        let record = TradeRecord {
            id: Uuid::new_v4().to_string(),
            symbol: self.config.trading.symbol.clone(),
            side,
            entry_price,
            price: final_exit_price,
            quantity,
            notional,
            gross_pnl,
            commission,
            realized_pnl: net_pnl,
            pnl_percent: pnl_pct,
            balance_after: self.account.balance,
            timestamp: Utc::now().timestamp_millis(),
            reason: reason.clone(),
        };

        // Priority 2: Notifica encerramento
        self.notifier.notify_trade_closed(
            &self.config.trading.symbol,
            if side == OrderSide::Sell { "LONG" } else { "SHORT" },
            entry_price,
            final_exit_price,
            quantity,
            net_pnl,
            &reason,
            self.config.trading.paper_trading,
        );

        self.trade_history.push(record);

        // Priority 4: Persistencia em disco
        let _ = self.storage.save_trades(&self.trade_history);
        let _ = self.storage.save_account_state(&self.account);
    }

    pub async fn emergency_close_all(&mut self) -> Result<()> {
        info!("BOTAO DE PANICO ACIONADO: Fechando todas as posicoes imediatamente!");

        if let Some(ref client) = self.client {
            let _ = client.cancel_all_open_orders(&self.config.trading.symbol).await;

            if self.position.amount > 0.0 {
                let close_side = match self.position.side {
                    PositionSide::Long => OrderSide::Sell,
                    PositionSide::Short => OrderSide::Buy,
                    PositionSide::None => OrderSide::Sell,
                };

                let cid = format!("panic_{}", Uuid::new_v4().simple());
                let _ = client
                    .place_market_order(
                        &self.config.trading.symbol,
                        close_side,
                        self.position.amount,
                        true,
                        Some(cid),
                        self.config.risk.qty_precision,
                    )
                    .await;
            }
        }

        if self.position.amount > 0.0 {
            let mark_price = self.last_candle.as_ref().map(|c| c.close).unwrap_or(self.position.entry_price);
            let pnl = self.position.unrealized_pnl;
            let entry_price = self.position.entry_price;
            self.record_trade(
                OrderSide::Sell,
                entry_price,
                mark_price,
                self.position.amount,
                pnl,
                "Emergency Close Manual".to_string(),
            ).await;
        }

        self.position.side = PositionSide::None;
        self.position.amount = 0.0;
        self.position.unrealized_pnl = 0.0;
        self.position.stop_loss = None;
        self.position.take_profit = None;

        Ok(())
    }

    pub async fn set_leverage(&mut self, leverage: u32) -> Result<u32> {
        if let Some(ref client) = self.client {
            client.set_leverage(&self.config.trading.symbol, leverage).await?;
        }
        self.config.trading.leverage = leverage;
        self.position.leverage = leverage;
        info!(
            "Alavancagem atualizada para {}x no par {} (Sincronizada na Binance)",
            leverage, self.config.trading.symbol
        );
        Ok(leverage)
    }
}
