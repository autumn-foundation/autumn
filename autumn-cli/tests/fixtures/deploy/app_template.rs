// Template for the tiny, real HTTP server the deploy e2e harness stands up as the
// deployed release (see `autumn-cli/tests/deploy_e2e.rs`). The harness substitutes
// `__VERSION__` and `__REFUSE_READY__` and compiles it to a fully static binary
// (`rustc -C target-feature=+crt-static`) so it runs in the fixture's older-glibc
// container regardless of the host's glibc.
//
// It is a genuine server — it binds `AUTUMN_SERVER__HOST:AUTUMN_SERVER__PORT`
// (exactly what the deploy's slot systemd unit sets) and answers `/ready` and `/`
// over real TCP — not a stub. Two behaviours are baked at compile time:
//   * `AUTUMN_MIGRATE=1`  -> exit 0 immediately, so the deploy's pre-cutover
//     migrate one-shot (`systemd-run --wait ... --setenv=AUTUMN_MIGRATE=1`) is a
//     harmless no-op for this DB-free app.
//   * `__REFUSE_READY__`  -> when true, `/ready` returns 503 forever, forcing the
//     readiness gate to time out (the AC-4 forced-failure case). `/` still answers
//     200 so a health check can tell "process up but not ready" from "process down".
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

const VERSION: &str = "__VERSION__";
const REFUSE_READY: bool = __REFUSE_READY__;

fn main() {
    if std::env::var("AUTUMN_MIGRATE").as_deref() == Ok("1") {
        // DB-free app: migrations are a no-op. Exit 0 so `systemd-run --wait`
        // sees success and the deploy proceeds to the readiness gate.
        println!("migrate: no-op (version {VERSION})");
        return;
    }
    let host = std::env::var("AUTUMN_SERVER__HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("AUTUMN_SERVER__PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).expect("bind app listener");
    eprintln!("e2eapp {VERSION} listening on {addr} (refuse_ready={REFUSE_READY})");
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");
            let (status, body) = if path == "/ready" {
                if REFUSE_READY {
                    ("503 Service Unavailable", "not ready".to_string())
                } else {
                    ("200 OK", "ready".to_string())
                }
            } else {
                ("200 OK", format!("e2eapp {VERSION}\n"))
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        });
    }
}
