//! Instrument: sensor que hizo la detección (MODIS, VIIRS, ...).

use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use duckdb::{params, Row, Statement};
use serde::{Deserialize, Serialize};

use super::crud::{self, Entity};
use crate::{error::ApiError, state::AppState, validation::required_text};

#[derive(Debug, Serialize)]
pub struct Instrument {
    pub id: i64,
    pub code: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewInstrument {
    pub code: String,
    pub name: String,
}

impl Entity for Instrument {
    type New = NewInstrument;

    const NAME: &'static str = "instrumento";
    const INSERT_SQL: &'static str =
        "INSERT INTO instruments (code, name) VALUES (?, ?) RETURNING id, code, name";
    const SELECT_BY_ID_SQL: &'static str = "SELECT id, code, name FROM instruments WHERE id = ?";

    fn validate(mut new: NewInstrument) -> Result<NewInstrument, String> {
        new.code = required_text("code", &new.code, 20)?.to_ascii_uppercase();
        new.name = required_text("name", &new.name, 200)?;
        Ok(new)
    }

    fn insert(stmt: &mut Statement<'_>, new: &NewInstrument) -> duckdb::Result<Self> {
        stmt.query_row(params![new.code, new.name], Self::from_row)
    }

    fn from_row(row: &Row<'_>) -> duckdb::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            code: row.get(1)?,
            name: row.get(2)?,
        })
    }
}

/// GET /instruments: todos los instrumentos (son pocos, sin paginar).
async fn list(State(state): State<AppState>) -> Result<Json<Vec<Instrument>>, ApiError> {
    let items = state
        .run_db(|conn| {
            let mut stmt = conn.prepare("SELECT id, code, name FROM instruments ORDER BY code")?;
            let rows = stmt
                .query_map([], Instrument::from_row)?
                .collect::<duckdb::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?;
    Ok(Json(items))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/instruments", post(crud::create::<Instrument>).get(list))
        .route("/instruments/{id}", get(crud::get_one::<Instrument>))
}
