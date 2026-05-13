//! Generic protocol and HTTP semantics that must bypass pouch's disk cache.

use http::{header, HeaderMap, StatusCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestBypassReason {
    SseAccept,
    Range,
    CacheControlNoCache,
    CacheControlNoStore,
}

impl RequestBypassReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SseAccept => "sse_accept",
            Self::Range => "range",
            Self::CacheControlNoCache => "cache_control_no_cache",
            Self::CacheControlNoStore => "cache_control_no_store",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseBypassReason {
    SseContentType,
    PartialContent,
}

impl ResponseBypassReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SseContentType => "sse_content_type",
            Self::PartialContent => "partial_content",
        }
    }
}

pub fn request_bypass_reason(headers: &HeaderMap) -> Option<RequestBypassReason> {
    if accepts_media_type(headers, "text/event-stream") {
        return Some(RequestBypassReason::SseAccept);
    }
    if headers.contains_key(header::RANGE) {
        return Some(RequestBypassReason::Range);
    }

    cache_control_bypass_reason(headers)
}

pub fn response_bypass_reason(
    status: StatusCode,
    headers: &HeaderMap,
) -> Option<ResponseBypassReason> {
    if status == StatusCode::PARTIAL_CONTENT {
        return Some(ResponseBypassReason::PartialContent);
    }
    if has_media_type(headers, header::CONTENT_TYPE, "text/event-stream") {
        return Some(ResponseBypassReason::SseContentType);
    }

    None
}

fn accepts_media_type(headers: &HeaderMap, expected: &str) -> bool {
    has_media_type(headers, header::ACCEPT, expected)
}

fn has_media_type(
    headers: &HeaderMap,
    name: impl http::header::AsHeaderName,
    expected: &str,
) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().ok().is_some_and(|value| {
            value.split(',').any(|part| {
                part.split(';')
                    .next()
                    .map(str::trim)
                    .is_some_and(|media_type| media_type.eq_ignore_ascii_case(expected))
            })
        })
    })
}

fn cache_control_bypass_reason(headers: &HeaderMap) -> Option<RequestBypassReason> {
    for value in headers.get_all(header::CACHE_CONTROL) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for token in value.split(',') {
            let directive = token
                .split_once('=')
                .map(|(name, _)| name)
                .unwrap_or(token)
                .trim();
            if directive.eq_ignore_ascii_case("no-cache") {
                return Some(RequestBypassReason::CacheControlNoCache);
            }
            if directive.eq_ignore_ascii_case("no-store") {
                return Some(RequestBypassReason::CacheControlNoStore);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{header, HeaderMap, HeaderValue};

    fn headers(items: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in items {
            headers.insert(
                http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn request_accept_text_event_stream_bypasses_case_insensitive_with_parameters() {
        let headers = headers(&[(
            "Accept",
            "application/json, Text/Event-Stream; charset=utf-8",
        )]);

        assert_eq!(
            request_bypass_reason(&headers),
            Some(RequestBypassReason::SseAccept)
        );
    }

    #[test]
    fn request_range_header_bypasses_cache() {
        let headers = headers(&[("Range", "bytes=0-1023")]);

        assert_eq!(
            request_bypass_reason(&headers),
            Some(RequestBypassReason::Range)
        );
    }

    #[test]
    fn request_cache_control_no_cache_bypasses_case_insensitive_multi_token() {
        let headers = headers(&[("Cache-Control", "max-age=0, No-Cache")]);

        assert_eq!(
            request_bypass_reason(&headers),
            Some(RequestBypassReason::CacheControlNoCache)
        );
    }

    #[test]
    fn request_cache_control_no_store_bypasses_case_insensitive_multi_token() {
        let headers = headers(&[("Cache-Control", "public, NO-STORE")]);

        assert_eq!(
            request_bypass_reason(&headers),
            Some(RequestBypassReason::CacheControlNoStore)
        );
    }

    #[test]
    fn request_without_generic_signals_does_not_bypass() {
        let headers = headers(&[("Accept", "text/html"), ("Cache-Control", "max-age=300")]);

        assert_eq!(request_bypass_reason(&headers), None);
    }

    #[test]
    fn response_text_event_stream_bypasses_with_parameters() {
        let headers = headers(&[("Content-Type", "Text/Event-Stream; charset=utf-8")]);

        assert_eq!(
            response_bypass_reason(StatusCode::OK, &headers),
            Some(ResponseBypassReason::SseContentType)
        );
    }

    #[test]
    fn response_206_partial_content_bypasses_cache() {
        let headers = HeaderMap::new();

        assert_eq!(
            response_bypass_reason(StatusCode::PARTIAL_CONTENT, &headers),
            Some(ResponseBypassReason::PartialContent)
        );
    }

    #[test]
    fn ordinary_success_response_does_not_bypass() {
        let headers = headers(&[(header::CONTENT_TYPE.as_str(), "text/html")]);

        assert_eq!(response_bypass_reason(StatusCode::OK, &headers), None);
    }
}
