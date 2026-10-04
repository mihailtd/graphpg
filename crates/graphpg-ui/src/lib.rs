//! Pre-built UI assets and axum routes for GraphiQL and Scalar OpenAPI references.

#[cfg(feature = "graphiql")]
mod graphiql {
    use axum::extract::Path;
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use rust_embed::RustEmbed;

    #[derive(RustEmbed)]
    #[folder = "assets/"]
    struct Assets;

    pub fn router<S>() -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        Router::new()
            .route("/graphiql", get(index_handler))
            .route("/graphiql/", get(index_handler))
            .route("/graphiql/{*path}", get(asset_handler))
    }

    async fn index_handler(headers: HeaderMap) -> Response {
        serve_asset("graphiql/index.html", &headers)
    }

    async fn asset_handler(Path(path): Path<String>, headers: HeaderMap) -> Response {
        let relative_path = format!("graphiql/{path}");
        serve_asset(&relative_path, &headers)
    }

    fn serve_asset(path: &str, headers: &HeaderMap) -> Response {
        let accepts_gzip = headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|val| val.to_str().ok())
            .map(|s| s.contains("gzip"))
            .unwrap_or(false);

        let filename = path.rsplit('/').next().unwrap_or(path);
        let is_hashed = filename.starts_with("graphiql.")
            && filename.contains('.')
            && !filename.ends_with(".html")
            && !filename.ends_with(".worker.js");

        let cache_control = if is_hashed {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };

        let content_type = guess_content_type(path);

        if accepts_gzip {
            let gz_path = format!("{path}.gz");
            if let Some(file) = Assets::get(&gz_path) {
                let etag = format_etag(&file.metadata.sha256_hash());
                if check_if_none_match(headers, &etag) {
                    return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
                }

                return (
                    [
                        (header::CONTENT_TYPE, content_type),
                        (header::CONTENT_ENCODING, "gzip"),
                        (header::CACHE_CONTROL, cache_control),
                        (header::ETAG, &etag[..]),
                    ],
                    file.data,
                )
                    .into_response();
            }
        }

        if let Some(file) = Assets::get(path) {
            let etag = format_etag(&file.metadata.sha256_hash());
            if check_if_none_match(headers, &etag) {
                return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
            }

            (
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::CACHE_CONTROL, cache_control),
                    (header::ETAG, &etag[..]),
                ],
                file.data,
            )
                .into_response()
        } else {
            StatusCode::NOT_FOUND.into_response()
        }
    }

    fn check_if_none_match(headers: &HeaderMap, etag: &str) -> bool {
        headers
            .get(header::IF_NONE_MATCH)
            .and_then(|val| val.to_str().ok())
            .map(|val| val == etag || val == "*")
            .unwrap_or(false)
    }

    fn format_etag(hash: &[u8; 32]) -> String {
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        format!("\"{hex}\"")
    }

    fn guess_content_type(path: &str) -> &'static str {
        if path.ends_with(".html") {
            "text/html; charset=utf-8"
        } else if path.ends_with(".js") || path.ends_with(".mjs") {
            "application/javascript; charset=utf-8"
        } else if path.ends_with(".css") {
            "text/css; charset=utf-8"
        } else if path.ends_with(".json") {
            "application/json; charset=utf-8"
        } else if path.ends_with(".txt") {
            "text/plain; charset=utf-8"
        } else {
            "application/octet-stream"
        }
    }
}

#[cfg(feature = "scalar")]
mod scalar {
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::{Html, IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;

    pub fn router<S>() -> Router<S>
    where
        S: Clone + Send + Sync + 'static,
    {
        Router::new()
            .route("/scalar", get(index_handler))
            .route("/scalar/", get(index_handler))
            .route("/scalar/scalar.js", get(js_handler))
    }

    async fn index_handler() -> Html<String> {
        let configuration = serde_json::json!({
            "url": "/openapi.json",
            "agent": { "disabled": true }
        });
        Html(scalar_api_reference::scalar_html(
            &configuration,
            Some("/scalar/scalar.js"),
        ))
    }

    async fn js_handler(headers: HeaderMap) -> Response {
        match scalar_api_reference::get_asset("scalar.js") {
            Some(content) => {
                let etag = format!("\"scalar-js-{}\"", content.len());
                if let Some(req_etag) = headers.get(header::IF_NONE_MATCH) {
                    if req_etag.as_bytes() == etag.as_bytes() || req_etag.as_bytes() == b"*" {
                        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
                    }
                }
                (
                    [
                        (
                            header::CONTENT_TYPE,
                            "application/javascript; charset=utf-8",
                        ),
                        (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
                        (header::ETAG, &etag[..]),
                    ],
                    content,
                )
                    .into_response()
            }
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }
}

#[cfg(feature = "graphiql")]
pub fn graphiql_router<S>() -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    graphiql::router()
}

#[cfg(feature = "scalar")]
pub fn scalar_router<S>() -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    scalar::router()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_graphiql_routes() {
        let app = graphiql_router();

        // 1. GET /graphiql
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/graphiql")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let etag = res
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // 2. GET /graphiql with If-None-Match returns 304
        let res_304 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/graphiql")
                    .header(header::IF_NONE_MATCH, &etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res_304.status(), StatusCode::NOT_MODIFIED);

        // 3. GET /graphiql with Accept-Encoding: gzip returns gzip
        let res_gz = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/graphiql")
                    .header(header::ACCEPT_ENCODING, "gzip, deflate, br")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res_gz.status(), StatusCode::OK);
        assert_eq!(
            res_gz.headers().get(header::CONTENT_ENCODING).unwrap(),
            "gzip"
        );

        // 4. GET /graphiql/editor.worker.js
        let res_worker = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/graphiql/editor.worker.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res_worker.status(), StatusCode::OK);
        assert_eq!(
            res_worker.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/javascript; charset=utf-8"
        );

        // 5. 404 for nonexistent asset
        let res_404 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/graphiql/nonexistent.xyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res_404.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_scalar_routes() {
        let app = scalar_router();

        // 1. GET /scalar
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/scalar")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html"));
        let body = res.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("/scalar/scalar.js"));
        assert!(html.contains("/openapi.json"));
        assert!(html.contains("\"disabled\":true") || html.contains("\"disabled\": true"));

        // 2. GET /scalar/scalar.js
        let res_js = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/scalar/scalar.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res_js.status(), StatusCode::OK);
        assert_eq!(
            res_js.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/javascript; charset=utf-8"
        );
        let etag = res_js
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        // 3. GET /scalar/scalar.js with If-None-Match returns 304
        let res_304 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/scalar/scalar.js")
                    .header(header::IF_NONE_MATCH, &etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res_304.status(), StatusCode::NOT_MODIFIED);
    }
}
