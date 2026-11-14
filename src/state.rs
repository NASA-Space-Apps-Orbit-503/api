//! Estado compartido y ejecución de trabajo de DuckDB.

use std::sync::{Arc, Mutex};

use duckdb::Connection;

use crate::{config::Config, error::ApiError};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Arc<Mutex<Connection>>,
    /// Si la extensión `spatial` de DuckDB está disponible (filtros por polígono).
    pub spatial: bool,
    /// Cliente HTTP para FIRMS.
    pub http: reqwest::Client,
}

impl AppState {
    /// Corre trabajo de DuckDB (bloqueante) fuera del runtime async.
    /// Cada petición usa un clon de la conexión: todos comparten la misma base,
    /// así que varias peticiones pueden correr en paralelo.
    pub async fn run_db<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = db.lock().unwrap_or_else(|e| e.into_inner()).try_clone()?;
            f(&mut conn)
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
    }
}
