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

#[test]
fn detects_go_net_http_route_providers() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("main.go"),
        r#"package main

import "net/http"

func main() {
    http.Handle("/healthz", nil)
    http.HandleFunc("/users", handleUsers)
}

func handleUsers(w http.ResponseWriter, r *http.Request) {}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<&str> = rows.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    // Handle / HandleFunc don't carry a method — repowise emits `*`
    assert!(ids.contains(&"http::*::/healthz"), "expected /healthz; got {ids:?}");
    assert!(ids.contains(&"http::*::/users"), "expected /users; got {ids:?}");
}

#[test]
fn detects_go_gin_route_providers() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("server.go"),
        r#"package main

import "github.com/gin-gonic/gin"

func main() {
    r := gin.Default()
    r.GET("/users/:id", getUser)
    r.POST("/users", createUser)
    r.Run()
}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<&str> = rows.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"http::GET::/users/{param}"), "expected gin GET; got {ids:?}");
    assert!(ids.contains(&"http::POST::/users"), "expected gin POST; got {ids:?}");
}

#[test]
fn detects_python_requests_and_httpx_consumers() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("client.py"),
        r#"import requests
import httpx

requests.get("http://api.example.com/users")
httpx.post("/api/items", json={})
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let consumers: Vec<&serde_json::Value> = rows.iter().filter(|r| r["role"] == "consumer").collect();
    let ids: Vec<&str> = consumers.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"http::GET::/users"),
        "expected requests GET /users (scheme+host stripped); got {ids:?}");
    assert!(ids.contains(&"http::POST::/api/items"),
        "expected httpx POST /api/items; got {ids:?}");
}

#[test]
fn detects_js_fetch_consumer() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("client.js"),
        r#"fetch('/api/users');
fetch('/api/items', { method: 'POST', body: '{}' });
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let consumers: Vec<&serde_json::Value> = rows.iter().filter(|r| r["role"] == "consumer").collect();
    let ids: Vec<&str> = consumers.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"http::GET::/api/users"),
        "expected fetch defaults to GET; got {ids:?}");
    assert!(ids.contains(&"http::POST::/api/items"),
        "expected fetch with method:POST; got {ids:?}");
}

#[test]
fn detects_spring_mapping_annotations() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("UserController.java"),
        r#"package com.example;

import org.springframework.web.bind.annotation.*;

@RestController
public class UserController {
    @GetMapping("/users")
    public List<User> list() { return null; }

    @PostMapping("/users")
    public User create() { return null; }

    @DeleteMapping(value = "/users/{id}")
    public void delete(@PathVariable Long id) {}
}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<&str> = rows.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"http::GET::/users"), "Spring GET; got {ids:?}");
    assert!(ids.contains(&"http::POST::/users"), "Spring POST; got {ids:?}");
    assert!(ids.contains(&"http::DELETE::/users/{param}"),
        "Spring @DeleteMapping(value=...); got {ids:?}");
}

#[test]
fn fastapi_regex_does_not_match_mock_patch_decorators() {
    // PostHog audit caught this: the pre-tightening regex matched any
    // `@<ident>.<verb>(...)` and emitted 282 false "fastapi" routes
    // from `@mock.patch(...)` / `@pytest.fixture` / similar test
    // decorators in posthog's tests/.
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("test_things.py"),
        r#"from unittest import mock
import pytest

@mock.patch("products.something.helper")
def test_a(): pass

@mock.patch("products.other.thing.cancel_workflow")
def test_b(): pass

@pytest.fixture
def some_fixture(): pass
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let fastapi_rows: Vec<_> = rows.iter().filter(|r| r["framework"] == "fastapi").collect();
    assert!(
        fastapi_rows.is_empty(),
        "expected 0 fastapi routes from @mock.patch decorators; got {fastapi_rows:?}"
    );
}

#[test]
fn express_regex_does_not_match_random_get_calls() {
    // PostHog audit: 93% of pre-fix "express" rows were false positives
    // from `params.get('ordering')`, `cache.get(...)`, `redis.get(...)`,
    // `Map.get(...)` etc. The fix requires receiver ∈ {app, router, …,
    // *Router} AND path starts with `/`.
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("client.ts"),
        r#"const params = new URLSearchParams(window.location.search);
const v1 = params.get('ordering');
const v2 = cache.get('user-id');
const v3 = redis.get('session');
const v4 = pipeline.get('orgId');
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let express_rows: Vec<_> = rows.iter().filter(|r| r["framework"] == "express").collect();
    assert!(
        express_rows.is_empty(),
        "expected 0 express routes from data-structure .get() calls; got {express_rows:?}"
    );
}

#[test]
fn detects_django_path_and_re_path_providers() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("urls.py"),
        r#"from django.urls import path, re_path
from . import views

urlpatterns = [
    path('api/projects/<int:pk>/', views.project_detail),
    path('api/users/', views.user_list),
    re_path(r'^api/teams/(?P<team_id>\d+)/$', views.team_detail),
]
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<&str> = rows.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&"http::*::/api/projects/{param}"),
        "expected Django path('api/projects/<int:pk>/'); got {ids:?}"
    );
    assert!(
        ids.contains(&"http::*::/api/users"),
        "expected Django path('api/users/'); got {ids:?}"
    );
    let drf = rows.iter().find(|r| r["contract_id"] == "http::*::/api/projects/{param}").unwrap();
    assert_eq!(drf["framework"], "django");
}

#[test]
fn detects_drf_router_register_and_action_decorator() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("urls.py"),
        r#"from rest_framework import routers
from . import views

router = routers.DefaultRouter()
router.register(r'projects', views.ProjectViewSet, basename='project')
router.register(r'dashboards', views.DashboardViewSet)
"#,
    ).unwrap();
    fs::write(
        tmp.path().join("views.py"),
        r#"from rest_framework.decorators import action
from rest_framework import viewsets

class ProjectViewSet(viewsets.ModelViewSet):
    @action(detail=True, methods=['POST'], url_path='archive')
    def archive(self, request, pk=None):
        pass

    @action(detail=False, methods=['GET'])
    def list_active(self, request):
        pass
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<&str> = rows.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&"http::*::/projects"),
        "router.register('projects'); got {ids:?}"
    );
    assert!(
        ids.contains(&"http::*::/dashboards"),
        "router.register('dashboards'); got {ids:?}"
    );
    assert!(
        ids.contains(&"http::POST::/archive"),
        "@action with explicit url_path; got {ids:?}"
    );
}

#[test]
fn detects_rails_route_providers() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("config");
    fs::create_dir(&cfg).unwrap();
    fs::write(
        cfg.join("routes.rb"),
        r#"Rails.application.routes.draw do
  get '/api/articles', to: 'articles#index'
  post '/api/articles', to: 'articles#create'
  delete '/api/articles/:id', to: 'articles#destroy'
  resources :users
  resources :comments, only: [:index, :create]
end
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<&str> = rows.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"http::GET::/api/articles"), "GET /api/articles; got {ids:?}");
    assert!(ids.contains(&"http::POST::/api/articles"), "POST; got {ids:?}");
    assert!(ids.contains(&"http::DELETE::/api/articles/{param}"), "DELETE w/ :id; got {ids:?}");
    assert!(ids.contains(&"http::*::/users"), "resources :users → base /users; got {ids:?}");
    let rails_rows: Vec<_> = rows.iter().filter(|r| r["framework"] == "rails").collect();
    assert!(!rails_rows.is_empty(), "expected at least one row tagged framework=rails");
}

#[test]
fn detects_rust_axum_route_providers() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("main.rs"),
        r#"use axum::{Router, routing::{get, post, delete}};

fn app() -> Router {
    Router::new()
        .route("/users", get(list_users))
        .route("/users", post(create_user))
        .route("/users/:id", delete(delete_user))
}

async fn list_users() {}
async fn create_user() {}
async fn delete_user() {}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<&str> = rows.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"http::GET::/users"), "axum GET; got {ids:?}");
    assert!(ids.contains(&"http::POST::/users"), "axum POST; got {ids:?}");
    assert!(ids.contains(&"http::DELETE::/users/{param}"), "axum DELETE w/ :id; got {ids:?}");
}

#[test]
fn detects_rust_rocket_route_macros() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("rocket_app.rs"),
        r#"use rocket::*;

#[get("/hello")]
fn hello() -> &'static str { "Hello, world!" }

#[post("/items", data = "<item>")]
fn create_item(item: String) -> &'static str { "ok" }
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<&str> = rows.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"http::GET::/hello"), "rocket #[get(...)]; got {ids:?}");
    assert!(ids.contains(&"http::POST::/items"), "rocket #[post(..., data=...)]; got {ids:?}");
}

#[test]
fn detects_php_laravel_route_providers() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("api.php"),
        r#"<?php

use App\Http\Controllers\UserController;

Route::get('/users', [UserController::class, 'index']);
Route::post('/users', [UserController::class, 'store']);
Route::put('/users/{id}', [UserController::class, 'update']);
Route::delete('/users/{id}', 'UserController@destroy');
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<&str> = rows.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"http::GET::/users"), "Laravel GET; got {ids:?}");
    assert!(ids.contains(&"http::POST::/users"), "Laravel POST; got {ids:?}");
    assert!(ids.contains(&"http::PUT::/users/{param}"), "Laravel PUT w/ {{id}}; got {ids:?}");
    assert!(ids.contains(&"http::DELETE::/users/{param}"), "Laravel DELETE; got {ids:?}");
}

#[test]
fn detects_aspnet_http_attributes_and_minimal_api() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("UsersController.cs"),
        r#"using Microsoft.AspNetCore.Mvc;

[ApiController]
[Route("api/users")]
public class UsersController : ControllerBase
{
    [HttpGet("{id}")]
    public IActionResult GetById(int id) => Ok();

    [HttpPost]
    public IActionResult Create() => Ok();
}
"#,
    ).unwrap();
    fs::write(
        tmp.path().join("Program.cs"),
        r#"var builder = WebApplication.CreateBuilder(args);
var app = builder.Build();

app.MapGet("/health", () => "ok");
app.MapPost("/items", (Item i) => Results.Created());
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<&str> = rows.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"http::GET::/{param}"), "[HttpGet(\"{{id}}\")]; got {ids:?}");
    assert!(ids.contains(&"http::POST::*"), "bare [HttpPost]; got {ids:?}");
    assert!(ids.contains(&"http::GET::/health"), "MapGet minimal API; got {ids:?}");
    assert!(ids.contains(&"http::POST::/items"), "MapPost minimal API; got {ids:?}");
}

#[test]
fn detects_csharp_httpclient_consumer() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("client.cs"),
        r#"using System.Net.Http;

public class ApiClient
{
    private readonly HttpClient _client;
    public async Task FetchUsers() {
        await _client.GetAsync("http://api.example.com/users");
        await _client.PostAsync("/items", null);
    }
}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let consumers: Vec<&serde_json::Value> = rows.iter().filter(|r| r["role"] == "consumer").collect();
    let ids: Vec<&str> = consumers.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"http::GET::/users"), "GetAsync; got {ids:?}");
    assert!(ids.contains(&"http::POST::/items"), "PostAsync; got {ids:?}");
}

#[test]
fn detects_rust_reqwest_consumer() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("client.rs"),
        r#"use reqwest::Client;

async fn fetch() -> anyhow::Result<()> {
    let client = Client::new();
    client.get("/api/users").send().await?;
    client.post("/api/items").json(&body).send().await?;
    Ok(())
}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let consumers: Vec<&serde_json::Value> = rows.iter().filter(|r| r["role"] == "consumer").collect();
    let ids: Vec<&str> = consumers.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"http::GET::/api/users"), "reqwest get; got {ids:?}");
    assert!(ids.contains(&"http::POST::/api/items"), "reqwest post; got {ids:?}");
}

#[test]
fn detects_python_grpc_servicer_and_stub() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("server.py"),
        r#"import grpc
from . import users_pb2_grpc

class UserServicer(users_pb2_grpc.UserServiceServicer):
    pass

def serve():
    server = grpc.server(...)
    users_pb2_grpc.add_UserServiceServicer_to_server(UserServicer(), server)
"#,
    ).unwrap();
    fs::write(
        tmp.path().join("client.py"),
        r#"import grpc
from . import users_pb2_grpc

channel = grpc.insecure_channel('localhost:50051')
stub = users_pb2_grpc.UserServiceStub(channel)
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let providers: Vec<_> = rows.iter()
        .filter(|r| r["kind"] == "grpc" && r["role"] == "provider").collect();
    let consumers: Vec<_> = rows.iter()
        .filter(|r| r["kind"] == "grpc" && r["role"] == "consumer").collect();
    assert!(
        providers.iter().any(|r| r["contract_id"] == "grpc::UserService"),
        "expected grpc::UserService provider from add_*Servicer_to_server; got {providers:?}"
    );
    assert!(
        consumers.iter().any(|r| r["contract_id"] == "grpc::UserService"),
        "expected grpc::UserService consumer from *Stub(channel); got {consumers:?}"
    );
}

#[test]
fn detects_java_grpc_impl_base_and_blocking_stub() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("UserServiceImpl.java"),
        r#"package com.example;
import io.grpc.stub.StreamObserver;

public class UserServiceImpl extends UserServiceGrpc.UserServiceImplBase {
    @Override
    public void getUser(Request req, StreamObserver<Response> resp) {}
}
"#,
    ).unwrap();
    fs::write(
        tmp.path().join("Client.java"),
        r#"package com.example;
import io.grpc.ManagedChannel;

public class Client {
    void call() {
        UserServiceGrpc.UserServiceBlockingStub stub = UserServiceGrpc.newBlockingStub(channel);
    }
}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<&str> = rows.iter().filter(|r| r["kind"] == "grpc")
        .map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.iter().any(|id| *id == "grpc::UserService"),
        "expected grpc::UserService from Java provider+consumer; got {ids:?}");
}

#[test]
fn detects_csharp_grpc_server_and_client() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("Program.cs"),
        r#"var builder = WebApplication.CreateBuilder(args);
var app = builder.Build();
app.MapGrpcService<UserServiceImpl>();
"#,
    ).unwrap();
    fs::write(
        tmp.path().join("UserClient.cs"),
        r#"using Grpc.Net.Client;

public class UserClient {
    public void Call() {
        var channel = GrpcChannel.ForAddress("https://localhost:5001");
        var client = new UserServiceClient(channel);
    }
}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let grpc: Vec<&serde_json::Value> = rows.iter().filter(|r| r["kind"] == "grpc").collect();
    let provider = grpc.iter().find(|r| r["role"] == "provider")
        .unwrap_or_else(|| panic!("no C# grpc provider in {grpc:?}"));
    let consumer = grpc.iter().find(|r| r["role"] == "consumer")
        .unwrap_or_else(|| panic!("no C# grpc consumer in {grpc:?}"));
    assert_eq!(provider["contract_id"], "grpc::UserService");
    assert_eq!(consumer["contract_id"], "grpc::UserService");
}

#[test]
fn detects_redis_pubsub_python() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("worker.py"),
        r#"import redis

r = redis.Redis()
r.publish('user-events', 'created')

pubsub = r.pubsub()
pubsub.subscribe('user-events', 'order-events')
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<(&str, &str, &str)> = rows.iter()
        .map(|r| (r["contract_id"].as_str().unwrap(),
                  r["role"].as_str().unwrap(),
                  r["framework"].as_str().unwrap())).collect();
    assert!(ids.iter().any(|t| t.0 == "topic::user-events" && t.1 == "publisher" && t.2 == "redis"),
        "expected publisher to user-events; got {ids:?}");
    assert!(ids.iter().any(|t| t.0 == "topic::user-events" && t.1 == "subscriber"),
        "expected subscriber for user-events; got {ids:?}");
    assert!(ids.iter().any(|t| t.0 == "topic::order-events" && t.1 == "subscriber"),
        "expected subscriber for order-events (multi-arg subscribe); got {ids:?}");
}

#[test]
fn detects_redis_pubsub_js_node() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("worker.js"),
        r#"const Redis = require('ioredis');
const pub = new Redis();
const sub = new Redis();

pub.publish('jobs', JSON.stringify(job));
sub.subscribe('jobs', (err, count) => {});
sub.subscribe('emails');
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<&str> = rows.iter()
        .filter(|r| r["framework"] == "redis")
        .map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"topic::jobs"), "jobs topic; got {ids:?}");
    assert!(ids.contains(&"topic::emails"), "emails topic; got {ids:?}");
}

#[test]
fn detects_redis_pubsub_go() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("worker.go"),
        r#"package main

import "github.com/redis/go-redis/v9"

func main() {
    rdb := redis.NewClient(...)
    rdb.Publish(ctx, "jobs", payload)
    pubsub := rdb.Subscribe(ctx, "jobs", "alerts")
}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ids: Vec<(&str, &str)> = rows.iter()
        .filter(|r| r["framework"] == "redis")
        .map(|r| (r["contract_id"].as_str().unwrap(),
                  r["role"].as_str().unwrap())).collect();
    assert!(ids.iter().any(|t| t == &("topic::jobs", "publisher")),
        "Go publish; got {ids:?}");
    assert!(ids.iter().any(|t| t == &("topic::jobs", "subscriber")),
        "Go subscribe (first arg); got {ids:?}");
    assert!(ids.iter().any(|t| t == &("topic::alerts", "subscriber")),
        "Go subscribe (multi-arg); got {ids:?}");
}

#[test]
fn detects_nats_pubsub() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("nats_worker.py"),
        r#"import nats

async def run():
    nc = await nats.connect("nats://localhost:4222")
    await nc.publish("events.user.created", payload)
    await nc.subscribe("events.user.>", cb=handle_user)
"#,
    ).unwrap();
    fs::write(
        tmp.path().join("nats_worker.go"),
        r#"package main
import "github.com/nats-io/nats.go"

func main() {
    nc, _ := nats.Connect("nats://localhost:4222")
    nc.Publish("events.order.placed", payload)
    nc.Subscribe("events.order.>", handleOrder)
}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let nats: Vec<&serde_json::Value> = rows.iter().filter(|r| r["framework"] == "nats").collect();
    let pubs: Vec<&str> = nats.iter().filter(|r| r["role"] == "publisher")
        .map(|r| r["contract_id"].as_str().unwrap()).collect();
    let subs: Vec<&str> = nats.iter().filter(|r| r["role"] == "subscriber")
        .map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(pubs.iter().any(|s| s == &"topic::events.user.created"),
        "Python NATS publish; got pubs={pubs:?}");
    assert!(pubs.iter().any(|s| s == &"topic::events.order.placed"),
        "Go NATS publish; got pubs={pubs:?}");
    assert!(!subs.is_empty(), "expected NATS subscribers; got {nats:?}");
}

// ───── Batch 2: WebSocket contracts (URL routes + socket.io events) ─────

#[test]
fn detects_websocket_routes_fastapi() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("app.py"),
        r#"from fastapi import FastAPI, WebSocket
app = FastAPI()

@app.websocket("/ws/chat")
async def chat(ws: WebSocket):
    await ws.accept()
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ws: Vec<_> = rows.iter().filter(|r| r["kind"] == "websocket").collect();
    assert!(ws.iter().any(|r| r["contract_id"] == "ws::/ws/chat" && r["role"] == "provider"),
        "FastAPI @app.websocket; got {ws:?}");
}

#[test]
fn detects_websocket_routes_express_and_browser_client() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("server.js"),
        r#"const expressWs = require('express-ws')(app);
app.ws('/ws/notifications', (ws, req) => {
    ws.on('message', msg => ws.send(msg));
});
"#,
    ).unwrap();
    fs::write(
        tmp.path().join("client.js"),
        r#"const ws = new WebSocket('ws://localhost/ws/notifications');
const wss = new WebSocket('wss://api.example.com/ws/chat');
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ws: Vec<_> = rows.iter().filter(|r| r["kind"] == "websocket").collect();
    assert!(ws.iter().any(|r| r["contract_id"] == "ws::/ws/notifications" && r["role"] == "provider"),
        "express app.ws('/path'); got {ws:?}");
    assert!(ws.iter().any(|r| r["contract_id"] == "ws::/ws/notifications" && r["role"] == "consumer"),
        "browser new WebSocket(ws://.../path); got {ws:?}");
    assert!(ws.iter().any(|r| r["contract_id"] == "ws::/ws/chat"),
        "browser new WebSocket(wss://.../path); got {ws:?}");
}

#[test]
fn detects_socketio_event_provider_and_consumer() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("server.js"),
        r#"const io = require('socket.io')(server);
io.on('connection', (socket) => {
    socket.on('chat:send', (msg) => {});
    socket.on('user:join', (u) => {});
});
"#,
    ).unwrap();
    fs::write(
        tmp.path().join("client.js"),
        r#"import { io } from 'socket.io-client';
const socket = io('http://localhost');
socket.emit('chat:send', { text: 'hi' });
socket.emit('user:join', { name: 'a' });
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let events: Vec<_> = rows.iter().filter(|r| r["framework"] == "socket.io").collect();
    assert!(events.iter().any(|r| r["contract_id"] == "event::chat:send" && r["role"] == "provider"),
        "socket.io on('chat:send'); got {events:?}");
    assert!(events.iter().any(|r| r["contract_id"] == "event::chat:send" && r["role"] == "consumer"),
        "socket.io emit('chat:send'); got {events:?}");
    assert!(events.iter().any(|r| r["contract_id"] == "event::user:join" && r["role"] == "provider"));
    assert!(events.iter().any(|r| r["contract_id"] == "event::user:join" && r["role"] == "consumer"));
}

#[test]
fn detects_websocket_go_gorilla() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("server.go"),
        r#"package main

import (
    "net/http"
    "github.com/gorilla/websocket"
)

var upgrader = websocket.Upgrader{}

func main() {
    http.HandleFunc("/ws/feed", func(w http.ResponseWriter, r *http.Request) {
        conn, _ := upgrader.Upgrade(w, r, nil)
        _ = conn
    })
}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    // The HandleFunc emits http::*::/ws/feed already; the upgrader inside
    // the handler retags it as a WebSocket provider for the same path.
    let ws: Vec<_> = rows.iter().filter(|r| r["kind"] == "websocket").collect();
    assert!(ws.iter().any(|r| r["contract_id"] == "ws::/ws/feed" && r["role"] == "provider"),
        "Go gorilla upgrader inside /ws/feed handler; got {ws:?}");
}

#[test]
fn detects_websocket_spring_and_actioncable() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("ChatHandler.java"),
        r#"@Controller
public class ChatHandler {
    @MessageMapping("/chat.send")
    public void send(ChatMessage msg) {}
}
"#,
    ).unwrap();
    fs::write(
        tmp.path().join("config.rb"),
        r#"Rails.application.routes.draw do
  mount ActionCable.server => '/cable'
end
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let ws: Vec<_> = rows.iter().filter(|r| r["kind"] == "websocket").collect();
    assert!(ws.iter().any(|r| r["contract_id"] == "ws::/chat.send"),
        "Spring @MessageMapping; got {ws:?}");
    assert!(ws.iter().any(|r| r["contract_id"] == "ws::/cable"),
        "Rails ActionCable mount; got {ws:?}");
}

// ───── Batch 1: gRPC + Redis + NATS expanded across languages ─────

#[test]
fn detects_rust_tonic_grpc_server_and_client() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("server.rs"),
        r#"use tonic::transport::Server;

pub mod users { tonic::include_proto!("users"); }

#[derive(Default)]
pub struct MyService {}

#[tonic::async_trait]
impl users::user_service_server::UserService for MyService {
    async fn get_user(&self, req: tonic::Request<users::Request>) -> Result<tonic::Response<users::Reply>, tonic::Status> { todo!() }
}

#[tokio::main]
async fn main() {
    Server::builder()
        .add_service(users::user_service_server::UserServiceServer::new(MyService::default()))
        .serve("[::1]:50051".parse().unwrap()).await.unwrap();
}
"#,
    ).unwrap();
    fs::write(
        tmp.path().join("client.rs"),
        r#"use tonic::transport::Channel;
pub mod users { tonic::include_proto!("users"); }

async fn run() {
    let mut client = users::user_service_client::UserServiceClient::connect("http://[::1]:50051").await.unwrap();
}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    let grpc: Vec<&serde_json::Value> = rows.iter().filter(|r| r["kind"] == "grpc").collect();
    assert!(grpc.iter().any(|r| r["role"] == "provider" && r["contract_id"] == "grpc::UserService"),
        "tonic provider; got {grpc:?}");
    assert!(grpc.iter().any(|r| r["role"] == "consumer" && r["contract_id"] == "grpc::UserService"),
        "tonic consumer; got {grpc:?}");
}

#[test]
fn detects_node_grpc_js_server_and_client() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("server.js"),
        r#"const grpc = require('@grpc/grpc-js');
const protoLoader = require('@grpc/proto-loader');

const server = new grpc.Server();
server.addService(usersProto.UserService.service, { GetUser: getUser });
"#,
    ).unwrap();
    fs::write(
        tmp.path().join("client.js"),
        r#"const grpc = require('@grpc/grpc-js');
const client = new usersProto.UserService('localhost:50051', grpc.credentials.createInsecure());
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let grpc: Vec<serde_json::Value> = parse(&stdout)
        .into_iter().filter(|r| r["kind"] == "grpc").collect();
    let ids: Vec<&str> = grpc.iter().map(|r| r["contract_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"grpc::UserService"),
        "@grpc/grpc-js server.addService(UserService.service); got {ids:?}");
}

#[test]
fn detects_ruby_grpc_server_and_stub() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("server.rb"),
        r#"require 'grpc'
require 'users_services_pb'

class UserServiceImpl < Users::UserService::Service
  def get_user(req, _call)
    Users::Reply.new
  end
end
"#,
    ).unwrap();
    fs::write(
        tmp.path().join("client.rb"),
        r#"require 'grpc'
require 'users_services_pb'

stub = Users::UserService::Stub.new('localhost:50051', :this_channel_is_insecure)
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let grpc: Vec<serde_json::Value> = parse(&stdout)
        .into_iter().filter(|r| r["kind"] == "grpc").collect();
    let by_role: std::collections::HashMap<&str, Vec<&str>> = {
        let mut m: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
        for r in &grpc {
            m.entry(r["role"].as_str().unwrap())
                .or_default()
                .push(r["contract_id"].as_str().unwrap());
        }
        m
    };
    assert!(by_role.get("provider").map(|v| v.contains(&"grpc::UserService")).unwrap_or(false),
        "Ruby `< X::Y::Service` provider; got {grpc:?}");
    assert!(by_role.get("consumer").map(|v| v.contains(&"grpc::UserService")).unwrap_or(false),
        "Ruby `X::Y::Stub.new` consumer; got {grpc:?}");
}

#[test]
fn detects_rust_redis_and_nats_pubsub() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("redis_worker.rs"),
        r#"use redis::Commands;

fn run(con: &mut redis::Connection) -> redis::RedisResult<()> {
    con.publish("user-events", "created")?;
    let mut pubsub = con.as_pubsub();
    pubsub.subscribe("user-events")?;
    Ok(())
}
"#,
    ).unwrap();
    fs::write(
        tmp.path().join("nats_worker.rs"),
        r#"use async_nats;

async fn run(client: async_nats::Client) {
    client.publish("events.user.created", "payload".into()).await.unwrap();
    let _ = client.subscribe("events.user.>").await.unwrap();
}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let topics: Vec<serde_json::Value> = parse(&stdout)
        .into_iter().filter(|r| r["kind"] == "topic").collect();
    let triples: Vec<(String, String, String)> = topics.iter()
        .map(|r| (r["contract_id"].as_str().unwrap().to_string(),
                  r["role"].as_str().unwrap().to_string(),
                  r["framework"].as_str().unwrap().to_string()))
        .collect();
    assert!(triples.iter().any(|(c, r, f)| c == "topic::user-events" && r == "publisher" && f == "redis"),
        "Rust redis-rs publish; got {triples:?}");
    assert!(triples.iter().any(|(c, r, _)| c == "topic::user-events" && r == "subscriber"),
        "Rust redis-rs subscribe; got {triples:?}");
    assert!(triples.iter().any(|(c, _, f)| c == "topic::events.user.created" && f == "nats"),
        "Rust async-nats publish; got {triples:?}");
}

#[test]
fn detects_java_jedis_redis_pubsub() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("Worker.java"),
        r#"import redis.clients.jedis.Jedis;
import redis.clients.jedis.JedisPubSub;

public class Worker {
    public void run(Jedis jedis) {
        jedis.publish("user-events", "payload");
        jedis.subscribe(new MyListener(), "user-events", "order-events");
    }
}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let topics: Vec<serde_json::Value> = parse(&stdout)
        .into_iter().filter(|r| r["kind"] == "topic").collect();
    let ids: Vec<(&str, &str)> = topics.iter()
        .map(|r| (r["contract_id"].as_str().unwrap(),
                  r["role"].as_str().unwrap())).collect();
    assert!(ids.contains(&("topic::user-events", "publisher")), "Jedis publish; got {ids:?}");
    assert!(ids.contains(&("topic::user-events", "subscriber")), "Jedis subscribe; got {ids:?}");
    assert!(ids.contains(&("topic::order-events", "subscriber")), "Jedis multi-arg subscribe; got {ids:?}");
}

#[test]
fn detects_csharp_stackexchange_redis_pubsub() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("Worker.cs"),
        r#"using StackExchange.Redis;

public class Worker {
    public void Run(ISubscriber sub) {
        sub.Publish("user-events", "payload");
        sub.Subscribe("user-events", (channel, msg) => {});
    }
}
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let topics: Vec<serde_json::Value> = parse(&stdout)
        .into_iter().filter(|r| r["kind"] == "topic").collect();
    let ids: Vec<(&str, &str)> = topics.iter()
        .map(|r| (r["contract_id"].as_str().unwrap(),
                  r["role"].as_str().unwrap())).collect();
    assert!(ids.contains(&("topic::user-events", "publisher")), "C# Publish; got {ids:?}");
    assert!(ids.contains(&("topic::user-events", "subscriber")), "C# Subscribe; got {ids:?}");
}

#[test]
fn detects_ruby_redis_pubsub() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("worker.rb"),
        r#"require 'redis'

redis = Redis.new
redis.publish('user-events', 'payload')
redis.subscribe('user-events', 'order-events') do |on|
  on.message { |ch, msg| puts msg }
end
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let topics: Vec<serde_json::Value> = parse(&stdout)
        .into_iter().filter(|r| r["kind"] == "topic").collect();
    let ids: Vec<(&str, &str)> = topics.iter()
        .map(|r| (r["contract_id"].as_str().unwrap(),
                  r["role"].as_str().unwrap())).collect();
    assert!(ids.contains(&("topic::user-events", "publisher")), "Ruby redis publish; got {ids:?}");
    assert!(ids.contains(&("topic::user-events", "subscriber")), "Ruby redis subscribe; got {ids:?}");
}

#[test]
fn detects_php_predis_pubsub() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("worker.php"),
        r#"<?php
use Predis\Client;
$client = new Client();
$client->publish('user-events', 'payload');
$pubsub = $client->pubSubLoop();
$pubsub->subscribe('user-events');
"#,
    ).unwrap();
    let (stdout, stderr, ok) = run_contracts(tmp.path(), &[]);
    assert!(ok, "stderr: {stderr}");
    let topics: Vec<serde_json::Value> = parse(&stdout)
        .into_iter().filter(|r| r["kind"] == "topic").collect();
    let ids: Vec<(&str, &str)> = topics.iter()
        .map(|r| (r["contract_id"].as_str().unwrap(),
                  r["role"].as_str().unwrap())).collect();
    assert!(ids.contains(&("topic::user-events", "publisher")), "Predis publish; got {ids:?}");
    assert!(ids.contains(&("topic::user-events", "subscriber")), "Predis subscribe; got {ids:?}");
}

#[test]
fn contracts_works_on_workspace_root() {
    use std::process::Command;
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    let api = tmp.path().join("api");
    let client = tmp.path().join("client");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&api).unwrap();
    fs::create_dir_all(&client).unwrap();

    // Make each a git repo
    for p in [&api, &client] {
        Command::new("git").args(["init", "-q"]).current_dir(p).output().unwrap();
    }

    fs::write(api.join("app.py"),
        "@app.get('/users')\ndef list_users(): return []\n").unwrap();
    fs::write(client.join("client.js"),
        "fetch('/users');\n").unwrap();

    Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["workspace", "init", ws.to_str().unwrap()])
        .output().unwrap();
    Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["workspace", "add", api.to_str().unwrap(), "--root", ws.to_str().unwrap()])
        .output().unwrap();
    Command::new(env!("CARGO_BIN_EXE_sigil"))
        .args(["workspace", "add", client.to_str().unwrap(), "--root", ws.to_str().unwrap()])
        .output().unwrap();

    let (stdout, stderr, ok) = run_contracts(&ws, &[]);
    assert!(ok, "stderr: {stderr}");
    let rows = parse(&stdout);
    // Workspace contracts must surface BOTH members. Each row must carry
    // a `repo` field so consumers know which member it came from.
    let provider = rows.iter().find(|r| r["role"] == "provider")
        .unwrap_or_else(|| panic!("no provider in {rows:?}"));
    assert_eq!(provider["repo"].as_str(), Some("api"));
    assert_eq!(provider["contract_id"].as_str(), Some("http::GET::/users"));

    let consumer = rows.iter().find(|r| r["role"] == "consumer")
        .unwrap_or_else(|| panic!("no consumer in {rows:?}"));
    assert_eq!(consumer["repo"].as_str(), Some("client"));
    assert_eq!(consumer["contract_id"].as_str(), Some("http::GET::/users"));
}
