//! Filtros comunes para detecciones: área, fechas, instrumento, satélite, confianza y día/noche.
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
    pub bbox: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub radius_km: Option<f64>,
    pub polygon: Option<String>,
    /// YYYY-MM-DD, inclusivo
    pub from: Option<String>,
    /// YYYY-MM-DD, inclusivo
    pub to: Option<String>,
    /// Lista separada por comas: MODIS,VIIRS
    pub instrument: Option<String>,
    /// Lista separada por comas: N,N20,Terra,Aqua
    pub satellite: Option<String>,
    /// low | nominal | high
    pub min_confidence: Option<String>,
    /// D | N
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

impl DetectionFilter {
    /// Convierte los parámetros en una condición SQL con sus valores.
    pub fn to_sql(&self, spatial_available: bool) -> Result<SqlFilter, ApiError> {
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

        // ---- Fechas
        let from = self
            .from
            .as_deref()
            .map(|d| date("from", d))
            .transpose()
            .map_err(ApiError::bad_request)?;
        let to = self
            .to
            .as_deref()
            .map(|d| date("to", d))
            .transpose()
            .map_err(ApiError::bad_request)?;
        if let (Some(f), Some(t)) = (&from, &to) {
            if f > t {
                return Err(ApiError::bad_request("from no puede ser posterior a to"));
            }
        }
        if let Some(f) = &from {
            conds.push("acq_date >= CAST(? AS DATE)".into());
            params.push(Value::Text(f.clone()));
        }
        if let Some(t) = &to {
            conds.push("acq_date <= CAST(? AS DATE)".into());
            params.push(Value::Text(t.clone()));
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
            from,
            to,
        })
    }
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
            from: Some("2020-01-01".into()),
            to: Some("2020-12-31".into()),
            instrument: Some("modis,viirs".into()),
            min_confidence: Some("nominal".into()),
            ..Default::default()
        };
        let sql = f.to_sql(false).unwrap();
        assert_eq!(sql.clause.matches('?').count(), sql.params.len());
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
}
