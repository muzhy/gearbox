use std::{
    collections::VecDeque,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Output},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tempfile::TempDir;

const KEY: &str = "fixture-key-do-not-print";
const SERVER_SECRET: &str = "sensitive-server-body-do-not-print";

struct Reply {
    status: u16,
    body: String,
    delay: Duration,
    location: Option<String>,
}

impl Reply {
    fn response(output: Vec<Value>) -> Self {
        Self::raw(
            200,
            json!({"status": "completed", "output": output}).to_string(),
        )
    }

    fn raw(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            delay: Duration::ZERO,
            location: None,
        }
    }
}

#[derive(Clone)]
struct Request {
    path: String,
    authorization: String,
    body: Value,
}

struct Stub {
    endpoint: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Stub {
    fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!(
            "http://{}/custom/responses?configured=1",
            listener.local_addr().unwrap()
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let mut replies = VecDeque::from(replies);
            while !thread_stop.load(Ordering::Relaxed) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("stub accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request = read_request(&mut stream);
                thread_requests.lock().unwrap().push(request);
                let reply = replies
                    .pop_front()
                    .unwrap_or_else(|| Reply::raw(500, "unexpected extra request"));
                let started = Instant::now();
                while started.elapsed() < reply.delay {
                    if thread_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                let location = reply
                    .location
                    .map(|url| format!("Location: {url}\r\n"))
                    .unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 {} Stub\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{location}\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.body
                );
                // The timeout test deliberately closes its connection before this write.
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            endpoint,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.expect("HTTP stub worker failed");
            }
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0, "connection closed before request headers");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(
            bytes.len() < 64 * 1024,
            "unexpectedly large request headers"
        );
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let path = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    assert!(headers.starts_with("POST "));
    let mut content_length = None;
    let mut authorization = String::new();
    for line in headers.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = Some(value.trim().parse::<usize>().unwrap());
            } else if name.eq_ignore_ascii_case("authorization") {
                authorization = value.trim().to_owned();
            }
        }
    }
    let length = content_length.expect("JSON requests must carry Content-Length");
    while bytes.len() - header_end < length {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0, "connection closed before request body");
        bytes.extend_from_slice(&chunk[..count]);
    }
    Request {
        path: path.to_owned(),
        authorization,
        body: serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap(),
    }
}

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new(stub: &Stub, max_requests: u32) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("data")).unwrap();
        fs::write(
            dir.path().join("data/index.txt"),
            "项目资料文件：project.txt",
        )
        .unwrap();
        fs::write(
            dir.path().join("data/project.txt"),
            "项目名称：Gearbox Demo；当前阶段：最小 Loop 原型",
        )
        .unwrap();
        fs::write(
            dir.path().join("data/config.toml"),
            format!(
                r#"[model]
endpoint = "{}"
api_key = "{KEY}"
name = "configured-test-model"
reasoning_effort = "high"

[limits]
max_model_requests = {max_requests}
request_timeout_secs = 3
max_output_tokens = 321
max_file_bytes = 4096
"#,
                stub.endpoint
            ),
        )
        .unwrap();
        Self { dir }
    }

    fn replace_config(&self, before: &str, after: &str) {
        let path = self.dir.path().join("data/config.toml");
        let original = fs::read_to_string(&path).unwrap();
        assert!(original.contains(before));
        fs::write(path, original.replace(before, after)).unwrap();
    }

    fn run(&self) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gearbox"));
        command
            .current_dir(self.dir.path())
            .args([
                "data",
                "读取 index.txt，找到资料，再告诉我项目名称和当前阶段。",
            ])
            // These stale settings must not override workspace/config.toml.
            .env("GEARBOX_MODEL_URL", "http://invalid.invalid/unused")
            .env("GEARBOX_API_KEY", "unused-environment-key")
            .env("GEARBOX_MODEL", "unused-environment-model");
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
    }
}

fn message(id: &str, text: &str, phase: &str) -> Value {
    json!({
        "type": "message", "id": id, "role": "assistant", "status": "completed", "phase": phase,
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    })
}

fn call(id: &str, path: &str) -> Value {
    json!({
        "type": "function_call", "id": format!("fc_{id}"), "call_id": id,
        "name": "read_file", "arguments": json!({"path": path}).to_string(), "status": "completed",
    })
}

fn answer(text: &str) -> Reply {
    Reply::response(vec![message("msg_final", text, "final_answer")])
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap()
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).unwrap()
}

fn tool_result(request: &Request, index: usize, call_id: &str) -> Value {
    let item = &request.body["input"][index];
    assert_eq!(item["type"], "function_call_output");
    assert_eq!(item["call_id"], call_id);
    serde_json::from_str(item["output"].as_str().unwrap()).unwrap()
}

#[test]
fn reads_index_then_project_and_replays_original_history() {
    let first = vec![
        json!({
            "type": "reasoning", "id": "rs_1", "summary": [],
            "encrypted_content": "encrypted-first", "provider_extension": {"keep": true},
        }),
        message("msg_commentary", "先读取索引，尚未完成。", "commentary"),
        call("read_index", "index.txt"),
    ];
    let second = vec![
        json!({"type": "reasoning", "id": "rs_2", "summary": [], "encrypted_content": "encrypted-second"}),
        call("read_project", "project.txt"),
    ];
    let stub = Stub::new(vec![
        Reply::response(first.clone()),
        Reply::response(second.clone()),
        answer("Gearbox Demo；最小 Loop 原型"),
    ]);
    let fixture = Fixture::new(&stub, 8);
    let output = fixture.run();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "Gearbox Demo；最小 Loop 原型\n");
    assert_eq!(stderr(&output).matches("tool: read_file").count(), 2);
    assert!(stderr(&output).contains("final answer received"));
    assert!(!stderr(&output).contains(KEY));

    let requests = stub.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.path, "/custom/responses?configured=1");
        assert_eq!(request.authorization, format!("Bearer {KEY}"));
        assert_eq!(request.body["model"], "configured-test-model");
        assert_eq!(request.body["reasoning"]["effort"], "high");
        assert_eq!(request.body["max_output_tokens"], 321);
        assert_eq!(request.body["stream"], false);
        assert_eq!(request.body["store"], false);
        assert_eq!(request.body["truncation"], "disabled");
        assert_eq!(
            request.body["include"],
            json!(["reasoning.encrypted_content"])
        );
        assert_eq!(request.body["tools"][0]["name"], "read_file");
        assert_eq!(request.body["tools"][0]["strict"], true);
        assert_eq!(
            request.body["tools"][0]["parameters"]["additionalProperties"],
            false
        );
        assert!(request.body.get("previous_response_id").is_none());
        assert!(!request.body.to_string().contains(KEY));
    }
    let initial = requests[0].body["input"].as_array().unwrap();
    assert_eq!(initial.len(), 2);
    assert_eq!(initial[0]["role"], "system");
    assert_eq!(initial[1]["role"], "user");
    assert!(
        !initial[1]["content"]
            .as_str()
            .unwrap()
            .contains("project.txt")
    );
    let next = requests[1].body["input"].as_array().unwrap();
    assert_eq!(&next[..2], initial);
    assert_eq!(&next[2..5], &first);
    assert_eq!(
        tool_result(&requests[1], 5, "read_index"),
        json!({"ok": true, "content": "项目资料文件：project.txt"})
    );
    let last = requests[2].body["input"].as_array().unwrap();
    assert_eq!(&last[..6], next);
    assert_eq!(&last[6..8], &second);
    assert_eq!(
        tool_result(&requests[2], 8, "read_project"),
        json!({"ok": true, "content": "项目名称：Gearbox Demo；当前阶段：最小 Loop 原型"})
    );
}

#[test]
fn preserves_same_batch_tool_order_and_call_associations() {
    let calls = vec![
        call("second_id", "index.txt"),
        call("first_id", "project.txt"),
    ];
    let stub = Stub::new(vec![Reply::response(calls.clone()), answer("done")]);
    let output = Fixture::new(&stub, 2).run();
    assert!(output.status.success(), "{}", stderr(&output));
    let requests = stub.requests();
    assert_eq!(requests.len(), 2);
    let input = requests[1].body["input"].as_array().unwrap();
    assert_eq!(&input[2..4], &calls);
    assert_eq!(tool_result(&requests[1], 4, "second_id")["ok"], true);
    assert_eq!(tool_result(&requests[1], 5, "first_id")["ok"], true);
    assert!(
        stderr(&output).find("index.txt").unwrap() < stderr(&output).find("project.txt").unwrap()
    );
}

#[test]
fn returns_tool_errors_to_model_and_allows_correction() {
    let mut unknown = call("unknown", "index.txt");
    unknown["name"] = json!("unknown_tool");
    let mut malformed = call("malformed", "index.txt");
    malformed["arguments"] = json!("not valid JSON");
    let stub = Stub::new(vec![
        Reply::response(vec![unknown, malformed, call("missing", "missing.txt")]),
        Reply::response(vec![call("corrected", "index.txt")]),
        answer("recovered"),
    ]);
    let output = Fixture::new(&stub, 3).run();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "recovered\n");
    let requests = stub.requests();
    assert_eq!(requests.len(), 3);
    for (index, id) in [(5, "unknown"), (6, "malformed"), (7, "missing")] {
        let result = tool_result(&requests[1], index, id);
        assert_eq!(result["ok"], false);
        assert!(result["error"].is_string());
    }
    assert_eq!(tool_result(&requests[2], 9, "corrected")["ok"], true);
}

#[test]
fn final_answer_succeeds_at_configured_limit_and_continuing_tools_stop_at_limit() {
    for limit in [2, 8] {
        for final_answer in [false, true] {
            let replies = (1..=limit)
                .map(|index| {
                    if final_answer && index == limit {
                        answer("finished at limit")
                    } else {
                        Reply::response(vec![call(&format!("call_{index}"), "index.txt")])
                    }
                })
                .collect();
            let stub = Stub::new(replies);
            let output = Fixture::new(&stub, limit).run();
            assert_eq!(output.status.success(), final_answer, "{}", stderr(&output));
            assert_eq!(stub.requests().len(), limit as usize);
            assert_eq!(
                stderr(&output).matches("Model request ").count(),
                limit as usize
            );
            assert_eq!(
                stderr(&output).matches("tool: read_file").count(),
                (limit - u32::from(final_answer)) as usize
            );
            if final_answer {
                assert_eq!(stdout(&output), "finished at limit\n");
            } else {
                assert!(stdout(&output).is_empty());
                assert!(stderr(&output).contains("exhausted; no final answer received"));
            }
        }
    }
}

#[test]
fn accepts_direct_answer_without_executing_a_tool() {
    let stub = Stub::new(vec![answer("direct answer")]);
    let output = Fixture::new(&stub, 1).run();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "direct answer\n");
    assert_eq!(stub.requests().len(), 1);
    assert!(!stderr(&output).contains("tool:"));
}

#[test]
fn uses_configured_file_size_in_real_tool_execution() {
    let stub = Stub::new(vec![
        Reply::response(vec![call("too_large", "index.txt")]),
        answer("file limit observed"),
    ]);
    let fixture = Fixture::new(&stub, 2);
    fixture.replace_config("max_file_bytes = 4096", "max_file_bytes = 4");
    let output = fixture.run();
    assert!(output.status.success(), "{}", stderr(&output));
    let requests = stub.requests();
    let result = tool_result(&requests[1], 3, "too_large");
    assert_eq!(result["ok"], false);
    assert!(result["error"].as_str().unwrap().contains("max_file_bytes"));
}

#[test]
fn uses_configured_timeout_and_does_not_retry() {
    let mut reply = answer("too late");
    reply.delay = Duration::from_secs(5);
    let stub = Stub::new(vec![reply]);
    let fixture = Fixture::new(&stub, 8);
    fixture.replace_config("request_timeout_secs = 3", "request_timeout_secs = 1");
    let started = Instant::now();
    let output = fixture.run();
    assert!(!output.status.success());
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("超时"));
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(stub.requests().len(), 1);
}

#[test]
fn http_and_json_errors_fail_once_without_exposing_response_bodies() {
    for status in [401, 429, 500, 200] {
        let stub = Stub::new(vec![Reply::raw(status, SERVER_SECRET)]);
        let output = Fixture::new(&stub, 8).run();
        assert!(!output.status.success());
        assert!(stdout(&output).is_empty());
        assert!(!stderr(&output).contains(KEY));
        assert!(!stderr(&output).contains(SERVER_SECRET));
        if status != 200 {
            assert!(stderr(&output).contains(&format!("HTTP {status}")));
        } else {
            assert!(stderr(&output).contains("JSON"));
        }
        assert_eq!(stub.requests().len(), 1);
        assert!(!stderr(&output).contains("tool:"));
    }
}

#[test]
fn does_not_follow_redirects() {
    let mut reply = Reply::raw(307, SERVER_SECRET);
    reply.location = Some("/redirect-target".to_owned());
    let stub = Stub::new(vec![reply, answer("must not follow")]);
    let output = Fixture::new(&stub, 8).run();
    assert!(!output.status.success());
    assert!(stderr(&output).contains("HTTP 307"));
    assert_eq!(stub.requests().len(), 1);
}

#[test]
fn invalid_call_batch_executes_no_tools_even_when_first_call_is_valid() {
    for duplicate_id in [false, true] {
        let mut invalid = call("duplicate", "project.txt");
        if !duplicate_id {
            invalid.as_object_mut().unwrap().remove("call_id");
        }
        let stub = Stub::new(vec![Reply::response(vec![
            call("duplicate", "index.txt"),
            invalid,
        ])]);
        let output = Fixture::new(&stub, 8).run();
        assert!(!output.status.success());
        assert!(stdout(&output).is_empty());
        assert!(!stderr(&output).contains("tool:"));
        assert_eq!(stub.requests().len(), 1);
    }
}

#[test]
fn refused_incomplete_empty_and_unsupported_responses_fail_without_tools() {
    let mut refusal = message("msg_refusal", "", "final_answer");
    refusal["content"] = json!([{ "type": "refusal", "refusal": SERVER_SECRET }]);
    let responses = vec![
        json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": [call("c", "index.txt")]}),
        json!({"status": "failed", "error": {"message": SERVER_SECRET}, "output": []}),
        json!({"status": "completed", "output": []}),
        json!({"status": "completed", "output": [message("empty", "  ", "final_answer")]}),
        json!({"status": "completed", "output": [call("c", "index.txt"), refusal]}),
        json!({"status": "completed", "output": [call("c", "index.txt"), {"type": "web_search_call"}]}),
    ];
    for response in responses {
        let stub = Stub::new(vec![Reply::raw(200, response.to_string())]);
        let output = Fixture::new(&stub, 8).run();
        assert!(!output.status.success());
        assert!(stdout(&output).is_empty());
        assert!(!stderr(&output).contains("tool:"));
        assert!(!stderr(&output).contains(SERVER_SECRET));
        assert_eq!(stub.requests().len(), 1);
    }
}
