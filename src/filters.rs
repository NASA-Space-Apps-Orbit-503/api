//! Filtros comunes para detecciones: área, rango de fechas (`startDate` + `endDate`),
//! instrumento, satélite, confianza y día/noche.
//! Los usan la búsqueda, la descarga CSV y todas las estadísticas.

use duckdb::types::Value;
use serde::Deserialize;

use crate::{
    error::ApiError,
    validation::{date, in_range},
};

/// Parámetros de query aceptados por todos los endpoints de consulta.
/// El área se indica con UNA de estas opciones:
///   - `bbox=oeste,sur,este,norte`
///   - `lat`, `lon` y `radius_km`
///   - `polygon` = geometría GeoJSON (Polygon o MultiPolygon)
#[derive(Debug, Default, Deserialize)]
pub struct DetectionFilter {
    #[serde(alias = "filter[bbox]")]
    pub bbox: Option<String>,
    #[serde(alias = "filter[lat]")]
    pub lat: Option<f64>,
    #[serde(alias = "filter[lon]")]
    pub lon: Option<f64>,
    #[serde(alias = "filter[radius_km]")]
    pub radius_km: Option<f64>,
    #[serde(alias = "filter[polygon]")]
    pub polygon: Option<String>,
    /// YYYY-MM-DD, inclusivo. Se envía junto con `endDate`.
    #[serde(rename = "startDate", alias = "filter[startDate]")]
    pub start_date: Option<String>,
    /// YYYY-MM-DD, inclusivo. Se envía junto con `startDate`.
    #[serde(rename = "endDate", alias = "filter[endDate]")]
    pub end_date: Option<String>,
    /// Lista separada por comas: MODIS,VIIRS
    #[serde(alias = "filter[instrument]")]
    pub instrument: Option<String>,
    /// Lista separada por comas: N,N20,Terra,Aqua
    #[serde(alias = "filter[satellite]")]
    pub satellite: Option<String>,
    /// low | nominal | high
    #[serde(alias = "filter[min_confidence]")]
    pub min_confidence: Option<String>,
    /// D | N
    #[serde(alias = "filter[daynight]")]
    pub daynight: Option<String>,
}

/// Condición SQL lista para usar sobre `detections_harmonized`.
#[derive(Debug, Clone, Default)]
pub struct SqlFilter {
    /// `WHERE ...` o vacío.
    pub clause: String,
    /// Valores para los `?` de `clause`, en orden.
    pub params: Vec<Value>,
    /// Fechas normalizadas (para rellenar series con ceros).
    pub from: Option<String>,
    pub to: Option<String>,
}

/// `oeste,sur,este,norte` → (oeste, sur, este, norte)
pub fn parse_bbox(text: &str) -> Result<(f64, f64, f64, f64), ApiError> {
    let err = || {
        ApiError::bad_request(
            "bbox debe ser oeste,sur,este,norte (por ejemplo -90.13,13.15,-87.68,14.45)",
        )
    };
    let nums: Vec<f64> = text
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| err())?;
    if nums.len() != 4 {
        return Err(err());
    }
    let (w, s, e, n) = (nums[0], nums[1], nums[2], nums[3]);
    in_range("bbox oeste", w, -180.0, 180.0).map_err(ApiError::bad_request)?;
    in_range("bbox este", e, -180.0, 180.0).map_err(ApiError::bad_request)?;
    in_range("bbox sur", s, -90.0, 90.0).map_err(ApiError::bad_request)?;
    in_range("bbox norte", n, -90.0, 90.0).map_err(ApiError::bad_request)?;
    if w > e || s > n {
        return Err(ApiError::bad_request(
            "bbox: oeste debe ser ≤ este y sur ≤ norte",
        ));
    }
    Ok((w, s, e, n))
}

/// Junta todos los pares [lon, lat] de una geometría GeoJSON.
fn collect_positions(v: &serde_json::Value, out: &mut Vec<(f64, f64)>) {
    if let Some(arr) = v.as_array() {
        if arr.len() >= 2 && arr[0].is_number() && arr[1].is_number() {
            if let (Some(lon), Some(lat)) = (arr[0].as_f64(), arr[1].as_f64()) {
                out.push((lon, lat));
            }
        } else {
            for item in arr {
                collect_positions(item, out);
            }
        }
    }
}

fn split_list(text: &str, upper: bool) -> Vec<String> {
    text.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if upper {
                s.to_ascii_uppercase()
            } else {
                s.to_string()
            }
        })
        .collect()
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// Junta dos cláusulas `WHERE ...` (cualquiera puede estar vacía) con AND.
pub fn join_clauses(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a} AND {}", b.trim_start_matches("WHERE ")),
    }
}

impl DetectionFilter {
    /// Convierte todos los parámetros (área, atributos y tiempo) en una condición SQL
    /// sobre `detections_harmonized`.
    pub fn to_sql(&self, spatial_available: bool) -> Result<SqlFilter, ApiError> {
        let mut sql = self.to_sql_without_time(spatial_available)?;
        let time = self.time_sql("acq_date")?;
        sql.clause = join_clauses(&sql.clause, &time.clause);
        sql.params.extend(time.params);
        sql.from = time.from;
        sql.to = time.to;
        Ok(sql)
    }

    /// Solo área y atributos (instrumento, satélite, confianza, día/noche), sin filtros de
    /// tiempo. Lo usan los cálculos que necesitan la historia completa como referencia.
    pub fn to_sql_without_time(&self, spatial_available: bool) -> Result<SqlFilter, ApiError> {
        let mut conds: Vec<String> = Vec::new();
        let mut params: Vec<Value> = Vec::new();

        let uses_point = self.lat.is_some() || self.lon.is_some() || self.radius_km.is_some();
        let area_modes = [self.bbox.is_some(), uses_point, self.polygon.is_some()]
            .iter()
            .filter(|b| **b)
            .count();
        if area_modes > 1 {
            return Err(ApiError::bad_request(
                "indica el área de una sola forma: bbox, lat/lon/radius_km o polygon",
            ));
        }

        // ---- Área: rectángulo
        if let Some(b) = &self.bbox {
            let (w, s, e, n) = parse_bbox(b)?;
            conds.push("longitude BETWEEN ? AND ? AND latitude BETWEEN ? AND ?".into());
            params.extend([w, e, s, n].map(Value::Double));
        }

        // ---- Área: punto + radio (distancia haversine en km)
        if uses_point {
            let (Some(lat), Some(lon), Some(r)) = (self.lat, self.lon, self.radius_km) else {
                return Err(ApiError::bad_request(
                    "para buscar por punto indica lat, lon y radius_km",
                ));
            };
            in_range("lat", lat, -90.0, 90.0).map_err(ApiError::bad_request)?;
            in_range("lon", lon, -180.0, 180.0).map_err(ApiError::bad_request)?;
            in_range("radius_km", r, 0.1, 2000.0).map_err(ApiError::bad_request)?;
            // Prefiltro rectangular (usa menos cálculo) y luego distancia exacta.
            let dlat = r / 111.0;
            let dlon = r / (111.0 * lat.to_radians().cos().abs().max(0.01));
            conds.push("latitude BETWEEN ? AND ? AND longitude BETWEEN ? AND ?".into());
            params.extend([lat - dlat, lat + dlat, lon - dlon, lon + dlon].map(Value::Double));
            conds.push(
                "2 * 6371 * asin(sqrt(pow(sin(radians(latitude - ?) / 2), 2) \
                 + cos(radians(?)) * cos(radians(latitude)) * pow(sin(radians(longitude - ?) / 2), 2))) <= ?"
                    .into(),
            );
            params.extend([lat, lat, lon, r].map(Value::Double));
        }

        // ---- Área: polígono GeoJSON (requiere la extensión spatial)
        if let Some(poly) = &self.polygon {
            if !spatial_available {
                return Err(ApiError::new(
                    axum::http::StatusCode::NOT_IMPLEMENTED,
                    "el filtro por polígono no está disponible en este servidor (falta la extensión spatial); usa bbox o lat/lon/radius_km",
                ));
            }
            let geo: serde_json::Value = serde_json::from_str(poly).map_err(|_| {
                ApiError::bad_request("polygon debe ser una geometría GeoJSON válida")
            })?;
            let kind = geo.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if kind != "Polygon" && kind != "MultiPolygon" {
                return Err(ApiError::bad_request(
                    "polygon debe ser de tipo Polygon o MultiPolygon",
                ));
            }
            let mut pts = Vec::new();
            collect_positions(
                geo.get("coordinates").unwrap_or(&serde_json::Value::Null),
                &mut pts,
            );
            if pts.len() < 4 {
                return Err(ApiError::bad_request(
                    "polygon necesita al menos 4 posiciones",
                ));
            }
            let (mut w, mut s, mut e, mut n) = (180.0f64, 90.0f64, -180.0f64, -90.0f64);
            for (lon, lat) in &pts {
                w = w.min(*lon);
                e = e.max(*lon);
                s = s.min(*lat);
                n = n.max(*lat);
            }
            conds.push("longitude BETWEEN ? AND ? AND latitude BETWEEN ? AND ?".into());
            params.extend([w, e, s, n].map(Value::Double));
            conds.push("ST_Contains(ST_GeomFromGeoJSON(?), ST_Point(longitude, latitude))".into());
            params.push(Value::Text(poly.clone()));
        }

        // ---- Instrumento y satélite
        if let Some(list) = self
            .instrument
            .as_deref()
            .map(|t| split_list(t, true))
            .filter(|l| !l.is_empty())
        {
            conds.push(format!("instrument IN ({})", placeholders(list.len())));
            params.extend(list.into_iter().map(Value::Text));
        }
        if let Some(list) = self
            .satellite
            .as_deref()
            .map(|t| split_list(t, false))
            .filter(|l| !l.is_empty())
        {
            conds.push(format!("satellite IN ({})", placeholders(list.len())));
            params.extend(list.into_iter().map(Value::Text));
        }

        // ---- Confianza armonizada
        if let Some(level) = self
            .min_confidence
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let n = match level.to_ascii_lowercase().as_str() {
                "low" | "l" => 1,
                "nominal" | "n" => 2,
                "high" | "h" => 3,
                _ => {
                    return Err(ApiError::bad_request(
                        "min_confidence debe ser low, nominal o high",
                    ))
                }
            };
            conds.push("confidence_level >= ?".into());
            params.push(Value::Int(n));
        }

        // ---- Día / noche
        if let Some(dn) = self
            .daynight
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let dn = dn.to_ascii_uppercase();
            if dn != "D" && dn != "N" {
                return Err(ApiError::bad_request("daynight debe ser D o N"));
            }
            conds.push("daynight = ?".into());
            params.push(Value::Text(dn));
        }

        let clause = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };
        Ok(SqlFilter {
            clause,
            params,
            from: None,
            to: None,
        })
    }

    /// Rango `startDate`..`endDate` (inclusivo) sobre la columna de fecha `col`.
    pub fn time_sql(&self, col: &str) -> Result<SqlFilter, ApiError> {
        let range = date_range(self.start_date.as_deref(), self.end_date.as_deref())?;
        let Some((start, end)) = range else {
            return Ok(SqlFilter::default());
        };
        Ok(SqlFilter {
            clause: format!("WHERE {col} BETWEEN CAST(? AS DATE) AND CAST(? AS DATE)"),
            params: vec![Value::Text(start.clone()), Value::Text(end.clone())],
            from: Some(start),
            to: Some(end),
        })
    }
}

/// `startDate` y `endDate` (YYYY-MM-DD): las dos o ninguna, y `startDate` ≤ `endDate`.
pub fn date_range(
    start: Option<&str>,
    end: Option<&str>,
) -> Result<Option<(String, String)>, ApiError> {
    fn clean(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|v| !v.is_empty())
    }
    match (clean(start), clean(end)) {
        (None, None) => Ok(None),
        (Some(s), Some(e)) => {
            let s = date("startDate", s).map_err(ApiError::bad_request)?;
            let e = date("endDate", e).map_err(ApiError::bad_request)?;
            if s > e {
                return Err(ApiError::bad_request(
                    "startDate no puede ser posterior a endDate",
                ));
            }
            Ok(Some((s, e)))
        }
        _ => Err(ApiError::bad_request(
            "startDate y endDate se envían juntas: indica las dos fechas o ninguna",
        )),
    }
}

/// Filtros aceptados con la sintaxis `filter[nombre]=valor` (estilo spatie/laravel-query-builder).
/// Cada uno es también un parámetro plano equivalente (`nombre=valor`).
pub const ALLOWED_FILTERS: &[&str] = &[
    "bbox",
    "lat",
    "lon",
    "radius_km",
    "polygon",
    "startDate",
    "endDate",
    "instrument",
    "satellite",
    "min_confidence",
    "daynight",
    "min_score",
    "severity",
];

/// Filtros de tiempo que ya no existen: se rechazan para que nadie reciba datos sin filtrar
/// creyendo que filtró.
const REMOVED_TIME_FILTERS: &[&str] = &["from", "to", "date", "year", "month", "day"];

/// Middleware: rechaza con 400 cualquier `filter[...]` que no esté en [`ALLOWED_FILTERS`]
/// y los filtros de tiempo viejos, en vez de ignorarlos en silencio.
pub async fn reject_unknown_filters(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::{extract::Query, response::IntoResponse};

    let pairs = Query::<Vec<(String, String)>>::try_from_uri(req.uri())
        .map(|Query(p)| p)
        .unwrap_or_default();
    let mut seen: Vec<&str> = Vec::new();
    for (key, _) in &pairs {
        // `month=3&filter[month]=4` (o la misma clave dos veces) es ambiguo: error JSON claro
        // en vez del texto plano de axum ("duplicate field").
        let plain = key
            .strip_prefix("filter[")
            .and_then(|k| k.strip_suffix(']'))
            .unwrap_or(key);
        if seen.contains(&plain) {
            return ApiError::bad_request(format!("{plain} está repetido en la query"))
                .into_response();
        }
        seen.push(plain);
        if REMOVED_TIME_FILTERS.contains(&plain) {
            return ApiError::bad_request(format!(
                "el filtro {plain} ya no existe: usa startDate y endDate (YYYY-MM-DD, las dos juntas)"
            ))
            .into_response();
        }
        if let Some(name) = key
            .strip_prefix("filter[")
            .and_then(|k| k.strip_suffix(']'))
        {
            if !ALLOWED_FILTERS.contains(&name) {
                return ApiError::bad_request(format!(
                    "filtro desconocido: {name}. Permitidos: {}",
                    ALLOWED_FILTERS.join(", ")
                ))
                .into_response();
            }
        } else if key.starts_with("filter[") {
            return ApiError::bad_request(format!("filtro mal formado: {key}")).into_response();
        }
    }
    next.run(req).await
}

/// Valor de fecha opcional como parámetro SQL (NULL si no hay).
pub fn opt_text(v: &Option<String>) -> Value {
    v.clone().map(Value::Text).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbox_ok() {
        assert!(parse_bbox("-90.13,13.15,-87.68,14.45").is_ok());
        assert!(parse_bbox("-87,13,-90,14").is_err());
        assert!(parse_bbox("1,2,3").is_err());
    }

    #[test]
    fn filtro_completo() {
        let f = DetectionFilter {
            bbox: Some("-90.13,13.15,-87.68,14.45".into()),
            start_date: Some("2020-01-01".into()),
            end_date: Some("2020-12-31".into()),
            instrument: Some("modis,viirs".into()),
            min_confidence: Some("nominal".into()),
            ..Default::default()
        };
        let sql = f.to_sql(false).unwrap();
        assert_eq!(sql.clause.matches('?').count(), sql.params.len());
        assert!(sql.clause.contains("acq_date BETWEEN"));
        assert_eq!(sql.from.as_deref(), Some("2020-01-01"));
    }

    #[test]
    fn area_doble_rechazada() {
        let f = DetectionFilter {
            bbox: Some("-90,13,-87,14".into()),
            lat: Some(13.7),
            ..Default::default()
        };
        assert!(f.to_sql(false).is_err());
    }

    #[test]
    fn rango_de_fechas() {
        assert_eq!(date_range(None, None).unwrap(), None);
        assert_eq!(date_range(Some(" "), None).unwrap(), None);
        assert!(date_range(Some("2024-01-01"), None).is_err());
        assert!(date_range(None, Some("2024-01-01")).is_err());
        assert!(date_range(Some("2024-02-01"), Some("2024-01-01")).is_err());
        assert!(date_range(Some("2024-02-30"), Some("2024-03-01")).is_err());
        assert_eq!(
            date_range(Some("2024-01-01"), Some("2024-01-01")).unwrap(),
            Some(("2024-01-01".into(), "2024-01-01".into()))
        );
    }

    #[test]
    fn sin_tiempo_y_union() {
        let f = DetectionFilter {
            bbox: Some("-90,13,-87,14".into()),
            start_date: Some("2024-01-01".into()),
            end_date: Some("2024-12-31".into()),
            ..Default::default()
        };
        let base = f.to_sql_without_time(false).unwrap();
        assert!(!base.clause.contains("acq_date"));
        assert_eq!(join_clauses("", "WHERE a"), "WHERE a");
        assert_eq!(join_clauses("WHERE a", "WHERE b"), "WHERE a AND b");
    }

    #[test]
    fn query_con_nombres_camel_case() {
        let uri: axum::http::Uri = "/x?startDate=2024-01-01&filter[endDate]=2024-01-31"
            .parse()
            .unwrap();
        let axum::extract::Query(f) =
            axum::extract::Query::<DetectionFilter>::try_from_uri(&uri).unwrap();
        assert_eq!(f.start_date.as_deref(), Some("2024-01-01"));
        assert_eq!(f.end_date.as_deref(), Some("2024-01-31"));
    }
}
