//! Thin wrapper around a shared `reqwest::Client` used by the cache scheme
//! handler when the cache misses or the URL is on the ignore list.

use std::sync::OnceLock;

/// Conditional request headers stripped before forwarding upstream.
///
/// Why: WebKit's HTTP cache adds these (e.g. `If-None-Match`,
/// `If-Modified-Since`) on its own when revisiting a URL. If we forward them
/// verbatim, the upstream server can answer `304 Not Modified` with no body,
/// but our cache layer treats non-2xx as a bypass — which surfaces as
/// `didFailWithError` to the webview and renders a blank page. We don't
/// support conditional GETs in the cache (todo §14), so the safest fix is to
/// strip these headers and always ask for a full 200 response.
const STRIPPED_REQUEST_HEADERS: &[&str] = &[
    "if-match",
    "if-none-match",
    "if-modified-since",
    "if-unmodified-since",
    "if-range",
];

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// Shared `reqwest::Client` configured with redirect-follow (limit 10) and
/// the default rustls TLS stack. No cookie jar, no custom user-agent (kept
/// transparent so the upstream server sees an unmodified request).
pub fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("failed to build reqwest client")
    })
}

/// GET a URL forwarding the supplied request headers (e.g. `Range`, `Accept`).
///
/// Conditional headers (see [`STRIPPED_REQUEST_HEADERS`]) are removed before
/// the request goes upstream so we never receive a `304 Not Modified` we
/// can't satisfy.
pub async fn fetch(
    url: &str,
    request_headers: &http::HeaderMap,
) -> reqwest::Result<reqwest::Response> {
    let mut clean_headers = request_headers.clone();
    for name in STRIPPED_REQUEST_HEADERS {
        // http::HeaderMap::remove is case-insensitive on the input name.
        clean_headers.remove(*name);
    }

    client().get(url).headers(clean_headers).send().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forwards_headers_and_returns_body() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/hello")
            .match_header("range", "bytes=0-9")
            .with_status(200)
            .with_header("content-type", "text/plain")
            .with_body("hello-body")
            .create_async()
            .await;

        let url = format!("{}/hello", server.url());
        let mut headers = http::HeaderMap::new();
        headers.insert("range", "bytes=0-9".parse().unwrap());

        let resp = fetch(&url, &headers).await.expect("fetch ok");
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .map(|v| v.to_str().unwrap()),
            Some("text/plain"),
        );
        let bytes = resp.bytes().await.unwrap();
        assert_eq!(&bytes[..], b"hello-body");

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn strips_conditional_headers() {
        let mut server = mockito::Server::new_async().await;
        // The mock only matches when the conditional headers are *absent*.
        // mockito's `Matcher::Missing` enforces that the request did not
        // carry the header upstream.
        let mock = server
            .mock("GET", "/asset")
            .match_header("if-none-match", mockito::Matcher::Missing)
            .match_header("if-modified-since", mockito::Matcher::Missing)
            .match_header("if-match", mockito::Matcher::Missing)
            .match_header("if-unmodified-since", mockito::Matcher::Missing)
            .match_header("if-range", mockito::Matcher::Missing)
            // Non-conditional headers must still be forwarded.
            .match_header("accept", "*/*")
            .with_status(200)
            .with_body("ok")
            .create_async()
            .await;

        let url = format!("{}/asset", server.url());
        let mut headers = http::HeaderMap::new();
        headers.insert("If-None-Match", "\"abc\"".parse().unwrap());
        headers.insert(
            "If-Modified-Since",
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        headers.insert("If-Match", "\"xyz\"".parse().unwrap());
        headers.insert(
            "If-Unmodified-Since",
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        headers.insert("If-Range", "\"abc\"".parse().unwrap());
        headers.insert("Accept", "*/*".parse().unwrap());

        let resp = fetch(&url, &headers).await.expect("fetch ok");
        assert_eq!(resp.status(), 200);

        mock.assert_async().await;
    }

    #[tokio::test]
    async fn client_is_singleton() {
        let a = client() as *const _;
        let b = client() as *const _;
        assert_eq!(a, b);
    }
}
