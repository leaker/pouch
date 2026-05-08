//! Thin wrapper around a shared `reqwest::Client` used by the cache scheme
//! handler when the cache misses or the URL is on the ignore list.

use std::sync::OnceLock;

use tracing::trace;

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

/// Shared `reqwest::Client` configured with redirect-follow (limit 10),
/// the default rustls TLS stack, and an in-process cookie jar so multiple
/// hook'd requests in the same session share Set-Cookie state. No custom
/// user-agent (kept transparent so the upstream server sees an unmodified
/// request).
///
/// **Cookie isolation caveat**: the webview maintains its own cookie store
/// (NSHTTPCookieStorage on macOS, the WebView2 cookie manager on Windows)
/// which is **not** synchronised with this jar. The reqwest jar mitigates
/// the simplest case — sequential reqwest requests on the same upstream
/// origin — but cookies set in the webview are not visible here, and vice
/// versa. Combined with response Set-Cookie forwarding (see the platform
/// `deliver_respond` paths), the webview's own jar still receives the
/// upstream Set-Cookie via the headers we forward, so subsequent webview
/// requests carry it naturally.
pub fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .cookie_store(true)
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

    // [CORS-DEBUG] Dump the headers we are about to send upstream. This is
    // the critical hypothesis-discriminator: if `Origin` is missing here
    // (whether because WKWebView never gave it to us, or because it was
    // dropped along the way) the upstream CDN will not echo back
    // Access-Control-Allow-Origin and the webview's CORS check will fail.
    //
    // Grep:
    //   outgoing_request_headers url=
    //   outgoing_origin=
    trace!(
        target: "hook",
        "outgoing_request_headers url={} count={}",
        url,
        clean_headers.len()
    );
    for (name, value) in clean_headers.iter() {
        trace!(
            target: "hook",
            "  outgoing_header url={} {}={}",
            url,
            name.as_str(),
            value.to_str().unwrap_or("<non-ascii>")
        );
    }
    let outgoing_origin = clean_headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<absent>");
    let outgoing_referer = clean_headers
        .get("referer")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<absent>");
    trace!(
        target: "hook",
        "fetch_outgoing url={} outgoing_origin={} outgoing_referer={}",
        url, outgoing_origin, outgoing_referer
    );

    let response = client()
        .get(url)
        .headers(clean_headers)
        .header("X-Hook-Bypass", "1")
        .send()
        .await?;

    // [CORS-DEBUG] Dump every response header from upstream. If
    // `Access-Control-Allow-Origin` is missing here, the problem is on
    // the upstream side (most likely H1 — origin not echoed because we
    // didn't send it). If it is present, the problem is downstream
    // (forward_response_headers blacklist or NSHTTPURLResponse delivery).
    //
    // Grep:
    //   upstream_response_headers url=
    //   upstream_acao=
    //   upstream_vary=
    let resp_headers = response.headers();
    trace!(
        target: "hook",
        "upstream_response_headers url={} status={} count={}",
        url,
        response.status(),
        resp_headers.len()
    );
    for (name, value) in resp_headers.iter() {
        trace!(
            target: "hook",
            "  upstream_header url={} {}={}",
            url,
            name.as_str(),
            value.to_str().unwrap_or("<non-ascii>")
        );
    }
    let upstream_acao = resp_headers
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<absent>");
    let upstream_vary = resp_headers
        .get("vary")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<absent>");
    let upstream_acac = resp_headers
        .get("access-control-allow-credentials")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<absent>");
    trace!(
        target: "hook",
        "fetch_response url={} status={} upstream_acao={} upstream_vary={} upstream_acac={}",
        url,
        response.status(),
        upstream_acao,
        upstream_vary,
        upstream_acac
    );

    Ok(response)
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
