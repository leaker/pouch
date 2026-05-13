//! WebSocket upgrade detection shared by the macOS MITM handler and the
//! Windows WebView2 interceptor.

use http::{header, HeaderMap};

/// Return true when request headers have WebSocket opening-handshake
/// characteristics.
///
/// The primary signal is the HTTP/1.1 upgrade pair:
/// `Upgrade: websocket` plus a `Connection` token named `Upgrade`.
/// Some platform layers may expose partial upgrade headers, so the paired
/// `Sec-WebSocket-Key` and `Sec-WebSocket-Version` headers are accepted as a
/// conservative fallback. This deliberately does not inspect host, port, URL,
/// or site-specific config.
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let has_upgrade_websocket = header_values_contain_token(headers, header::UPGRADE, "websocket");
    let has_connection_upgrade =
        header_values_contain_token(headers, header::CONNECTION, "upgrade");
    let has_sec_websocket_pair =
        headers.contains_key("sec-websocket-key") && headers.contains_key("sec-websocket-version");

    (has_upgrade_websocket && has_connection_upgrade) || has_sec_websocket_pair
}

fn header_values_contain_token(
    headers: &HeaderMap,
    name: impl http::header::AsHeaderName,
    expected: &str,
) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().ok().is_some_and(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|token| token.eq_ignore_ascii_case(expected))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::is_websocket_upgrade;
    use http::HeaderMap;

    #[test]
    fn detects_case_insensitive_upgrade_header() {
        let mut headers = HeaderMap::new();
        headers.insert("Upgrade", "WebSocket".parse().unwrap());
        headers.insert("Connection", "Upgrade".parse().unwrap());

        assert!(is_websocket_upgrade(&headers));
    }

    #[test]
    fn detects_connection_multi_token_upgrade() {
        let mut headers = HeaderMap::new();
        headers.insert("upgrade", "websocket".parse().unwrap());
        headers.insert("connection", "keep-alive, Upgrade".parse().unwrap());

        assert!(is_websocket_upgrade(&headers));
    }

    #[test]
    fn detects_sec_websocket_fallback() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "sec-websocket-key",
            "dGhlIHNhbXBsZSBub25jZQ==".parse().unwrap(),
        );
        headers.insert("sec-websocket-version", "13".parse().unwrap());

        assert!(is_websocket_upgrade(&headers));
    }

    #[test]
    fn ordinary_get_headers_do_not_match() {
        let mut headers = HeaderMap::new();
        headers.insert("accept", "text/html".parse().unwrap());
        headers.insert("connection", "keep-alive".parse().unwrap());

        assert!(!is_websocket_upgrade(&headers));
    }

    #[test]
    fn upgrade_without_connection_token_does_not_match() {
        let mut headers = HeaderMap::new();
        headers.insert("upgrade", "websocket".parse().unwrap());
        headers.insert("connection", "keep-alive".parse().unwrap());

        assert!(!is_websocket_upgrade(&headers));
    }
}
