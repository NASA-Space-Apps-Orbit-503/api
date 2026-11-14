//! Una carpeta por entidad: modelo, validación, SQL y rutas viven juntos.

pub mod burning_activity_stats;
pub mod crud;
pub mod detection;
pub mod instrument;

use axum::Router;

use crate::state::AppState;

/// Rutas de todas las entidades.
pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(instrument::routes())
        .merge(detection::routes())
        .merge(burning_activity_stats::routes())
}
