//! Anomalías de actividad de quema por celda y día.
//!
//! El área se divide en una cuadrícula de `cell_deg` grados. Para cada celda y día con
//! actividad se compara lo observado (detecciones) con lo esperado: el promedio diario de esa
//! misma celda en el mismo mes de los años anteriores (desde el primer año con datos del área).
//!
//! `score` es la desviación de Poisson estandarizada con la transformación de Anscombe,
//! `2 · (√(observado + 3/8) − √(esperado + 3/8))`, que se lee como un z-score y está definida
//! aunque lo esperado sea 0.
//!
//! Los filtros de tiempo (`from`, `to`, `date`, `year`, `month`, `day`) eligen QUÉ días se
//! reportan; la referencia histórica siempre usa todos los años anteriores.
//!
//! Endpoints:
//!   GET /stats/anomalies       listado paginado y ordenable
//!   GET /stats/anomalies/{id}  una anomalía (el id codifica fecha y celda)

use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use duckdb::types::Value;
use serde::{Deserialize, Serialize};

use crate::{
    analytics::{query_rows, with_extra},
    entities::crud::PageParams,
    error::ApiError,
    filters::{join_clauses, DetectionFilter, SqlFilter},
    state::AppState,
    timestamp::date_ms,
    validation::date,
};

const DEFAULT_CELL_DEG: f64 = 0.1;
const DEFAULT_MIN_SCORE: f64 = 2.0;
const DEFAULT_MIN_BASELINE_YEARS: i64 = 3;

/// Severidad según `score`.
pub const SEVERITIES: &[(&str, f64)] = &[
    ("extreme", 8.0),
    ("high", 5.0),
    ("moderate", 3.0),
    ("low", 2.0),
];

#[derive(Debug, Default, Deserialize)]
pub struct AnomalyParams {
    /// Tamaño de celda en grados (0.001 a 1). Por defecto 0.1 (≈ 11 km).
    pub cell_deg: Option<f64>,
    /// Años anteriores mínimos para tener referencia (1..50). Por defecto 3.
    pub min_baseline_years: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AnomalyListParams {
    /// Score mínimo. Por defecto 2.
    #[serde(alias = "filter[min_score]")]
    pub min_score: Option<f64>,
    /// Lista separada por comas: low,moderate,high,extreme
    #[serde(alias = "filter[severity]")]
    pub severity: Option<String>,
    /// score | -score (por defecto) | date | -date | observed | -observed
    pub sort: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Location {
    /// Centro de la celda.
    pub latitude: f64,
    pub longitude: f64,
    pub cell_deg: f64,
}

#[derive(Debug, Serialize)]
pub struct AnomalyItem {
    /// `{fecha}_{cell_deg}_{fila}_{columna}`; sirve para GET /stats/anomalies/{id}.
    pub id: String,
    /// Día de la anomalía en epoch ms UTC.
    pub date: i64,
    pub location: Location,
    /// Detecciones en la celda ese día.
    pub observed: i64,
    /// Promedio diario de la celda en el mismo mes de los años anteriores.
    pub expected: f64,
    /// observed / expected. Null si expected es 0.
    pub ratio: Option<f64>,
    /// Desviación de Poisson estandarizada (≈ z-score).
    pub score: f64,
    /// normal | low | moderate | high | extreme
    pub severity: String,
    pub max_frp: Option<f64>,
    /// Años anteriores usados como referencia.
    pub baseline_years: i64,
}

#[derive(Debug, Serialize)]
pub struct AnomalyPage {
    pub cell_deg: f64,
    pub total: i64,
    pub limit: u32,
    pub offset: u64,
    pub items: Vec<AnomalyItem>,
}

/// Identificador de una celda en un día.
#[derive(Debug, PartialEq)]
struct AnomalyId {
    date: String,
    cell_deg: f64,
    row: i64,
    col: i64,
}

impl AnomalyId {
    fn parse(text: &str) -> Result<Self, ApiError> {
        let err =
            || ApiError::bad_request("id debe tener la forma YYYY-MM-DD_cellDeg_fila_columna");
        let parts: Vec<&str> = text.split('_').collect();
        let [d, c, r, k] = parts.as_slice() else {
            return Err(err());
        };
        let date = date("id", d).map_err(|_| err())?;
        let cell_deg = check_cell_deg(c.parse().map_err(|_| err())?)?;
        Ok(Self {
            date,
            cell_deg,
            row: r.parse().map_err(|_| err())?,
            col: k.parse().map_err(|_| err())?,
        })
    }
}

fn format_id(date: &str, cell_deg: f64, row: i64, col: i64) -> String {
    format!("{date}_{cell_deg}_{row}_{col}")
}

fn check_cell_deg(c: f64) -> Result<f64, ApiError> {
    if c.is_finite() && (0.001..=1.0).contains(&c) {
        Ok(c)
    } else {
        Err(ApiError::bad_request("cell_deg debe estar entre 0.001 y 1"))
    }
}

impl AnomalyParams {
    fn cell_deg(&self) -> Result<f64, ApiError> {
        check_cell_deg(self.cell_deg.unwrap_or(DEFAULT_CELL_DEG))
    }

    fn min_baseline_years(&self) -> Result<i64, ApiError> {
        let years = self
            .min_baseline_years
            .unwrap_or(DEFAULT_MIN_BASELINE_YEARS);
        if !(1..=50).contains(&years) {
            return Err(ApiError::bad_request(
                "min_baseline_years debe estar entre 1 y 50",
            ));
        }
        Ok(years)
    }
}

fn severity_sql() -> String {
    let cases: String = SEVERITIES
        .iter()
        .map(|(name, min)| format!("WHEN score >= {min:?} THEN '{name}' "))
        .collect();
    format!("CASE {cases}ELSE 'normal' END")
}

/// CTEs que terminan en `graded`: una fila por celda y día con actividad, ya puntuada.
/// La referencia de cada mes empieza en el primer año en que ese mes está cubierto por los
/// datos (si el primer dato es de noviembre de 2000, enero cuenta desde 2001).
/// Usa los `?` de `base.params` y luego uno más para `min_baseline_years`.
fn graded_cte(base: &SqlFilter, cell: f64) -> String {
    // Años bisiestos en [1, x]: x//4 - x//100 + x//400.
    let leaps = |x: &str| format!("(({x}) // 4 - ({x}) // 100 + ({x}) // 400)");
    format!(
        "WITH cells AS ( \
            SELECT acq_date AS d, floor(latitude / {cell})::BIGINT AS r, \
                   floor(longitude / {cell})::BIGINT AS k, count(*)::BIGINT AS n, \
                   max(frp)::DOUBLE AS max_frp \
            FROM detections_harmonized {clause} GROUP BY 1, 2, 3), \
         first_date AS (SELECT min(d) AS fd FROM cells), \
         monthly AS (SELECT r, k, year(d)::BIGINT AS y, month(d)::BIGINT AS m, sum(n)::BIGINT AS t \
                     FROM cells GROUP BY 1, 2, 3, 4), \
         history AS (SELECT r, k, y, m, coalesce(sum(t) OVER (PARTITION BY r, k, m ORDER BY y \
                         RANGE BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING), 0)::BIGINT AS base_total \
                     FROM monthly), \
         scored AS ( \
            SELECT c.d, c.r, c.k, c.n, c.max_frp, h.base_total, \
                   (h.y - s.fy)::BIGINT AS base_years, \
                   ((h.y - s.fy) * day(last_day(make_date(2001, h.m, 1))) \
                    + CASE WHEN h.m = 2 THEN {leaps_to} - {leaps_from} ELSE 0 END)::BIGINT AS base_days \
            FROM cells c \
            JOIN history h ON h.r = c.r AND h.k = c.k AND h.y = year(c.d) AND h.m = month(c.d) \
            CROSS JOIN first_date f \
            CROSS JOIN LATERAL (SELECT (year(f.fd) + CASE WHEN month(f.fd) > h.m THEN 1 ELSE 0 END)::BIGINT AS fy) s), \
         expected AS (SELECT *, base_total::DOUBLE / base_days AS expected FROM scored \
                      WHERE base_years >= ? AND base_days > 0), \
         points AS (SELECT *, 2 * (sqrt(n + 0.375) - sqrt(expected + 0.375)) AS score FROM expected), \
         graded AS (SELECT *, {severity} AS severity FROM points)",
        clause = base.clause,
        leaps_to = leaps("h.y - 1"),
        leaps_from = leaps("s.fy - 1"),
        severity = severity_sql(),
    )
}

const SELECT_COLUMNS: &str = "d::VARCHAR, r, k, n, expected, score, severity, max_frp, base_years";

fn map_item(cell: f64) -> impl FnMut(&duckdb::Row<'_>) -> duckdb::Result<AnomalyItem> {
    move |r| {
        let date: String = r.get(0)?;
        let row: i64 = r.get(1)?;
        let col: i64 = r.get(2)?;
        let observed: i64 = r.get(3)?;
        let expected: f64 = r.get(4)?;
        Ok(AnomalyItem {
            id: format_id(&date, cell, row, col),
            date: date_ms(&date).unwrap_or_default(),
            location: Location {
                latitude: (row as f64 + 0.5) * cell,
                longitude: (col as f64 + 0.5) * cell,
                cell_deg: cell,
            },
            observed,
            expected,
            ratio: (expected > 0.0).then(|| observed as f64 / expected),
            score: r.get(5)?,
            severity: r.get(6)?,
            max_frp: r.get(7)?,
            baseline_years: r.get(8)?,
        })
    }
}

fn order_by(sort: Option<&str>) -> Result<&'static str, ApiError> {
    Ok(match sort.map(str::trim).unwrap_or("-score") {
        "" | "-score" => "score DESC, d, r, k",
        "score" => "score, d, r, k",
        "date" => "d, score DESC, r, k",
        "-date" => "d DESC, score DESC, r, k",
        "observed" => "n, d, r, k",
        "-observed" => "n DESC, d, r, k",
        _ => {
            return Err(ApiError::bad_request(
                "sort debe ser score, -score, date, -date, observed o -observed",
            ))
        }
    })
}

fn severity_list(text: Option<&str>) -> Result<Vec<String>, ApiError> {
    let Some(text) = text else {
        return Ok(Vec::new());
    };
    text.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if SEVERITIES.iter().any(|(name, _)| *name == s) {
                Ok(s)
            } else {
                Err(ApiError::bad_request(
                    "severity debe ser una lista de low, moderate, high o extreme",
                ))
            }
        })
        .collect()
}

/// GET /stats/anomalies
async fn list(
    State(state): State<AppState>,
    Query(filter): Query<DetectionFilter>,
    Query(ap): Query<AnomalyParams>,
    Query(lp): Query<AnomalyListParams>,
    Query(page): Query<PageParams>,
) -> Result<Json<AnomalyPage>, ApiError> {
    let (cell, min_years) = (ap.cell_deg()?, ap.min_baseline_years()?);
    let base = filter.to_sql_without_time(state.spatial)?;
    let time = filter.time_sql("d")?;

    let min_score = lp.min_score.unwrap_or(DEFAULT_MIN_SCORE);
    if !min_score.is_finite() {
        return Err(ApiError::bad_request("min_score debe ser un número"));
    }
    let severities = severity_list(lp.severity.as_deref())?;
    let order = order_by(lp.sort.as_deref())?;
    let (limit, offset) = page.resolve(100, state.cfg.max_rows);

    let mut clause = join_clauses(&time.clause, "WHERE score >= ?");
    let mut filter_params = time.params;
    filter_params.push(Value::Double(min_score));
    if !severities.is_empty() {
        let marks = vec!["?"; severities.len()].join(", ");
        clause = join_clauses(&clause, &format!("WHERE severity IN ({marks})"));
        filter_params.extend(severities.into_iter().map(Value::Text));
    }

    let cte = graded_cte(&base, cell);
    let count_sql = format!("{cte} SELECT count(*)::BIGINT FROM graded {clause}");
    let page_sql = format!(
        "{cte} SELECT {SELECT_COLUMNS} FROM graded {clause} ORDER BY {order} LIMIT ? OFFSET ?"
    );
    let params = with_extra(&base.params, [Value::BigInt(min_years)]);
    let params = with_extra(&params, filter_params);
    let page_params = with_extra(
        &params,
        [Value::BigInt(limit as i64), Value::BigInt(offset as i64)],
    );

    let (total, items) = state
        .run_db(move |conn| {
            let total: i64 =
                conn.query_row(&count_sql, duckdb::params_from_iter(params.iter()), |r| {
                    r.get(0)
                })?;
            let items = query_rows(conn, &page_sql, &page_params, map_item(cell))?;
            Ok((total, items))
        })
        .await?;

    Ok(Json(AnomalyPage {
        cell_deg: cell,
        total,
        limit,
        offset,
        items,
    }))
}

/// GET /stats/anomalies/{id}
/// Recalcula esa celda y día con los filtros de área y atributos de la query
/// (los de tiempo se ignoran: la fecha viene en el id).
async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(filter): Query<DetectionFilter>,
    Query(ap): Query<AnomalyParams>,
) -> Result<Json<AnomalyItem>, ApiError> {
    let id = AnomalyId::parse(&id)?;
    // El tamaño de celda viene en el id; el `cell_deg` de la query no se usa.
    let min_years = ap.min_baseline_years()?;
    let base = filter.to_sql_without_time(state.spatial)?;
    let cell = id.cell_deg;

    let sql = format!(
        "{cte} SELECT {SELECT_COLUMNS} FROM graded WHERE d = CAST(? AS DATE) AND r = ? AND k = ?",
        cte = graded_cte(&base, cell)
    );
    let params = with_extra(
        &base.params,
        [
            Value::BigInt(min_years),
            Value::Text(id.date.clone()),
            Value::BigInt(id.row),
            Value::BigInt(id.col),
        ],
    );

    let mut items = state
        .run_db(move |conn| query_rows(conn, &sql, &params, map_item(cell)))
        .await?;
    items.pop().map(Json).ok_or_else(|| {
        ApiError::not_found(
            "no hay actividad en esa celda y día con estos filtros, o no hay suficientes años de referencia",
        )
    })
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/stats/anomalies", get(list))
        .route("/stats/anomalies/{id}", get(get_one))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_ida_y_vuelta() {
        let text = format_id("2024-03-14", 0.1, 136, -893);
        assert_eq!(text, "2024-03-14_0.1_136_-893");
        assert_eq!(
            AnomalyId::parse(&text).unwrap(),
            AnomalyId {
                date: "2024-03-14".into(),
                cell_deg: 0.1,
                row: 136,
                col: -893,
            }
        );
        assert!(AnomalyId::parse("2024-03-14_0.1_136").is_err());
        assert!(AnomalyId::parse("2024-02-30_0.1_1_1").is_err());
        assert!(AnomalyId::parse("2024-03-14_5_1_1").is_err());
    }

    #[test]
    fn orden_y_severidad() {
        assert!(order_by(None).unwrap().starts_with("score DESC"));
        assert!(order_by(Some("-date")).is_ok());
        assert!(order_by(Some("id")).is_err());
        assert_eq!(severity_list(Some("High, low")).unwrap(), ["high", "low"]);
        assert!(severity_list(Some("normal")).is_err());
        assert!(severity_sql().starts_with("CASE WHEN score >= 8.0 THEN 'extreme'"));
    }

    #[test]
    fn placeholders_cuadran() {
        let base = DetectionFilter {
            bbox: Some("-90,13,-87,14".into()),
            instrument: Some("MODIS".into()),
            ..Default::default()
        }
        .to_sql_without_time(false)
        .unwrap();
        let cte = graded_cte(&base, 0.1);
        assert_eq!(cte.matches('?').count(), base.params.len() + 1);
    }
    /// Ejecuta las CTEs contra DuckDB real: una celda con 1 detección por marzo en 2020-2023
    /// y 20 el 2024-03-14.
    #[test]
    fn sql_en_duckdb() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        for y in 2020..=2023 {
            conn.execute(
                "INSERT INTO detections (latitude, longitude, acq_date, acq_time, satellite, instrument) \
                 VALUES (13.65, -89.25, CAST(? AS DATE), '1200', 'Aqua', 'MODIS')",
                [format!("{y}-03-10")],
            )
            .unwrap();
        }
        for t in 0..20 {
            conn.execute(
                "INSERT INTO detections (latitude, longitude, acq_date, acq_time, satellite, instrument) \
                 VALUES (13.65, -89.25, DATE '2024-03-14', ?, 'Aqua', 'MODIS')",
                [format!("{t:04}")],
            )
            .unwrap();
        }

        // Primer dato del área: noviembre de 2019, en otra celda. Marzo de 2019 no está
        // cubierto, así que la referencia de marzo empieza en 2020 (4 años para 2024, no 5).
        conn.execute(
            "INSERT INTO detections (latitude, longitude, acq_date, acq_time, satellite, instrument) \
             VALUES (14.05, -88.05, DATE '2019-11-05', '1200', 'Aqua', 'MODIS')",
            [],
        )
        .unwrap();

        let base = DetectionFilter::default()
            .to_sql_without_time(false)
            .unwrap();
        let cte = graded_cte(&base, 0.1);
        let sql = format!(
            "{cte} SELECT {SELECT_COLUMNS} FROM graded WHERE score >= ? ORDER BY score DESC"
        );
        let params = with_extra(&base.params, [Value::BigInt(3), Value::Double(2.0)]);
        let items = query_rows(&conn, &sql, &params, map_item(0.1)).unwrap();

        assert_eq!(items.len(), 1, "solo 2024-03-14 supera el score mínimo");
        let a = &items[0];
        assert_eq!(a.id, "2024-03-14_0.1_136_-893");
        assert_eq!(a.observed, 20);
        assert_eq!(a.baseline_years, 4);
        // 4 detecciones en 4 marzos de 31 días.
        assert!((a.expected - 4.0 / 124.0).abs() < 1e-9);
        let score = 2.0 * ((20.375f64).sqrt() - (4.0 / 124.0 + 0.375f64).sqrt());
        assert!((a.score - score).abs() < 1e-9);
        assert_eq!(a.severity, "high");
        assert!((a.location.latitude - 13.65).abs() < 1e-9);
        assert!((a.location.longitude + 89.25).abs() < 1e-9);

        // Por id: misma celda y día.
        let by_id = format!(
            "{cte} SELECT {SELECT_COLUMNS} FROM graded WHERE d = CAST(? AS DATE) AND r = ? AND k = ?"
        );
        let params = with_extra(
            &base.params,
            [
                Value::BigInt(3),
                Value::Text("2024-03-14".into()),
                Value::BigInt(136),
                Value::BigInt(-893),
            ],
        );
        let one = query_rows(&conn, &by_id, &params, map_item(0.1)).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].score, a.score);

        // Con menos años de referencia aparece también 2023 (score bajo, "normal").
        let params = with_extra(&base.params, [Value::BigInt(1), Value::Double(-100.0)]);
        let all = query_rows(&conn, &sql, &params, map_item(0.1)).unwrap();
        assert_eq!(all.len(), 4);
        assert!(all.iter().skip(1).all(|i| i.severity == "normal"));
    }
}
