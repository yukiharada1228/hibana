//! Bound receive/dispatch work before allocating request bodies. These local
//! limits protect each CP; PostgreSQL still enforces fleet execution quotas.
use axum::{
    body::{Body, Bytes, HttpBody},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use hibana_shared::http::MAX_REQUEST_BYTES;
use std::time::Duration;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) async fn read(body: Body) -> Result<Bytes, Box<Response>> {
    if body.size_hint().lower() > MAX_REQUEST_BYTES as u64 {
        return Err(Box::new(
            (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
        ));
    }
    match tokio::time::timeout(
        RECEIVE_TIMEOUT,
        axum::body::to_bytes(body, MAX_REQUEST_BYTES),
    )
    .await
    {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) => {
            use std::error::Error as _;
            let too_large = error
                .source()
                .is_some_and(|error| error.is::<http_body_util::LengthLimitError>());
            Err(Box::new(
                if too_large {
                    (StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
                } else {
                    (StatusCode::BAD_REQUEST, "invalid request body")
                }
                .into_response(),
            ))
        }
        Err(_) => Err(Box::new(
            (StatusCode::REQUEST_TIMEOUT, "request body receive timeout").into_response(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn incomplete_bodies_timeout_and_oversized_bodies_are_rejected() {
        let pending =
            Body::from_stream(futures::stream::pending::<Result<Bytes, std::io::Error>>());
        let started = tokio::time::Instant::now();
        assert_eq!(
            read(pending).await.unwrap_err().status(),
            StatusCode::REQUEST_TIMEOUT
        );
        assert_eq!(started.elapsed(), RECEIVE_TIMEOUT);
        assert_eq!(
            read(Body::from(vec![0; MAX_REQUEST_BYTES + 1]))
                .await
                .unwrap_err()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let streamed = Body::from_stream(futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from(vec![0; MAX_REQUEST_BYTES])),
            Ok(Bytes::from_static(b"x")),
        ]));
        assert_eq!(
            read(streamed).await.unwrap_err().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let broken = Body::from_stream(futures::stream::iter([Err::<Bytes, _>(
            std::io::Error::other("fixture"),
        )]));
        assert_eq!(
            read(broken).await.unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(read(Body::from("ok")).await.unwrap(), "ok");
    }
}
