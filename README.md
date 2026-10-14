# 🦀 Binance Futures Trading Bot em Rust

Bot de negociação algorítmica para **Binance USDT-M Futures**, projetado para alta velocidade, zero alocação no caminho crítico de processamento de candles e gestão de risco determinística.

Integra as principais estratégias da comunidade **TradingView (Pine Script)** — **Supertrend ATR, Médias Exponenciais Rápidas/Lentas (EMA Ribbon), RSI e Trailing Stops dinâmicos** — com um painel de controle web em tempo real e simulador nativo (Paper Trading).

---

## ⚡ Características Principais

* **Performance & Baixa Latência:**
  * Escrito 100% em **Rust** assíncrono com runtime `tokio`.
  * Ingestão contínua de fluxo de mercado via WebSocket com reconexão automática e backoff exponencial.
  * Cálculos de indicadores técnicos em $O(1)$ streaming (sem alocação dinâmica por candle/tick).
* **Binance USDT-M Futures:**
  * Suporte nativo a **Testnet** e **Produção**.
  * Assinatura criptográfica HMAC-SHA256 de alta precisão.
  * Ajuste automático de alavancagem (`leverage`) e tipo de margem (`ISOLATED`).
  * Ordens de mercado com identificadores de cliente (`newClientOrderId`) idempotentes.
* **Motor de Risco & Circuit Breaker:**
  * Dimensionamento dinâmico de lotes baseado na distância em USDT até o Stop Loss (ATR).
  * Proteção contra Drawdown: se a perda acumulada ultrapassar a porcentagem configurada, o bot bloqueia novas operações automaticamente.
  * Modo **Paper Trading** ativado por padrão: simula execução, taxas, saldo e PnL virtual sem risco de capital.
* **Painel de Gestão & Telemetria Web (Axum):**
  * Dashboard web responsivo moderno integrado (acessível em `http://127.0.0.1:8080`).
  * Atualização ao vivo de saldo, equity, win rate, posição aberta, PnL e histórico de trades.
  * Botões de controle em tempo real: **Pausar**, **Retomar** e **Botão de Pânico (Panic Close)** para fechamento imediato a mercado.

---

## 📐 Estratégia Técnica (TradingView Pine Engine)

O bot combina 3 camadas de confirmação baseadas nos scripts de maior relevância do TradingView:

1. **Filtro de Tendência Estrutural (EMA Ribbon):**
   * **Compra (Long):** Preço acima da EMA Lenta (21) e EMA Rápida (9) $\ge$ EMA Lenta.
   * **Venda (Short):** Preço abaixo da EMA Lenta (21) e EMA Rápida (9) $\le$ EMA Lenta.
2. **Gatilho de Entrada & Trailing (Supertrend ATR):**
   * Entrada engatilhada quando o Supertrend inverte de polaridade (Bear $\to$ Bull para Long, Bull $\to$ Bear para Short).
3. **Filtro de Momento & Exaustão (RSI 14):**
   * Impede compras no topo de exaustão ($RSI > 70$) e vendas no fundo de repique ($RSI < 30$).
4. **Proteção Dinâmica (Stop Loss & Take Profit):**
   * Stop Loss baseado na banda oposta do Supertrend e múltiplo do ATR ($2.0 \times ATR$).
   * Take Profit estendido ($3.0 \times ATR$) mantendo relação risco/retorno favorável ($1:1.5+$).

---

## 🛠️ Como Executar

### 1. Compilação
```bash
# Compilar versão de lançamento (otimizada com LTO)
cargo build --release
```

O binário final estará em `target/release/binance-futures-bot.exe`.

### 2. Configuração (`config.toml`)
Edite o arquivo `config.toml` na raiz do projeto:

```toml
[exchange]
api_key = "SUA_API_KEY_AQUI"
api_secret = "SEU_API_SECRET_AQUI"
testnet = true # Mantenha true para testnet, false para conta real

[trading]
symbol = "BTCUSDT"
interval = "1m"
leverage = 5
margin_type = "ISOLATED"
paper_trading = true # true = Simulador sem dinheiro real; false = Ordens reais

[risk]
risk_per_trade_percent = 1.0 # Arrisca no maximo 1% da banca por operacao
max_drawdown_percent = 5.0   # Trava o bot se o drawdown bater 5%
sl_atr_multiplier = 2.0
tp_atr_multiplier = 3.0

[server]
host = "127.0.0.1"
port = 8080
```

*Também é possível definir as chaves de API pelas variáveis de ambiente:*
```bash
set BINANCE_API_KEY=sua_chave
set BINANCE_API_SECRET=seu_segredo
```

### 3. Inicialização
```bash
cargo run --release
```

Ao iniciar, abra seu navegador em:
👉 **[http://localhost:8080](http://localhost:8080)**

---

## 🌐 Endpoints da API REST

* `GET /api/status`: Retorna o JSON completo do estado do bot, posição atual, saldo, indicadores e últimos 20 trades.
* `POST /api/action/pause`: Pausa novas entradas do bot.
* `POST /api/action/resume`: Retoma a geração de ordens e reseta o circuit breaker.
* `POST /api/action/emergency_close`: **Botão de Pânico** — cancela todas as ordens abertas e liquida todas as posições a mercado imediatamente.
