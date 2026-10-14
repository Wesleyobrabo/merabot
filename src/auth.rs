// Rust, auth.rs, dashboard password gate (cookie session) for access over the Cloudflare tunnel
use crate::server::AppState;
use axum::{
    body::Body,
    extract::{Form, State},
    http::{header, HeaderMap, Request, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};

const COOKIE_NAME: &str = "bot_session";
const SESSION_MAX_AGE_SECS: u64 = 60 * 60 * 24 * 30;
const MAX_FAILS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);

struct Guard {
    fails: u32,
    locked_until: Option<Instant>,
}

pub struct AuthState {
    password_hash: [u8; 32],
    session_token: String,
    guard: Mutex<Guard>,
}

impl AuthState {
    pub fn new(password: &str) -> Self {
        let password_hash: [u8; 32] = Sha256::digest(password.as_bytes()).into();
        // Token de sessao derivado da senha: trocar a senha invalida todas as sessoes
        let session_token = hex::encode(Sha256::digest(format!("bot-session-v1|{}", password).as_bytes()));
        Self {
            password_hash,
            session_token,
            guard: Mutex::new(Guard { fails: 0, locked_until: None }),
        }
    }

    fn check_password(&self, candidate: &str) -> bool {
        let h: [u8; 32] = Sha256::digest(candidate.as_bytes()).into();
        ct_eq(&h, &self.password_hash)
    }

    fn cookie_valid(&self, headers: &HeaderMap) -> bool {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|kv| kv.trim().strip_prefix(&format!("{}=", COOKIE_NAME)).map(str::to_owned))
            .any(|tok| ct_eq(tok.as_bytes(), self.session_token.as_bytes()))
    }

    fn lock_remaining_secs(&self) -> Option<u64> {
        let mut g = self.guard.lock().unwrap_or_else(|e| e.into_inner());
        match g.locked_until {
            Some(t) if t > Instant::now() => Some((t - Instant::now()).as_secs() + 1),
            Some(_) => {
                g.locked_until = None;
                g.fails = 0;
                None
            }
            None => None,
        }
    }

    fn register_fail(&self) {
        let mut g = self.guard.lock().unwrap_or_else(|e| e.into_inner());
        g.fails += 1;
        if g.fails >= MAX_FAILS {
            g.locked_until = Some(Instant::now() + LOCKOUT);
            warn!("Login do dashboard: {} falhas seguidas, bloqueado por {}s.", g.fails, LOCKOUT.as_secs());
        }
    }

    fn reset_fails(&self) {
        let mut g = self.guard.lock().unwrap_or_else(|e| e.into_inner());
        g.fails = 0;
        g.locked_until = None;
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn auth_middleware(State(state): State<AppState>, req: Request<Body>, next: Next) -> Response {
    let Some(auth) = state.auth.as_ref() else {
        return next.run(req).await;
    };
    let path = req.uri().path();
    if path == "/login" || auth.cookie_valid(req.headers()) {
        return next.run(req).await;
    }
    if path.starts_with("/api/") {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "success": false, "error": "Nao autenticado" })),
        )
            .into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}

pub async fn login_get(State(state): State<AppState>) -> Response {
    if state.auth.is_none() {
        return Redirect::to("/").into_response();
    }
    Html(login_page(None)).into_response()
}

#[derive(Deserialize)]
pub struct LoginForm {
    password: String,
}

pub async fn login_post(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    let Some(auth) = state.auth.as_ref() else {
        return Redirect::to("/").into_response();
    };

    if let Some(secs) = auth.lock_remaining_secs() {
        let msg = format!("Muitas tentativas. Aguarde {}s.", secs);
        return (StatusCode::TOO_MANY_REQUESTS, Html(login_page(Some(&msg)))).into_response();
    }

    if auth.check_password(&form.password) {
        auth.reset_fails();
        info!("Login do dashboard: sessao autenticada.");
        let cookie = format!(
            "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
            COOKIE_NAME, auth.session_token, SESSION_MAX_AGE_SECS
        );
        return Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::LOCATION, "/")
            .header(header::SET_COOKIE, cookie)
            .body(Body::empty())
            .unwrap();
    }

    auth.register_fail();
    warn!("Login do dashboard: senha incorreta.");
    tokio::time::sleep(Duration::from_secs(1)).await;
    (StatusCode::UNAUTHORIZED, Html(login_page(Some("Senha incorreta.")))).into_response()
}

fn login_page(error: Option<&str>) -> String {
    let err_html = error
        .map(|e| format!("<p class=\"err\">{}</p>", e))
        .unwrap_or_default();
    format!(
        r##"<!DOCTYPE html>
<html lang="pt-BR">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>Acesso ao Bot</title>
<style>
  * {{ box-sizing: border-box; }}
  body {{ margin:0; min-height:100vh; display:flex; align-items:center; justify-content:center;
         background: radial-gradient(circle at 30% 20%, #1b2340 0%, #0b0e17 60%);
         font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; color:#e6e9f2; }}
  form {{ width:min(360px, 90vw); padding:32px 28px; border-radius:18px;
          background: rgba(255,255,255,0.05); border:1px solid rgba(255,255,255,0.10);
          backdrop-filter: blur(14px); box-shadow: 0 20px 60px rgba(0,0,0,0.5); }}
  h1 {{ margin:0 0 6px; font-size:1.3rem; }}
  .sub {{ margin:0 0 22px; font-size:.85rem; color:#8b93ad; }}
  input {{ width:100%; padding:14px 16px; border-radius:12px; border:1px solid rgba(255,255,255,0.14);
           background: rgba(0,0,0,0.35); color:#fff; font-size:1rem; outline:none; }}
  input:focus {{ border-color:#5b8cff; box-shadow:0 0 0 3px rgba(91,140,255,0.25); }}
  button {{ width:100%; margin-top:14px; padding:14px; border:0; border-radius:12px; cursor:pointer;
            font-size:1rem; font-weight:600; color:#fff; background: linear-gradient(135deg,#5b8cff,#8a5bff);
            transition: transform .15s, filter .15s; }}
  button:hover {{ filter:brightness(1.1); transform:translateY(-1px); }}
  .err {{ margin:12px 0 0; color:#ff6b7a; font-size:.9rem; }}
</style>
</head>
<body>
<form method="POST" action="/login">
  <h1>Binance Futures Bot</h1>
  <p class="sub">Digite a senha para acessar o painel.</p>
  <input id="password" type="password" name="password" placeholder="Senha" autocomplete="current-password" autofocus required>
  <button id="login-btn" type="submit">Entrar</button>
  {err_html}
</form>
</body>
</html>"##,
        err_html = err_html
    )
}
