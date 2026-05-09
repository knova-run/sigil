//! Integration tests for `sigil contracts` — extract HTTP / gRPC / queue
//! contracts (providers + consumers) from source code.
//!
//! Output: JSONL, one row per contract entry, with at minimum:
//!   { kind, role, method?, path?, topic?, file, line }

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn run_contracts(root: &std::path::Path, extra: &[&str]) -> (String, String, bool) {
    let output = Command::new(env!("CARGO_BIN_EXE_sigil"))
        .arg("contracts")
        .arg("--root")
        .arg(root)
        .args(extra)
        .output()
        .expect("failed to run sigil");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

fn parse(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("each line should be JSON"))
        .collect()
}

#[test]
fn http_paths_are_normalized_to_match_repowise_form() {
    // Match repowise's normalize_http_path: path-style params (`:id`,
    // `{userId}`, `[id]`) collapse to `{param}`; query strings and trailing
    // slashes are stripped; case is lowered. The contract_id field is the
    // canonical join key for cross-repo matching.
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("api.py"),
        r#"@app.get("/Api/Users/{user_id}/")
def get_user(user_id: int): pass

@app.post("/api/items?page=1")
def list_items(): pass
"#,
    )
    .unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<&str> = rows.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&"http::GET::/api/users/{param}"),
        "expected normalized id 'http::GET::/api/users/{{param}}', got {ids:?}"
    );
    assert!(
        ids.contains(&"http::POST::/api/items"),
        "expected normalized id 'http::POST::/api/items', got {ids:?}"
    );
}

#[test]
fn detects_kafka_publisher_topic() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("worker.py"),
        r#"from kafka import KafkaProducer
producer = KafkaProducer(bootstrap_servers='broker:9092')

def emit(event):
    producer.send('notifications-events', value=event)
"#,
    )
    .unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let topics: Vec<&serde_json::Value> = rows.iter().filter(|r| r["kind"] == "topic").collect();
    assert!(
        topics.iter().any(|r| r["topic"] == "notifications-events" && r["role"] == "publisher"),
        "expected notifications-events publisher, got {topics:?}"
    );
}

#[test]
fn detects_grpc_service_and_rpc_methods_from_proto() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("notify.proto"),
        r#"syntax = "proto3";
package notify.v1;

service NotifyService {
  rpc SendEmail(SendEmailRequest) returns (SendEmailResponse);
  rpc SendSms(SendSmsRequest) returns (SendSmsResponse);
}

message SendEmailRequest { string to = 1; }
message SendEmailResponse { string id = 1; }
"#,
    )
    .unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let grpc: Vec<&serde_json::Value> = rows.iter().filter(|r| r["kind"] == "grpc").collect();
    assert!(
        grpc.iter().any(|r| r["path"] == "NotifyService/SendEmail" && r["role"] == "provider"),
        "expected NotifyService/SendEmail provider, got {grpc:?}"
    );
    assert!(
        grpc.iter().any(|r| r["path"] == "NotifyService/SendSms"),
        "expected NotifyService/SendSms, got {grpc:?}"
    );
}

#[test]
fn detects_express_route_provider_and_axios_consumer() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("server.js"),
        r#"const express = require('express');
const app = express();
app.get('/users/:id', (req, res) => res.json({}));
app.post('/users', (req, res) => res.json({}));
"#,
    )
    .unwrap();
    fs::write(
        tmp.path().join("client.ts"),
        r#"import axios from 'axios';
async function fetchUser(id: string) {
    return await axios.get(`/api/users/${id}`);
}
async function createUser() {
    return await axios.post('/api/users', { name: 'x' });
}
"#,
    )
    .unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let providers: Vec<&serde_json::Value> = rows.iter().filter(|r| r["role"] == "provider").collect();
    assert!(
        providers.iter().any(|r| r["contract_id"] == "http::GET::/users/{param}"),
        "expected GET /users/{{param}} provider contract_id, got {providers:?}"
    );
    let consumers: Vec<&serde_json::Value> = rows.iter().filter(|r| r["role"] == "consumer").collect();
    assert!(
        consumers.iter().any(|r| r["contract_id"] == "http::POST::/api/users"),
        "expected POST /api/users consumer contract_id, got {consumers:?}"
    );
}

#[test]
fn detects_fastapi_route_provider() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("api.py"),
        r#"from fastapi import FastAPI
app = FastAPI()

@app.get("/users/{user_id}")
def get_user(user_id: int):
    return {}

@app.post("/users")
def create_user():
    return {}
"#,
    )
    .unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let providers: Vec<&serde_json::Value> = rows.iter().filter(|r| r["role"] == "provider").collect();
    // path is the normalized form ({user_id} → {param}); the original raw
    // form is preserved in the framework signature on disk if needed.
    assert!(
        providers.iter().any(|r| r["contract_id"] == "http::GET::/users/{param}"),
        "expected GET /users/{{param}} contract_id, got {providers:?}"
    );
    assert!(
        providers.iter().any(|r| r["contract_id"] == "http::POST::/users"),
        "expected POST /users contract_id, got {providers:?}"
    );
}
