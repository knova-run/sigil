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
