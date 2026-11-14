//! Endpoints genéricos de exploración: salud, tablas y consulta SQL de solo lectura.

use axum::{
    extract::{rejection::JsonRejection, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use duckdb::Connection;
use serde::Deserialize;
use serde_json::json;

use crate::{error::ApiError, state::AppState};

/// Respuesta con un JSON que ya viene armado como texto desde DuckDB.
pub struct JsonText(pub String);

impl IntoResponse for JsonText {
    fn into_response(self) -> Response {
        ([(header::CONTENT_TYPE, "application/json")], self.0).into_response()
    }
}

/// Ejecuta un SELECT y devuelve las filas como texto JSON (arreglo de objetos).
fn query_json(conn: &Connection, sql: &str, limit: u32) -> duckdb::Result<String> {
    let wrapped = format!(
        "SELECT COALESCE(json_group_array(to_json(t)), '[]')::VARCHAR \
         FROM (SELECT * FROM ({sql}) LIMIT {limit}) AS t"
    );
    conn.query_row(&wrapped, [], |row| row.get(0))
}

/// Rutas protegidas por API key.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/tables", get(list_tables))
        .route("/tables/{name}", get(preview_table))
        .route("/query", post(run_query))
}

/// GET /health (pública)
pub async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .run_db(|c| Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i32>(0))?))
        .await
        .map(|_| Json(json!({ "ok": true })))
        .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e.message))
}

/// GET /tables
async fn list_tables(State(state): State<AppState>) -> Result<JsonText, ApiError> {
    let max = state.cfg.max_rows;
    let rows = state
        .run_db(move |c| {
            Ok(query_json(
                c,
                "SELECT table_schema AS schema, table_name AS name, table_type AS type \
                 FROM information_schema.tables ORDER BY 1, 2",
                max,
            )?)
        })
        .await?;
    Ok(JsonText(rows))
}

#[derive(Deserialize)]
struct PreviewParams {
    limit: Option<u32>,
}

/// GET /tables/{name}?limit=100
async fn preview_table(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(p): Query<PreviewParams>,
) -> Result<JsonText, ApiError> {
    if !is_identifier(&name) {
        return Err(ApiError::bad_request("nombre de tabla inválido"));
    }
    let limit = p
        .limit
        .filter(|n| *n > 0)
        .unwrap_or(100)
        .min(state.cfg.max_rows);
    let rows = state
        .run_db(move |c| Ok(query_json(c, &format!("SELECT * FROM \"{name}\""), limit)?))
        .await?;
    Ok(JsonText(rows))
}

#[derive(Deserialize)]
struct QueryBody {
    sql: String,
}

/// POST /query  {"sql": "SELECT ..."}: consulta libre de solo lectura.
async fn run_query(
    State(state): State<AppState>,
    payload: Result<Json<QueryBody>, JsonRejection>,
) -> Result<JsonText, ApiError> {
    let Json(body) = payload.map_err(ApiError::from_rejection)?;
    let sql = body.sql.trim().trim_end_matches(';').trim().to_string();
    if sql.contains(';') {
        return Err(ApiError::bad_request("solo se permite una sentencia"));
    }
    let first = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if !matches!(first.as_str(), "SELECT" | "WITH" | "FROM") {
        return Err(ApiError::bad_request(
            "solo se permiten consultas SELECT / WITH / FROM",
        ));
    }
    let max = state.cfg.max_rows;
    // Al ir como subconsulta, DuckDB rechaza cualquier cosa que no sea una consulta.
    let rows = state.run_db(move |c| Ok(query_json(c, &sql, max)?)).await?;
    Ok(JsonText(rows))
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
