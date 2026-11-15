//! Las respuestas JSON devuelven las fechas como epoch en milisegundos (UTC).
//! Un mes se representa con su primer día a las 00:00 UTC.
//!
//! En SQL se usa `epoch_ms(<fecha>::TIMESTAMP)`; estas funciones cubren las fechas que ya
//! están en Rust como texto (los límites de un filtro, la configuración).

const MS_PER_DAY: i64 = 86_400_000;

/// Días desde 1970-01-01 (algoritmo de Howard Hinnant, calendario gregoriano proléptico).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 {
        yoe + era * 400 + 1
    } else {
        yoe + era * 400
    };
    (year, month, day)
}

/// "YYYY-MM-DD" o "YYYY-MM" (primer día del mes) → epoch ms a las 00:00 UTC.
/// None si el texto no tiene ese formato.
pub fn date_ms(text: &str) -> Option<i64> {
    let mut parts = text.trim().splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = match parts.next() {
        Some(d) => d.parse().ok()?,
        None => 1,
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * MS_PER_DAY)
}

/// Año, mes y día → epoch ms a las 00:00 UTC.
pub fn ymd_ms(year: i32, month: i32, day: i32) -> i64 {
    days_from_civil(year.into(), month.into(), day.into()) * MS_PER_DAY
}

/// Igual que `date_ms`, para un valor opcional.
pub fn opt_date_ms(text: Option<&str>) -> Option<i64> {
    text.and_then(date_ms)
}

/// Epoch ms → "YYYY-MM-DD" (UTC). Para el CSV, que mantiene el formato de FIRMS.
pub fn ms_to_date(ms: i64) -> String {
    let (y, m, d) = civil_from_days(ms.div_euclid(MS_PER_DAY));
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_dates_and_months() {
        assert_eq!(date_ms("1970-01-01"), Some(0));
        assert_eq!(date_ms("2024-01-01"), Some(1_704_067_200_000));
        assert_eq!(date_ms("2024-03"), Some(1_709_251_200_000));
        assert_eq!(date_ms("2024-02-29"), Some(1_709_164_800_000));
        assert_eq!(ymd_ms(2024, 3, 1), 1_709_251_200_000);
        assert_eq!(date_ms("2024-13-01"), None);
        assert_eq!(date_ms("abc"), None);
    }

    #[test]
    fn round_trips_to_text() {
        for d in ["1969-12-31", "2000-02-29", "2024-03-14", "2100-12-31"] {
            assert_eq!(ms_to_date(date_ms(d).unwrap()), d);
        }
        // Una hora cualquiera dentro del día devuelve ese mismo día.
        assert_eq!(ms_to_date(1_710_444_480_000), "2024-03-14");
    }
}
