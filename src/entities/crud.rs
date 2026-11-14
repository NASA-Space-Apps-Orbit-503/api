//! Endpoints genéricos que comparten todas las entidades:
//! crear (uno o varios en la misma ruta POST) y obtener por id.

use axum::{
    extract::{
        rejection::{JsonRejection, PathRejection},
        Path, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use duckdb::{Row, Statement};
use serde::{de::DeserializeOwned, Serialize};

use crate::{error::ApiError, state::AppState};

/// Lo que cada entidad tiene que definir para tener sus endpoints.
pub trait Entity: Serialize + Send + Sized + 'static {
    /// Cuerpo que manda el cliente para crearla (sin `id`).
    type New: DeserializeOwned + Send + 'static;

    /// Nombre para los mensajes de error.
    const NAME: &'static str;
    /// INSERT ... VALUES (?, ...) RETURNING <columnas en el orden de `from_row`>
    const INSERT_SQL: &'static str;
    /// SELECT <columnas en el orden de `from_row`> ... WHERE id = ?
    const SELECT_BY_ID_SQL: &'static str;

    /// Valida y normaliza lo que mandó el cliente.
    fn validate(new: Self::New) -> Result<Self::New, String>;
    /// Ejecuta `INSERT_SQL` ya preparado con los valores de `new`.
    fn insert(stmt: &mut Statement<'_>, new: &Self::New) -> duckdb::Result<Self>;
    /// Construye la entidad desde una fila.
    fn from_row(row: &Row<'_>) -> duckdb::Result<Self>;
}

#[derive(Serialize)]
pub struct BulkCreated<T> {
    pub count: usize,
    pub items: Vec<T>,
}

/// Página de resultados de un listado.
#[derive(Serialize)]
pub struct Page<T> {
    /// Total de registros que cumplen el filtro (sin paginar).
    pub total: i64,
    pub limit: u32,
    pub offset: u64,
    pub items: Vec<T>,
}

/// `?limit=&offset=` de los listados.
#[derive(Debug, Default, serde::Deserialize)]
pub struct PageParams {
    pub limit: Option<u32>,
    pub offset: Option<u64>,
}

impl PageParams {
    /// (limit, offset) con valores por defecto y tope.
    pub fn resolve(&self, default: u32, max: u32) -> (u32, u64) {
        let limit = self.limit.filter(|n| *n > 0).unwrap_or(default).min(max);
        (limit, self.offset.unwrap_or(0))
    }
}

/// Ejecuta `SELECT count(*) ... {clause}` y `SELECT <cols> ... {clause} ORDER BY ... LIMIT/OFFSET`
/// y arma una página tipada.
pub fn fetch_page<E: Entity>(
    conn: &duckdb::Connection,
    count_sql: &str,
    page_sql: &str,
    params: &[duckdb::types::Value],
    limit: u32,
    offset: u64,
) -> Result<Page<E>, ApiError> {
    let total: i64 = conn.query_row(count_sql, duckdb::params_from_iter(params.iter()), |r| {
        r.get(0)
    })?;
    let mut all: Vec<duckdb::types::Value> = params.to_vec();
    all.push(duckdb::types::Value::BigInt(limit as i64));
    all.push(duckdb::types::Value::BigInt(offset as i64));
    let mut stmt = conn.prepare(page_sql)?;
    let items = stmt
        .query_map(duckdb::params_from_iter(all.iter()), E::from_row)?
        .collect::<duckdb::Result<Vec<E>>>()?;
    Ok(Page {
        total,
        limit,
        offset,
        items,
    })
}

/// POST /<entidad>
/// El cuerpo puede ser UN objeto o UN ARREGLO de objetos:
///   - objeto  → 201 con la entidad creada
///   - arreglo → 201 con `{count, items}`. Todo o nada: si un elemento falla
///     no se guarda ninguno y el error trae su `index` (base 0).
pub async fn create<E: Entity>(
    State(state): State<AppState>,
    payload: Result<Json<serde_json::Value>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = payload.map_err(ApiError::from_rejection)?;

    match body {
        serde_json::Value::Array(values) => {
            let created = create_many::<E>(&state, values).await?;
            Ok((
                StatusCode::CREATED,
                Json(BulkCreated {
                    count: created.len(),
                    items: created,
                }),
            )
                .into_response())
        }
        value @ serde_json::Value::Object(_) => {
            let new: E::New =
                serde_json::from_value(value).map_err(|e| ApiError::bad_request(e.to_string()))?;
            let new = E::validate(new).map_err(ApiError::unprocessable)?;
            let created = state
                .run_db(move |conn| {
                    let mut stmt = conn.prepare(E::INSERT_SQL)?;
                    Ok(E::insert(&mut stmt, &new)?)
                })
                .await?;
            Ok((StatusCode::CREATED, Json(created)).into_response())
        }
        _ => Err(ApiError::bad_request(
            "el cuerpo debe ser un objeto o un arreglo de objetos",
        )),
    }
}

/// Crea varios registros en una sola transacción.
async fn create_many<E: Entity>(
    state: &AppState,
    values: Vec<serde_json::Value>,
) -> Result<Vec<E>, ApiError> {
    if values.is_empty() {
        return Err(ApiError::unprocessable("la lista no puede estar vacía"));
    }
    if values.len() > state.cfg.max_bulk {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("como máximo {} elementos por petición", state.cfg.max_bulk),
        ));
    }

    // Leer y validar todo antes de tocar la base.
    let items: Vec<E::New> = values
        .into_iter()
        .enumerate()
        .map(|(i, value)| {
            let new: E::New = serde_json::from_value(value)
                .map_err(|e| ApiError::bad_request(e.to_string()).at(i))?;
            E::validate(new).map_err(|m| ApiError::unprocessable(m).at(i))
        })
        .collect::<Result<_, _>>()?;

    state
        .run_db(move |conn| {
            let tx = conn.transaction()?;
            let created = {
                let mut stmt = tx.prepare(E::INSERT_SQL)?;
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    out.push(E::insert(&mut stmt, item).map_err(|e| ApiError::from(e).at(i))?);
                }
                out
            };
            tx.commit()?;
            Ok(created)
        })
        .await
}

/// GET /<entidad>/{id}
pub async fn get_one<E: Entity>(
    State(state): State<AppState>,
    path: Result<Path<i64>, PathRejection>,
) -> Result<Json<E>, ApiError> {
    let Path(id) = path.map_err(|_| ApiError::bad_request("el id debe ser un número entero"))?;

    let found = state
        .run_db(
            move |conn| match conn.query_row(E::SELECT_BY_ID_SQL, [id], E::from_row) {
                Ok(entity) => Ok(Some(entity)),
                Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            },
        )
        .await?;

    found
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("{} con id {id} no existe", E::NAME)))
}
