//! The embedded dashboard's routes, security headers and cache policy.

use super::*;

#[tokio::test]
async fn dashboard_fallback_preserves_api_and_asset_route_precedence() {
    let temp = tempfile::tempdir().expect("temporary directory");

    let application = web_router(
        state(&temp).await,
        UiAssets::Embedded(TEST_BUNDLE),
        FakeAuth,
    );
    assert_dashboard_routes(&application).await;
    assert_api_routes(&application).await;
    assert_api_only_and_ui_modes(&temp).await;
}

/// Exercise the shipped bundle: every script is a same-origin file, so the
/// constant CSP needs no inline-script hashes.
#[cfg(feature = "embedded-ui")]
#[tokio::test]
async fn compiled_dashboard_serves_assets_without_inline_scripts() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let assets = UiAssets::resolve();
    let UiAssets::Embedded(bundle) = assets else {
        panic!("embedded-ui must resolve to the compiled bundle");
    };
    let application = web_router(state(&temp).await, assets, FakeAuth);
    let shell = application.send(request("/dashboard/")).await;
    assert_eq!(shell.status, StatusCode::OK);
    let csp = shell.header("content-security-policy").expect("CSP");
    let html = shell.text();
    assert!(
        csp.contains("script-src 'self' 'wasm-unsafe-eval';"),
        "{csp}"
    );
    assert!(!html.contains("{{"), "every shell placeholder is filled");

    let scripts = html.split("<script").skip(1).collect::<Vec<_>>();
    assert!(!scripts.is_empty(), "the shell must load the dashboard");
    for script in scripts {
        let (attributes, rest) = script.split_once('>').expect("script opening tag");
        assert!(
            attributes
                .split_whitespace()
                .any(|attr| attr.starts_with("src=")),
            "inline script in the shell: {script}"
        );
        assert!(
            rest.starts_with("</script>"),
            "script with a body: {script}"
        );
    }

    for extension in [".js", ".wasm", ".css"] {
        assert!(
            bundle
                .iter()
                .any(|(name, _)| name.ends_with(extension) && html.contains(name)),
            "the shell must reference a bundled {extension} asset"
        );
    }
    for (name, expected) in bundle {
        let asset = application
            .send(request(&format!("/dashboard/{name}")))
            .await;
        assert_eq!(asset.status, StatusCode::OK, "{name}");
        assert_eq!(asset.body.as_ref(), *expected, "{name}");
    }
}

async fn assert_dashboard_routes(application: &axum::Router) {
    for uri in ["/", "/dashboard"] {
        let redirect = application.send(request(uri)).await;
        assert_eq!(redirect.status, StatusCode::PERMANENT_REDIRECT, "{uri}");
        assert_eq!(
            redirect.header(http::header::LOCATION),
            Some("/dashboard/"),
            "{uri}"
        );
    }
    for uri in ["/dashboard/", "/dashboard/applications/notes"] {
        let shell = application.send(request(uri)).await;
        assert_eq!(shell.status, StatusCode::OK, "{uri}");
        assert!(shell.text().contains("dashboard-shell"), "{uri}");
    }
    let asset = application.send(request("/dashboard/app.js")).await;
    assert_eq!(asset.status, StatusCode::OK);
    assert!(asset.text().contains("console.log"));
    let hashed = application
        .send(request("/dashboard/piqueld-ui-a1b2c3d4_bg.wasm"))
        .await;
    assert_eq!(hashed.status, StatusCode::OK);
    assert!(hashed.body.starts_with(b"\0"));
    for uri in ["/dashboard/missing.js", "/api/v1/unknown", "/unknown"] {
        let missing = application.send(request(uri)).await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{uri}");
        assert!(!missing.text().contains("dashboard-shell"), "{uri}");
        if uri.starts_with("/api/") {
            assert!(missing.text().contains("endpoint_not_found"), "{uri}");
        }
    }
}

async fn assert_api_routes(application: &axum::Router) {
    let health = application.send(request("/health")).await;
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(health.text(), r#"{"status":"ok"}"#);

    let openapi = application.send(request("/api/v1/openapi.json")).await;
    assert_eq!(openapi.status, StatusCode::OK);
    let openapi = openapi.json();
    assert_eq!(openapi["openapi"], "3.0.3");
    assert!(openapi["paths"]["/api/v1/environments/{id}/detail"].is_object());
}

async fn assert_api_only_and_ui_modes(temp: &TempDir) {
    let api_only = api_router(state(temp).await, FakeAuth);
    let root = api_only.send(request("/")).await;
    assert_eq!(root.status, StatusCode::NOT_FOUND);
    assert!(!root.text().contains("endpoint_not_found"));
    let health = api_only.send(request("/health")).await;
    assert_eq!(health.status, StatusCode::NOT_FOUND);
    let unknown = api_only.send(request("/api/v1/unknown")).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert!(unknown.text().contains("endpoint_not_found"));

    let disabled = web_router(state(temp).await, UiAssets::Disabled, FakeAuth);
    for uri in ["/", "/dashboard/"] {
        let response = disabled.send(request(uri)).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn dashboard_responses_carry_security_headers() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let application = web_router(
        state(&temp).await,
        UiAssets::Embedded(TEST_BUNDLE),
        FakeAuth,
    );
    let shell = application.send(request("/dashboard/")).await;
    assert_eq!(shell.status, StatusCode::OK);
    let csp = shell
        .header("content-security-policy")
        .expect("dashboard carries a content security policy");
    assert!(csp.contains("default-src 'self'"), "{csp}");
    assert!(csp.contains("wasm-unsafe-eval"), "{csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert_eq!(shell.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(shell.header("referrer-policy"), Some("no-referrer"));

    let head = application
        .send(request_as(Method::HEAD, "/dashboard/"))
        .await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.body.is_empty());

    let post = application
        .send(request_as(Method::POST, "/dashboard/"))
        .await;
    assert_eq!(post.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(post.header(http::header::ALLOW), Some("GET, HEAD"));
    for (method, path, status) in [
        (Method::GET, "/", StatusCode::PERMANENT_REDIRECT),
        (Method::GET, "/dashboard/missing.js", StatusCode::NOT_FOUND),
        (Method::POST, "/dashboard", StatusCode::METHOD_NOT_ALLOWED),
    ] {
        let response = application.send(request_as(method, path)).await;
        assert_eq!(response.status, status);
        for name in [
            "content-security-policy",
            "x-content-type-options",
            "referrer-policy",
        ] {
            assert_eq!(
                response.headers.get_all(name).iter().count(),
                1,
                "{path}: {name}"
            );
            assert_eq!(response.header(name), shell.header(name));
        }
    }
    for path in ["/health", "/api/v1/missing"] {
        let response = application.send(request(path)).await;
        assert!(
            !response.headers.contains_key("content-security-policy"),
            "{path}"
        );
    }
}

#[tokio::test]
async fn dashboard_cache_policy_tracks_content_hashing_and_shell_fallbacks() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let application = web_router(
        state(&temp).await,
        UiAssets::Embedded(TEST_BUNDLE),
        FakeAuth,
    );

    let hashed = application
        .send(request("/dashboard/piqueld-ui-a1b2c3d4_bg.wasm"))
        .await;
    assert_eq!(hashed.header("content-type"), Some("application/wasm"));
    assert_eq!(
        hashed.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );

    let unhashed = application.send(request("/dashboard/app.js")).await;
    assert_eq!(
        unhashed.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(unhashed.header("cache-control"), Some("no-cache"));

    // A short all-hex stem is an ordinary name, not a content digest.
    let hex_stemmed = application.send(request("/dashboard/added.css")).await;
    assert_eq!(hex_stemmed.header("cache-control"), Some("no-cache"));

    // The shell, also served for deep links, is never cached.
    for path in ["/dashboard/", "/dashboard/applications/notes"] {
        let shell = application.send(request(path)).await;
        assert_eq!(shell.header("cache-control"), Some("no-store"), "{path}");
    }
}
