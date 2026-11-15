//! Detection: un hotspot detectado por satélite (formato FIRMS).
//!
//! Endpoints:
//!   POST /detections            crear una (objeto) o varias (arreglo, todo o nada)
//!   GET  /detections/{id}       obtener una
//!   GET  /detections            buscar con filtros (JSON paginado o CSV)
//!   POST /detections/import     importar un CSV de FIRMS (MODIS o VIIRS) y recalcular la armonización

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use duckdb::{params, Connection, Row, Statement};
use serde::{Deserialize, Serialize};

use super::crud::{self, Entity, PageParams};
use crate::{
    config::HarmonizationConfig,
    error::ApiError,
    filters::DetectionFilter,
    firms, harmonization,
    state::AppState,
    validation::{date, hhmm, in_range, optional_min, optional_text, required_text},
};

#[derive(Debug, Serialize)]
pub struct Detection {
    pub id: i64,
    pub latitude: f64,
    pub longitude: f64,
    pub bright_ti4: Option<f64>,
    pub scan: Option<f64>,
    pub track: Option<f64>,
    /// YYYY-MM-DD
    pub acq_date: String,
    /// HHMM en UTC
    pub acq_time: String,
    pub satellite: String,
    /// Código del instrumento (instruments.code)
    pub instrument: String,
    pub confidence: Option<String>,
    pub version: Option<String>,
    pub bright_ti5: Option<f64>,
    pub frp: Option<f64>,
    /// "D" (día) o "N" (noche)
    pub daynight: Option<String>,
    /// Tipo de punto caliente (solo en el archivo SP de FIRMS):
    /// 0 vegetación, 1 volcán, 2 otra fuente estática, 3 mar.
    #[serde(rename = "type")]
    pub fire_type: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewDetection {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub bright_ti4: Option<f64>,
    #[serde(default)]
    pub scan: Option<f64>,
    #[serde(default)]
    pub track: Option<f64>,
    pub acq_date: String,
    pub acq_time: String,
    pub satellite: String,
    pub instrument: String,
    #[serde(default)]
    pub confidence: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub bright_ti5: Option<f64>,
    #[serde(default)]
    pub frp: Option<f64>,
    #[serde(default)]
    pub daynight: Option<String>,
    #[serde(default, rename = "type")]
    pub fire_type: Option<i32>,
}

/// Columnas en el orden que espera `from_row`.
macro_rules! columns {
    () => {
        "id, latitude, longitude, bright_ti4, scan, track, acq_date::VARCHAR, acq_time, \
         satellite, instrument, confidence, \"version\", bright_ti5, frp, daynight, \"type\""
    };
}

macro_rules! insert_into {
    () => {
        "INSERT INTO detections (latitude, longitude, bright_ti4, scan, track, acq_date, acq_time, \
         satellite, instrument, confidence, \"version\", bright_ti5, frp, daynight, \"type\") \
         VALUES (?, ?, ?, ?, ?, CAST(? AS DATE), ?, ?, ?, ?, ?, ?, ?, ?, ?) "
    };
}

/// Los 15 valores de una detección nueva, en el orden de `insert_into!`.
macro_rules! detection_params {
    ($d:expr) => {
        params![
            $d.latitude,
            $d.longitude,
            $d.bright_ti4,
            $d.scan,
            $d.track,
            $d.acq_date,
            $d.acq_time,
            $d.satellite,
            $d.instrument,
            $d.confidence,
            $d.version,
            $d.bright_ti5,
            $d.frp,
            $d.daynight,
            $d.fire_type
        ]
    };
}

/// Inserta ignorando las detecciones que ya existen (misma clave natural). Si la existente no
/// tiene `type` (vino de un CSV NRT) y la nueva sí (archivo SP), se completa.
const INSERT_IGNORE_SQL: &str = concat!(
    insert_into!(),
    "ON CONFLICT (latitude, longitude, acq_date, acq_time, satellite, instrument) \
     DO UPDATE SET \"type\" = excluded.\"type\" \
     WHERE detections.\"type\" IS NULL AND excluded.\"type\" IS NOT NULL"
);

impl Entity for Detection {
    type New = NewDetection;

    const NAME: &'static str = "detección";
    const INSERT_SQL: &'static str = concat!(insert_into!(), "RETURNING ", columns!());
    const SELECT_BY_ID_SQL: &'static str =
        concat!("SELECT ", columns!(), " FROM detections WHERE id = ?");

    fn validate(mut d: NewDetection) -> Result<NewDetection, String> {
        in_range("latitude", d.latitude, -90.0, 90.0)?;
        in_range("longitude", d.longitude, -180.0, 180.0)?;
        optional_min("bright_ti4", d.bright_ti4, 0.0)?;
        optional_min("bright_ti5", d.bright_ti5, 0.0)?;
        optional_min("scan", d.scan, 0.0)?;
        optional_min("track", d.track, 0.0)?;
        optional_min("frp", d.frp, 0.0)?;

        d.acq_date = date("acq_date", &d.acq_date)?;
        d.acq_time = hhmm("acq_time", &d.acq_time)?;
        d.satellite = required_text("satellite", &d.satellite, 20)?;
        d.instrument = required_text("instrument", &d.instrument, 20)?.to_ascii_uppercase();
        d.confidence = optional_text("confidence", d.confidence, 10)?;
        d.version = optional_text("version", d.version, 20)?;

        d.daynight = optional_text("daynight", d.daynight, 1)?.map(|v| v.to_ascii_uppercase());
        if let Some(dn) = &d.daynight {
            if dn != "D" && dn != "N" {
                return Err("daynight debe ser \"D\" o \"N\"".into());
            }
        }
        if let Some(t) = d.fire_type {
            if !(0..=3).contains(&t) {
                return Err("type debe ser 0, 1, 2 o 3".into());
            }
        }
        Ok(d)
    }

    fn insert(stmt: &mut Statement<'_>, d: &NewDetection) -> duckdb::Result<Self> {
        stmt.query_row(detection_params!(d), Self::from_row)
    }

    fn from_row(row: &Row<'_>) -> duckdb::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            latitude: row.get(1)?,
            longitude: row.get(2)?,
            bright_ti4: row.get(3)?,
            scan: row.get(4)?,
            track: row.get(5)?,
            acq_date: row.get(6)?,
            acq_time: row.get(7)?,
            satellite: row.get(8)?,
            instrument: row.get(9)?,
            confidence: row.get(10)?,
            version: row.get(11)?,
            bright_ti5: row.get(12)?,
            frp: row.get(13)?,
            daynight: row.get(14)?,
            fire_type: row.get(15)?,
        })
    }
}

// ---------------------------------------------------------------- Búsqueda y CSV

#[derive(Debug, Default, Deserialize)]
pub struct SearchParams {
    /// json (por defecto) | csv
    pub format: Option<String>,
}

/// GET /detections?bbox=...&from=...&to=...&limit=...&offset=...&format=json|csv
async fn search(
    State(state): State<AppState>,
    Query(filter): Query<DetectionFilter>,
    Query(page): Query<PageParams>,
    Query(opts): Query<SearchParams>,
) -> Result<Response, ApiError> {
    let sql = filter.to_sql(state.spatial)?;
    let as_csv = match opts
        .format
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None | Some("json") => false,
        Some("csv") => true,
        Some(_) => return Err(ApiError::bad_request("format debe ser json o csv")),
    };

    let count_sql = format!("SELECT count(*) FROM detections_harmonized {}", sql.clause);
    let page_sql = format!(
        "SELECT {} FROM detections_harmonized {} ORDER BY acq_date, acq_time, id LIMIT ? OFFSET ?",
        columns!(),
        sql.clause
    );

    if as_csv {
        let (limit, offset) = page.resolve(state.cfg.max_export_rows, state.cfg.max_export_rows);
        let file_name = format!(
            "hotspots_{}_{}.csv",
            sql.from.as_deref().unwrap_or("inicio"),
            sql.to.as_deref().unwrap_or("hoy")
        );
        let params = sql.params;
        let bytes = state
            .run_db(move |conn| {
                let page = crud::fetch_page::<Detection>(
                    conn, &count_sql, &page_sql, &params, limit, offset,
                )?;
                let mut w = csv::Writer::from_writer(Vec::new());
                if page.items.is_empty() {
                    // Encabezado aunque no haya filas, para que el archivo sea válido.
                    w.write_record(CSV_HEADER).map_err(csv_err)?;
                }
                for d in &page.items {
                    w.serialize(d).map_err(csv_err)?;
                }
                w.into_inner()
                    .map_err(|e| ApiError::internal(e.to_string()))
            })
            .await?;
        return Ok((
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{file_name}\""),
                ),
            ],
            bytes,
        )
            .into_response());
    }

    let (limit, offset) = page.resolve(100, state.cfg.max_rows);
    let params = sql.params;
    let page = state
        .run_db(move |conn| {
            crud::fetch_page::<Detection>(conn, &count_sql, &page_sql, &params, limit, offset)
        })
        .await?;
    Ok(Json(page).into_response())
}

const CSV_HEADER: [&str; 16] = [
    "id",
    "latitude",
    "longitude",
    "bright_ti4",
    "scan",
    "track",
    "acq_date",
    "acq_time",
    "satellite",
    "instrument",
    "confidence",
    "version",
    "bright_ti5",
    "frp",
    "daynight",
    "type",
];

fn csv_err(e: csv::Error) -> ApiError {
    ApiError::internal(format!("error al generar el CSV: {e}"))
}

// ---------------------------------------------------------------- Importación

/// Error de una fila del CSV.
#[derive(Debug, Serialize)]
pub struct RowError {
    /// Número de línea en el archivo (la 1 es el encabezado).
    pub line: usize,
    pub error: String,
}

/// Resultado de una importación.
#[derive(Debug, Serialize)]
pub struct ImportResult {
    /// Filas de datos leídas del CSV.
    pub received: usize,
    /// Filas válidas.
    pub valid: usize,
    /// Filas nuevas guardadas.
    pub inserted: usize,
    /// Filas válidas que ya existían (no se duplican).
    pub duplicates: usize,
    /// Filas que ya existían sin `type` y se completaron con el del CSV (archivo SP).
    pub updated: usize,
    /// Filas con errores (no se guardan).
    pub invalid: usize,
    /// Primeros errores encontrados (máximo 50).
    pub errors: Vec<RowError>,
    /// Armonización recalculada tras guardar filas nuevas.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harmonization: Option<harmonization::RunSummary>,
    /// Si el recálculo falló (las filas igual quedaron guardadas).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harmonization_error: Option<String>,
}

/// Guarda detecciones ya validadas, ignorando las que ya existen (solo se les completa `type`).
/// Todo en una transacción. Devuelve (insertadas, existentes actualizadas).
pub fn insert_ignoring_duplicates(
    conn: &mut Connection,
    rows: &[NewDetection],
) -> Result<(usize, usize), ApiError> {
    let tx = conn.transaction()?;
    let count = |tx: &duckdb::Transaction<'_>| -> Result<i64, ApiError> {
        Ok(tx.query_row("SELECT count(*) FROM detections", [], |r| r.get(0))?)
    };
    let before = count(&tx)?;
    let changed = {
        let mut stmt = tx.prepare(INSERT_IGNORE_SQL)?;
        let mut n = 0usize;
        for (i, d) in rows.iter().enumerate() {
            n += stmt
                .execute(detection_params!(d))
                .map_err(|e| ApiError::from(e).at(i))?;
        }
        n
    };
    // execute() cuenta igual una inserción y una actualización: se separan con el total.
    let inserted = (count(&tx)? - before).max(0) as usize;
    tx.commit()?;
    Ok((inserted, changed.saturating_sub(inserted)))
}

/// Valida, guarda y arma el resumen de una importación ya leída del CSV.
/// Con `harmonize`, si hubo filas nuevas recalcula la armonización.
pub fn import_parsed(
    conn: &mut Connection,
    parsed: Vec<firms::ParsedRow>,
    harmonize: Option<&HarmonizationConfig>,
) -> Result<ImportResult, ApiError> {
    let received = parsed.len();
    let mut valid = Vec::with_capacity(received);
    let mut errors = Vec::new();
    let mut invalid = 0usize;
    for row in parsed {
        match row.result {
            Ok(d) => valid.push(d),
            Err(error) => {
                invalid += 1;
                if errors.len() < 50 {
                    errors.push(RowError {
                        line: row.line,
                        error,
                    });
                }
            }
        }
    }
    let (inserted, updated) = if valid.is_empty() {
        (0, 0)
    } else {
        insert_ignoring_duplicates(conn, &valid)?
    };
    let (harmonization, harmonization_error) = match harmonize {
        Some(cfg) if inserted + updated > 0 => match harmonization::recompute(conn, cfg) {
            Ok(run) => (Some(run), None),
            Err(e) => {
                eprintln!("armonización: error {}: {}", e.status.as_u16(), e.message);
                (None, Some(e.message))
            }
        },
        _ => (None, None),
    };
    Ok(ImportResult {
        received,
        valid: valid.len(),
        inserted,
        duplicates: valid.len() - inserted,
        updated,
        invalid,
        errors,
        harmonization,
        harmonization_error,
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct ImportParams {
    /// Instrumento a usar si el CSV no trae la columna `instrument`.
    pub instrument: Option<String>,
    /// Si es true, guarda las filas válidas aunque haya inválidas. Por defecto false.
    pub skip_invalid: Option<bool>,
    /// Si es false, no recalcula la armonización (útil al cargar muchos CSV seguidos;
    /// al final, POST /harmonization/rebuild). Por defecto true.
    pub harmonize: Option<bool>,
}

/// POST /detections/import  (cuerpo: el CSV tal como lo descarga FIRMS)
async fn import(
    State(state): State<AppState>,
    Query(opts): Query<ImportParams>,
    body: String,
) -> Result<Response, ApiError> {
    if body.trim().is_empty() {
        return Err(ApiError::unprocessable(
            "el cuerpo está vacío: manda el contenido del CSV",
        ));
    }
    let parsed =
        firms::parse_csv(&body, opts.instrument.as_deref()).map_err(ApiError::unprocessable)?;
    if parsed.len() > state.cfg.max_export_rows as usize * 5 {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "el CSV es demasiado grande; divídelo o cárgalo con el CLI de DuckDB",
        ));
    }

    // Sin skip_invalid, si hay filas inválidas no se guarda nada y se devuelven los errores.
    let has_invalid = parsed.iter().any(|r| r.result.is_err());
    if has_invalid && !opts.skip_invalid.unwrap_or(false) {
        let received = parsed.len();
        let mut errors = Vec::new();
        let mut invalid = 0;
        for r in &parsed {
            if let Err(e) = &r.result {
                invalid += 1;
                if errors.len() < 50 {
                    errors.push(RowError {
                        line: r.line,
                        error: e.clone(),
                    });
                }
            }
        }
        let summary = ImportResult {
            received,
            valid: received - invalid,
            inserted: 0,
            duplicates: 0,
            updated: 0,
            invalid,
            errors,
            harmonization: None,
            harmonization_error: None,
        };
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, Json(summary)).into_response());
    }

    let cfg = state.cfg.clone();
    let harmonize = opts.harmonize.unwrap_or(true);
    let result = state
        .run_db(move |conn| import_parsed(conn, parsed, harmonize.then_some(&cfg.harmonization)))
        .await?;
    Ok((StatusCode::OK, Json(result)).into_response())
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/detections", post(crud::create::<Detection>).get(search))
        .route("/detections/import", post(import))
        .route("/detections/{id}", get(crud::get_one::<Detection>))
}
