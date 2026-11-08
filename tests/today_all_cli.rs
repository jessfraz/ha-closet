use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
    time::Duration,
};

use chrono::Utc;
use closet::models::{Generation, GenerationStatus, Occasion, TodayRequest, WeatherMode};
use serde::Deserialize;

#[derive(Deserialize)]
struct Response {
    data: Generation,
}

#[test]
fn cli_defaults_to_all_and_makes_one_explicit_accepted_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
            assert!(headers.len() < 16384);
        }
        let headers = String::from_utf8(headers).unwrap();
        assert!(headers.starts_with("POST /v1/today/generate HTTP/1.1"));
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|length| length.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        let inputs: TodayRequest = serde_json::from_slice(&body).unwrap();
        assert_eq!(inputs.occasion, Occasion::All);
        assert_eq!(inputs.weather_mode, WeatherMode::Ignored);
        let accepted = Generation {
            id: "example-all-generation".into(),
            status: GenerationStatus::Running,
            inputs,
            started_at: Utc::now(),
            finished_at: None,
            error_code: None,
        };
        let body = serde_json::to_vec(&accepted).unwrap();
        write!(stream, "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        stream.write_all(&body).unwrap();
        stream.flush().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_ha-closet"));
    for key in [
        "CLOSET_API_URL",
        "CLOSET_API_TOKEN",
        "HOME_ASSISTANT_URL",
        "HOME_ASSISTANT_TOKEN",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env_remove(key);
    }
    let output = command
        .env("CLOSET_API_URL", origin)
        .env(
            "CLOSET_API_TOKEN",
            "example-owner-credential-at-least-32-characters",
        )
        .args([
            "today",
            "generate",
            "--weather-mode",
            "ignored",
            "--idempotency-key",
            "example-all-request",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Response = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.data.inputs.occasion, Occasion::All);
    worker.join().unwrap();
}
