//! Phase 1 passthrough HTTP handler — logs every intercepted request /
//! response at trace level and forwards both unchanged. Phase 3 will replace
//! this with the policy / cache / inject pipeline shared with Windows.

use hudsucker::{Body, HttpContext, HttpHandler, RequestOrResponse};
use http::{Request, Response};

use super::is_learned_passthrough;

#[derive(Clone, Default)]
pub struct PouchHandler;

impl HttpHandler for PouchHandler {
    /// Called once per CONNECT request. Returning `false` makes hudsucker
    /// skip TLS termination and byte-forward the tunnel via
    /// `TcpStream::connect + io::copy_bidirectional` — the only way to
    /// support cert-pinning sites and legacy TLS 1.0/1.1 + RSA-KX/CBC
    /// servers that rustls + aws-lc-rs cannot speak. Hosts are added to
    /// the passthrough set automatically by the self-learning layer (see
    /// [`super::LearnerLayer`]) when a TLS handshake against them fails.
    async fn should_intercept(&mut self, _ctx: &HttpContext, req: &Request<Body>) -> bool {
        let Some(authority) = req.uri().authority() else {
            return true;
        };
        let host_port = authority.as_str();
        if is_learned_passthrough(host_port) {
            tracing::debug!(
                target: "hook",
                "[mitm] passthrough reason=learned host={host_port}"
            );
            return false;
        }
        true
    }

    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        req: Request<Body>,
    ) -> RequestOrResponse {
        tracing::trace!(
            target: "hook",
            "[mitm] request method={} uri={}",
            req.method(),
            req.uri()
        );
        RequestOrResponse::Request(req)
    }

    async fn handle_response(
        &mut self,
        _ctx: &HttpContext,
        res: Response<Body>,
    ) -> Response<Body> {
        tracing::trace!(
            target: "hook",
            "[mitm] response status={}",
            res.status()
        );
        res
    }
}
