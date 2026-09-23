//! Browser trust boundary for the unauthenticated TCP transport.

use super::ApiError;
use axum::{
    extract::Request,
    http::{StatusCode, Uri, header, uri::Authority},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{net::IpAddr, sync::Arc};

#[derive(Clone)]
pub(super) struct BrowserPolicy {
    hosts: Arc<[String]>,
}

impl BrowserPolicy {
    pub(super) fn new(hosts: Vec<String>) -> Self {
        Self {
            hosts: hosts.into(),
        }
    }

    pub(super) async fn enforce(self, request: Request, next: Next) -> Response {
        if let Err(error) = self.check(&request) {
            return error.into_response();
        }
        next.run(request).await
    }

    fn check(&self, request: &Request) -> Result<(), ApiError> {
        let denied = || {
            ApiError::new(
                StatusCode::FORBIDDEN,
                "browser_access_denied",
                "request authority or browser origin is not trusted",
            )
        };
        let mut hosts = request.headers().get_all(header::HOST).iter();
        let host = hosts.next().and_then(|value| value.to_str().ok());
        if hosts.next().is_some() {
            return Err(denied());
        }
        let authority: Authority = host.ok_or_else(denied)?.parse().map_err(|_| denied())?;
        let host = authority.host();
        if !Self::valid_authority(&authority) {
            return Err(denied());
        }
        let ip_host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        if ip_host.parse::<IpAddr>().is_err()
            && !host.eq_ignore_ascii_case("localhost")
            && !self
                .hosts
                .iter()
                .any(|allowed| host.eq_ignore_ascii_case(allowed))
        {
            return Err(denied());
        }
        // Absolute-form targets must agree with Host, including the port.
        if request
            .uri()
            .authority()
            .is_some_and(|target| !target.as_str().eq_ignore_ascii_case(authority.as_str()))
        {
            return Err(denied());
        }
        let mut origins = request.headers().get_all(header::ORIGIN).iter();
        if let Some(origin) = origins.next() {
            let origin: Uri = origin
                .to_str()
                .map_err(|_| denied())?
                .parse()
                .map_err(|_| denied())?;
            let default_port = if request.uri().scheme_str() == Some("https") {
                443
            } else {
                80
            };
            if origins.next().is_some()
                || origin.scheme_str() != Some(request.uri().scheme_str().unwrap_or("http"))
                || origin.authority().is_none_or(|origin| {
                    !Self::valid_authority(origin)
                        || !origin.host().eq_ignore_ascii_case(authority.host())
                        || origin.port_u16().unwrap_or(default_port)
                            != authority.port_u16().unwrap_or(default_port)
                })
                || origin
                    .path_and_query()
                    .is_some_and(|path| path.as_str() != "/")
            {
                return Err(denied());
            }
        }
        if !request.method().is_safe()
            && request
                .headers()
                .get("sec-fetch-site")
                .is_some_and(|site| site != "same-origin" && site != "none")
        {
            return Err(denied());
        }
        Ok(())
    }

    // `Authority` alone accepts suffixes after IPv6 brackets and nonnumeric
    // ports. Reject those rather than trusting only its extracted host.
    fn valid_authority(authority: &Authority) -> bool {
        let host = authority.host();
        let Some(suffix) = authority.as_str().strip_prefix(host) else {
            return false;
        };
        !authority.as_str().contains('@')
            && (!host.contains(':') || host.starts_with('['))
            && (suffix.is_empty()
                || suffix.strip_prefix(':').is_some_and(|port| {
                    !port.is_empty()
                        && port.bytes().all(|byte| byte.is_ascii_digit())
                        && port.parse::<u16>().is_ok()
                }))
    }
}
