//! Autenticación por header `x-api-key`.
//! Con `PUBLIC_READ=true` las peticiones GET no la necesitan; las de escritura siempre.

use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{error::ApiError, state::AppState};

pub async fn require_api_key(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let is_read = matches!(*req.method(), Method::GET | Method::HEAD);
    if state.cfg.public_read && is_read {
        return next.run(req).await;
    }

    let ok = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|k| constant_time_eq(k.as_bytes(), state.cfg.api_key.as_bytes()));
    if ok {
        next.run(req).await
    } else {
        ApiError::new(StatusCode::UNAUTHORIZED, "x-api-key inválida o ausente").into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
