//! Apertura de la base, esquema, extensiones y endurecimiento.

use duckdb::Connection;

const SCHEMA: &str = include_str!("schema.sql");

/// Abre la base. Devuelve también si la extensión `spatial` quedó disponible.
pub fn open(path: &str) -> duckdb::Result<(Connection, bool)> {
    let conn = Connection::open(path)?;

    // Crea tablas, vistas, secuencias e instrumentos base si no existen.
    conn.execute_batch(SCHEMA)?;

    // Las extensiones se cargan ANTES de cerrar el acceso externo.
    // JSON es obligatoria; spatial es opcional (solo para filtros por polígono).
    if conn.execute_batch("LOAD json;").is_err() {
        conn.execute_batch("INSTALL json; LOAD json;")?;
    }
    let spatial = conn.execute_batch("LOAD spatial;").is_ok()
        || conn.execute_batch("INSTALL spatial; LOAD spatial;").is_ok();

    // Impedir que el SQL de /query lea archivos del contenedor
    // (read_csv('/etc/...'), /proc/self/environ con la API key, etc.)
    // y que cambie la configuración.
    conn.execute_batch("SET enable_external_access = false; SET lock_configuration = true;")?;
    Ok((conn, spatial))
}
