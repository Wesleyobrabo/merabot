use anyhow::Result;
use binance_futures_bot::binance::{BinanceFuturesClient, BinanceFuturesWs};
use binance_futures_bot::config::AppConfig;
use binance_futures_bot::execution::ExecutionEngine;
use binance_futures_bot::risk::RiskManager;
use binance_futures_bot::server::{start_server, AppState};
use binance_futures_bot::strategy::PineSupertrendRsiStrategy;
use binance_futures_bot::types::{BotStatus, Candle};

use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    println!(r#"
========================================================================
   ____  _                            ______      __                      
  / __ )(_)___  ____ _____  ________ / ____/_  __/ /___  __________  _____
 / __  / / __ \/ __ `/ __ \/ ___/ _ \ /_  / / / / __/ / / / ___/ _ \/ ___/
/ /_/ / / / / / /_/ / / / / /__/  __/ __// /_/ / /_/ /_/ / /  /  __(__  ) 
\____/_/_/ /_/\__,_/_/ /_/\___/\___/_/   \__,_/\__/\__,_/_/   \___/____/  
========================================================================
    "#);

    let mut config = AppConfig::load_or_create("config.toml")?;

    info!("Configuracao carregada com sucesso.");
    info!("Par negociado: {}", config.trading.symbol);
    info!("Timeframe: {}", config.trading.interval);
    info!("Alavancagem: {}x ({})", config.trading.leverage, config.trading.margin_type);
    info!("Modo: {}", if config.trading.paper_trading { "PAPER TRADING (SIMULADO)" } else { "LIVE TRADING" });
    info!("Risco por trade: {}% | Max Drawdown: {}%", config.risk.risk_per_trade_percent, config.risk.max_drawdown_percent);

    let rest_client = BinanceFuturesClient::new(
        config.exchange.get_api_key().to_string(),
        config.exchange.get_api_secret().to_string(),
        config.exchange.get_rest_url().to_string(),
    );

    if let Err(e) = rest_client.sync_server_time().await {
        warn!("Aviso: Nao foi possivel sincronizar relogio com a Binance ({}). Utilizando relogio local.", e);
    }

    // 1. Extração dinâmica das regras de lote, precisão, nocional e teto da Binance baseadas no par
    info!("Consultando regras operacionais do par {} na Binance...", config.trading.symbol);
    let mut symbol_rules_opt = None;
    match rest_client.get_symbol_rules(&config.trading.symbol).await {
        Ok(rules) => {
            config.risk.min_order_qty = rules.min_qty;
            config.risk.qty_precision = rules.qty_precision;
            config.risk.price_precision = rules.price_precision;
            config.risk.min_notional_usdt = rules.min_notional;
            if let Some(bracket_cap) = rules.max_notional_bracket {
                if bracket_cap > 0.0 {
                    config.risk.max_position_usdt = config.risk.max_position_usdt.min(bracket_cap);
                }
            }
            info!(
                "[REGRAS BINANCE APLICADAS] Par: {} | Lote Mín: {} | Precisão Qty: {} dec | Precisão Preço: {} dec | Nocional Mín: {:.2} USDT | Teto Posição: {:.2} USDT | Alavancagem Máx: {}x",
                rules.symbol, config.risk.min_order_qty, config.risk.qty_precision, config.risk.price_precision, config.risk.min_notional_usdt, config.risk.max_position_usdt, rules.max_leverage.unwrap_or(50)
            );
            symbol_rules_opt = Some(rules);
        }
        Err(e) => {
            warn!("Aviso: Nao foi possivel carregar regras da Binance dinamicamente ({}). Utilizando configuracoes locais do config.toml.", e);
        }
    }

    // 2. Warm-up inicial dos indicadores com candles historicos das ultimas 24h+ (300 candles)
    let mut initial_candles = Vec::new();
    info!("Buscando candles historicos de {} para aquecimento da estrategia 24/7...", config.trading.interval);
    match rest_client.get_klines_with_fallback(&config.trading.symbol, &config.trading.interval, 300).await {
        Ok(klines) => {
            info!("{} candles historicos carregados com sucesso.", klines.len());
            initial_candles = klines;
        }
        Err(e) => {
            warn!("Nao foi possivel carregar klines historicas via REST: {}. Indicadores iniciarao a frio.", e);
        }
    }

    // Instancia os componentes centrais com as configs sincronizadas
    let mut strategy = PineSupertrendRsiStrategy::new(config.strategy.clone(), config.risk.clone());
    strategy.mera_engine.set_config(config.mera.clone());
    if !initial_candles.is_empty() {
        strategy.warmup(&initial_candles);
    }

    // 2.1 Warm-up da Cascata MTF (1D Macro + 4h Contexto)
    let d1_klines = rest_client.get_klines(&config.trading.symbol, "1d", 120).await.unwrap_or_default();
    let h4_klines = rest_client.get_klines(&config.trading.symbol, "4h", 150).await.unwrap_or_default();
    if !d1_klines.is_empty() || !h4_klines.is_empty() {
        info!("[MTF CASCATA] Aquecidos {} candles diários (1D) e {} candles de 4h da Binance.", d1_klines.len(), h4_klines.len());
        strategy.warmup_htf(d1_klines, h4_klines);
    }

    let mut risk_manager = RiskManager::new(config.risk.clone());
    if let Some(ref rules) = symbol_rules_opt {
        risk_manager.update_from_symbol_rules(rules);
    }
    let mut execution = ExecutionEngine::new(config.clone());
    execution.init().await?;

    let state_execution = Arc::new(RwLock::new(execution));
    let state_risk = Arc::new(RwLock::new(risk_manager));
    let state_strategy = Arc::new(RwLock::new(strategy));
    let state_status = Arc::new(RwLock::new(BotStatus::Running));

    // Senha do dashboard: env > config.toml > gerada automaticamente se o tunnel publico estiver ativo
    let configured_password = std::env::var("DASHBOARD_PASSWORD")
        .ok()
        .or_else(|| config.server.password.clone())
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    let dashboard_password = match configured_password {
        Some(p) => Some(p),
        None if config.server.auto_tunnel => {
            let generated = uuid::Uuid::new_v4().simple().to_string();
            warn!("Nenhuma senha de dashboard configurada. Senha temporaria gerada: {}", generated);
            Some(generated)
        }
        None => {
            warn!("Dashboard SEM senha (defina [server].password ou DASHBOARD_PASSWORD).");
            None
        }
    };

    // Inicializa o servidor Web de Gestao e Dashboard em background
    let app_state = AppState {
        execution: Arc::clone(&state_execution),
        risk_manager: Arc::clone(&state_risk),
        strategy: Arc::clone(&state_strategy),
        status: Arc::clone(&state_status),
        symbol: config.trading.symbol.clone(),
        is_paper: config.trading.paper_trading,
        auth: dashboard_password
            .as_deref()
            .map(|p| Arc::new(binance_futures_bot::auth::AuthState::new(p))),
    };

    let host = config.server.host.clone();
    let port = config.server.port;
    tokio::spawn(async move {
        start_server(app_state, &host, port).await;
    });

    // Canal assincrono para fluxo continuo de candles
    let (candle_tx, mut candle_rx) = mpsc::channel::<Candle>(1000);

    // Inicia stream WebSocket
    let ws_url = config.exchange.get_ws_url().to_string();
    let ws_client = BinanceFuturesWs::new(
        ws_url,
        config.trading.symbol.clone(),
        config.trading.interval.clone(),
    );

    tokio::spawn(async move {
        ws_client.run(candle_tx).await;
    });

    // Sincronizacao periodica do saldo da carteira da Binance (em modo Live)
    if config.trading.auto_sync_balance && !config.trading.paper_trading {
        let sync_exec = Arc::clone(&state_execution);
        let sync_interval_secs = config.trading.sync_balance_interval_secs.max(10);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(sync_interval_secs));
            loop {
                ticker.tick().await;
                let mut exec = sync_exec.write().await;
                match exec.sync_live_balance().await {
                    Ok(Some(info)) => {
                        info!(
                            "[SINCRONIZACAO CARTEIRA] Saldo: {:.2} USDT | Disponivel: {:.2} USDT | Equity: {:.2} USDT",
                            info.total_balance, info.available_balance, exec.account.equity
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("Falha ao sincronizar saldo da carteira com a Binance: {}", e);
                    }
                }
            }
        });
    }

    // Auto-Tunnel Cloudflare (permite abrir no celular de qualquer lugar sem configuracao manual)
    let mut _active_tunnel = None;
    let mut dashboard_url = config.server.public_url.clone().unwrap_or_else(|| {
        let host = if config.server.host == "0.0.0.0" {
            "192.168.1.247"
        } else {
            &config.server.host
        };
        format!("http://{}:{}", host, config.server.port)
    });

    if config.server.auto_tunnel {
        if let Some(tunnel) = binance_futures_bot::tunnel::CloudflareTunnel::start(config.server.port).await {
            dashboard_url = tunnel.url.clone();
            _active_tunnel = Some(tunnel);
        }
    }

    // Dispara notificacao inicial de status operacional (Discord / Telegram)
    {
        let exec = state_execution.read().await;
        exec.notifier.notify_startup(
            &config.trading.symbol,
            &config.trading.interval,
            config.trading.leverage,
            config.trading.paper_trading,
            exec.account.balance,
            config.risk.max_daily_loss_percent,
            config.risk.max_drawdown_percent,
            &dashboard_url,
        );
    }

    info!("Bot operacional. Escutando fluxo de mercado da Binance Futures...");

    // Loop principal de processamento de mercado com Graceful Shutdown
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Sinal de interrupção (Ctrl+C) recebido! Iniciando encerramento gracioso...");
                let exec = state_execution.read().await;
                let _ = exec.storage.save_trades(&exec.trade_history);
                let _ = exec.storage.save_account_state(&exec.account);
                let _ = exec.storage.save_position(&exec.position);
                info!("Estado e posições persistidos com segurança no disco. Encerrando bot.");
                break;
            }
            maybe_candle = candle_rx.recv() => {
                let candle = match maybe_candle {
                    Some(c) => c,
                    None => {
                        warn!("Canal de candles encerrado. Finalizando loop principal.");
                        break;
                    }
                };

                let current_status = *state_status.read().await;
                if current_status != BotStatus::Running {
                    // Continua atualizando precos para dashboard, mas nao gera nem executa ordens novas
                    let mut exec = state_execution.write().await;
                    let mut risk = state_risk.write().await;
                    exec.update_candle(&candle, &mut risk);
                    continue;
                }

                // 1. Atualiza preco, reconcilia execucoes nativas da Binance e recalcula PnL
                {
                    let mut exec = state_execution.write().await;
                    let mut risk = state_risk.write().await;
                    let _ = exec.reconcile_live_position(candle.is_closed).await;
                    exec.update_candle(&candle, &mut risk);
                }

                // 2. Avalia sinal da estrategia
                let current_pos = {
                    let exec = state_execution.read().await;
                    exec.position.clone()
                };

                let signal_opt = {
                    let mut strat = state_strategy.write().await;
                    strat.on_candle(&candle, &current_pos)
                };

                // 3. Executa sinal se emitido
                if let Some(signal) = signal_opt {
                    let mut exec = state_execution.write().await;
                    let mut risk = state_risk.write().await;
                    if let Err(e) = exec.process_signal(signal, &mut risk).await {
                        error!("Erro ao processar sinal de execucao: {}", e);
                    }
                }
            }
        }
    }

    Ok(())
}
