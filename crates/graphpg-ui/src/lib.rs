//! The web UIs of graphpg, embedded in the binary: GraphiQL for the GraphQL API and the Scalar
//! API reference for the OpenAPI description. Both work offline and need no third-party hosts
//! (so a strict Content-Security-Policy can allow `'self'` only).
//!
//! GraphiQL is built from `packages/graphiql-ui` (`moon run graphiql-ui:build`) into
//! `assets/graphiql/`, which this crate embeds. Every file is stored gzip-compressed: clients
//! that accept gzip get the stored bytes, the rest get them inflated.

#[cfg(feature = "graphiql")]
mod graphiql {
    use axum::extract::Path;
    use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use flate2::read::GzDecoder;
    use rust_embed::RustEmbed;
    use std::io::Read;

    /// `assets/` at compile time; absent until the UI was built (see [`unbuilt`]).
    #[derive(RustEmbed)]
    #[folder = "assets/"]
    #[allow_missing = true]
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
        serve("index.html", &headers)
    }

    async fn asset_handler(Path(path): Path<String>, headers: HeaderMap) -> Response {
        serve(&path, &headers)
    }

    /// What the page says when the binary was built without the UI.
    fn unbuilt() -> Response {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "The GraphiQL assets are not part of this build. Run `moon run graphiql-ui:build`, \
             then rebuild graphpg.\n",
        )
            .into_response()
    }

    fn serve(path: &str, headers: &HeaderMap) -> Response {
        let Some(file) = Assets::get(&format!("graphiql/{path}.gz")) else {
            return if Assets::get("graphiql/index.html.gz").is_none() {
                unbuilt()
            } else {
                StatusCode::NOT_FOUND.into_response()
            };
        };

        let gzip = accepts_gzip(headers);
        // The two representations have different bytes, so different validators
        let hash = hex(&file.metadata.sha256_hash());
        let etag = if gzip {
            format!("\"{hash}\"")
        } else {
            format!("\"{hash}-identity\"")
        };
        let cache_control = if is_hashed(path) {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        let common = [
            (header::ETAG, header_value(&etag)),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static(cache_control),
            ),
            (header::VARY, HeaderValue::from_static("Accept-Encoding")),
        ];

        if if_none_match(headers, &etag) {
            return (StatusCode::NOT_MODIFIED, common).into_response();
        }

        let content_type = (
            header::CONTENT_TYPE,
            HeaderValue::from_static(content_type(path)),
        );
        if gzip {
            let encoding = (header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            return (common, [content_type, encoding], file.data).into_response();
        }
        let mut inflated = Vec::new();
        match GzDecoder::new(file.data.as_ref()).read_to_end(&mut inflated) {
            Ok(_) => (common, [content_type], inflated).into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }

    fn header_value(value: &str) -> HeaderValue {
        HeaderValue::from_str(value).expect("an ETag is ASCII")
    }

    fn hex(hash: &[u8; 32]) -> String {
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Whether the client accepts gzip (RFC 9110 §12.5.3): `gzip` or `*` with a quality above
    /// zero, an explicit `gzip` winning over `*`.
    pub(super) fn accepts_gzip(headers: &HeaderMap) -> bool {
        let Some(value) = headers
            .get(header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok())
        else {
            return false;
        };
        let mut gzip = None;
        let mut any = None;
        for item in value.split(',') {
            let mut parts = item.split(';');
            let coding = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
            let quality = parts
                .filter_map(|p| p.trim().strip_prefix("q="))
                .next()
                .and_then(|q| q.trim().parse::<f32>().ok())
                .unwrap_or(1.0);
            match coding.as_str() {
                "gzip" | "x-gzip" => gzip = Some(quality > 0.0),
                "*" => any = Some(quality > 0.0),
                _ => {}
            }
        }
        gzip.or(any).unwrap_or(false)
    }

    fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
        headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == "*" || v.split(',').any(|candidate| candidate.trim() == etag))
    }

    /// `graphiql.<HASH>.js` and friends: the name carries a hash of the content, so the file
    /// never changes under its name.
    pub(super) fn is_hashed(path: &str) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        let mut parts = name.split('.');
        parts.next() == Some("graphiql")
            && parts.next().is_some_and(|hash| {
                hash.len() == 8
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            })
    }

    fn content_type(path: &str) -> &'static str {
        match path.rsplit('.').next() {
            Some("html") => "text/html; charset=utf-8",
            Some("js" | "mjs") => "application/javascript; charset=utf-8",
            Some("css") => "text/css; charset=utf-8",
            Some("json") => "application/json; charset=utf-8",
            Some("txt") => "text/plain; charset=utf-8",
            _ => "application/octet-stream",
        }
    }
}

#[cfg(feature = "scalar")]
mod scalar {
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::{Html, IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;
    use std::hash::{Hash, Hasher};
    use std::sync::OnceLock;

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

    /// A validator derived from the bytes, so it changes whenever the bundle does.
    fn etag(content: &[u8]) -> &'static str {
        static ETAG: OnceLock<String> = OnceLock::new();
        ETAG.get_or_init(|| {
            let mut hasher = std::hash::DefaultHasher::new();
            content.hash(&mut hasher);
            format!("\"scalar-{:016x}\"", hasher.finish())
        })
    }

    async fn js_handler(headers: HeaderMap) -> Response {
        let Some(content) = scalar_api_reference::get_asset("scalar.js") else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let etag = etag(&content);
        let cache = (header::CACHE_CONTROL, "public, max-age=31536000, immutable");
        let unchanged = headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == "*" || v.split(',').any(|c| c.trim() == etag));
        if unchanged {
            return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag), cache]).into_response();
        }
        (
            [
                (
                    header::CONTENT_TYPE,
                    "application/javascript; charset=utf-8",
                ),
                (header::ETAG, etag),
                cache,
            ],
            content,
        )
            .into_response()
    }
}

/// Routes of the GraphiQL IDE: `/graphiql` and the assets below it.
#[cfg(feature = "graphiql")]
pub fn graphiql_router<S>() -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    graphiql::router()
}

/// Routes of the Scalar API reference: `/scalar`, showing the OpenAPI document at
/// `/openapi.json`.
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
    use axum::response::Response;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn get(app: &Router, uri: &str, headers: &[(&str, &str)]) -> Response {
        let mut request = Request::builder().uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        app.clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn body(response: Response) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    fn header_of(response: &Response, name: header::HeaderName) -> &str {
        response.headers().get(name).unwrap().to_str().unwrap()
    }

    #[cfg(feature = "graphiql")]
    mod graphiql {
        use super::*;
        use crate::graphiql::{accepts_gzip, is_hashed};
        use axum::http::HeaderMap;

        fn accept(value: &str) -> bool {
            let mut headers = HeaderMap::new();
            headers.insert(header::ACCEPT_ENCODING, value.parse().unwrap());
            accepts_gzip(&headers)
        }

        #[test]
        fn accept_encoding_follows_rfc_9110() {
            assert!(!accepts_gzip(&HeaderMap::new()));
            assert!(accept("gzip"));
            assert!(accept("deflate, gzip;q=0.5, br"));
            assert!(accept("*"));
            assert!(!accept("gzip;q=0"));
            assert!(!accept("gzip; q=0.0, *"), "an explicit gzip beats *");
            assert!(accept("gzip;q=0.1"));
            assert!(!accept("br, deflate"));
            assert!(!accept("*;q=0"));
        }

        #[test]
        fn only_content_hashed_names_are_immutable() {
            assert!(is_hashed("graphiql.TAM6DLTY.js"));
            assert!(is_hashed("graphiql.OSW2YSMT.css.LEGAL.txt"));
            assert!(!is_hashed("index.html"));
            assert!(!is_hashed("editor.worker.js"));
            assert!(!is_hashed("graphiql.js"));
            assert!(!is_hashed("graphiql.short.js"));
            assert!(!is_hashed("manifest.json"));
        }

        #[tokio::test]
        async fn serves_the_page_and_its_assets() {
            let app = graphiql_router();

            let page = get(&app, "/graphiql", &[]).await;
            assert_eq!(page.status(), StatusCode::OK);
            assert_eq!(
                header_of(&page, header::CONTENT_TYPE),
                "text/html; charset=utf-8"
            );
            assert_eq!(header_of(&page, header::CACHE_CONTROL), "no-cache");
            assert_eq!(header_of(&page, header::VARY), "Accept-Encoding");
            let html = String::from_utf8(body(page).await).unwrap();
            assert!(!html.contains("https://"), "no external hosts: {html}");

            // The page names its hashed script and style; both are served, immutable
            for (needle, content_type) in [
                (".js\"", "application/javascript; charset=utf-8"),
                (".css\"", "text/css; charset=utf-8"),
            ] {
                let end = html.find(needle).unwrap() + needle.len() - 1;
                let start = html[..end].rfind("/graphiql/").unwrap();
                let uri = &html[start..end];
                let asset = get(&app, uri, &[]).await;
                assert_eq!(asset.status(), StatusCode::OK, "{uri}");
                assert_eq!(header_of(&asset, header::CONTENT_TYPE), content_type);
                assert!(header_of(&asset, header::CACHE_CONTROL).contains("immutable"));
            }

            let worker = get(&app, "/graphiql/editor.worker.js", &[]).await;
            assert_eq!(worker.status(), StatusCode::OK);
            assert_eq!(header_of(&worker, header::CACHE_CONTROL), "no-cache");
        }

        #[tokio::test]
        async fn negotiates_gzip_and_validates_the_cache() {
            let app = graphiql_router();
            let uri = "/graphiql/editor.worker.js";

            let identity = get(&app, uri, &[]).await;
            assert!(identity.headers().get(header::CONTENT_ENCODING).is_none());
            assert_eq!(header_of(&identity, header::VARY), "Accept-Encoding");
            let identity_etag = header_of(&identity, header::ETAG).to_string();
            let plain = body(identity).await;
            assert!(!plain.is_empty());

            let gzip = get(&app, uri, &[("accept-encoding", "gzip, deflate, br")]).await;
            assert_eq!(header_of(&gzip, header::CONTENT_ENCODING), "gzip");
            let gzip_etag = header_of(&gzip, header::ETAG).to_string();
            let compressed = body(gzip).await;
            assert!(compressed.len() < plain.len());
            assert_ne!(
                gzip_etag, identity_etag,
                "different bytes, different validators"
            );

            // gzip;q=0 refuses gzip
            let refused = get(&app, uri, &[("accept-encoding", "gzip;q=0")]).await;
            assert!(refused.headers().get(header::CONTENT_ENCODING).is_none());
            assert_eq!(body(refused).await, plain);

            // Each validator revalidates its own representation
            let unchanged = get(
                &app,
                uri,
                &[("accept-encoding", "gzip"), ("if-none-match", &gzip_etag)],
            )
            .await;
            assert_eq!(unchanged.status(), StatusCode::NOT_MODIFIED);
            assert_eq!(header_of(&unchanged, header::VARY), "Accept-Encoding");
            let stale = get(&app, uri, &[("if-none-match", &gzip_etag)]).await;
            assert_eq!(
                stale.status(),
                StatusCode::OK,
                "a gzip validator is not the identity's"
            );
        }

        #[tokio::test]
        async fn hides_what_is_not_a_public_asset() {
            let app = graphiql_router();
            for uri in [
                "/graphiql/nonexistent.xyz",
                "/graphiql/index.html.gz",
                "/graphiql/../Cargo.toml",
                "/graphiql/%2e%2e/Cargo.toml",
            ] {
                let response = get(&app, uri, &[]).await;
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
            }
        }
    }

    #[cfg(feature = "scalar")]
    mod scalar {
        use super::*;

        #[tokio::test]
        async fn serves_the_reference_and_its_script() {
            let app = scalar_router();

            let page = get(&app, "/scalar", &[]).await;
            assert_eq!(page.status(), StatusCode::OK);
            assert!(header_of(&page, header::CONTENT_TYPE).starts_with("text/html"));
            let html = String::from_utf8(body(page).await).unwrap();
            assert!(html.contains("/scalar/scalar.js"));
            assert!(html.contains("/openapi.json"));
            assert!(!html.contains("cdn.jsdelivr.net"), "offline: {html}");

            let script = get(&app, "/scalar/scalar.js", &[]).await;
            assert_eq!(script.status(), StatusCode::OK);
            assert_eq!(
                header_of(&script, header::CONTENT_TYPE),
                "application/javascript; charset=utf-8"
            );
            let etag = header_of(&script, header::ETAG).to_string();

            let unchanged = get(&app, "/scalar/scalar.js", &[("if-none-match", &etag)]).await;
            assert_eq!(unchanged.status(), StatusCode::NOT_MODIFIED);
        }
    }
}
