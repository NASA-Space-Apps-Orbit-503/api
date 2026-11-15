//! Validaciones compartidas por las entidades. Los mensajes van al cliente.

/// Texto obligatorio: sin espacios a los lados, no vacío y con largo máximo.
pub fn required_text(field: &str, value: &str, max: usize) -> Result<String, String> {
    let v = value.trim();
    if v.is_empty() {
        return Err(format!("{field} es obligatorio"));
    }
    if v.chars().count() > max {
        return Err(format!("{field} admite como máximo {max} caracteres"));
    }
    Ok(v.to_string())
}

/// Texto opcional: un texto vacío se guarda como null.
pub fn optional_text(
    field: &str,
    value: Option<String>,
    max: usize,
) -> Result<Option<String>, String> {
    match value.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) if v.chars().count() > max => {
            Err(format!("{field} admite como máximo {max} caracteres"))
        }
        Some(v) => Ok(Some(v.to_string())),
    }
}

/// Confianza de FIRMS como categoría: `low` | `nominal` | `high`.
/// Acepta la forma de VIIRS (`l` / `n` / `h` o la palabra completa) y la de MODIS
/// (0-100: menos de 30 low, menos de 80 nominal, el resto high; umbrales de la guía de FIRMS).
pub fn confidence(field: &str, value: Option<String>) -> Result<Option<String>, String> {
    let Some(v) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let category = match v.to_ascii_lowercase().as_str() {
        "l" | "low" => "low",
        "n" | "nominal" => "nominal",
        "h" | "high" => "high",
        other => match other.parse::<f64>() {
            Ok(n) if (0.0..=100.0).contains(&n) => {
                if n < 30.0 {
                    "low"
                } else if n < 80.0 {
                    "nominal"
                } else {
                    "high"
                }
            }
            _ => return Err(format!(
                "{field} debe ser low, nominal o high (también l/n/h o un valor MODIS de 0 a 100)"
            )),
        },
    };
    Ok(Some(category.to_string()))
}

/// Número obligatorio dentro de [min, max].
pub fn in_range(field: &str, value: f64, min: f64, max: f64) -> Result<(), String> {
    if value.is_finite() && value >= min && value <= max {
        Ok(())
    } else {
        Err(format!("{field} debe estar entre {min} y {max}"))
    }
}

/// Número opcional mayor o igual a `min`.
pub fn optional_min(field: &str, value: Option<f64>, min: f64) -> Result<(), String> {
    match value {
        Some(v) if !v.is_finite() || v < min => {
            Err(format!("{field} debe ser mayor o igual a {min}"))
        }
        _ => Ok(()),
    }
}

pub fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub fn days_in_month(year: i32, month: i32) -> i32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Comprueba que `year-month-day` sea una fecha real.
pub fn valid_ymd(year: i32, month: i32, day: i32) -> bool {
    (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month)
}

/// Fecha en formato `YYYY-MM-DD`.
pub fn date(field: &str, value: &str) -> Result<String, String> {
    let v = value.trim();
    let err = || format!("{field} debe tener formato YYYY-MM-DD y ser una fecha válida");
    let parts: Vec<&str> = v.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return Err(err());
    }
    let nums: Vec<i32> = parts
        .iter()
        .map(|p| p.parse::<i32>())
        .collect::<Result<_, _>>()
        .map_err(|_| err())?;
    if valid_ymd(nums[0], nums[1], nums[2]) {
        Ok(v.to_string())
    } else {
        Err(err())
    }
}

/// Hora UTC al estilo FIRMS. Acepta `"0412"`, `"412"` o `"04:12"` y devuelve `"0412"`.
pub fn hhmm(field: &str, value: &str) -> Result<String, String> {
    let digits: String = value.trim().chars().filter(|c| *c != ':').collect();
    let err = || format!("{field} debe ser una hora HHMM (por ejemplo \"0412\")");
    if digits.is_empty() || digits.len() > 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(err());
    }
    let padded = format!("{digits:0>4}");
    let hh: u32 = padded[..2].parse().map_err(|_| err())?;
    let mm: u32 = padded[2..].parse().map_err(|_| err())?;
    if hh < 24 && mm < 60 {
        Ok(padded)
    } else {
        Err(err())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fechas() {
        assert!(date("d", "2024-02-29").is_ok());
        assert!(date("d", "2023-02-29").is_err());
        assert!(date("d", "2024-2-01").is_err());
    }

    #[test]
    fn confianza_en_categorias() {
        let c = |v: &str| confidence("confidence", Some(v.into()));
        assert_eq!(c("n").unwrap().as_deref(), Some("nominal"));
        assert_eq!(c(" H ").unwrap().as_deref(), Some("high"));
        assert_eq!(c("Low").unwrap().as_deref(), Some("low"));
        assert_eq!(c("29").unwrap().as_deref(), Some("low"));
        assert_eq!(c("30").unwrap().as_deref(), Some("nominal"));
        assert_eq!(c("79.5").unwrap().as_deref(), Some("nominal"));
        assert_eq!(c("80").unwrap().as_deref(), Some("high"));
        assert_eq!(c("").unwrap(), None);
        assert!(c("101").is_err());
        assert!(c("x").is_err());
        assert_eq!(confidence("confidence", None).unwrap(), None);
    }

    #[test]
    fn horas() {
        assert_eq!(hhmm("t", "412").unwrap(), "0412");
        assert_eq!(hhmm("t", "04:12").unwrap(), "0412");
        assert!(hhmm("t", "2460").is_err());
    }
}
