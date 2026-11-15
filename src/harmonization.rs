//! Armonización VIIRS → MODIS de la serie mensual (port del notebook "Armonización").
//!
//! El factor y la anomalía dependen de TODO el histórico (traslape de sensores, mediana por
//! mes), así que no se calculan por CSV: después de cada importación con filas nuevas se
//! recalcula todo y se guarda en tablas derivadas (`harmonization_*`, `harmonized_monthly`).
//!
//! Pasos (todos en SQL de DuckDB):
//!   1. Detecciones MODIS/VIIRS con satélite reconocido, dentro del área, con fecha local y
//!      celda de `cell_deg`. Los duplicados (satélite, instante y coordenadas a 4 decimales)
//!      ya los quita la vista `detections_harmonized`.
//!   2. Limpieza: confianza mínima, `type` = 0 y fuera las celdas con fuego en más del
//!      `static_threshold` de los meses (solo si hay al menos `static_min_months` meses).
//!   3. Satélites de la serie: MODIS solo Aqua; VIIRS un satélite por día según la prioridad
//!      (Suomi NPP hasta `snpp_end`, luego NOAA-20...).
//!   4. Celdas con fuego por día y sensor (días sin fuego = 0), suma mensual = celdas-día.
//!   5. Factor por mes calendario = Σ MODIS / Σ VIIRS en los meses completos de ambos sensores
//!      hasta `overlap_end`. Si VIIRS tiene menos de `min_cells_month`, factor de la época.
//!   6. Serie: MODIS antes del traslape, VIIRS × factor desde el inicio del traslape.
//!   7. Validación dejando fuera un año a la vez y anomalía robusta
//!      (valor − mediana) / (p75 − p25) con los años de referencia.
//!
//! Endpoints:
//!   POST /stats/harmonization/rebuild  recalcular a mano
//!   GET  /stats/harmonization     última corrida, factores y validación
//!   GET  /stats/harmonized        serie mensual armonizada con anomalía (startDate + endDate)

use std::sync::Mutex;

use axum::{
    extract::{Query, State},
    routing::{get, post},
    Json, Router,
};
use duckdb::{types::Value, Connection, Row};
use serde::{Deserialize, Serialize};

use crate::{
    analytics::query_rows, config::HarmonizationConfig, error::ApiError, filters::date_range,
    state::AppState,
};

/// Un solo recálculo a la vez: dos importaciones simultáneas chocarían al reescribir las tablas.
static RECOMPUTE: Mutex<()> = Mutex::new(());

/// Texto como literal SQL. Solo se usa con valores de la configuración del servidor.
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Factores por mes calendario a partir de `src`, que devuelve (k, ms, modis, viirs).
/// `k` agrupa cálculos independientes (0 para el factor final, el año excluido en la validación).
fn factor_sql(src: &str, cfg: &HarmonizationConfig) -> String {
    let dry = if cfg.dry_months.is_empty() {
        "-1".to_string()
    } else {
        cfg.dry_months
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let min = cfg.min_cells_month;
    format!(
        "WITH src AS ({src}),
        g AS (SELECT k, month(ms) AS m, sum(modis) AS modis, sum(viirs) AS viirs FROM src GROUP BY ALL),
        grid AS (SELECT k, m FROM (SELECT DISTINCT k FROM src) CROSS JOIN range(1, 13) t(m)),
        gm AS (
            SELECT grid.k, grid.m, coalesce(g.modis, 0) AS modis, coalesce(g.viirs, 0) AS viirs,
                   CASE WHEN grid.m IN ({dry}) THEN 'dry' ELSE 'wet' END AS season
            FROM grid LEFT JOIN g ON g.k = grid.k AND g.m = grid.m),
        se AS (SELECT k, season, sum(modis) / NULLIF(sum(viirs), 0) AS f_season FROM gm GROUP BY ALL)
        SELECT gm.k, gm.m::INTEGER AS m, gm.season, gm.modis::DOUBLE AS modis, gm.viirs::DOUBLE AS viirs,
               gm.modis / NULLIF(gm.viirs, 0) AS f_month, se.f_season,
               CASE WHEN gm.viirs >= {min} THEN gm.modis / NULLIF(gm.viirs, 0) ELSE se.f_season END AS factor,
               CASE WHEN gm.viirs >= {min} THEN 'month' ELSE 'season' END AS source
        FROM gm JOIN se ON se.k = gm.k AND se.season = gm.season"
    )
}

/// Tablas temporales con todos los pasos del método.
fn pipeline_sql(cfg: &HarmonizationConfig) -> String {
    let c = cfg.cell_deg;
    let bbox = match cfg.bbox {
        Some([w, s, e, n]) => {
            format!("AND latitude BETWEEN {s} AND {n} AND longitude BETWEEN {w} AND {e}")
        }
        None => String::new(),
    };
    let min_conf = min_confidence_level(cfg);
    let off = cfg.utc_offset_hours;
    let thr = cfg.static_threshold;
    let static_min = cfg.static_min_months;
    let snpp_end = sql_str(&cfg.snpp_end);
    let overlap_end = sql_str(&cfg.overlap_end);
    let prio = if cfg.viirs_priority.is_empty() {
        "(NULL::VARCHAR, 0)".to_string()
    } else {
        cfg.viirs_priority
            .iter()
            .enumerate()
            .map(|(i, s)| format!("({}, {})", sql_str(s), i + 1))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let [y0, y1] = cfg.reference_years;
    let factors = factor_sql("SELECT 0 AS k, ms, modis, viirs FROM h_overlap", cfg);
    let loyo = factor_sql(
        "SELECT y.year AS k, o.ms, o.modis, o.viirs \
         FROM (SELECT DISTINCT year(ms) AS year FROM h_overlap) y \
         JOIN h_overlap o ON year(o.ms) <> y.year",
        cfg,
    );

    format!(
        "
-- 1. Unificar: satélite reconocido, dentro del área, fecha local y celda (sin duplicados: vista).
CREATE OR REPLACE TEMP TABLE h_det AS
SELECT sensor, sat,
       CAST(utc + to_hours({off}) AS DATE) AS d,
       floor(latitude / {c})::BIGINT AS cell_row,
       floor(longitude / {c})::BIGINT AS cell_col,
       confidence_level, fire_type
FROM (
    SELECT instrument AS sensor, satellite_name AS sat, latitude, longitude,
           confidence_level, coalesce(\"type\", 0) AS fire_type,
           acq_date + make_time(CAST(substr(acq_time, 1, 2) AS BIGINT),
                                CAST(substr(acq_time, 3, 2) AS BIGINT), 0.0) AS utc
    FROM detections_harmonized
    WHERE instrument IN ('MODIS', 'VIIRS') AND satellite_name IS NOT NULL
      AND length(acq_time) = 4 {bbox}
);

-- 2. Limpiar: confianza, type = 0 y fuentes estáticas.
CREATE OR REPLACE TEMP TABLE h_clean0 AS
SELECT * FROM h_det WHERE confidence_level >= {min_conf} AND fire_type = 0;

CREATE OR REPLACE TEMP TABLE h_static AS
WITH tot AS (SELECT count(DISTINCT date_trunc('month', d)) AS n FROM h_clean0),
cm AS (
    SELECT cell_row, cell_col, count(DISTINCT date_trunc('month', d)) AS months
    FROM h_clean0 GROUP BY cell_row, cell_col)
SELECT cm.cell_row, cm.cell_col, cm.months, tot.n AS total_months
FROM cm CROSS JOIN tot
WHERE tot.n >= {static_min} AND cm.months > {thr} * tot.n;

CREATE OR REPLACE TEMP TABLE h_clean AS
SELECT c.* FROM h_clean0 c
ANTI JOIN h_static s ON c.cell_row = s.cell_row AND c.cell_col = s.cell_col;

-- 3. Satélites de la serie: Aqua y un VIIRS por día según la prioridad.
CREATE OR REPLACE TEMP TABLE h_series AS
WITH prio(sat, rnk) AS (VALUES {prio}),
periods AS (
    SELECT sat, min(d) AS first_d,
           CASE WHEN sat = 'Suomi NPP' THEN least(max(d), CAST({snpp_end} AS DATE) - 1)
                ELSE max(d) END AS last_d
    FROM h_clean GROUP BY sat),
ref AS (
    SELECT v.d, arg_min(p.sat, prio.rnk) AS sat
    FROM (SELECT DISTINCT d FROM h_clean WHERE sensor = 'VIIRS') v
    JOIN periods p ON v.d BETWEEN p.first_d AND p.last_d
    JOIN prio ON prio.sat = p.sat
    GROUP BY v.d)
SELECT c.sensor, c.sat, c.d, c.cell_row * 1000000 + c.cell_col AS cell
FROM h_clean c
WHERE (c.sensor = 'MODIS' AND c.sat = 'Aqua')
   OR (c.sensor = 'VIIRS' AND EXISTS (SELECT 1 FROM ref WHERE ref.d = c.d AND ref.sat = c.sat));

-- 4. Celdas con fuego por día (con ceros) y por mes.
CREATE OR REPLACE TEMP TABLE h_daily AS
WITH agg AS (SELECT sensor, d, count(DISTINCT cell) AS cells FROM h_series GROUP BY sensor, d),
rng AS (SELECT sensor, min(d) AS a, max(d) AS b FROM agg GROUP BY sensor),
days AS (
    SELECT sensor, CAST(t AS DATE) AS d FROM (
        SELECT sensor, unnest(generate_series(CAST(a AS TIMESTAMP), CAST(b AS TIMESTAMP), INTERVAL 1 DAY)) AS t
        FROM rng))
SELECT days.sensor, days.d, coalesce(agg.cells, 0) AS cells
FROM days LEFT JOIN agg ON agg.sensor = days.sensor AND agg.d = days.d;

CREATE OR REPLACE TEMP TABLE h_msensor AS
WITH rng AS (SELECT sensor, min(d) AS a, max(d) AS b FROM h_daily GROUP BY sensor),
m AS (
    SELECT sensor, CAST(date_trunc('month', d) AS DATE) AS ms, sum(cells)::DOUBLE AS value
    FROM h_daily GROUP BY ALL)
SELECT m.sensor, m.ms, m.value,
       m.ms >= CASE WHEN day(rng.a) = 1 THEN rng.a
                    ELSE CAST(date_trunc('month', rng.a) + INTERVAL 1 MONTH AS DATE) END
       AND last_day(m.ms) <= rng.b AS complete
FROM m JOIN rng ON rng.sensor = m.sensor;

CREATE OR REPLACE TEMP TABLE h_monthly AS
WITH months AS (
    SELECT CAST(t AS DATE) AS ms FROM (
        SELECT unnest(generate_series(CAST(a AS TIMESTAMP), CAST(b AS TIMESTAMP), INTERVAL 1 MONTH)) AS t
        FROM (SELECT min(ms) AS a, max(ms) AS b FROM h_msensor)))
SELECT months.ms, mo.value AS modis, vi.value AS viirs,
       coalesce(mo.complete, false) AS modis_complete,
       coalesce(vi.complete, false) AS viirs_complete
FROM months
LEFT JOIN h_msensor mo ON mo.sensor = 'MODIS' AND mo.ms = months.ms
LEFT JOIN h_msensor vi ON vi.sensor = 'VIIRS' AND vi.ms = months.ms;

-- 5. Traslape y factor por mes calendario.
CREATE OR REPLACE TEMP TABLE h_overlap AS
SELECT ms, modis, viirs FROM h_monthly
WHERE modis_complete AND viirs_complete AND ms <= CAST({overlap_end} AS DATE);

CREATE OR REPLACE TEMP TABLE h_factors AS {factors};

-- 6. Serie armonizada y anomalía robusta.
CREATE OR REPLACE TEMP TABLE h_harmonized AS
WITH st AS (SELECT min(ms) AS s FROM h_overlap),
lastd AS (SELECT max(d) AS b FROM h_daily),
base AS (
    SELECT m.ms, m.modis, m.viirs,
           CASE WHEN st.s IS NOT NULL AND m.ms >= st.s THEN m.viirs * f.factor ELSE m.modis END AS harmonized,
           CASE WHEN st.s IS NOT NULL AND m.ms >= st.s THEN 'viirs_factor' ELSE 'modis' END AS source,
           last_day(m.ms) <= lastd.b AS complete
    FROM h_monthly m CROSS JOIN st CROSS JOIN lastd
    LEFT JOIN h_factors f ON f.m = month(m.ms)),
clim AS (
    SELECT month(ms) AS m, median(harmonized) AS med,
           quantile_cont(harmonized, 0.25) AS p25, quantile_cont(harmonized, 0.75) AS p75
    FROM base
    WHERE complete AND harmonized IS NOT NULL AND year(ms) BETWEEN {y0} AND {y1}
    GROUP BY 1),
an AS (
    SELECT base.*, clim.med, clim.p25, clim.p75,
           CASE WHEN base.complete
                THEN (base.harmonized - clim.med) / NULLIF(clim.p75 - clim.p25, 0) END AS anomaly
    FROM base LEFT JOIN clim ON clim.m = month(base.ms))
SELECT *,
       CASE WHEN anomaly IS NULL THEN NULL
            WHEN anomaly <= -1 THEN 'very_low'
            WHEN anomaly <= -0.5 THEN 'low'
            WHEN anomaly <= 0.5 THEN 'normal'
            WHEN anomaly <= 1 THEN 'high'
            ELSE 'very_high' END AS level
FROM an;

-- 7. Validación dejando fuera un año a la vez.
CREATE OR REPLACE TEMP TABLE h_loyo AS {loyo};

CREATE OR REPLACE TEMP TABLE h_validation AS
SELECT year(o.ms) AS year, count(*) AS months, sum(o.modis) AS modis, sum(o.viirs) AS viirs_raw,
       sum(o.viirs * f.factor) AS viirs_harmonized
FROM h_overlap o JOIN h_loyo f ON f.k = year(o.ms) AND f.m = month(o.ms)
GROUP BY 1;
"
    )
}

fn min_confidence_level(cfg: &HarmonizationConfig) -> i32 {
    if cfg.min_confidence == "high" {
        3
    } else {
        2
    }
}

const DROP_TEMP_SQL: &str = "
DROP TABLE IF EXISTS h_det; DROP TABLE IF EXISTS h_clean0; DROP TABLE IF EXISTS h_static;
DROP TABLE IF EXISTS h_clean; DROP TABLE IF EXISTS h_series; DROP TABLE IF EXISTS h_daily;
DROP TABLE IF EXISTS h_msensor; DROP TABLE IF EXISTS h_monthly; DROP TABLE IF EXISTS h_overlap;
DROP TABLE IF EXISTS h_factors; DROP TABLE IF EXISTS h_harmonized; DROP TABLE IF EXISTS h_loyo;
DROP TABLE IF EXISTS h_validation;";

/// Resumen de una corrida.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    /// Momento del cálculo en epoch ms UTC.
    pub computed_at: i64,
    /// ok | no_overlap (solo MODIS, sin factor) | insufficient_data (sin datos de Aqua MODIS)
    pub status: String,
    /// Detecciones tras quitar duplicados y aplicar el área.
    pub detections: i64,
    pub after_confidence: i64,
    pub after_type: i64,
    /// Celdas descartadas como fuentes estáticas.
    pub static_cells: i64,
    pub after_static: i64,
    /// Detecciones usadas en la serie (Aqua + el VIIRS de referencia).
    pub used: i64,
    /// Primer día de cada mes en epoch ms UTC.
    pub first_month: Option<i64>,
    pub last_month: Option<i64>,
    pub overlap_start: Option<i64>,
    pub overlap_end: Option<i64>,
    pub overlap_months: i64,
    /// Diferencia anual promedio de VIIRS crudo contra MODIS en el traslape (%).
    pub mean_raw_diff_pct: Option<f64>,
    /// Diferencia anual absoluta promedio de VIIRS armonizado (validación cruzada) contra MODIS (%).
    pub mean_abs_harmonized_diff_pct: Option<f64>,
    /// Correlación mensual MODIS vs VIIRS armonizado (descriptiva, no independiente).
    pub correlation: Option<f64>,
    /// Configuración usada.
    pub params: serde_json::Value,
}

const RUN_COLUMNS: &str = "epoch_ms(computed_at), status, detections, \
    after_confidence, after_type, static_cells, after_static, used, \
    epoch_ms(first_month::TIMESTAMP), epoch_ms(last_month::TIMESTAMP), \
    epoch_ms(overlap_start::TIMESTAMP), epoch_ms(overlap_end::TIMESTAMP), overlap_months, \
    mean_raw_diff_pct, mean_abs_harmonized_diff_pct, correlation, params";

fn run_from_row(row: &Row<'_>) -> duckdb::Result<RunSummary> {
    let params: String = row.get(16)?;
    Ok(RunSummary {
        computed_at: row.get(0)?,
        status: row.get(1)?,
        detections: row.get(2)?,
        after_confidence: row.get(3)?,
        after_type: row.get(4)?,
        static_cells: row.get(5)?,
        after_static: row.get(6)?,
        used: row.get(7)?,
        first_month: row.get(8)?,
        last_month: row.get(9)?,
        overlap_start: row.get(10)?,
        overlap_end: row.get(11)?,
        overlap_months: row.get(12)?,
        mean_raw_diff_pct: row.get(13)?,
        mean_abs_harmonized_diff_pct: row.get(14)?,
        correlation: row.get(15)?,
        params: serde_json::from_str(&params).unwrap_or(serde_json::Value::Null),
    })
}

/// Última corrida guardada.
pub fn latest_run(conn: &Connection) -> Result<Option<RunSummary>, ApiError> {
    let sql = format!("SELECT {RUN_COLUMNS} FROM harmonization_runs ORDER BY id DESC LIMIT 1");
    Ok(query_rows(conn, &sql, &[], run_from_row)?
        .into_iter()
        .next())
}

/// Recalcula la armonización completa y reemplaza las tablas derivadas.
pub fn recompute(conn: &mut Connection, cfg: &HarmonizationConfig) -> Result<RunSummary, ApiError> {
    let _guard = RECOMPUTE.lock().unwrap_or_else(|e| e.into_inner());
    let c = cfg.cell_deg;
    let min_conf = min_confidence_level(cfg);
    let params_json = serde_json::to_string(cfg).map_err(|e| ApiError::internal(e.to_string()))?;
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0);

    let tx = conn.transaction()?;
    tx.execute_batch(&pipeline_sql(cfg))?;

    let (aqua, overlap): (i64, i64) = tx.query_row(
        "SELECT (SELECT count(*) FROM h_series WHERE sensor = 'MODIS'), (SELECT count(*) FROM h_overlap)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let status = if aqua == 0 {
        "insufficient_data"
    } else if overlap == 0 {
        "no_overlap"
    } else {
        "ok"
    };

    let harmonized_insert = if aqua == 0 {
        ""
    } else {
        "INSERT INTO harmonized_monthly (month_start, modis, viirs, harmonized, source, complete, \
             median, p25, p75, anomaly, \"level\") \
         SELECT ms, modis, viirs, harmonized, source, complete, med, p25, p75, anomaly, level \
         FROM h_harmonized;"
    };
    tx.execute_batch(&format!(
        "DELETE FROM harmonization_factors;
         INSERT INTO harmonization_factors (\"month\", season, modis, viirs, f_month, f_season, factor, source)
         SELECT m, season, modis, viirs, f_month, f_season, factor, source FROM h_factors;
         DELETE FROM harmonized_monthly;
         {harmonized_insert}
         DELETE FROM harmonization_validation;
         INSERT INTO harmonization_validation (\"year\", months, modis, viirs_raw, viirs_harmonized,
             raw_diff_pct, harmonized_diff_pct)
         SELECT year, months, modis, viirs_raw, viirs_harmonized,
                100 * (viirs_raw / NULLIF(modis, 0) - 1),
                100 * (viirs_harmonized / NULLIF(modis, 0) - 1)
         FROM h_validation;
         DELETE FROM harmonization_static_cells;
         INSERT INTO harmonization_static_cells (cell_row, cell_col, latitude, longitude, months, total_months)
         SELECT cell_row, cell_col, (cell_row + 0.5) * {c}, (cell_col + 0.5) * {c}, months, total_months
         FROM h_static;"
    ))?;

    tx.execute(
        &format!(
            "INSERT INTO harmonization_runs (computed_at, status, detections, after_confidence,
                 after_type, static_cells, after_static, used, first_month, last_month,
                 overlap_start, overlap_end, overlap_months, mean_raw_diff_pct,
                 mean_abs_harmonized_diff_pct, correlation, params)
             SELECT make_timestamp(CAST(? AS BIGINT)), ?,
                 (SELECT count(*) FROM h_det),
                 (SELECT count(*) FROM h_det WHERE confidence_level >= {min_conf}),
                 (SELECT count(*) FROM h_clean0),
                 (SELECT count(*) FROM h_static),
                 (SELECT count(*) FROM h_clean),
                 (SELECT count(*) FROM h_series),
                 (SELECT min(ms) FROM h_monthly),
                 (SELECT max(ms) FROM h_monthly),
                 (SELECT min(ms) FROM h_overlap),
                 (SELECT max(ms) FROM h_overlap),
                 (SELECT count(*) FROM h_overlap),
                 (SELECT avg(100 * (viirs_raw / NULLIF(modis, 0) - 1)) FROM h_validation),
                 (SELECT avg(abs(100 * (viirs_harmonized / NULLIF(modis, 0) - 1))) FROM h_validation),
                 (SELECT corr(o.modis, o.viirs * f.factor)
                    FROM h_overlap o JOIN h_factors f ON f.m = month(o.ms)),
                 ?"
        ),
        duckdb::params![now_us, status, params_json],
    )?;
    // Historial acotado de corridas.
    tx.execute_batch(
        "DELETE FROM harmonization_runs WHERE id NOT IN \
         (SELECT id FROM harmonization_runs ORDER BY id DESC LIMIT 100);",
    )?;
    tx.execute_batch(DROP_TEMP_SQL)?;
    tx.commit()?;

    latest_run(conn)?.ok_or_else(|| ApiError::internal("no se guardó la corrida de armonización"))
}

// ---------------------------------------------------------------- Lectura

#[derive(Debug, Serialize)]
pub struct Factor {
    /// 1..12
    pub month: i32,
    /// dry | wet
    pub season: String,
    /// Celdas-día MODIS y VIIRS en los meses de traslape.
    pub modis: f64,
    pub viirs: f64,
    pub f_month: Option<f64>,
    pub f_season: Option<f64>,
    /// Factor aplicado (celdas MODIS por cada celda VIIRS).
    pub factor: Option<f64>,
    /// month | season
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct ValidationYear {
    pub year: i32,
    pub months: i64,
    pub modis: f64,
    pub viirs_raw: f64,
    /// VIIRS × factor calculado sin ese año.
    pub viirs_harmonized: Option<f64>,
    pub raw_diff_pct: Option<f64>,
    pub harmonized_diff_pct: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct HarmonizationReport {
    /// None si todavía no se calculó.
    pub run: Option<RunSummary>,
    pub factors: Vec<Factor>,
    pub validation: Vec<ValidationYear>,
}

#[derive(Debug, Serialize)]
pub struct HarmonizedMonth {
    /// Primer día del mes en epoch ms UTC.
    pub month: i64,
    /// Celdas-día MODIS Aqua.
    pub modis: Option<f64>,
    /// Celdas-día VIIRS (satélite de referencia).
    pub viirs: Option<f64>,
    /// Equivalente MODIS.
    pub harmonized: Option<f64>,
    /// modis | viirs_factor
    pub source: String,
    /// false si el mes no terminó o no tiene datos hasta el final.
    pub complete: bool,
    /// Mediana, p25 y p75 del mismo mes en los años de referencia.
    pub median: Option<f64>,
    pub p25: Option<f64>,
    pub p75: Option<f64>,
    /// (harmonized − median) / (p75 − p25). Null en meses incompletos.
    pub anomaly: Option<f64>,
    /// very_low | low | normal | high | very_high
    pub level: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HarmonizedSeries {
    pub run: Option<RunSummary>,
    pub items: Vec<HarmonizedMonth>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SeriesParams {
    /// YYYY-MM-DD: desde el mes de esa fecha. Se envía junto con `endDate`.
    #[serde(rename = "startDate", alias = "filter[startDate]")]
    pub start_date: Option<String>,
    /// YYYY-MM-DD: hasta el mes de esa fecha. Se envía junto con `startDate`.
    #[serde(rename = "endDate", alias = "filter[endDate]")]
    pub end_date: Option<String>,
    /// Si es true, solo meses completos.
    pub complete: Option<bool>,
}

impl SeriesParams {
    fn to_sql(&self) -> Result<(String, Vec<Value>), ApiError> {
        let mut clauses = Vec::new();
        let mut params = Vec::new();
        if let Some((start, end)) =
            date_range(self.start_date.as_deref(), self.end_date.as_deref())?
        {
            clauses.push(
                "month_start BETWEEN CAST(date_trunc('month', CAST(? AS DATE)) AS DATE) \
                 AND CAST(? AS DATE)"
                    .to_string(),
            );
            params.extend([Value::Text(start), Value::Text(end)]);
        }
        if self.complete == Some(true) {
            clauses.push("complete".into());
        }
        let clause = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        Ok((clause, params))
    }
}

fn series(conn: &Connection, p: &SeriesParams) -> Result<Vec<HarmonizedMonth>, ApiError> {
    let (clause, params) = p.to_sql()?;
    let sql = format!(
        "SELECT epoch_ms(month_start::TIMESTAMP), modis, viirs, harmonized, source, complete, \
                median, p25, p75, anomaly, \"level\" \
         FROM harmonized_monthly {clause} ORDER BY month_start"
    );
    query_rows(conn, &sql, &params, |r| {
        Ok(HarmonizedMonth {
            month: r.get(0)?,
            modis: r.get(1)?,
            viirs: r.get(2)?,
            harmonized: r.get(3)?,
            source: r.get(4)?,
            complete: r.get(5)?,
            median: r.get(6)?,
            p25: r.get(7)?,
            p75: r.get(8)?,
            anomaly: r.get(9)?,
            level: r.get(10)?,
        })
    })
}

fn report(conn: &Connection) -> Result<HarmonizationReport, ApiError> {
    let factors = query_rows(
        conn,
        "SELECT \"month\", season, modis, viirs, f_month, f_season, factor, source \
         FROM harmonization_factors ORDER BY \"month\"",
        &[],
        |r| {
            Ok(Factor {
                month: r.get(0)?,
                season: r.get(1)?,
                modis: r.get(2)?,
                viirs: r.get(3)?,
                f_month: r.get(4)?,
                f_season: r.get(5)?,
                factor: r.get(6)?,
                source: r.get(7)?,
            })
        },
    )?;
    let validation = query_rows(
        conn,
        "SELECT \"year\", months, modis, viirs_raw, viirs_harmonized, raw_diff_pct, harmonized_diff_pct \
         FROM harmonization_validation ORDER BY \"year\"",
        &[],
        |r| {
            Ok(ValidationYear {
                year: r.get(0)?,
                months: r.get(1)?,
                modis: r.get(2)?,
                viirs_raw: r.get(3)?,
                viirs_harmonized: r.get(4)?,
                raw_diff_pct: r.get(5)?,
                harmonized_diff_pct: r.get(6)?,
            })
        },
    )?;
    Ok(HarmonizationReport {
        run: latest_run(conn)?,
        factors,
        validation,
    })
}

/// POST /stats/harmonization/rebuild
async fn rebuild(State(state): State<AppState>) -> Result<Json<RunSummary>, ApiError> {
    let cfg = state.cfg.clone();
    let run = state
        .run_db(move |conn| recompute(conn, &cfg.harmonization))
        .await?;
    Ok(Json(run))
}

/// GET /stats/harmonization
async fn get_report(State(state): State<AppState>) -> Result<Json<HarmonizationReport>, ApiError> {
    Ok(Json(state.run_db(|conn| report(conn)).await?))
}

/// GET /stats/harmonized?year=...&month=...&from=...&to=...&complete=true
async fn get_series(
    State(state): State<AppState>,
    Query(p): Query<SeriesParams>,
) -> Result<Json<HarmonizedSeries>, ApiError> {
    // Validar antes de tomar la conexión.
    p.to_sql()?;
    let out = state
        .run_db(move |conn| {
            Ok(HarmonizedSeries {
                run: latest_run(conn)?,
                items: series(conn, &p)?,
            })
        })
        .await?;
    Ok(Json(out))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/stats/harmonization/rebuild", post(rebuild))
        .route("/stats/harmonization", get(get_report))
        .route("/stats/harmonized", get(get_series))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timestamp::date_ms;

    fn insert(conn: &Connection, lat: f64, lon: f64, day: &str, time: &str, sat: &str, inst: &str) {
        conn.execute(
            "INSERT INTO detections (latitude, longitude, acq_date, acq_time, satellite, instrument, confidence) \
             VALUES (?, ?, CAST(? AS DATE), ?, ?, ?, 'high')",
            duckdb::params![lat, lon, day, time, sat, inst],
        )
        .unwrap();
    }

    /// La vista quita duplicados como el notebook (satélite, instante y coordenadas a 4
    /// decimales). Par real de producción: IDs 40892 y 40893, a ~5 m, mismo pase de NOAA-20.
    #[test]
    fn vista_quita_duplicados_a_cuatro_decimales() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        conn.execute_batch(
            "INSERT INTO detections (latitude, longitude, acq_date, acq_time, satellite, instrument, \"type\")
             VALUES (13.71422, -89.47637, DATE '2020-04-22', '1935', 'N20', 'VIIRS', NULL),
                    (13.71417, -89.47636, DATE '2020-04-22', '1935', 'N20', 'VIIRS', 0),
                    -- Mismo satélite con otro código: también es la misma detección.
                    (13.71421, -89.47638, DATE '2020-04-22', '1935', 'NOAA-20', 'VIIRS', NULL),
                    -- Distintas: otro lugar (5.º decimal que cambia el 4.º), otra hora, otro satélite.
                    (13.71460, -89.47637, DATE '2020-04-22', '1935', 'N20', 'VIIRS', NULL),
                    (13.71422, -89.47637, DATE '2020-04-22', '1936', 'N20', 'VIIRS', NULL),
                    (13.71422, -89.47637, DATE '2020-04-22', '1935', 'N', 'VIIRS', NULL);",
        )
        .unwrap();
        let total: i64 = conn
            .query_row("SELECT count(*) FROM detections_harmonized", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(total, 4);
        // De las tres equivalentes queda la que tiene `type`.
        let kept: (f64, Option<i32>) = conn
            .query_row(
                "SELECT latitude, \"type\" FROM detections_harmonized \
                 WHERE acq_time = '1935' AND satellite_name = 'NOAA-20' AND round(latitude, 4) = 13.7142",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kept, (13.71417, Some(0)));
    }

    fn cfg() -> HarmonizationConfig {
        HarmonizationConfig {
            reference_years: [2020, 2021],
            ..Default::default()
        }
    }

    /// 2020: solo MODIS. 2021: MODIS (2 celdas por día de marzo) y VIIRS (1 celda) → factor 2.
    #[test]
    fn factor_y_serie_en_duckdb() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        // Días de enero 2020 a diciembre 2021 para que ambos rangos cubran meses completos.
        insert(&conn, 13.5, -89.0, "2020-01-01", "1900", "A", "MODIS");
        insert(&conn, 13.5, -89.0, "2020-03-10", "1900", "Aqua", "MODIS");
        for d in 1..=31 {
            let day = format!("2021-03-{d:02}");
            insert(&conn, 13.50, -89.00, &day, "1900", "Aqua", "MODIS");
            insert(&conn, 13.60, -89.10, &day, "1900", "Aqua", "MODIS");
            insert(&conn, 13.50, -89.00, &day, "1900", "N", "VIIRS");
            // Terra y NOAA-20 no entran en la serie (Suomi NPP tiene prioridad).
            insert(&conn, 13.70, -89.20, &day, "1600", "T", "MODIS");
            insert(&conn, 13.70, -89.20, &day, "1910", "N20", "VIIRS");
        }
        // Duplicado con otro código de satélite: se cuenta una vez.
        insert(
            &conn,
            13.50,
            -89.00,
            "2021-03-01",
            "1900",
            "Suomi NPP",
            "VIIRS",
        );
        // Fuera del área.
        insert(&conn, 20.0, -89.0, "2021-03-05", "1900", "Aqua", "MODIS");
        insert(&conn, 13.5, -89.0, "2021-01-01", "1900", "N", "VIIRS");
        insert(&conn, 13.5, -89.0, "2021-12-31", "1900", "Aqua", "MODIS");
        insert(&conn, 13.5, -89.0, "2021-12-31", "1900", "N", "VIIRS");

        let run = recompute(&mut conn, &cfg()).unwrap();
        assert_eq!(run.status, "ok");
        assert_eq!(run.overlap_start, date_ms("2021-01"));
        assert_eq!(run.overlap_end, date_ms("2021-12"));
        assert_eq!(run.overlap_months, 12);
        assert_eq!(
            run.static_cells, 0,
            "menos de 12 meses con fuego: no se aplica el filtro"
        );
        // Mismos formatos que el resto de la API.
        assert_eq!(run.params["bbox"], "-90.13,13.15,-87.68,14.45");
        assert_eq!(run.params["overlap_end"], date_ms("2025-12").unwrap());
        assert_eq!(run.params["snpp_end"], date_ms("2026-11-02").unwrap());

        let r = report(&conn).unwrap();
        assert_eq!(r.factors.len(), 12);
        let march = &r.factors[2];
        // Marzo: 31 celdas-día VIIRS (≥ 30) → factor del propio mes = 62 / 31.
        assert_eq!(march.viirs, 31.0);
        assert_eq!(march.modis, 62.0);
        assert_eq!(march.source, "month");
        assert!((march.factor.unwrap() - 2.0).abs() < 1e-9);
        // Enero tiene 1 celda-día VIIRS → factor de la época seca:
        // MODIS mar 62 + dic 1 = 63; VIIRS ene 1 + mar 31 + dic 1 = 33.
        let jan = &r.factors[0];
        assert_eq!(jan.source, "season");
        assert!((jan.factor.unwrap() - 63.0 / 33.0).abs() < 1e-9);

        let all = series(&conn, &SeriesParams::default()).unwrap();
        let m2020_03 = all
            .iter()
            .find(|m| Some(m.month) == date_ms("2020-03"))
            .unwrap();
        assert_eq!(m2020_03.source, "modis");
        assert_eq!(m2020_03.harmonized, Some(1.0));
        let m2021_03 = all
            .iter()
            .find(|m| Some(m.month) == date_ms("2021-03"))
            .unwrap();
        assert_eq!(m2021_03.source, "viirs_factor");
        assert_eq!(m2021_03.viirs, Some(31.0));
        assert!((m2021_03.harmonized.unwrap() - 62.0).abs() < 1e-9);

        let only = series(
            &conn,
            &SeriesParams {
                start_date: Some("2021-03-10".into()),
                end_date: Some("2021-03-31".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(only.len(), 1);

        // Recalcular reemplaza, no duplica.
        recompute(&mut conn, &cfg()).unwrap();
        assert_eq!(report(&conn).unwrap().factors.len(), 12);
    }

    /// Una detección NRT (sin `type`) seguida del archivo SP (type = 1, volcán): se completa
    /// el `type` y la detección sale de la serie.
    #[test]
    fn archivo_sp_completa_type() {
        use crate::entities::detection::{insert_ignoring_duplicates, NewDetection};
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        let row = |t: Option<i32>| NewDetection {
            latitude: 13.5,
            longitude: -89.0,
            bright_ti4: None,
            scan: None,
            track: None,
            acq_date: "2021-03-01".into(),
            acq_time: "1900".into(),
            satellite: "Aqua".into(),
            instrument: "MODIS".into(),
            confidence: Some("high".into()),
            version: None,
            bright_ti5: None,
            frp: None,
            daynight: None,
            fire_type: t,
        };
        assert_eq!(
            insert_ignoring_duplicates(&mut conn, &[row(None)]).unwrap(),
            (1, 0)
        );
        assert_eq!(
            insert_ignoring_duplicates(&mut conn, &[row(Some(1))]).unwrap(),
            (0, 1)
        );
        // Ya tiene type: no se pisa.
        assert_eq!(
            insert_ignoring_duplicates(&mut conn, &[row(Some(0))]).unwrap(),
            (0, 0)
        );
        let t: i32 = conn
            .query_row("SELECT \"type\" FROM detections", [], |r| r.get(0))
            .unwrap();
        assert_eq!(t, 1);
        let run = recompute(&mut conn, &cfg()).unwrap();
        assert_eq!(run.after_type, 0);
        assert_eq!(run.status, "insufficient_data");
    }

    #[test]
    fn sin_aqua_no_hay_serie() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        insert(&conn, 13.5, -89.0, "2021-03-01", "1900", "N", "VIIRS");
        let run = recompute(&mut conn, &cfg()).unwrap();
        assert_eq!(run.status, "insufficient_data");
        assert!(series(&conn, &SeriesParams::default()).unwrap().is_empty());
    }

    #[test]
    fn esquema_migra_bases_viejas() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE SEQUENCE instruments_id_seq START 1;
             CREATE TABLE instruments (id BIGINT PRIMARY KEY DEFAULT nextval('instruments_id_seq'), code VARCHAR NOT NULL UNIQUE, name VARCHAR NOT NULL);
             CREATE SEQUENCE detections_id_seq START 1;
             CREATE TABLE detections (id BIGINT PRIMARY KEY DEFAULT nextval('detections_id_seq'),
                 latitude DOUBLE NOT NULL, longitude DOUBLE NOT NULL, bright_ti4 DOUBLE, scan DOUBLE,
                 track DOUBLE, acq_date DATE NOT NULL, acq_time VARCHAR NOT NULL, satellite VARCHAR NOT NULL,
                 instrument VARCHAR NOT NULL REFERENCES instruments (code), confidence VARCHAR,
                 \"version\" VARCHAR, bright_ti5 DOUBLE, frp DOUBLE, daynight VARCHAR);
             CREATE INDEX detections_acq_date_idx ON detections (acq_date);
             CREATE UNIQUE INDEX detections_natural_key
                 ON detections (latitude, longitude, acq_date, acq_time, satellite, instrument);",
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO instruments (code, name) VALUES ('MODIS', 'MODIS'), ('VIIRS', 'VIIRS');
             INSERT INTO detections (latitude, longitude, acq_date, acq_time, satellite, instrument, confidence)
             VALUES (13.1, -89.1, DATE '2020-01-01', '1200', 'Aqua', 'MODIS', '85'),
                    (13.2, -89.1, DATE '2020-01-01', '1200', 'Aqua', 'MODIS', '29'),
                    (13.3, -89.1, DATE '2020-01-01', '1200', 'N', 'VIIRS', 'n'),
                    (13.4, -89.1, DATE '2020-01-01', '1200', 'N', 'VIIRS', 'raro');",
        )
        .unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        // Reaplicar el esquema no cambia nada (idempotente).
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        let conf: Vec<(Option<String>, Option<i32>)> = query_rows(
            &conn,
            "SELECT confidence, confidence_level FROM detections_harmonized ORDER BY latitude",
            &[],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
        assert_eq!(
            conf,
            vec![
                (Some("high".into()), Some(3)),
                (Some("low".into()), Some(1)),
                (Some("nominal".into()), Some(2)),
                (None, None),
            ]
        );
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM information_schema.columns \
                 WHERE table_name = 'detections' AND column_name = 'type'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn filtros_de_la_serie() {
        assert!(SeriesParams {
            start_date: Some("2020-03-15".into()),
            ..Default::default()
        }
        .to_sql()
        .is_err());
        let (clause, params) = SeriesParams {
            start_date: Some("2020-03-15".into()),
            end_date: Some("2021-12-31".into()),
            complete: Some(true),
        }
        .to_sql()
        .unwrap();
        assert!(clause.contains("month_start BETWEEN"));
        assert!(clause.ends_with("complete"));
        assert_eq!(params.len(), 2);
    }
}
