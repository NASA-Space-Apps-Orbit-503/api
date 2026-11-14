//! Estadísticas para el calendario de actividad de quema, calculadas en el momento
//! a partir de `detections_harmonized` y de los mismos filtros de la búsqueda.
//!
//! Armonización MODIS / VIIRS (`metric`):
//!   - `cells` (por defecto): cuenta CELDAS de una cuadrícula común (`cell_deg`, 0.01° ≈ 1.1 km)
//!     con al menos un hotspot ese día. Así un fuego visto como 1 píxel por MODIS (1 km) y como
//!     varios por VIIRS (375 m), o por varios satélites a la vez, cuenta igual.
//!   - `detections`: cuenta detecciones crudas, sin armonizar.
//!
//! Endpoints:
//!   GET /stats/calendar     serie por mes o por día, con ceros en los periodos sin actividad
//!   GET /stats/climatology  por mes del año: mediana, promedio, mínimo, máximo... entre años
//!
//! Las anomalías por celda y día están en `anomalies.rs` (`/stats/anomalies`).
//!   GET /stats/summary      totales, años y meses con más actividad

use axum::{
    extract::{Query, State},
    routing::get,
    Json, Router,
};
use duckdb::{types::Value, Connection, Row};
use serde::{Deserialize, Serialize};

use crate::{
    error::ApiError,
    filters::{opt_text, DetectionFilter, SqlFilter},
    state::AppState,
};

// ---------------------------------------------------------------- Métrica

#[derive(Debug, Default, Deserialize)]
pub struct MetricParams {
    /// cells (por defecto) | detections
    pub metric: Option<String>,
    /// Tamaño de celda en grados para `metric=cells` (0.001 a 1). Por defecto 0.01.
    pub cell_deg: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
pub enum Metric {
    Detections,
    Cells(f64),
}

impl MetricParams {
    pub fn resolve(&self) -> Result<Metric, ApiError> {
        match self
            .metric
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            None | Some("") | Some("cells") => {
                let c = self.cell_deg.unwrap_or(0.01);
                if !(c.is_finite() && (0.001..=1.0).contains(&c)) {
                    return Err(ApiError::bad_request("cell_deg debe estar entre 0.001 y 1"));
                }
                Ok(Metric::Cells(c))
            }
            Some("detections") => Ok(Metric::Detections),
            Some(_) => Err(ApiError::bad_request("metric debe ser cells o detections")),
        }
    }
}

impl Metric {
    pub fn name(&self) -> &'static str {
        match self {
            Metric::Detections => "detections",
            Metric::Cells(_) => "cells",
        }
    }

    pub fn cell_deg(&self) -> Option<f64> {
        match self {
            Metric::Detections => None,
            Metric::Cells(c) => Some(*c),
        }
    }
}

/// Subconsulta con una fila por día: `d` (fecha), `n` (valor de la métrica), `max_frp`.
/// Usa los `?` de `filter.params`.
pub fn daily_sql(filter: &SqlFilter, metric: &Metric) -> String {
    let value = match metric {
        Metric::Detections => "count(*)".to_string(),
        // Identificador de celda: fila * 10^6 + columna (únicos para cell_deg ≥ 0.001).
        Metric::Cells(c) => format!(
            "count(DISTINCT floor(latitude / {c})::BIGINT * 1000000 + floor(longitude / {c})::BIGINT)"
        ),
    };
    format!(
        "SELECT acq_date AS d, ({value})::BIGINT AS n, max(frp)::DOUBLE AS max_frp \
         FROM detections_harmonized {} GROUP BY 1",
        filter.clause
    )
}

/// Ejecuta una consulta con parámetros y mapea cada fila.
pub(crate) fn query_rows<T>(
    conn: &Connection,
    sql: &str,
    params: &[Value],
    map: impl FnMut(&Row<'_>) -> duckdb::Result<T>,
) -> Result<Vec<T>, ApiError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(duckdb::params_from_iter(params.iter()), map)?
        .collect::<duckdb::Result<Vec<T>>>()?;
    Ok(rows)
}

pub(crate) fn with_extra(base: &[Value], extra: impl IntoIterator<Item = Value>) -> Vec<Value> {
    let mut all = base.to_vec();
    all.extend(extra);
    all
}

/// Serie de meses (o días) entre `lo` y `hi` de la tabla `bounds`.
fn series_cte(unit: &str) -> String {
    let interval = if unit == "day" {
        "INTERVAL 1 DAY"
    } else {
        "INTERVAL 1 MONTH"
    };
    format!(
        "series AS (SELECT CAST(ts AS DATE) AS p FROM ( \
            SELECT unnest(generate_series(lo::TIMESTAMP, hi::TIMESTAMP, {interval})) AS ts \
            FROM bounds WHERE lo IS NOT NULL AND hi IS NOT NULL AND lo <= hi))"
    )
}

/// `bounds`: desde `from` (o el primer dato) hasta `to` (o el último dato), truncado a `unit`.
fn bounds_cte(unit: &str) -> String {
    format!(
        "bounds AS (SELECT \
            date_trunc('{unit}', coalesce(CAST(? AS DATE), min(p)))::DATE AS lo, \
            date_trunc('{unit}', coalesce(CAST(? AS DATE), max(p)))::DATE AS hi FROM agg)"
    )
}

fn parse_common(
    state: &AppState,
    filter: &DetectionFilter,
    metric: &MetricParams,
) -> Result<(SqlFilter, Metric), ApiError> {
    Ok((filter.to_sql(state.spatial)?, metric.resolve()?))
}

// ---------------------------------------------------------------- /stats/calendar

#[derive(Debug, Default, Deserialize)]
pub struct CalendarParams {
    /// month (por defecto) | day
    pub granularity: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CalendarPoint {
    pub year: i64,
    pub month: i64,
    /// Solo con granularity=day; null por mes.
    pub day: Option<i64>,
    pub count: i64,
    pub max_frp: Option<f64>,
    /// Solo por mes: días del mes con al menos un hotspot; null por día.
    pub active_days: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct CalendarResponse {
    pub metric: &'static str,
    pub cell_deg: Option<f64>,
    pub granularity: &'static str,
    pub from: Option<String>,
    pub to: Option<String>,
    pub items: Vec<CalendarPoint>,
}

async fn calendar(
    State(state): State<AppState>,
    Query(filter): Query<DetectionFilter>,
    Query(mp): Query<MetricParams>,
    Query(cp): Query<CalendarParams>,
) -> Result<Json<CalendarResponse>, ApiError> {
    let (sql, metric) = parse_common(&state, &filter, &mp)?;
    let by_day = match cp
        .granularity
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None | Some("month") => false,
        Some("day") => true,
        Some(_) => return Err(ApiError::bad_request("granularity debe ser month o day")),
    };
    let unit = if by_day { "day" } else { "month" };
    // Que la serie con ceros respete year/month/day (si no, rellenaría los periodos excluidos).
    let period = filter.period_sql("s.p", by_day)?;

    let query = format!(
        "WITH daily AS ({daily}), \
         agg AS (SELECT date_trunc('{unit}', d)::DATE AS p, sum(n)::BIGINT AS n, \
                        max(max_frp) AS max_frp, count(*)::BIGINT AS active_days FROM daily GROUP BY 1), \
         {bounds}, {series} \
         SELECT year(s.p)::BIGINT, month(s.p)::BIGINT, day(s.p)::BIGINT, coalesce(a.n, 0)::BIGINT, \
                a.max_frp::DOUBLE, coalesce(a.active_days, 0)::BIGINT \
         FROM series s LEFT JOIN agg a ON a.p = s.p {period} ORDER BY s.p",
        daily = daily_sql(&sql, &metric),
        bounds = bounds_cte(unit),
        series = series_cte(unit),
        period = period.clause,
    );
    let mut params = with_extra(&sql.params, [opt_text(&sql.from), opt_text(&sql.to)]);
    params.extend(period.params);

    let items = state
        .run_db(move |conn| {
            query_rows(conn, &query, &params, |r| {
                Ok(CalendarPoint {
                    year: r.get(0)?,
                    month: r.get(1)?,
                    day: if by_day { Some(r.get(2)?) } else { None },
                    count: r.get(3)?,
                    max_frp: r.get(4)?,
                    active_days: if by_day { None } else { Some(r.get(5)?) },
                })
            })
        })
        .await?;

    Ok(Json(CalendarResponse {
        metric: metric.name(),
        cell_deg: metric.cell_deg(),
        granularity: unit,
        from: sql.from,
        to: sql.to,
        items,
    }))
}

// ---------------------------------------------------------------- /stats/climatology

#[derive(Debug, Serialize)]
pub struct MonthClimatology {
    /// 1..12
    pub month: i64,
    /// Cuántos años entran en el cálculo para ese mes.
    pub years: i64,
    pub median: f64,
    pub mean: f64,
    pub stddev: Option<f64>,
    pub min: i64,
    pub max: i64,
    pub p25: f64,
    pub p75: f64,
    /// Año con el valor máximo para ese mes.
    pub max_year: i64,
}

#[derive(Debug, Serialize)]
pub struct ClimatologyResponse {
    pub metric: &'static str,
    pub cell_deg: Option<f64>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub months: Vec<MonthClimatology>,
}

async fn climatology(
    State(state): State<AppState>,
    Query(filter): Query<DetectionFilter>,
    Query(mp): Query<MetricParams>,
) -> Result<Json<ClimatologyResponse>, ApiError> {
    let (sql, metric) = parse_common(&state, &filter, &mp)?;
    let period = filter.period_sql("s.p", false)?;
    let query = format!(
        "WITH daily AS ({daily}), \
         agg AS (SELECT date_trunc('month', d)::DATE AS p, sum(n)::BIGINT AS n FROM daily GROUP BY 1), \
         {bounds}, {series}, \
         monthly AS (SELECT s.p AS p, coalesce(a.n, 0)::BIGINT AS n FROM series s LEFT JOIN agg a ON a.p = s.p {period}) \
         SELECT month(p)::BIGINT, count(*)::BIGINT, median(n)::DOUBLE, avg(n)::DOUBLE, \
                stddev_samp(n)::DOUBLE, min(n)::BIGINT, max(n)::BIGINT, \
                quantile_cont(n, 0.25)::DOUBLE, quantile_cont(n, 0.75)::DOUBLE, \
                arg_max(year(p), n)::BIGINT \
         FROM monthly GROUP BY 1 ORDER BY 1",
        daily = daily_sql(&sql, &metric),
        bounds = bounds_cte("month"),
        series = series_cte("month"),
        period = period.clause,
    );
    let mut params = with_extra(&sql.params, [opt_text(&sql.from), opt_text(&sql.to)]);
    params.extend(period.params);

    let months = state
        .run_db(move |conn| {
            query_rows(conn, &query, &params, |r| {
                Ok(MonthClimatology {
                    month: r.get(0)?,
                    years: r.get(1)?,
                    median: r.get(2)?,
                    mean: r.get(3)?,
                    stddev: r.get(4)?,
                    min: r.get(5)?,
                    max: r.get(6)?,
                    p25: r.get(7)?,
                    p75: r.get(8)?,
                    max_year: r.get(9)?,
                })
            })
        })
        .await?;

    Ok(Json(ClimatologyResponse {
        metric: metric.name(),
        cell_deg: metric.cell_deg(),
        from: sql.from,
        to: sql.to,
        months,
    }))
}

// ---------------------------------------------------------------- /stats/summary

#[derive(Debug, Default, Deserialize)]
pub struct SummaryParams {
    /// Cuántos años y meses devolver en los rankings (1..50). Por defecto 5.
    pub top: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct InstrumentCount {
    pub instrument: String,
    pub detections: i64,
}

#[derive(Debug, Serialize)]
pub struct YearTotal {
    pub year: i64,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct MonthTotal {
    pub year: i64,
    pub month: i64,
    pub count: i64,
    pub max_frp: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct SummaryResponse {
    pub metric: &'static str,
    pub cell_deg: Option<f64>,
    /// Detecciones crudas que cumplen el filtro.
    pub detections: i64,
    /// Total según la métrica elegida.
    pub total: i64,
    pub first_date: Option<String>,
    pub last_date: Option<String>,
    pub max_frp: Option<f64>,
    pub by_instrument: Vec<InstrumentCount>,
    pub top_years: Vec<YearTotal>,
    pub top_months: Vec<MonthTotal>,
}

async fn summary(
    State(state): State<AppState>,
    Query(filter): Query<DetectionFilter>,
    Query(mp): Query<MetricParams>,
    Query(sp): Query<SummaryParams>,
) -> Result<Json<SummaryResponse>, ApiError> {
    let (sql, metric) = parse_common(&state, &filter, &mp)?;
    let top = sp.top.unwrap_or(5).clamp(1, 50) as i64;
    let daily = daily_sql(&sql, &metric);
    let clause = sql.clause.clone();
    let params = sql.params.clone();

    let resp = state
        .run_db(move |conn| {
            let (detections, first_date, last_date, max_frp): (i64, Option<String>, Option<String>, Option<f64>) =
                conn.query_row(
                    &format!(
                        "SELECT count(*)::BIGINT, min(acq_date)::VARCHAR, max(acq_date)::VARCHAR, max(frp)::DOUBLE \
                         FROM detections_harmonized {clause}"
                    ),
                    duckdb::params_from_iter(params.iter()),
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?;

            let by_instrument = query_rows(
                conn,
                &format!(
                    "SELECT instrument, count(*)::BIGINT FROM detections_harmonized {clause} \
                     GROUP BY 1 ORDER BY 2 DESC"
                ),
                &params,
                |r| {
                    Ok(InstrumentCount {
                        instrument: r.get(0)?,
                        detections: r.get(1)?,
                    })
                },
            )?;

            let total: i64 = conn.query_row(
                &format!("WITH daily AS ({daily}) SELECT coalesce(sum(n), 0)::BIGINT FROM daily"),
                duckdb::params_from_iter(params.iter()),
                |r| r.get(0),
            )?;

            let top_years = query_rows(
                conn,
                &format!(
                    "WITH daily AS ({daily}) SELECT year(d)::BIGINT AS y, sum(n)::BIGINT AS t \
                     FROM daily GROUP BY 1 ORDER BY t DESC, y LIMIT ?"
                ),
                &with_extra(&params, [Value::BigInt(top)]),
                |r| {
                    Ok(YearTotal {
                        year: r.get(0)?,
                        count: r.get(1)?,
                    })
                },
            )?;

            let top_months = query_rows(
                conn,
                &format!(
                    "WITH daily AS ({daily}) SELECT year(d)::BIGINT AS y, month(d)::BIGINT AS m, \
                     sum(n)::BIGINT AS t, max(max_frp)::DOUBLE FROM daily \
                     GROUP BY 1, 2 ORDER BY t DESC, y, m LIMIT ?"
                ),
                &with_extra(&params, [Value::BigInt(top)]),
                |r| {
                    Ok(MonthTotal {
                        year: r.get(0)?,
                        month: r.get(1)?,
                        count: r.get(2)?,
                        max_frp: r.get(3)?,
                    })
                },
            )?;

            Ok(SummaryResponse {
                metric: metric.name(),
                cell_deg: metric.cell_deg(),
                detections,
                total,
                first_date,
                last_date,
                max_frp,
                by_instrument,
                top_years,
                top_months,
            })
        })
        .await?;
    Ok(Json(resp))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/stats/calendar", get(calendar))
        .route("/stats/climatology", get(climatology))
        .route("/stats/summary", get(summary))
}
