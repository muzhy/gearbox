use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[test]
fn update_models_fills_provider_only_config_and_is_idempotent() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        break stream;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "model catalog request timed out");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("model catalog listener failed: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut buffer = [0; 1024];
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0, "request closed before headers");
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() < 8192, "request headers too large");
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
            assert!(request.contains("Bearer fixture-key"));
            let body = r#"{"data":[{"id":"first"},{"id":"second"},{"id":"first"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });

    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    let original = format!(
        r#"[providers.test]
base_url = "http://{address}/v1"
protocol = "openai-responses"
api_key = "fixture-key"

[limits]
max_model_requests = 2
request_timeout_secs = 2
max_output_tokens = 128
max_file_bytes = 1024

[logging]
level = "error"
directory = "logs"
"#
    );
    fs::write(&config_path, &original).unwrap();

    let run_update = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gearbox"));
        command
            .current_dir(directory.path())
            .arg("update-models")
            .arg("--config=config.toml");
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(name);
        }
        command.output().unwrap()
    };

    let first = run_update();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("added 2 model(s)"));
    let updated = fs::read_to_string(&config_path).unwrap();
    assert!(updated.starts_with(&original));
    let parsed: toml::Value = toml::from_str(&updated).unwrap();
    let models = parsed["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["id"].as_str(), Some("first"));
    assert_eq!(models[1]["id"].as_str(), Some("second"));
    assert_eq!(models[0]["reasoning"].as_bool(), Some(false));

    let second = run_update();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(String::from_utf8_lossy(&second.stdout).contains("already contains"));
    assert_eq!(fs::read_to_string(&config_path).unwrap(), updated);
    server.join().unwrap();
}
