//! Error único de la API. Siempre responde `{"error": "...", "index": n?}`.

use axum::{
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    /// Posición del elemento con problema en una creación masiva.
    pub index: Option<usize>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            index: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn unprocessable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    /// Marca el error con la posición del elemento (creación masiva).
    pub fn at(mut self, index: usize) -> Self {
        self.index = Some(index);
        self
    }

    /// Convierte el rechazo de axum al leer el JSON (mal formado, tipos incorrectos,
    /// campos desconocidos o cuerpo demasiado grande) a nuestro formato.
    pub fn from_rejection(rejection: JsonRejection) -> Self {
        let status = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            StatusCode::PAYLOAD_TOO_LARGE
        } else {
            StatusCode::BAD_REQUEST
        };
        Self::new(status, rejection.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = match self.index {
            Some(i) => json!({ "error": self.message, "index": i }),
            None => json!({ "error": self.message }),
        };
        (self.status, Json(body)).into_response()
    }
}

impl From<duckdb::Error> for ApiError {
    fn from(e: duckdb::Error) -> Self {
        let message = e.to_string();
        // Duplicados (UNIQUE) o instrumento inexistente (FOREIGN KEY).
        let status = if message.contains("Constraint Error") {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        };
        Self::new(status, message)
    }
}
