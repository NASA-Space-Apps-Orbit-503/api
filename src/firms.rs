//! Integración con NASA FIRMS:
//!   - lectura de CSV de FIRMS (MODIS y VIIRS, NRT o SP)
//!   - descarga desde la API de área de FIRMS
//!   - POST /firms/sync y sincronización periódica en segundo plano
//!
//! Armonización de columnas al leer:
//!   MODIS `brightness` → `bright_ti4`, MODIS `bright_t31` → `bright_ti5`.

use std::time::Duration;

use axum::{
    extract::{rejection::JsonRejection, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    config::FIRMS_SOURCES,
    entities::{
        crud::Entity,
        detection::{import_parsed, Detection, ImportResult, NewDetection},
    },
    error::ApiError,
    filters::parse_bbox,
    state::AppState,
    validation::date,
};

/// Una fila del CSV ya convertida (o el motivo por el que no se pudo).
#[derive(Debug)]
pub struct ParsedRow {
    /// Número de línea en el archivo (la 1 es el encabezado).
    pub line: usize,
    pub result: Result<NewDetection, String>,
}

/// Lee un CSV de FIRMS. Acepta columnas en cualquier orden e ignora las que no conoce.
/// `fallback_instrument` se usa si el CSV no trae la columna `instrument`.
pub fn parse_csv(text: &str, fallback_instrument: Option<&str>) -> Result<Vec<ParsedRow>, String> {
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(text.as_bytes());

    let headers = rdr
        .headers()
        .map_err(|e| format!("no se pudo leer el encabezado del CSV: {e}"))?
        .clone();
    let col = |names: &[&str]| -> Option<usize> {
        names
            .iter()
            .find_map(|n| headers.iter().position(|h| h.eq_ignore_ascii_case(n)))
    };

    let c_lat = col(&["latitude", "lat"]).ok_or("el CSV no tiene la columna latitude")?;
    let c_lon = col(&["longitude", "lon"]).ok_or("el CSV no tiene la columna longitude")?;
    let c_date = col(&["acq_date"]).ok_or("el CSV no tiene la columna acq_date")?;
    let c_time = col(&["acq_time"]).ok_or("el CSV no tiene la columna acq_time")?;
    let c_ti4 = col(&["bright_ti4", "brightness"]);
    let c_ti5 = col(&["bright_ti5", "bright_t31"]);
    let c_scan = col(&["scan"]);
    let c_track = col(&["track"]);
    let c_sat = col(&["satellite"]);
    let c_inst = col(&["instrument"]);
    let c_conf = col(&["confidence"]);
    let c_ver = col(&["version"]);
    let c_frp = col(&["frp"]);
    let c_dn = col(&["daynight"]);

    // Si no viene la columna instrument: el parámetro, o se deduce por las columnas de MODIS.
    let default_instrument = fallback_instrument
        .map(|s| s.trim().to_ascii_uppercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if col(&["brightness"]).is_some() {
                "MODIS".to_string()
            } else {
                "VIIRS".to_string()
            }
        });

    let mut out = Vec::new();
    for (i, rec) in rdr.records().enumerate() {
        let line = i + 2;
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                out.push(ParsedRow {
                    line,
                    result: Err(format!("fila ilegible: {e}")),
                });
                continue;
            }
        };
        // Saltar líneas vacías.
        if rec.iter().all(|f| f.is_empty()) {
            continue;
        }

        let text = |c: Option<usize>| -> Option<String> {
            c.and_then(|c| rec.get(c))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let number = |c: Option<usize>, name: &str| -> Result<Option<f64>, String> {
            match text(c) {
                None => Ok(None),
                Some(v) => v
                    .parse::<f64>()
                    .map(Some)
                    .map_err(|_| format!("{name} no es un número: {v}")),
            }
        };

        let result = (|| -> Result<NewDetection, String> {
            let new = NewDetection {
                latitude: number(Some(c_lat), "latitude")?.ok_or("latitude está vacío")?,
                longitude: number(Some(c_lon), "longitude")?.ok_or("longitude está vacío")?,
                bright_ti4: number(c_ti4, "bright_ti4")?,
                scan: number(c_scan, "scan")?,
                track: number(c_track, "track")?,
                acq_date: text(Some(c_date)).ok_or("acq_date está vacío")?,
                acq_time: text(Some(c_time)).ok_or("acq_time está vacío")?,
                satellite: text(c_sat).unwrap_or_else(|| "desconocido".to_string()),
                instrument: text(c_inst).unwrap_or_else(|| default_instrument.clone()),
                confidence: text(c_conf),
                version: text(c_ver),
                bright_ti5: number(c_ti5, "bright_ti5")?,
                frp: number(c_frp, "frp")?,
                daynight: text(c_dn),
            };
            Detection::validate(new)
        })();

        out.push(ParsedRow { line, result });
    }
    Ok(out)
}

// ---------------------------------------------------------------- Descarga

/// Descarga un CSV de la API de área de FIRMS.
/// `area` = oeste,sur,este,norte · `days` = 1..10 · `date` = día inicial (opcional).
pub async fn fetch_area_csv(
    state: &AppState,
    source: &str,
    area: &str,
    days: u32,
    date: Option<&str>,
) -> Result<String, ApiError> {
    let key = state.cfg.firms_map_key.as_deref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "FIRMS_MAP_KEY no está configurada en el servidor",
        )
    })?;
    let mut url =
        format!("https://firms.modaps.eosdis.nasa.gov/api/area/csv/{key}/{source}/{area}/{days}");
    if let Some(d) = date {
        url.push('/');
        url.push_str(d);
    }

    let resp = state.http.get(&url).send().await.map_err(|e| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("no se pudo contactar a FIRMS: {}", e.without_url()),
        )
    })?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("respuesta inválida de FIRMS: {}", e.without_url()),
        )
    })?;

    // FIRMS responde errores como texto plano ("Invalid MAP_KEY.", "Invalid area coordinate.").
    let first_line = body.lines().next().unwrap_or("").to_ascii_lowercase();
    if !status.is_success() || !first_line.contains("latitude") {
        let snippet: String = body.chars().take(200).collect();
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("FIRMS respondió {}: {}", status.as_u16(), snippet.trim()),
        ));
    }
    Ok(body)
}

// ---------------------------------------------------------------- POST /firms/sync

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRequest {
    /// Fuente de FIRMS (VIIRS_SNPP_NRT, MODIS_NRT, ...).
    pub source: String,
    /// oeste,sur,este,norte. Por defecto FIRMS_SYNC_AREA (El Salvador).
    #[serde(default)]
    pub area: Option<String>,
    /// Días a descargar (1..10). Por defecto 1.
    #[serde(default)]
    pub days: Option<u32>,
    /// Día inicial YYYY-MM-DD. Sin él, los días más recientes.
    #[serde(default)]
    pub date: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SyncResult {
    pub source: String,
    pub area: String,
    pub days: u32,
    pub date: Option<String>,
    #[serde(flatten)]
    pub import: ImportResult,
}

/// Descarga una fuente de FIRMS y la importa.
pub async fn sync_once(
    state: &AppState,
    source: &str,
    area: &str,
    days: u32,
    date_from: Option<String>,
) -> Result<SyncResult, ApiError> {
    let source = source.trim().to_ascii_uppercase();
    if !FIRMS_SOURCES.contains(&source.as_str()) {
        return Err(ApiError::unprocessable(format!(
            "source debe ser una de: {}",
            FIRMS_SOURCES.join(", ")
        )));
    }
    let (w, s, e, n) = parse_bbox(area)?;
    let area = format!("{w},{s},{e},{n}");
    if !(1..=10).contains(&days) {
        return Err(ApiError::unprocessable("days debe estar entre 1 y 10"));
    }
    let date_from = date_from
        .as_deref()
        .map(|d| date("date", d))
        .transpose()
        .map_err(ApiError::unprocessable)?;

    let csv_text = fetch_area_csv(state, &source, &area, days, date_from.as_deref()).await?;
    let parsed =
        parse_csv(&csv_text, None).map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, e))?;
    let import = state
        .run_db(move |conn| import_parsed(conn, parsed))
        .await?;

    Ok(SyncResult {
        source,
        area,
        days,
        date: date_from,
        import,
    })
}

async fn sync_handler(
    State(state): State<AppState>,
    payload: Result<Json<SyncRequest>, JsonRejection>,
) -> Result<Json<SyncResult>, ApiError> {
    let Json(req) = payload.map_err(ApiError::from_rejection)?;
    let area = req
        .area
        .unwrap_or_else(|| state.cfg.firms_sync_area.clone());
    let result = sync_once(&state, &req.source, &area, req.days.unwrap_or(1), req.date).await?;
    Ok(Json(result))
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/firms/sync", post(sync_handler))
}

// ---------------------------------------------------------------- Sincronización periódica

/// Corre para siempre: cada `FIRMS_SYNC_HOURS` descarga las fuentes configuradas.
pub async fn run_scheduler(state: AppState) {
    let hours = state.cfg.firms_sync_hours;
    println!(
        "sincronización FIRMS cada {hours} h: {:?} en {}",
        state.cfg.firms_sync_sources, state.cfg.firms_sync_area
    );
    // Esperar un poco tras el arranque.
    tokio::time::sleep(Duration::from_secs(15)).await;
    loop {
        for source in &state.cfg.firms_sync_sources {
            let area = state.cfg.firms_sync_area.clone();
            match sync_once(&state, source, &area, state.cfg.firms_sync_days, None).await {
                Ok(r) => println!(
                    "FIRMS {source}: {} filas, {} nuevas, {} repetidas, {} inválidas",
                    r.import.received, r.import.inserted, r.import.duplicates, r.import.invalid
                ),
                Err(e) => eprintln!("FIRMS {source}: error {}: {}", e.status.as_u16(), e.message),
            }
        }
        tokio::time::sleep(Duration::from_secs(hours * 3600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIIRS: &str = "latitude,longitude,bright_ti4,scan,track,acq_date,acq_time,satellite,instrument,confidence,version,bright_ti5,frp,daynight\n\
13.69,-89.21,330.5,0.39,0.36,2024-03-14,748,N,VIIRS,n,2.0NRT,295.1,4.2,N\n";

    const MODIS: &str = "latitude,longitude,brightness,scan,track,acq_date,acq_time,satellite,confidence,version,bright_t31,frp,daynight\n\
13.70,-89.20,310.2,1.0,1.0,2024-03-14,1630,Aqua,65,6.1NRT,295.0,12.3,D\n\
99,-89.20,310.2,1.0,1.0,2024-03-14,1630,Aqua,65,6.1NRT,295.0,12.3,D\n";

    #[test]
    fn lee_viirs() {
        let rows = parse_csv(VIIRS, None).unwrap();
        let d = rows[0].result.as_ref().unwrap();
        assert_eq!(d.acq_time, "0748");
        assert_eq!(d.instrument, "VIIRS");
    }

    #[test]
    fn lee_modis_y_armoniza_columnas() {
        let rows = parse_csv(MODIS, None).unwrap();
        let d = rows[0].result.as_ref().unwrap();
        assert_eq!(d.instrument, "MODIS");
        assert_eq!(d.bright_ti4, Some(310.2));
        assert_eq!(d.bright_ti5, Some(295.0));
        assert!(rows[1].result.is_err()); // latitude 99
        assert_eq!(rows[1].line, 3);
    }
}
