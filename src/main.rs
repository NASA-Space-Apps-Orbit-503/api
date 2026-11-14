//! api-nasa: API HTTP en Rust para el calendario de actividad de quema
//! (NASA Space Apps · Harmonization of MODIS and VIIRS Hot Spots).
//!
//! La configuración completa está en `config.rs` y en el README.

mod analytics;
mod anomalies;
mod auth;
mod config;
mod db;
mod entities;
mod error;
mod explorer;
mod filters;
mod firms;
mod state;
mod validation;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    extract::DefaultBodyLimit,
    http::{header, HeaderName, HeaderValue, Method},
    middleware,
    routing::get,
    Router,
};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use config::Config;
use state::AppState;

fn cors_layer(origins: &[String]) -> CorsLayer {
    let base = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, HeaderName::from_static("x-api-key")])
        // Para que el frontend pueda leer el nombre del archivo CSV.
        .expose_headers([header::CONTENT_DISPOSITION])
        .max_age(Duration::from_secs(3600));

    if origins.iter().any(|o| o == "*") {
        base.allow_origin(Any)
    } else {
        let list: Vec<HeaderValue> = origins
            .iter()
            .filter_map(|o| HeaderValue::from_str(o.trim_end_matches('/')).ok())
            .collect();
        base.allow_origin(AllowOrigin::list(list))
    }
}

#[tokio::main]
async fn main() {
    let cfg = Config::from_env();

    let (conn, spatial) =
        db::open(&cfg.db_path).unwrap_or_else(|e| panic!("no se pudo abrir {}: {e}", cfg.db_path));
    if !spatial {
        eprintln!(
            "aviso: extensión spatial no disponible; el filtro por polígono queda desactivado"
        );
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent("api-nasa")
        .build()
        .expect("no se pudo crear el cliente HTTP");

    let state = AppState {
        cfg: Arc::new(cfg),
        db: Arc::new(Mutex::new(conn)),
        spatial,
        http,
    };

    // Sincronización periódica con FIRMS (si hay MAP_KEY y FIRMS_SYNC_HOURS > 0).
    if state.cfg.firms_map_key.is_some() && state.cfg.firms_sync_hours > 0 {
        tokio::spawn(firms::run_scheduler(state.clone()));
    }

    let protected = Router::new()
        .merge(entities::routes())
        .merge(analytics::routes())
        .merge(anomalies::routes())
        .merge(firms::routes())
        .merge(explorer::routes())
        // Se agrega antes que la autenticación, así queda por dentro: primero 401, luego 400.
        .route_layer(middleware::from_fn(filters::reject_unknown_filters))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_api_key,
        ));

    let app = Router::new()
        .route("/health", get(explorer::health))
        .merge(protected)
        .layer(DefaultBodyLimit::max(state.cfg.max_body_mb * 1024 * 1024))
        // CORS al final = capa más externa: responde las peticiones OPTIONS del navegador.
        .layer(cors_layer(&state.cfg.cors_origins))
        .with_state(state.clone());

    let port = state.cfg.port.clone();
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("no se pudo abrir el puerto");
    println!(
        "api-nasa escuchando en :{port} (base: {}, spatial: {spatial}, lectura pública: {})",
        state.cfg.db_path, state.cfg.public_read
    );
    axum::serve(listener, app).await.unwrap();
}
