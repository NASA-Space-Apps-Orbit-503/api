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
    /// YYYY-MM-DD, inclusivo
    #[serde(alias = "filter[from]")]
    pub from: Option<String>,
    /// YYYY-MM-DD, inclusivo
    #[serde(alias = "filter[to]")]
    pub to: Option<String>,
    /// Un día exacto, YYYY-MM-DD
    #[serde(alias = "filter[date]")]
    pub date: Option<String>,
    /// Lista separada por comas: 2023,2024
    #[serde(alias = "filter[year]")]
    pub year: Option<String>,
    /// Lista separada por comas (1..12): 3,4
    #[serde(alias = "filter[month]")]
    pub month: Option<String>,
    /// Lista separada por comas (1..31): 1,15
    #[serde(alias = "filter[day]")]
    pub day: Option<String>,
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

/// Lista de enteros separada por comas, cada uno dentro de [min, max].
pub fn int_list(field: &str, text: Option<&str>, min: i64, max: i64) -> Result<Vec<i64>, ApiError> {
    let Some(text) = text else {
        return Ok(Vec::new());
    };
    split_list(text, false)
        .iter()
        .map(|p| match p.parse::<i64>() {
            Ok(n) if (min..=max).contains(&n) => Ok(n),
            _ => Err(ApiError::bad_request(format!(
                "{field} debe ser una lista de enteros entre {min} y {max} separada por comas"
            ))),
        })
        .collect()
}

impl DetectionFilter {
    /// Convierte todos los parámetros (área, atributos y tiempo) en una condición SQL
    /// sobre `detections_harmonized`.
    pub fn to_sql(&self, spatial_available: bool) -> Result<SqlFilter, ApiError> {
        let mut sql = self.to_sql_without_time(spatial_available)?;
        let time = self.time_sql("acq_date", true)?;
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

    /// Filtros de tiempo (`from`, `to`, `date`, `year`, `month`, `day`) sobre la columna de
    /// fecha `col`. Con `include_day = false` se ignoran `date` y `day` (series por mes).
    pub fn time_sql(&self, col: &str, include_day: bool) -> Result<SqlFilter, ApiError> {
        self.time_sql_with(col, include_day, true)
    }

    /// Como [`Self::time_sql`] pero sin `from`/`to`: para series que ya están acotadas por
    /// esas fechas y cuyos periodos empiezan el día 1 (un `from` a mitad de mes los excluiría).
    pub fn period_sql(&self, col: &str, include_day: bool) -> Result<SqlFilter, ApiError> {
        self.time_sql_with(col, include_day, false)
    }

    fn time_sql_with(
        &self,
        col: &str,
        include_day: bool,
        include_range: bool,
    ) -> Result<SqlFilter, ApiError> {
        let mut conds: Vec<String> = Vec::new();
        let mut params: Vec<Value> = Vec::new();

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
        if let (true, Some(f)) = (include_range, &from) {
            conds.push(format!("{col} >= CAST(? AS DATE)"));
            params.push(Value::Text(f.clone()));
        }
        if let (true, Some(t)) = (include_range, &to) {
            conds.push(format!("{col} <= CAST(? AS DATE)"));
            params.push(Value::Text(t.clone()));
        }

        let years = int_list("year", self.year.as_deref(), 1900, 2100)?;
        if !years.is_empty() {
            conds.push(format!("year({col}) IN ({})", placeholders(years.len())));
            params.extend(years.into_iter().map(Value::BigInt));
        }
        let months = int_list("month", self.month.as_deref(), 1, 12)?;
        if !months.is_empty() {
            conds.push(format!("month({col}) IN ({})", placeholders(months.len())));
            params.extend(months.into_iter().map(Value::BigInt));
        }
        if include_day {
            let days = int_list("day", self.day.as_deref(), 1, 31)?;
            if !days.is_empty() {
                conds.push(format!("day({col}) IN ({})", placeholders(days.len())));
                params.extend(days.into_iter().map(Value::BigInt));
            }
            if let Some(d) = self.date.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let d = date("date", d).map_err(ApiError::bad_request)?;
                conds.push(format!("{col} = CAST(? AS DATE)"));
                params.push(Value::Text(d));
            }
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

/// Filtros aceptados con la sintaxis `filter[nombre]=valor` (estilo spatie/laravel-query-builder).
/// Cada uno es también un parámetro plano equivalente (`nombre=valor`).
pub const ALLOWED_FILTERS: &[&str] = &[
    "bbox",
    "lat",
    "lon",
    "radius_km",
    "polygon",
    "from",
    "to",
    "date",
    "year",
    "month",
    "day",
    "instrument",
    "satellite",
    "min_confidence",
    "daynight",
    "min_score",
    "severity",
];

/// Middleware: rechaza con 400 cualquier `filter[...]` que no esté en [`ALLOWED_FILTERS`],
/// en vez de ignorarlo en silencio.
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
        if let Some(name) = key.strip_prefix("filter[").and_then(|k| k.strip_suffix(']')) {
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
    #[test]
    fn filtros_de_tiempo() {
        let f = DetectionFilter {
            year: Some("2023, 2024".into()),
            month: Some("3".into()),
            day: Some("14".into()),
            date: Some("2024-03-14".into()),
            ..Default::default()
        };
        let sql = f.to_sql(false).unwrap();
        assert_eq!(sql.clause.matches('?').count(), sql.params.len());
        assert!(sql.clause.contains("year(acq_date) IN (?, ?)"));

        let monthly = f.period_sql("p", false).unwrap();
        assert!(!monthly.clause.contains("day("));
        assert!(DetectionFilter {
            month: Some("13".into()),
            ..Default::default()
        }
        .to_sql(false)
        .is_err());
    }

    #[test]
    fn sin_tiempo_y_union() {
        let f = DetectionFilter {
            bbox: Some("-90,13,-87,14".into()),
            year: Some("2024".into()),
            ..Default::default()
        };
        let base = f.to_sql_without_time(false).unwrap();
        assert!(!base.clause.contains("year("));
        assert_eq!(join_clauses("", "WHERE a"), "WHERE a");
        assert_eq!(join_clauses("WHERE a", "WHERE b"), "WHERE a AND b");
    }
}
