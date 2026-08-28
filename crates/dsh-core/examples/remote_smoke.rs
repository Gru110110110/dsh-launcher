//! Manual end-to-end smoke check for the remote proxy against a real
//! loopback Harness web service. Uses an isolated temporary desktop home and
//! only issues read-only GETs plus login POSTs to the proxy itself.
//!
//! Run: DSH_SMOKE_UPSTREAM=http://127.0.0.1:3080 cargo run -p dsh-core --example remote_smoke

use std::io::Read;

use dsh_core::{ApplicationPaths, remote::RemoteService};

fn main() {
    let upstream = env_upstream();
    let temp = tempfile::tempdir().expect("temp home");
    let paths = ApplicationPaths::from_home(temp.path());
    let service = RemoteService::new(paths).expect("remote service");
    service.set_master(true).expect("master on");
    service.set_upstream(Some(&upstream)).expect("upstream");
    let snapshot = service.snapshot();
    let password = snapshot.lan.password.clone();
    let proxy_addr = {
        // The snapshot URL carries the LAN address; for the smoke check we
        // reach the same listener through loopback instead.
        let url = snapshot.lan.url.as_deref().expect("lan listener running");
        let port = url.rsplit(':').next().expect("port");
        format!("127.0.0.1:{port}")
    };
    println!("proxy port: {proxy_addr}");

    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client");
    let base = format!("http://{proxy_addr}");

    // 1. Unauthenticated requests must not reach upstream.
    let response = client.get(&base).send().expect("GET /");
    assert_eq!(response.status(), 302, "unauthenticated GET must redirect");
    assert_eq!(
        response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok()),
        Some("/__dsh-remote/login")
    );

    // 2. Wrong password bounces back to the login page with an error flag.
    let response = client
        .post(format!("{base}/__dsh-remote/login"))
        .body("password=00000000")
        .send()
        .expect("login wrong");
    assert_eq!(response.status(), 302);
    assert!(
        response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("error=1"))
    );

    // 3. Correct password issues a session cookie.
    let response = client
        .post(format!("{base}/__dsh-remote/login"))
        .body(format!("password={password}"))
        .send()
        .expect("login right");
    assert_eq!(response.status(), 302);
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .expect("session cookie")
        .split(';')
        .next()
        .unwrap_or_default()
        .to_owned();
    assert!(cookie.starts_with("dsh_remote_lan="), "{cookie}");

    // 4. Authenticated request proxies the real Harness web UI.
    let response = client
        .get(&base)
        .header("cookie", &cookie)
        .send()
        .expect("GET / authed");
    assert_eq!(response.status(), 200, "proxied GET must succeed");
    let mut body = String::new();
    response
        .take(512 * 1024)
        .read_to_string(&mut body)
        .expect("body");
    assert!(
        body.contains("<html") || body.contains("<!doctype"),
        "upstream HTML: {body:?}"
    );
    assert!(
        body.contains("<head><script data-dsh-remote-polyfill=\"1\">"),
        "LAN polyfills must be injected right after <head>: {:.400}",
        body
    );

    // 5. Logout revokes the session immediately.
    let response = client
        .get(format!("{base}/__dsh-remote/logout"))
        .header("cookie", &cookie)
        .send()
        .expect("logout");
    assert_eq!(response.status(), 302);
    let response = client
        .get(&base)
        .header("cookie", &cookie)
        .send()
        .expect("GET / after logout");
    assert_eq!(response.status(), 302, "revoked session must redirect");

    service.shutdown();
    println!("remote smoke check passed against {upstream}");
}

fn env_upstream() -> String {
    std::env::var("DSH_SMOKE_UPSTREAM").unwrap_or_else(|_| "http://127.0.0.1:3080".to_owned())
}
