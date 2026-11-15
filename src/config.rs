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

/// Parámetros de la armonización MODIS → VIIRS (`src/harmonization.rs`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct HarmonizationConfig {
    /// oeste, sur, este, norte. None = sin recorte por área.
    /// Por defecto, la misma área que FIRMS_SYNC_AREA, para que los meses con datos NRT y los
    /// del archivo histórico cubran la misma zona.
    pub bbox: Option<[f64; 4]>,
    /// Tamaño de celda en grados (0.01 ≈ 1.1 km).
    pub cell_deg: f64,
    /// nominal (nominal + high) | high
    pub min_confidence: String,
    /// Celda con fuego en más de esta fracción de los meses = fuente estática.
    pub static_threshold: f64,
    /// Meses con fuego mínimos para aplicar el filtro de fuentes estáticas
    /// (con pocos meses, toda celda con fuego parecería estática).
    pub static_min_months: i64,
    /// Último día con datos de Suomi NPP (exclusivo), YYYY-MM-DD.
    pub snpp_end: String,
    /// Satélites VIIRS de referencia, en orden de prioridad.
    pub viirs_priority: Vec<String>,
    /// Último mes usado para calcular el factor, como YYYY-MM-01.
    pub overlap_end: String,
    /// Si un mes tiene menos celdas-día VIIRS en el traslape, usa el factor de su época.
    pub min_cells_month: f64,
    /// Meses de la época seca (el resto es la lluviosa).
    pub dry_months: Vec<u32>,
    /// Años (inclusive) para la mediana histórica de la anomalía.
    pub reference_years: [i32; 2],
    /// Diferencia de la hora local con UTC, en horas (El Salvador: -6, sin horario de verano).
    pub utc_offset_hours: i32,
}

impl HarmonizationConfig {
    pub fn from_env() -> Self {
        let bbox = match var("HARMONIZE_BBOX").as_deref() {
            Some("none" | "off" | "false") => None,
            other => {
                let sync_area = var("FIRMS_SYNC_AREA");
                let text = other.or(sync_area.as_deref()).unwrap_or(EL_SALVADOR_BBOX);
                let (w, s, e, n) = crate::filters::parse_bbox(text)
                    .unwrap_or_else(|e| panic!("HARMONIZE_BBOX inválido: {}", e.message));
                Some([w, s, e, n])
            }
        };
        let min_confidence = var("HARMONIZE_MIN_CONFIDENCE")
            .unwrap_or_else(|| "nominal".into())
            .to_ascii_lowercase();
        assert!(
            matches!(min_confidence.as_str(), "nominal" | "high"),
            "HARMONIZE_MIN_CONFIDENCE debe ser nominal o high"
        );
        let snpp_end = var("HARMONIZE_SNPP_END").unwrap_or_else(|| "2026-11-02".into());
        crate::validation::date("HARMONIZE_SNPP_END", &snpp_end).unwrap_or_else(|e| panic!("{e}"));
        let overlap_end = format!(
            "{}-01",
            var("HARMONIZE_OVERLAP_END").unwrap_or_else(|| "2025-12".into())
        );
        crate::validation::date("HARMONIZE_OVERLAP_END (YYYY-MM)", &overlap_end)
            .unwrap_or_else(|e| panic!("{e}"));
        let dry_months: Vec<u32> = list("HARMONIZE_DRY_MONTHS", "11,12,1,2,3,4")
            .iter()
            .map(|m| match m.parse::<u32>() {
                Ok(n) if (1..=12).contains(&n) => n,
                _ => panic!("HARMONIZE_DRY_MONTHS debe ser una lista de meses 1..12"),
            })
            .collect();
        let years = var("HARMONIZE_REFERENCE_YEARS").unwrap_or_else(|| "2003-2025".into());
        let reference_years = match years
            .split_once('-')
            .map(|(a, b)| (a.trim().parse(), b.trim().parse()))
        {
            Some((Ok(a), Ok(b))) if a <= b => [a, b],
            _ => panic!("HARMONIZE_REFERENCE_YEARS debe ser AAAA-AAAA"),
        };
        let finite = |key: &str, default: f64| -> f64 {
            let v = num(key, default);
            assert!(v.is_finite(), "{key} debe ser un número finito");
            v
        };
        let cell_deg = finite("HARMONIZE_CELL_DEG", 0.01);
        assert!(
            (0.001..=1.0).contains(&cell_deg),
            "HARMONIZE_CELL_DEG debe estar entre 0.001 y 1"
        );
        let static_threshold = finite("HARMONIZE_STATIC_THRESHOLD", 0.30);
        assert!(
            (0.0..=1.0).contains(&static_threshold),
            "HARMONIZE_STATIC_THRESHOLD debe estar entre 0 y 1"
        );
        let utc_offset_hours = num("HARMONIZE_UTC_OFFSET", -6i32);
        assert!(
            (-12..=14).contains(&utc_offset_hours),
            "HARMONIZE_UTC_OFFSET debe estar entre -12 y 14"
        );
        Self {
            bbox,
            cell_deg,
            min_confidence,
            static_threshold,
            static_min_months: num("HARMONIZE_STATIC_MIN_MONTHS", 12i64).max(1),
            snpp_end,
            viirs_priority: list("HARMONIZE_VIIRS_PRIORITY", "Suomi NPP,NOAA-20,NOAA-21"),
            overlap_end,
            min_cells_month: finite("HARMONIZE_MIN_CELLS_MONTH", 30.0).max(0.0),
            dry_months,
            reference_years,
            utc_offset_hours,
        }
    }
}

impl Default for HarmonizationConfig {
    /// Los valores del notebook de armonización (el área es la de FIRMS_SYNC_AREA por defecto).
    fn default() -> Self {
        Self {
            bbox: Some([-90.13, 13.15, -87.68, 14.45]),
            cell_deg: 0.01,
            min_confidence: "nominal".into(),
            static_threshold: 0.30,
            static_min_months: 12,
            snpp_end: "2026-11-02".into(),
            viirs_priority: vec!["Suomi NPP".into(), "NOAA-20".into(), "NOAA-21".into()],
            overlap_end: "2025-12-01".into(),
            min_cells_month: 30.0,
            dry_months: vec![11, 12, 1, 2, 3, 4],
            reference_years: [2003, 2025],
            utc_offset_hours: -6,
        }
    }
}

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
    pub harmonization: HarmonizationConfig,
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
            harmonization: HarmonizationConfig::from_env(),
        }
    }
}
