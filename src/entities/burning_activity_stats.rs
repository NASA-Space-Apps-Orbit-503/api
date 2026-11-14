//! Burning Activity Stats: resumen diario de actividad de quema
//! (cuántos hotspots hubo ese día y el FRP máximo).
//!
//! La tabla guarda UNA serie (un área). Se puede llenar a mano (POST con un objeto o un arreglo) o
//! recalcular desde las detecciones con POST /burning-activity-stats/rebuild.
//! Para áreas que elige el usuario, usa /stats/* (se calcula en el momento).

use axum::{
    extract::{Query, State},
    routing::{get, post},
    Json, Router,
};
use duckdb::{params, types::Value, Row, Statement};
use serde::{Deserialize, Serialize};

use super::crud::{self, Entity, Page, PageParams};
use crate::{
    analytics::{daily_sql, MetricParams},
    error::ApiError,
    filters::DetectionFilter,
    state::AppState,
    validation::{optional_min, valid_ymd},
};

#[derive(Debug, Serialize)]
pub struct BurningActivityStat {
    pub id: i64,
    pub day: i32,
    pub month: i32,
    pub year: i32,
    pub count: i64,
    pub max_frp: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewBurningActivityStat {
    pub day: i32,
    pub month: i32,
    pub year: i32,
    pub count: i64,
    #[serde(default)]
    pub max_frp: Option<f64>,
}

impl Entity for BurningActivityStat {
    type New = NewBurningActivityStat;

    const NAME: &'static str = "estadística";
    const INSERT_SQL: &'static str =
        "INSERT INTO burning_activity_stats (\"day\", \"month\", \"year\", \"count\", max_frp) \
         VALUES (?, ?, ?, ?, ?) RETURNING id, \"day\", \"month\", \"year\", \"count\", max_frp";
    const SELECT_BY_ID_SQL: &'static str =
        "SELECT id, \"day\", \"month\", \"year\", \"count\", max_frp \
         FROM burning_activity_stats WHERE id = ?";

    fn validate(s: NewBurningActivityStat) -> Result<NewBurningActivityStat, String> {
        if !(1900..=2100).contains(&s.year) {
            return Err("year debe estar entre 1900 y 2100".into());
        }
        if !(1..=12).contains(&s.month) {
            return Err("month debe estar entre 1 y 12".into());
        }
        if !valid_ymd(s.year, s.month, s.day) {
            return Err(format!(
                "day {} no existe en {:04}-{:02}",
                s.day, s.year, s.month
            ));
        }
        if s.count < 0 {
            return Err("count no puede ser negativo".into());
        }
        optional_min("max_frp", s.max_frp, 0.0)?;
        Ok(s)
    }

    fn insert(stmt: &mut Statement<'_>, s: &NewBurningActivityStat) -> duckdb::Result<Self> {
        stmt.query_row(
            params![s.day, s.month, s.year, s.count, s.max_frp],
            Self::from_row,
        )
    }

    fn from_row(row: &Row<'_>) -> duckdb::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            day: row.get(1)?,
            month: row.get(2)?,
            year: row.get(3)?,
            count: row.get(4)?,
            max_frp: row.get(5)?,
        })
    }
}

const COLUMNS: &str = "id, \"day\", \"month\", \"year\", \"count\", max_frp";

#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    pub year: Option<i32>,
    pub month: Option<i32>,
}

/// GET /burning-activity-stats?year=&month=&limit=&offset=
async fn list(
    State(state): State<AppState>,
    Query(lp): Query<ListParams>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<BurningActivityStat>>, ApiError> {
    let mut conds = Vec::new();
    let mut params = Vec::new();
    if let Some(y) = lp.year {
        conds.push("\"year\" = ?");
        params.push(Value::Int(y));
    }
    if let Some(m) = lp.month {
        conds.push("\"month\" = ?");
        params.push(Value::Int(m));
    }
    let clause = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conds.join(" AND "))
    };
    let count_sql = format!("SELECT count(*) FROM burning_activity_stats {clause}");
    let page_sql = format!(
        "SELECT {COLUMNS} FROM burning_activity_stats {clause} \
         ORDER BY \"year\", \"month\", \"day\", id LIMIT ? OFFSET ?"
    );
    let (limit, offset) = page.resolve(366, state.cfg.max_rows);
    let result = state
        .run_db(move |conn| {
            crud::fetch_page::<BurningActivityStat>(
                conn, &count_sql, &page_sql, &params, limit, offset,
            )
        })
        .await?;
    Ok(Json(result))
}

#[derive(Debug, Serialize)]
pub struct RebuildResult {
    /// Filas que quedaron en la tabla.
    pub rows: usize,
    pub metric: &'static str,
    pub cell_deg: Option<f64>,
}

/// POST /burning-activity-stats/rebuild?bbox=...&metric=cells
/// Borra la tabla y la vuelve a llenar con un registro por día con actividad,
/// calculado desde las detecciones que cumplen el filtro.
async fn rebuild(
    State(state): State<AppState>,
    Query(filter): Query<DetectionFilter>,
    Query(mp): Query<MetricParams>,
) -> Result<Json<RebuildResult>, ApiError> {
    let sql = filter.to_sql(state.spatial)?;
    let metric = mp.resolve()?;
    let insert = format!(
        "INSERT INTO burning_activity_stats (\"day\", \"month\", \"year\", \"count\", max_frp) \
         SELECT day(d), month(d), year(d), n, max_frp FROM ({}) ORDER BY d",
        daily_sql(&sql, &metric)
    );
    let params = sql.params;
    let rows = state
        .run_db(move |conn| {
            let tx = conn.transaction()?;
            tx.execute("DELETE FROM burning_activity_stats", [])?;
            let n = tx.execute(&insert, duckdb::params_from_iter(params.iter()))?;
            tx.commit()?;
            Ok(n)
        })
        .await?;
    Ok(Json(RebuildResult {
        rows,
        metric: metric.name(),
        cell_deg: metric.cell_deg(),
    }))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/burning-activity-stats",
            post(crud::create::<BurningActivityStat>).get(list),
        )
        .route("/burning-activity-stats/rebuild", post(rebuild))
        .route(
            "/burning-activity-stats/{id}",
            get(crud::get_one::<BurningActivityStat>),
        )
}
