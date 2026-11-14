//! Configuración desde variables de entorno.

use std::env;

/// El Salvador (oeste, sur, este, norte). Área por defecto de la sincronización con FIRMS.
pub const EL_SALVADOR_BBOX: &str = "-90.13,13.15,-87.68,14.45";

pub const FIRMS_SOURCES: &[&str] = &[
    "MODIS_NRT",
    "MODIS_SP",
    "VIIRS_SNPP_NRT",
    "VIIRS_SNPP_SP",
    "VIIRS_NOAA20_NRT",
    "VIIRS_NOAA20_SP",
    "VIIRS_NOAA21_NRT",
];

#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: String,
    pub db_path: String,
    pub port: String,
    /// Máximo de filas por respuesta JSON de lectura.
    pub max_rows: u32,
    /// Máximo de elementos por creación masiva.
    pub max_bulk: usize,
    /// Tamaño máximo del cuerpo de una petición (MB).
    pub max_body_mb: usize,
    /// Máximo de filas en una descarga CSV.
    pub max_export_rows: u32,
    /// Si es true, los GET no piden API key.
    pub public_read: bool,
    /// Orígenes permitidos por CORS. Vacío = ninguno, ["*"] = cualquiera.
    pub cors_origins: Vec<String>,
    /// MAP_KEY de FIRMS. Sin ella no hay sincronización.
    pub firms_map_key: Option<String>,
    /// Cada cuántas horas sincronizar con FIRMS (0 = desactivado).
    pub firms_sync_hours: u64,
    pub firms_sync_sources: Vec<String>,
    pub firms_sync_area: String,
    pub firms_sync_days: u32,
}

fn var(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn num<T: std::str::FromStr>(key: &str, default: T) -> T {
    var(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn list(key: &str, default: &str) -> Vec<String> {
    var(key)
        .unwrap_or_else(|| default.to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            api_key: var("API_KEY").expect("falta API_KEY"),
            db_path: var("DB_PATH").unwrap_or_else(|| "/data/marro.duckdb".into()),
            port: var("PORT").unwrap_or_else(|| "8080".into()),
            max_rows: num("MAX_ROWS", 10_000u32).max(1),
            max_bulk: num("MAX_BULK", 10_000usize).max(1),
            max_body_mb: num("MAX_BODY_MB", 50usize).max(1),
            max_export_rows: num("MAX_EXPORT_ROWS", 200_000u32).max(1),
            public_read: matches!(
                var("PUBLIC_READ")
                    .as_deref()
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("true" | "1" | "yes")
            ),
            cors_origins: list("CORS_ORIGINS", "*"),
            firms_map_key: var("FIRMS_MAP_KEY"),
            firms_sync_hours: num("FIRMS_SYNC_HOURS", 0u64),
            firms_sync_sources: list(
                "FIRMS_SYNC_SOURCES",
                "VIIRS_SNPP_NRT,VIIRS_NOAA20_NRT,VIIRS_NOAA21_NRT,MODIS_NRT",
            ),
            firms_sync_area: var("FIRMS_SYNC_AREA").unwrap_or_else(|| EL_SALVADOR_BBOX.into()),
            firms_sync_days: num("FIRMS_SYNC_DAYS", 2u32).clamp(1, 10),
        }
    }
}
