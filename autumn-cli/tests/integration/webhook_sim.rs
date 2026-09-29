use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::thread;

fn run_webhook_sim_against_status(status_line: &'static str, body: &'static str) -> Output {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind webhook capture server");
    let addr = listener.local_addr().expect("capture server local addr");
    let response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept simulated webhook");
        let mut raw_request = Vec::new();
        let mut buffer = [0_u8; 1024];

        loop {
            let bytes_read = stream
                .read(&mut buffer)
                .expect("read simulated webhook request");
            if bytes_read == 0 {
                break;
            }

            raw_request.extend_from_slice(&buffer[..bytes_read]);
            if raw_request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        stream
            .write_all(response.as_bytes())
            .expect("write simulated webhook response");
    });

    let autumn_bin = env!("CARGO_BIN_EXE_autumn");
    let output = Command::new(autumn_bin)
        .args([
            "webhook",
            "sim",
            "generic",
            &format!("http://{addr}/webhook"),
            "--secret",
            "secret",
            "--payload",
            r#"{"ok":true}"#,
        ])
        .output()
        .expect("run autumn webhook sim");

    handle.join().expect("capture server should finish");
    output
}

#[test]
fn webhook_sim_exits_nonzero_when_endpoint_rejects_request() {
    let output = run_webhook_sim_against_status("409 Conflict", "duplicate delivery");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "autumn webhook sim should fail on non-success HTTP status\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("409 Conflict"),
        "stderr should include rejected response status\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("duplicate delivery"),
        "stderr should include rejected response body\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
