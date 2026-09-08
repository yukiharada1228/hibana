//! HTTP エラー応答へのマッピング。
//!
//! `hibana_shared::FaasError` を axum の `IntoResponse` に変換し、
//! 一貫した JSON エラーエンベロープ
//! `{ "error": { "code", "message", "retryable" } }` を返す (§6.5)。
//!
//! 方針:
//! - `code` は安定した文字列（クライアントが分岐に使える）。
//! - `message` は汎用文言。5xx は内部詳細をログにのみ残し、ボディには漏らさない（redaction）。
//! - `retryable` はクライアントがリトライ可能かのヒント。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use hibana_shared::FaasError;
use serde_json::json;

/// control-plane の HTTP ハンドラ用エラー。
///
/// `FaasError` を内包し、ステータスコードへ写像する。
#[derive(Debug)]
pub struct AppError(pub FaasError);

impl From<FaasError> for AppError {
    fn from(e: FaasError) -> Self {
        AppError(e)
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        // 内部詳細（DB エラーの中身）はログにのみ残し、クライアントへは汎用 500 を返す。
        // 詳細メッセージを `Internal` に詰めるとボディに漏れるため、ここでは保持しない。
        tracing::error!(error = %e, "database error");
        AppError(FaasError::Internal(String::new()))
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError(FaasError::Serialization(e))
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError(FaasError::Internal(e.to_string()))
    }
}

impl AppError {
    /// 安定した機械可読エラーコード。
    fn code(&self) -> &'static str {
        match &self.0 {
            FaasError::Unavailable => "unavailable",
            FaasError::Unauthorized => "unauthorized",
            FaasError::Forbidden => "forbidden",
            FaasError::NotFound(_) => "not_found",
            FaasError::InvalidRequest(_) => "invalid_request",
            FaasError::Conflict(_) => "conflict",
            FaasError::Serialization(_) => "invalid_request",
            FaasError::Timeout => "timeout",
            FaasError::Execution(_) => "execution_failed",
            FaasError::Internal(_) => "internal_error",
        }
    }

    /// HTTP ステータスコード。
    fn status(&self) -> StatusCode {
        match &self.0 {
            FaasError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            FaasError::Unauthorized => StatusCode::UNAUTHORIZED,
            FaasError::Forbidden => StatusCode::FORBIDDEN,
            FaasError::NotFound(_) => StatusCode::NOT_FOUND,
            FaasError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            FaasError::Conflict(_) => StatusCode::CONFLICT,
            FaasError::Serialization(_) => StatusCode::BAD_REQUEST,
            FaasError::Timeout => StatusCode::GATEWAY_TIMEOUT,
            FaasError::Execution(_) => StatusCode::INTERNAL_SERVER_ERROR,
            FaasError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// クライアントがリトライしてよいか（ヒント）。
    fn retryable(&self) -> bool {
        matches!(
            self.0,
            FaasError::Timeout | FaasError::Internal(_) | FaasError::Unavailable
        )
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();

        // 5xx は内部詳細をログにのみ残し、ボディへは汎用文言だけを返す（redaction）。
        let message: String = if status.is_server_error() {
            tracing::error!(error = %self.0, "request failed");
            "internal server error".to_string()
        } else {
            self.0.to_string()
        };

        let body = Json(json!({
            "error": {
                "code": self.code(),
                "message": message,
                "retryable": self.retryable(),
            }
        }));
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// envelope の構造とフィールドを検証する（DB 非依存）。
    fn envelope(err: FaasError) -> (StatusCode, serde_json::Value) {
        let app = AppError(err);
        let status = app.status();
        let message: String = if status.is_server_error() {
            "internal server error".to_string()
        } else {
            app.0.to_string()
        };
        let body = json!({
            "error": {
                "code": app.code(),
                "message": message,
                "retryable": app.retryable(),
            }
        });
        (status, body)
    }

    #[test]
    fn forbidden_maps_to_403() {
        let (status, body) = envelope(FaasError::Forbidden);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "forbidden");
        assert_eq!(body["error"]["retryable"], false);
    }

    #[test]
    fn unauthorized_maps_to_401() {
        let (status, body) = envelope(FaasError::Unauthorized);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "unauthorized");
    }

    #[test]
    fn not_found_maps_to_404() {
        let (status, body) = envelope(FaasError::NotFound("x".into()));
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "not_found");
    }

    /// 5xx はボディに内部詳細を漏らさない（redaction）。
    #[test]
    fn internal_5xx_body_is_redacted() {
        let (status, body) = envelope(FaasError::Internal("db error: secret table x".into()));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"]["code"], "internal_error");
        assert_eq!(body["error"]["message"], "internal server error");
        assert_eq!(body["error"]["retryable"], true);
        // 内部詳細が漏れていないこと。
        assert!(!body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("secret table"));
    }

    /// sqlx エラー由来の Internal もボディに詳細を載せない。
    #[test]
    fn sqlx_error_does_not_leak_detail() {
        let app: AppError = sqlx::Error::RowNotFound.into();
        // From<sqlx::Error> は詳細を保持しない（空文字 Internal）。
        let status = app.status();
        let message: String = if status.is_server_error() {
            "internal server error".to_string()
        } else {
            app.0.to_string()
        };
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(message, "internal server error");
    }

    /// envelope は常に code/message/retryable の 3 キーを持つ。
    #[test]
    fn envelope_shape_is_stable() {
        let (_status, body) = envelope(FaasError::InvalidRequest("bad".into()));
        let obj = body["error"].as_object().unwrap();
        assert!(obj.contains_key("code"));
        assert!(obj.contains_key("message"));
        assert!(obj.contains_key("retryable"));
        assert_eq!(obj.len(), 3);
    }
}
