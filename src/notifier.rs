use crate::config::NotificationConfig;
use reqwest::Client;
use serde_json::json;
use std::sync::Arc;
use tracing::{error, info};

#[derive(Clone)]
pub struct Notifier {
    config: Arc<NotificationConfig>,
    client: Client,
}

impl Notifier {
    pub fn new(config: NotificationConfig) -> Self {
        Self {
            config: Arc::new(config),
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
        }
    }

    #[allow(dead_code)]
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn notify_startup(
        &self,
        symbol: &str,
        interval: &str,
        leverage: u32,
        is_paper: bool,
        balance: f64,
        daily_loss_limit: f64,
        max_drawdown: f64,
        dashboard_url: &str,
    ) {
        if !self.config.enabled {
            return;
        }

        let mode_tag = if is_paper { "🧪 PAPER TRADING (SIMULADO)" } else { "🚀 LIVE TRADING (REAL)" };
        
        let telegram_msg = format!(
            "🟢 <b>BINANCE FUTURES BOT OPERACIONAL</b>\n\
            ━━━━━━━━━━━━━━━━━━━━\n\
            <b>Modo:</b> {}\n\
            <b>Par:</b> {} (Gráfico: {})\n\
            <b>Alavancagem:</b> {}x\n\
            <b>Saldo da Carteira:</b> ${:.2} USDT\n\
            <b>Limite Diário (Circuit Breaker):</b> {:.1}%\n\
            <b>Drawdown Máximo Global:</b> {:.1}%\n\
            <b>Módulos Ativos:</b>\n\
            • Stop Loss & Take Profit Nativos\n\
            • Trailing Breakeven Dinâmico\n\
            • Filtro de Volume (SMA 20)\n\
            • Watchdog Anti-Queda de WebSocket\n\
            • Reconciliação Automática de Ordens\n\
            ━━━━━━━━━━━━━━━━━━━━\n\
            🔗 <b>Dashboard Web:</b> <a href=\"{}\">Abrir Painel</a>\n\
            <i>Todos os sistemas prontos e monitorando candles.</i>",
            mode_tag, symbol, interval, leverage, balance, daily_loss_limit, max_drawdown, dashboard_url
        );

        let discord_embed = json!({
            "title": "🟢 BINANCE FUTURES BOT OPERACIONAL",
            "url": dashboard_url,
            "color": 65280,
            "description": format!(
                "**Modo:** {}\n\
                **Par:** {} (Gráfico: {})\n\
                **Alavancagem:** {}x\n\
                **Saldo da Carteira:** ${:.2} USDT\n\
                **Limite Diário:** {:.1}% | **Max Drawdown:** {:.1}%\n\n\
                ━━━━━━━━━━━━━━━━━━━━\n\
                📲 [**CLIQUE AQUI PARA ABRIR O DASHBOARD NO NAVEGADOR**]({})\n\
                ━━━━━━━━━━━━━━━━━━━━\n\
                *Todos os sistemas prontos e monitorando candles.*",
                mode_tag, symbol, interval, leverage, balance, daily_loss_limit, max_drawdown, dashboard_url
            ),
            "footer": {
                "text": "Binance Futures Algorithmic Bot"
            }
        });

        self.broadcast_with_discord_embed(telegram_msg, discord_embed);
    }

    pub fn notify_trade_opened(
        &self,
        symbol: &str,
        side: &str,
        price: f64,
        qty: f64,
        sl: Option<f64>,
        tp: Option<f64>,
        leverage: u32,
        is_paper: bool,
    ) {
        if !self.config.enabled {
            return;
        }

        let mode_tag = if is_paper { "🧪 [PAPER]" } else { "🚀 [LIVE]" };
        let sl_str = sl.map(|v| format!("${:.2}", v)).unwrap_or_else(|| "--".to_string());
        let tp_str = tp.map(|v| format!("${:.2}", v)).unwrap_or_else(|| "--".to_string());

        let msg = format!(
            "<b>{} POSIÇÃO ABERTA</b>\n\
            <b>Par:</b> {}\n\
            <b>Lado:</b> {}\n\
            <b>Preço de Entrada:</b> ${:.2}\n\
            <b>Quantidade:</b> {:.4} ({}x)\n\
            <b>Stop Loss:</b> {}\n\
            <b>Take Profit:</b> {}",
            mode_tag, symbol, side, price, qty, leverage, sl_str, tp_str
        );

        self.broadcast(msg);
    }

    pub fn notify_trade_closed(
        &self,
        symbol: &str,
        side: &str,
        entry_price: f64,
        exit_price: f64,
        qty: f64,
        pnl: f64,
        reason: &str,
        is_paper: bool,
    ) {
        if !self.config.enabled {
            return;
        }

        let mode_tag = if is_paper { "🧪 [PAPER]" } else { "🚀 [LIVE]" };
        let pnl_icon = if pnl >= 0.0 { "🟢" } else { "🔴" };
        let pnl_pct = if entry_price > 0.0 {
            ((exit_price - entry_price) / entry_price * 100.0) * if side == "Sell" { 1.0 } else { -1.0 }
        } else {
            0.0
        };

        let msg = format!(
            "<b>{} POSIÇÃO ENCERRADA</b>\n\
            <b>Par:</b> {}\n\
            <b>Lado:</b> {}\n\
            <b>Entrada:</b> ${:.2} | <b>Saída:</b> ${:.2}\n\
            <b>Quantidade:</b> {:.4}\n\
            <b>Resultado:</b> {} <b>{:.2} USDT</b> ({:+.2}%)\n\
            <b>Motivo:</b> {}",
            mode_tag, symbol, side, entry_price, exit_price, qty, pnl_icon, pnl, pnl_pct, reason
        );

        self.broadcast(msg);
    }

    pub fn notify_circuit_breaker(&self, current_dd: f64, max_dd: f64) {
        if !self.config.enabled {
            return;
        }

        let msg = format!(
            "🚨 <b>CIRCUIT BREAKER ATIVADO!</b>\n\
            O drawdown atual atingiu <b>{:.2}%</b> (Limite: {:.2}%).\n\
            Novas entradas estão <b>BLOQUEADAS</b> para preservação de capital.",
            current_dd, max_dd
        );

        self.broadcast(msg);
    }

    pub fn notify_breakeven(&self, symbol: &str, new_sl: f64) {
        if !self.config.enabled {
            return;
        }

        let msg = format!(
            "🛡️ <b>BREAKEVEN ACIONADO</b>\n\
            <b>Par:</b> {}\n\
            Stop Loss protegido no ponto de entrada: <b>${:.2}</b>",
            symbol, new_sl
        );

        self.broadcast(msg);
    }

    fn broadcast_with_discord_embed(&self, telegram_html: String, discord_embed: serde_json::Value) {
        let config = Arc::clone(&self.config);
        let client = self.client.clone();

        tokio::spawn(async move {
            // 1. Envia Telegram se configurado
            if !config.telegram_bot_token.trim().is_empty() && !config.telegram_chat_id.trim().is_empty() {
                let url = format!(
                    "https://api.telegram.org/bot{}/sendMessage",
                    config.telegram_bot_token.trim()
                );
                let payload = json!({
                    "chat_id": config.telegram_chat_id.trim(),
                    "text": telegram_html,
                    "parse_mode": "HTML"
                });

                if let Err(e) = client.post(&url).json(&payload).send().await {
                    error!("Falha ao enviar notificacao via Telegram: {}", e);
                } else {
                    info!("Notificacao enviada com sucesso ao Telegram.");
                }
            }

            // 2. Envia Discord com Embed rico e link 1-click
            if !config.discord_webhook_url.trim().is_empty() {
                let payload = json!({
                    "embeds": [discord_embed]
                });

                if let Err(e) = client.post(config.discord_webhook_url.trim()).json(&payload).send().await {
                    error!("Falha ao enviar notificacao via Discord: {}", e);
                } else {
                    info!("Notificacao com embed enviada com sucesso ao Discord.");
                }
            }
        });
    }

    pub fn notify_error(&self, symbol: &str, error_msg: &str) {
        if !self.config.enabled {
            return;
        }
        let msg = format!(
            "🚨 <b>ALERTA DE ERRO CRÍTICO - {}</b>\n\
            ━━━━━━━━━━━━━━━━━━━━\n\
            <b>Mensagem:</b> {}\n\
            <b>Atenção:</b> Verifique sua conta e posições imediatamente!",
            symbol, error_msg
        );
        self.broadcast(msg);
    }

    fn broadcast(&self, html_text: String) {
        let config = Arc::clone(&self.config);
        let client = self.client.clone();

        tokio::spawn(async move {
            // 1. Envia Telegram se configurado
            if !config.telegram_bot_token.trim().is_empty() && !config.telegram_chat_id.trim().is_empty() {
                let url = format!(
                    "https://api.telegram.org/bot{}/sendMessage",
                    config.telegram_bot_token.trim()
                );
                let payload = json!({
                    "chat_id": config.telegram_chat_id.trim(),
                    "text": html_text,
                    "parse_mode": "HTML"
                });

                if let Err(e) = client.post(&url).json(&payload).send().await {
                    error!("Falha ao enviar notificacao via Telegram: {}", e);
                } else {
                    info!("Notificacao enviada com sucesso ao Telegram.");
                }
            }

            // 2. Envia Discord se configurado
            if !config.discord_webhook_url.trim().is_empty() {
                // Remove tags HTML para formato simples no Discord
                let plain_text = html_text
                    .replace("<b>", "**")
                    .replace("</b>", "**")
                    .replace("<i>", "*")
                    .replace("</i>", "*")
                    .replace("<br>", "\n");

                let payload = json!({
                    "content": plain_text
                });

                if let Err(e) = client.post(config.discord_webhook_url.trim()).json(&payload).send().await {
                    error!("Falha ao enviar notificacao via Discord: {}", e);
                } else {
                    info!("Notificacao enviada com sucesso ao Discord.");
                }
            }
        });
    }
}
