//! Phase 1 passthrough HTTP handler — logs every intercepted request /
//! response at trace level and forwards both unchanged. Phase 3 will replace
//! this with the policy / cache / inject pipeline shared with Windows.

use hudsucker::{Body, HttpContext, HttpHandler, RequestOrResponse};
use http::{Request, Response};

#[derive(Clone, Default)]
pub struct PouchHandler;

impl HttpHandler for PouchHandler {
    /// Phase 1 stays MITM for every CONNECT — `true` means hudsucker
    /// terminates TLS with our rcgen-minted leaf cert and we see the
    /// decrypted request/response in the handler methods below. A future
    /// commit introduces a self-learning passthrough layer that flips this
    /// to `false` for hosts whose upstream TLS handshake is unreachable.
    async fn should_intercept(&mut self, _ctx: &HttpContext, _req: &Request<Body>) -> bool {
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
