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
        // Token de sessão derivado da senha: trocar a senha invalida todas as sessões
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
        .map(|e| format!("<div class=\"error\">{}</div>", e))
        .unwrap_or_default();
    format!(
        r##"<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <title>Binance Futures Bot - Login</title>
    <style>
        body {{ font-family: Arial, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #f0f0f0; }}
        .login-box {{ background: white; padding: 2rem; border-radius: 8px; box-shadow: 0 2px 10px rgba(0,0,0,0.1); }}
        h1 {{ margin-top: 0; color: #333; }}
        input {{ width: 100%; padding: 0.5rem; margin: 0.5rem 0; border: 1px solid #ddd; border-radius: 4px; }}
        button {{ width: 100%; padding: 0.5rem; background: #007bff; color: white; border: none; border-radius: 4px; cursor: pointer; }}
        button:hover {{ background: #0056b3; }}
        .error {{ color: red; margin: 0.5rem 0; }}
    </style>
</head>
<body>
    <div class="login-box">
        <h1>Binance Futures Bot</h1>
        <p>Digite a senha para acessar o painel.</p>
        <form method="POST" action="/login">
            <input type="password" name="password" placeholder="Senha" required>
            <button type="submit">Entrar</button>
        </form>
        {err_html}
    </div>
</body>
</html>"##,
        err_html = err_html
    )
}