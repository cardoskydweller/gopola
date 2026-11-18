//! A stand-in for Ollama's `POST /api/embed`, `POST /v1/systemone` and unload, for tests: a second implementation of the contract in
//! tests/fixtures/ollama/README.md. It answers from the captured fixtures there, with deterministic topic
//! vectors in place of real ones, and can be as rude as the real server (`Rude`).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Each topic is a set of words that mean the same thing for the test. A text gets 1.0 on the
/// axis of every topic it mentions, and 0.1 on the last axis so no vector is zero.
const TOPICS: &[&[&str]] = &[
    &["ollama", "laptop", "local model"],
    &["protobuf", "binary format"],
    &["safari", "apple's browser"],
    &["wrong", "told you"],
];

/// How the fake misbehaves. The real server loads a model cold in about 2 s, can fail with a
/// plain-text 500, or drop the connection; a fake that never does hides what gopola does then.
#[derive(Clone, Copy, Debug)]
pub enum Rude {
    Polite,
    /// Wait this long before answering each request, like a cold model load.
    DelayMs(u64),
    /// 500 with a body that is not JSON.
    Status500,
    /// 200 with one row fewer than inputs.
    WrongRowCount,
    /// Read the request, then close the connection without answering.
    HangUp,
    /// `/v1/systemone` answers the not-pulled 404 whatever the model (d3 was removed). Embed is unchanged.
    NoDecisionModel,
    /// `/v1/systemone` answers a `noul` of 1.5, outside [0, 1]. Embed is unchanged.
    NoulOutOfRange,
    /// `/v1/systemone` answers 400 `{"error": "rejected"}` for a state that contains this text (compared in
    /// lower case), and the normal answer for any other. Embed is unchanged.
    RejectState(&'static str),
    /// An unload (`/api/generate` with `keep_alive` 0) is counted, then answered 500. Everything else is polite.
    UnloadFails,
    /// `POST <route>` is answered with this status line (`302 Found`) and a `Location`, and is not counted;
    /// every other request is polite.
    Redirect { route: &'static str, status: &'static str, location: &'static str },
}

/// The decision model that `/v1/systemone` answers; any other model is not pulled.
pub const DECISION_MODEL: &str = "d3:4b-nvfp4";

pub struct FakeOllama {
    pub url: String,
    /// Requests to `POST /api/embed` (the other routes are counted in `systemone` and `unloads`; a request to any
    /// route that the fake does not know is answered 404 and not counted).
    pub requests: Arc<AtomicUsize>,
    /// Inputs per `POST /api/embed` request, in the order they came: the batches gopola sent.
    pub batches: Arc<Mutex<Vec<usize>>>,
    /// Every `POST /api/embed` body as parsed, in the order they came.
    pub bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    /// Every `POST /v1/systemone` body as parsed, in the order they came.
    pub systemone: Arc<Mutex<Vec<serde_json::Value>>>,
    /// `POST /api/generate` requests with `keep_alive` 0: the unloads.
    pub unloads: Arc<AtomicUsize>,
}

/// What every connection's thread records into, shared with the `FakeOllama` that a test holds.
#[derive(Clone, Default)]
struct Seen {
    requests: Arc<AtomicUsize>,
    batches: Arc<Mutex<Vec<usize>>>,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    systemone: Arc<Mutex<Vec<serde_json::Value>>>,
    unloads: Arc<AtomicUsize>,
}

/// A fixture from tests/fixtures/ollama, by file stem.
pub fn fixture(name: &str) -> serde_json::Value {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ollama").join(format!("{name}.json"));
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
}

impl FakeOllama {
    pub fn start(dim: usize, known_model: &str) -> FakeOllama {
        FakeOllama::start_rude(dim, known_model, Rude::Polite)
    }

    pub fn start_rude(dim: usize, known_model: &str, rude: Rude) -> FakeOllama {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Seen::default();
        let (shared, known) = (seen.clone(), known_model.to_string());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (shared, known) = (shared.clone(), known.clone());
                std::thread::spawn(move || serve(stream, dim, &known, rude, &shared));
            }
        });
        FakeOllama { url, requests: seen.requests, batches: seen.batches, bodies: seen.bodies, systemone: seen.systemone, unloads: seen.unloads }
    }
}

/// One connection: requests one after another until the client closes it (keep-alive, as the real server).
fn serve(stream: TcpStream, dim: usize, known: &str, rude: Rude, seen: &Seen) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut out = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        // Headers arrive lowercase from ureq; read them case-insensitively, then exactly content-length bytes.
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let l = line.trim_end().to_ascii_lowercase();
            if l.is_empty() {
                break;
            }
            if let Some(v) = l.strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; len];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let is = |path: &str| request_line.starts_with(&format!("POST {path} "));
        if let Rude::Redirect { route, status, location } = rude
            && is(route)
        {
            let _ = out.write_all(format!("HTTP/1.1 {status}\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n").as_bytes());
            continue;
        }
        let (systemone, generate) = (is("/v1/systemone"), is("/api/generate"));
        if !(is("/api/embed") || systemone || generate) {
            send(&mut out, "404 Not Found", "text/plain; charset=utf-8", b"404 page not found", false);
            continue;
        }
        let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if generate {
            // Only an unload (keep_alive 0) is captured; any other generate is not a route this fake knows.
            if req["keep_alive"] == 0 {
                seen.unloads.fetch_add(1, Ordering::SeqCst);
                if matches!(rude, Rude::UnloadFails) {
                    send(&mut out, "500 Internal Server Error", "application/json; charset=utf-8", br#"{"error":"boom"}"#, false);
                    continue;
                }
                let mut answer = fixture("unload-d3")["response"].clone();
                answer["model"] = req["model"].clone();
                send(&mut out, "200 OK", "application/json; charset=utf-8", answer.to_string().as_bytes(), false);
            } else {
                send(&mut out, "404 Not Found", "text/plain; charset=utf-8", b"404 page not found", false);
            }
            continue;
        }
        let inputs: Vec<String> = match &req["input"] {
            serde_json::Value::String(s) => vec![s.clone()],
            serde_json::Value::Array(a) => a.iter().map(|t| t.as_str().unwrap_or("").to_string()).collect(),
            _ => Vec::new(),
        };
        if systemone {
            seen.systemone.lock().unwrap().push(req.clone());
        } else {
            seen.requests.fetch_add(1, Ordering::SeqCst);
            seen.batches.lock().unwrap().push(inputs.len());
            seen.bodies.lock().unwrap().push(req.clone());
        }
        match rude {
            Rude::HangUp => return,
            Rude::DelayMs(ms) => std::thread::sleep(std::time::Duration::from_millis(ms)),
            Rude::Status500 => {
                send(&mut out, "500 Internal Server Error", "text/plain; charset=utf-8", b"Internal Server Error", false);
                continue;
            }
            Rude::Polite | Rude::WrongRowCount | Rude::NoDecisionModel | Rude::NoulOutOfRange | Rude::RejectState(_) | Rude::UnloadFails | Rude::Redirect { .. } => {}
        }
        if systemone {
            answer_systemone(&mut out, &req, rude);
            continue;
        }
        let model = req["model"].as_str().unwrap_or("").to_string();
        if model != known {
            let f = fixture("model-missing");
            let captured_model = f["request"]["model"].as_str().unwrap();
            let error = f["response"]["error"].as_str().unwrap().replace(captured_model, &model);
            let status = format!("{} Not Found", f["status"]);
            send(&mut out, &status, "application/json; charset=utf-8", serde_json::json!({"error": error}).to_string().as_bytes(), false);
            continue;
        }
        let template = match &req["input"] {
            serde_json::Value::String(_) => "embed-single-string",
            _ if inputs.is_empty() => "embed-empty",
            _ => "embed-array-2",
        };
        let mut answer = fixture(template)["response"].clone();
        let mut rows: Vec<Vec<f32>> = inputs.iter().map(|t| vector(&t.to_lowercase(), dim)).collect();
        if matches!(rude, Rude::WrongRowCount) {
            rows.pop();
        }
        let chunked = !rows.is_empty();
        answer["model"] = serde_json::json!(model);
        answer["embeddings"] = serde_json::json!(rows);
        send(&mut out, "200 OK", "application/json; charset=utf-8", answer.to_string().as_bytes(), chunked);
    }
}

/// `POST /v1/systemone`, after the rude modes shared with embed: see "How the fake answers" in the README.
fn answer_systemone(out: &mut TcpStream, req: &serde_json::Value, rude: Rude) {
    let json = "application/json; charset=utf-8";
    let model = req["model"].as_str().unwrap_or("");
    if model != DECISION_MODEL || matches!(rude, Rude::NoDecisionModel) {
        let f = fixture("systemone-model-missing");
        let error = f["response"]["error"].as_str().unwrap().replace(f["request"]["model"].as_str().unwrap(), model);
        send(out, "404 Not Found", json, serde_json::json!({"error": error}).to_string().as_bytes(), false);
        return;
    }
    if !(1..=64).contains(&req["questions"].as_object().map_or(0, |q| q.len())) {
        send(out, "400 Bad Request", json, fixture("systemone-no-questions")["response"].to_string().as_bytes(), false);
        return;
    }
    let state = req["state"].as_str().unwrap_or("").to_lowercase();
    if matches!(rude, Rude::RejectState(t) if state.contains(&t.to_lowercase())) {
        send(out, "400 Bad Request", json, br#"{"error":"rejected"}"#, false);
        return;
    }
    let noul = if matches!(rude, Rude::NoulOutOfRange) {
        1.5
    } else if state.contains("wrong") || state.contains("told you") {
        0.9
    } else {
        0.1
    };
    let mut answer = fixture("systemone-noul")["response"].clone();
    answer["model"] = serde_json::json!(model);
    answer["answers"]["friction"]["noul"] = serde_json::json!(noul);
    send(out, "200 OK", json, answer.to_string().as_bytes(), false);
}

/// Write one answer. Its length is computed here: a wrong `Content-Length` makes ureq fail with
/// `Peer disconnected`. `chunked` sends the body as one chunk, the way Ollama sends a 200 with rows.
fn send(out: &mut TcpStream, status: &str, content_type: &str, body: &[u8], chunked: bool) {
    let head = if chunked {
        format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n", body.len())
    } else {
        format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n", body.len())
    };
    let _ = out.write_all(head.as_bytes());
    let _ = out.write_all(body);
    if chunked {
        let _ = out.write_all(b"\r\n0\r\n\r\n");
    }
    let _ = out.flush();
}

fn vector(text: &str, dim: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dim];
    for (i, words) in TOPICS.iter().enumerate() {
        if words.iter().any(|w| text.contains(w)) {
            v[i] = 1.0;
        }
    }
    v[dim - 1] = 0.1;
    v
}
