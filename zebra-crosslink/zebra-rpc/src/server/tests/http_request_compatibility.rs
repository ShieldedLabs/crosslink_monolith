//! Tests for the HTTP request compatibility middleware.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use http_body::Frame;
use jsonrpsee::{
    core::BoxError,
    server::{HttpBody, HttpRequest, HttpResponse},
};
use tower::Service;

use crate::server::http_request_compatibility::HttpRequestMiddleware;

/// A body that always returns an error, simulating a TCP RST during body collection.
struct ErrorBody;

impl http_body::Body for ErrorBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(Some(Err("connection reset".into())))
    }
}

/// A mock inner service that returns a minimal JSON-RPC 2.0 response.
#[derive(Clone)]
struct MockRpcService;

impl Service<HttpRequest> for MockRpcService {
    type Response = HttpResponse;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, BoxError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: HttpRequest) -> Self::Future {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let response = HttpResponse::new(HttpBody::from(body.to_string()));
        Box::pin(async { Ok(response) })
    }
}

/// Verifies that body collection errors return `Err` instead of panicking.
///
/// Previously, the middleware called `.expect()` on `body.collect().await`,
/// so a TCP RST during body reading would panic the process.
#[tokio::test]
async fn request_body_error_returns_err_instead_of_panic() {
    let error_body = HttpBody::new(ErrorBody);
    let request = HttpRequest::builder()
        .method("POST")
        .header("content-type", "appliion/json")
        .body(error_body)
        .expect("valid request");

    let mut middleware = HttpRequestMiddleware::new(MockRpcService, None, 2_097_152);
    let result = middleware.call(request).await;

    assert!(
        result.is_err(),
        "body collection error should return Err, not panic"
    );
}

/// Verifies that a request body exceeding `max_request_body_size` is rejected.
#[tokio::test]
async fn oversized_request_body_is_rejected() {
    let limit = 64;
    let oversized = vec![b'x'; limit + 1];
    let body = HttpBody::from(oversized);
    let request = HttpRequest::builder()
        .method("POST")
        .header("content-type", "application/json")
        .body(body)
        .expect("valid request");

    let mut middleware = HttpRequestMiddleware::new(MockRpcService, None, limit);
    let result = middleware.call(request).await;

    assert!(result.is_err(), "oversized request body should be rejected");
}

/// A request without the auth cookie, or with a wrong one, gets an HTTP 401 with a
/// `WWW-Authenticate` challenge and a JSON-RPC error, not a dropped connection (which clients
/// report as "empty reply from server").
#[tokio::test]
async fn unauthenticated_request_gets_401_not_an_empty_reply() {
    use crate::server::cookie::Cookie;
    use http_body_util::BodyExt;

    let cookie = Cookie::default();
    for auth in [None, Some("Basic X19jb29raWVfXzp3cm9uZw==")] {
        let mut builder = HttpRequest::builder()
            .method("POST")
            .header("content-type", "application/json");
        if let Some(auth) = auth {
            builder = builder.header("authorization", auth);
        }
        let request = builder
            .body(HttpBody::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":[]}"#.to_string(),
            ))
            .expect("valid request");

        let mut middleware = HttpRequestMiddleware::new(MockRpcService, Some(cookie.clone()), 2_097_152);
        let response = middleware
            .call(request)
            .await
            .expect("an unauthenticated request is answered, not turned into a transport error");

        assert_eq!(response.status(), hyper::StatusCode::UNAUTHORIZED, "auth header {auth:?}");
        assert!(
            response.headers().contains_key(hyper::header::WWW_AUTHENTICATE),
            "a 401 carries a challenge"
        );
        let body = response.into_body().collect().await.expect("body").to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("JSON-RPC error body");
        assert_eq!(json["error"]["code"], 401);
        assert!(json["error"]["message"].as_str().unwrap().contains(".cookie"));
    }
}
