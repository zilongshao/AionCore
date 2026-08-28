mod common;

use aionui_common::ProviderWithModel;
use aionui_db::{
    CreateProviderParams, IConversationRepository, IProviderRepository, MessagePageDirection, MessagePageParams,
    SqliteProviderRepository,
};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tower::ServiceExt;

use aionui_api_types::TeamMcpStdioConfig;
use aionui_team::mcp::protocol::{read_frame, write_frame};
use common::{
    body_json, build_app, build_app_with_mock_agents, delete_with_token, get_request, get_with_token, json_with_token,
    setup_and_login,
};

const DEFAULT_TEAM_ASSISTANT_ID: &str = "team-e2e-assistant";
const DEFAULT_TEAM_AGENT_ID: &str = "632f31d2";
const HERMES_TEAM_ASSISTANT_ID: &str = "team-e2e-hermes-assistant";
const HERMES_AGENT_ID: &str = "55f3ed1c";

fn team_agent(name: &str, role: &str) -> serde_json::Value {
    json!({
        "name": name,
        "role": role,
        "model": "claude",
        "assistant_id": DEFAULT_TEAM_ASSISTANT_ID
    })
}

fn two_agent_body() -> serde_json::Value {
    json!({
        "name": "Alpha",
        "agents": [
            team_agent("Lead", "lead"),
            team_agent("Worker", "teammate")
        ]
    })
}

async fn ensure_default_team_agent_installed(services: &aionui_app::AppServices) {
    let provider_repo = SqliteProviderRepository::new(services.database.pool().clone());
    provider_repo
        .create(CreateProviderParams {
            id: None,
            platform: "openai",
            name: "Team E2E Provider",
            base_url: "https://example.invalid",
            api_key_encrypted: "stub",
            models: r#"["claude"]"#,
            enabled: true,
            capabilities: "[]",
            context_limit: None,
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            model_settings: "{}",
            bedrock_config: None,
            is_full_url: false,
        })
        .await
        .expect("seed deterministic team provider");

    let command = std::env::current_exe()
        .expect("test executable path")
        .to_string_lossy()
        .to_string();
    let source_info = json!({ "binary_name": command }).to_string();

    sqlx::query(
        "UPDATE agent_metadata \
         SET agent_source = 'internal', agent_source_info = ?, command = ?, args = '[]', env = '[]', \
             updated_at = unixepoch('now','subsec') * 1000 \
         WHERE id = ?",
    )
    .bind(&source_info)
    .bind(&command)
    .bind(DEFAULT_TEAM_AGENT_ID)
    .execute(services.database.pool())
    .await
    .expect("seed deterministic team agent");

    services
        .agent_registry
        .reload_one(DEFAULT_TEAM_AGENT_ID)
        .await
        .expect("reload deterministic team agent");
}

async fn ensure_default_team_assistant(
    app: &mut axum::Router,
    services: &aionui_app::AppServices,
    token: &str,
    csrf: &str,
) {
    ensure_default_team_agent_installed(services).await;
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({
            "id": DEFAULT_TEAM_ASSISTANT_ID,
            "name": "Team E2E Assistant",
            "agent_id": DEFAULT_TEAM_AGENT_ID
        }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert!(
        resp.status() == StatusCode::CREATED || resp.status() == StatusCode::CONFLICT,
        "expected team assistant seed to be created or already exist, got {}",
        resp.status()
    );
}

async fn mark_hermes_backend_team_mcp_stdio_capable(services: &aionui_app::AppServices) {
    let capabilities = json!({
        "mcp_capabilities": { "stdio": true },
        "shell": true
    })
    .to_string();
    let command = std::env::current_exe()
        .expect("test executable path")
        .to_string_lossy()
        .to_string();
    let source_info = json!({ "binary_name": command }).to_string();
    let result = sqlx::query(
        "UPDATE agent_metadata \
         SET agent_capabilities = ?, command = ?, agent_source_info = ?, \
             last_check_status = 'online', last_check_kind = 'manual', \
             last_check_error_code = NULL, last_check_error_message = NULL, last_check_guidance = NULL, \
             last_check_at = unixepoch('now','subsec') * 1000, \
             last_success_at = unixepoch('now','subsec') * 1000, \
             updated_at = unixepoch('now','subsec') * 1000 \
         WHERE agent_type = 'acp' AND backend = 'hermes'",
    )
    .bind(capabilities)
    .bind(&command)
    .bind(source_info)
    .execute(services.database.pool())
    .await
    .expect("mark Hermes backend as team MCP capable");
    assert!(
        result.rows_affected() > 0,
        "fixture must include Hermes ACP backend metadata"
    );
    services
        .agent_registry
        .reload_one(HERMES_AGENT_ID)
        .await
        .expect("reload Hermes capability update");
}

async fn create_team_test_provider(
    services: &aionui_app::AppServices,
    name: &str,
    model: &str,
    enabled: bool,
) -> aionui_db::models::Provider {
    let models = serde_json::to_string(&vec![model]).unwrap();
    SqliteProviderRepository::new(services.database.pool().clone())
        .create(CreateProviderParams {
            id: None,
            platform: "openai",
            name,
            base_url: "https://api.openai.com/v1",
            api_key_encrypted: "stub",
            models: &models,
            enabled,
            capabilities: "[]",
            context_limit: None,
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            model_settings: "{}",
            bedrock_config: None,
            is_full_url: false,
        })
        .await
        .expect("create deterministic Team provider")
}

async fn ensure_hermes_team_assistant(
    app: &mut axum::Router,
    services: &aionui_app::AppServices,
    token: &str,
    csrf: &str,
    model: &str,
) {
    mark_hermes_backend_team_mcp_stdio_capable(services).await;
    let req = json_with_token(
        "POST",
        "/api/assistants",
        json!({
            "id": HERMES_TEAM_ASSISTANT_ID,
            "name": "Team E2E Hermes Assistant",
            "agent_id": HERMES_AGENT_ID,
            "models": [model],
            "defaults": {
                "model": {
                    "mode": "fixed",
                    "value": model
                }
            }
        }),
        token,
        csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::CREATED, "body = {body}");

    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/assistants/{HERMES_TEAM_ASSISTANT_ID}"),
            token,
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert_eq!(body["data"]["team_selectable"], true, "body = {body}");
}

async fn create_team(
    app: &mut axum::Router,
    services: &aionui_app::AppServices,
    token: &str,
    csrf: &str,
) -> serde_json::Value {
    ensure_default_team_assistant(app, services, token, csrf).await;
    let req = json_with_token("POST", "/api/teams", two_agent_body(), token, csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let json = body_json(resp).await;
    assert_eq!(status, StatusCode::CREATED, "body = {json}");
    assert!(json["success"].as_bool().unwrap());
    json["data"].clone()
}

async fn mcp_send(stream: &mut TcpStream, req: &Value) {
    let bytes = serde_json::to_vec(req).unwrap();
    write_frame(stream, &bytes).await.unwrap();
}

async fn mcp_recv(stream: &mut TcpStream) -> Value {
    let frame = read_frame(stream).await.unwrap();
    serde_json::from_slice(&frame).unwrap()
}

async fn mcp_connect(port: u16, auth_token: &str, slot_id: &str) -> TcpStream {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("tcp connect to TeamMcpServer");
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "auth_token": auth_token,
            "slot_id": slot_id,
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "app-team-e2e", "version": "0.1" }
        }
    });
    mcp_send(&mut stream, &init_req).await;
    let resp = mcp_recv(&mut stream).await;
    assert!(
        resp["result"]["serverInfo"]["name"].is_string(),
        "initialize failed: {resp}"
    );
    stream
}

async fn mcp_call_tool(stream: &mut TcpStream, id: u64, tool: &str, args: Value) -> Value {
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": tool, "arguments": args }
    });
    mcp_send(stream, &req).await;
    mcp_recv(stream).await
}

fn mcp_text(resp: &Value) -> &str {
    resp["result"]["content"][0]["text"].as_str().unwrap_or("")
}

// ===========================================================================
// §1 Team CRUD (TC-*, TL-*, TG-*, TD-*, TR-*)
// ===========================================================================

// TC-1: Create team with multiple assistants
#[tokio::test]
async fn tc1_create_team_with_multiple_agents() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    assert_eq!(data["name"], "Alpha");
    assert_eq!(data["assistants"].as_array().unwrap().len(), 2);
    assert_eq!(data["assistants"][0]["role"], "lead");
    assert_eq!(data["assistants"][1]["role"], "teammate");
    assert!(data["leader_assistant_id"].is_string());
    assert_eq!(data["leader_assistant_id"], data["assistants"][0]["slot_id"]);
}

// TC-2: Create single assistant team
#[tokio::test]
async fn tc2_create_single_agent_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_team_assistant(&mut app, &services, &token, &csrf).await;

    let body = json!({
        "name": "Solo",
        "agents": [team_agent("Lead", "lead")]
    });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["assistants"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn tc_create_team_with_builtin_hermes_persists_provider_model_binding() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let model = "hermes-team-model";
    let provider = create_team_test_provider(&services, "Hermes Team Provider", model, true).await;
    ensure_hermes_team_assistant(&mut app, &services, &token, &csrf, model).await;

    let req = json_with_token(
        "POST",
        "/api/teams",
        json!({
            "name": "Hermes Team",
            "agents": [{
                "name": "Hermes Lead",
                "role": "lead",
                "model": "default",
                "assistant_id": HERMES_TEAM_ASSISTANT_ID
            }]
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::CREATED, "body = {body}");

    let conversation_id = body["data"]["assistants"][0]["conversation_id"]
        .as_str()
        .expect("Hermes conversation id");
    assert_eq!(body["data"]["assistants"][0]["model"], model);
    let row = services
        .conversation_repo
        .get(conversation_id)
        .await
        .unwrap()
        .expect("Hermes conversation row");
    let binding: ProviderWithModel = serde_json::from_str(row.model.as_deref().expect("Hermes model binding")).unwrap();
    assert_eq!(binding.provider_id, provider.id);
    assert_eq!(binding.model, model);
    assert_eq!(binding.use_model, None);

    let extra: Value = serde_json::from_str(&row.extra).unwrap();
    assert!(
        extra.get("provider_id").is_none(),
        "Hermes backend name must not be persisted as a provider id: {extra}"
    );
    assert_eq!(extra["current_model_id"], model);
}

#[tokio::test]
async fn tc_create_team_with_builtin_hermes_rejects_missing_provider() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let model = "hermes-model-without-provider";
    ensure_hermes_team_assistant(&mut app, &services, &token, &csrf, model).await;

    let req = json_with_token(
        "POST",
        "/api/teams",
        json!({
            "name": "Hermes Missing Provider",
            "agents": [{
                "name": "Hermes Lead",
                "role": "lead",
                "model": model,
                "assistant_id": HERMES_TEAM_ASSISTANT_ID
            }]
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body = {body}");
    assert_eq!(body["code"], "TEAM_PROVIDER_NOT_AVAILABLE");
    assert_eq!(body["details"]["agent_index"], 0);
    assert_eq!(body["details"]["requested_model"], model);
}

#[tokio::test]
async fn tc_create_team_with_builtin_hermes_rejects_disabled_provider() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let model = "hermes-disabled-provider-model";
    create_team_test_provider(&services, "Disabled Hermes Provider", model, false).await;
    ensure_hermes_team_assistant(&mut app, &services, &token, &csrf, model).await;

    let req = json_with_token(
        "POST",
        "/api/teams",
        json!({
            "name": "Hermes Disabled Provider",
            "agents": [{
                "name": "Hermes Lead",
                "role": "lead",
                "model": model,
                "assistant_id": HERMES_TEAM_ASSISTANT_ID
            }]
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body = {body}");
    assert_eq!(body["code"], "TEAM_PROVIDER_NOT_AVAILABLE");
    assert_eq!(body["details"]["agent_index"], 0);
    assert_eq!(body["details"]["requested_model"], model);
}

#[tokio::test]
async fn tc_create_team_with_builtin_hermes_selects_provider_without_partial_creation() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let model = "hermes-ambiguous-model";
    let provider_a = create_team_test_provider(&services, "Hermes Provider A", model, true).await;
    let provider_b = create_team_test_provider(&services, "Hermes Provider B", model, true).await;
    ensure_hermes_team_assistant(&mut app, &services, &token, &csrf, model).await;

    let conversation_count_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    let team_count_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM teams")
        .fetch_one(services.database.pool())
        .await
        .unwrap();

    let req = json_with_token(
        "POST",
        "/api/teams",
        json!({
            "name": "Hermes Ambiguous Provider",
            "agents": [
                {
                    "name": "Hermes Lead",
                    "role": "lead",
                    "model": model,
                    "provider_id": provider_a.id,
                    "assistant_id": HERMES_TEAM_ASSISTANT_ID
                },
                {
                    "name": "Hermes Worker",
                    "role": "teammate",
                    "model": model,
                    "assistant_id": HERMES_TEAM_ASSISTANT_ID
                }
            ]
        }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::CONFLICT, "body = {body}");
    assert_eq!(body["code"], "TEAM_PROVIDER_SELECTION_REQUIRED");
    let selections = body["details"]["selections"].as_array().expect("provider selections");
    assert_eq!(selections.len(), 1, "body = {body}");
    assert_eq!(selections[0]["agent_index"], 1);
    assert_eq!(selections[0]["agent_name"], "Hermes Worker");
    assert_eq!(selections[0]["assistant_id"], HERMES_TEAM_ASSISTANT_ID);
    assert_eq!(selections[0]["requested_model"], model);
    let candidates = selections[0]["candidates"].as_array().expect("provider candidates");
    assert_eq!(candidates.len(), 2, "body = {body}");
    let mut candidate_ids = candidates
        .iter()
        .map(|candidate| candidate["provider_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    candidate_ids.sort();
    let mut expected_ids = vec![provider_a.id.clone(), provider_b.id.clone()];
    expected_ids.sort();
    assert_eq!(candidate_ids, expected_ids);
    for candidate in candidates {
        assert_eq!(candidate["platform"], "openai");
        assert_eq!(candidate["resolved_model"], model);
        assert!(candidate["provider_name"].is_string());
        assert!(candidate.get("api_key").is_none());
        assert!(candidate.get("base_url").is_none());
    }
    let rendered = body.to_string();
    assert!(!rendered.contains("https://api.openai.com/v1"));
    assert!(!rendered.contains("stub"));

    let conversation_count_after_conflict: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    let team_count_after_conflict: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM teams")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(conversation_count_after_conflict, conversation_count_before);
    assert_eq!(team_count_after_conflict, team_count_before);

    let invalid_retry = json_with_token(
        "POST",
        "/api/teams",
        json!({
            "name": "Hermes Ambiguous Provider",
            "agents": [
                {
                    "name": "Hermes Lead",
                    "role": "lead",
                    "model": model,
                    "provider_id": provider_a.id,
                    "assistant_id": HERMES_TEAM_ASSISTANT_ID
                },
                {
                    "name": "Hermes Worker",
                    "role": "teammate",
                    "model": model,
                    "provider_id": "missing-provider",
                    "assistant_id": HERMES_TEAM_ASSISTANT_ID
                }
            ]
        }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(invalid_retry).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body = {body}");
    assert_eq!(body["code"], "TEAM_PROVIDER_SELECTION_INVALID");
    assert_eq!(body["details"]["agent_index"], 1);
    assert_eq!(body["details"]["provider_id"], "missing-provider");
    assert_eq!(body["details"]["requested_model"], model);

    let retry = json_with_token(
        "POST",
        "/api/teams",
        json!({
            "name": "Hermes Ambiguous Provider",
            "agents": [
                {
                    "name": "Hermes Lead",
                    "role": "lead",
                    "model": model,
                    "provider_id": provider_a.id,
                    "assistant_id": HERMES_TEAM_ASSISTANT_ID
                },
                {
                    "name": "Hermes Worker",
                    "role": "teammate",
                    "model": model,
                    "provider_id": provider_b.id,
                    "assistant_id": HERMES_TEAM_ASSISTANT_ID
                }
            ]
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(retry).await.unwrap();
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::CREATED, "body = {body}");

    for assistant in body["data"]["assistants"].as_array().expect("created assistants") {
        let expected_provider_id = match assistant["name"].as_str().unwrap() {
            "Hermes Lead" => &provider_a.id,
            "Hermes Worker" => &provider_b.id,
            name => panic!("unexpected assistant {name}"),
        };
        let conversation_id = assistant["conversation_id"].as_str().unwrap();
        let row = services
            .conversation_repo
            .get(conversation_id)
            .await
            .unwrap()
            .expect("Hermes conversation row");
        let binding: ProviderWithModel = serde_json::from_str(row.model.as_deref().unwrap()).unwrap();
        assert_eq!(&binding.provider_id, expected_provider_id);
        assert_eq!(binding.model, model);
    }
}

#[tokio::test]
async fn tc_create_team_rejects_existing_agent_conversation_id() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({
        "name": "No Adoption",
        "agents": [
            {
                "name": "Lead",
                "role": "lead",
                "model": "claude",
                "assistant_id": DEFAULT_TEAM_ASSISTANT_ID,
                "conversation_id": "solo-conv-1"
            }
        ]
    });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
    assert_eq!(json["code"], "BAD_REQUEST");
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("existing conversations are no longer supported")
    );
}

// TC-3: Each assistant has a conversation
#[tokio::test]
async fn tc3_each_agent_has_conversation_id() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    for agent in data["assistants"].as_array().unwrap() {
        assert!(agent["conversation_id"].is_string());
        assert!(!agent["conversation_id"].as_str().unwrap().is_empty());
    }
    assert_ne!(
        data["assistants"][0]["conversation_id"],
        data["assistants"][1]["conversation_id"]
    );
}

#[tokio::test]
async fn tc3b_create_team_writes_legacy_extra_shape() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let conversation_id = data["assistants"][0]["conversation_id"].as_str().unwrap();

    let repo = aionui_db::SqliteConversationRepository::new(services.database.pool().clone());
    let row = repo.get(conversation_id).await.unwrap().unwrap();
    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();

    assert_eq!(extra["teamId"], data["id"]);
    assert!(extra["slot_id"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(extra["role"], "lead");
    assert_eq!(extra["backend"], "aionrs");
    assert_eq!(extra["session_mode"], "yolo");
    assert_eq!(extra["current_model_id"], Value::Null);
    let model: Value = serde_json::from_str(row.model.as_deref().expect("aionrs model binding")).unwrap();
    assert_eq!(model["model"], "claude");
}

#[tokio::test]
async fn tc3c_team_conversation_rejects_standalone_runtime_ensure() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let conversation_id = data["assistants"][0]["conversation_id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conversation_id}/runtime/ensure"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["success"], false);
    assert_eq!(body["code"], "TEAM_RUNTIME_REQUIRED");
    assert_eq!(body["details"]["conversation_id"], conversation_id);
    assert_eq!(body["details"]["team_id"], data["id"]);
}

// TC-4: Explicit lead role is returned first
#[tokio::test]
async fn tc4_explicit_lead_is_returned_first() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_team_assistant(&mut app, &services, &token, &csrf).await;

    let body = json!({
        "name": "T",
        "agents": [
            team_agent("A", "teammate"),
            team_agent("B", "lead")
        ]
    });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["assistants"][0]["name"], "B");
    assert_eq!(json["data"]["assistants"][0]["role"], "lead");
    assert_eq!(json["data"]["assistants"][1]["name"], "A");
    assert_eq!(json["data"]["assistants"][1]["role"], "teammate");
    assert_eq!(
        json["data"]["leader_assistant_id"],
        json["data"]["assistants"][0]["slot_id"]
    );
}

// TC-5: Empty agents returns 400
#[tokio::test]
async fn tc5_empty_agents_returns_error() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({ "name": "Empty", "agents": [] });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// TC-6: Missing name returns 400
#[tokio::test]
async fn tc6_missing_name_returns_error() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_team_assistant(&mut app, &services, &token, &csrf).await;

    let body = json!({ "agents": [json!({
        "name": "L",
        "role": "lead",
        "model": "c",
        "assistant_id": DEFAULT_TEAM_ASSISTANT_ID
    })] });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn tc6b_workspace_with_whitespace_segment_is_accepted() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_team_assistant(&mut app, &services, &token, &csrf).await;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("Archive ");
    std::fs::create_dir_all(&workspace).unwrap();

    let body = json!({
        "name": "Alpha",
        "workspace": workspace.to_string_lossy(),
        "agents": [team_agent("Lead", "lead")]
    });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
}

#[tokio::test]
async fn tc6c_create_team_rejects_missing_workspace_path() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    ensure_default_team_assistant(&mut app, &services, &token, &csrf).await;
    let missing_workspace =
        std::env::temp_dir().join(format!("aionui-team-missing-{}", aionui_common::generate_short_id()));

    let body = json!({
        "name": "Alpha",
        "workspace": missing_workspace.to_string_lossy(),
        "agents": [team_agent("Lead", "lead")]
    });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp).await;
    assert_eq!(json["code"], "WORKSPACE_PATH_UNAVAILABLE");
    assert_eq!(json["details"]["operation"], "create");
    assert_eq!(
        json["details"]["workspace_path"],
        missing_workspace.to_string_lossy().to_string()
    );

    let req = get_with_token("/api/teams", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(
        json["data"].as_array().unwrap().is_empty(),
        "invalid team should not be persisted"
    );
}

// TC-7: Unauthenticated returns 401
#[tokio::test]
async fn tc7_unauthenticated_returns_401() {
    let (app, _services) = build_app().await;

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/teams")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["code"], "UNAUTHORIZED");
}

// TL-1: Empty team list
#[tokio::test]
async fn tl1_empty_team_list() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/teams", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());
}

// TL-2: List multiple teams
#[tokio::test]
async fn tl2_list_multiple_teams() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    create_team(&mut app, &services, &token, &csrf).await;
    ensure_default_team_assistant(&mut app, &services, &token, &csrf).await;

    let body = json!({
        "name": "Beta",
        "agents": [team_agent("Lead", "lead")]
    });
    let req = json_with_token("POST", "/api/teams", body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let req = get_with_token("/api/teams", &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn team_api_rejects_cross_user_access() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (owner_token, owner_csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let (other_token, other_csrf) = setup_and_login(&mut app, &services, "alice", "StrongP@ss2").await;

    let data = create_team(&mut app, &services, &owner_token, &owner_csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let slot_id = data["assistants"][1]["slot_id"].as_str().unwrap();

    let req = get_with_token("/api/teams", &other_token);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());

    let req = get_with_token(&format!("/api/teams/{team_id}"), &other_token);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let forbidden_requests = [
        json_with_token(
            "PATCH",
            &format!("/api/teams/{team_id}/name"),
            json!({ "name": "Nope" }),
            &other_token,
            &other_csrf,
        ),
        json_with_token(
            "POST",
            &format!("/api/teams/{team_id}/messages"),
            json!({ "content": "Nope" }),
            &other_token,
            &other_csrf,
        ),
        json_with_token(
            "POST",
            &format!("/api/teams/{team_id}/agents/{slot_id}/messages"),
            json!({ "content": "Nope" }),
            &other_token,
            &other_csrf,
        ),
        json_with_token(
            "POST",
            &format!("/api/teams/{team_id}/session"),
            json!({}),
            &other_token,
            &other_csrf,
        ),
        json_with_token(
            "DELETE",
            &format!("/api/teams/{team_id}/session"),
            json!({}),
            &other_token,
            &other_csrf,
        ),
        json_with_token(
            "POST",
            &format!("/api/teams/{team_id}/session-mode"),
            json!({ "mode": "auto" }),
            &other_token,
            &other_csrf,
        ),
    ];

    for req in forbidden_requests {
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}

#[tokio::test]
async fn pause_team_slot_endpoint_requires_owned_team_and_active_run() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let lead_slot_id = data["assistants"][0]["slot_id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/runs/not-a-run/agents/{lead_slot_id}/pause"),
        json!({"reason": "user stopped"}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(body["success"].as_bool().is_some_and(|success| !success));
}

#[tokio::test]
async fn trs1_run_state_returns_null_for_existing_team_without_active_run() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let stop_req = delete_with_token(&format!("/api/teams/{team_id}/session"), &token, &csrf);
    let stop_resp = app.clone().oneshot(stop_req).await.unwrap();
    assert_eq!(stop_resp.status(), StatusCode::OK);

    let req = get_with_token(&format!("/api/teams/{team_id}/run-state"), &token);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["success"], true);
    assert!(body["data"]["active_run"].is_null());
}

#[tokio::test]
async fn trs2_run_state_returns_active_run_payload() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let send_req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/messages"),
        json!({ "content": "hello", "files": [] }),
        &token,
        &csrf,
    );
    let send_resp = app.clone().oneshot(send_req).await.unwrap();
    assert_eq!(send_resp.status(), StatusCode::OK);
    let send_body = body_json(send_resp).await;
    let team_run_id = send_body["data"]["run"]["team_run_id"].as_str().unwrap();
    assert!(matches!(
        send_body["data"]["enqueue_status"].as_str(),
        Some("accepted" | "queued" | "blocked_runtime_starting")
    ));

    let req = get_with_token(&format!("/api/teams/{team_id}/run-state"), &token);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["success"], true);
    assert_eq!(body["data"]["active_run"]["team_id"], team_id);
    assert_eq!(body["data"]["active_run"]["team_run_id"], team_run_id);
    assert_eq!(body["data"]["active_run"]["source"], "user_message");
    assert_eq!(body["data"]["active_run"]["has_user_intervention"], false);
    assert_eq!(body["data"]["active_run"]["status"], "accepted");
    assert!(body["data"]["active_run"]["queued_intent_count"].is_number());
    assert!(body["data"]["active_run"]["starting_batch_count"].is_number());
    assert!(body["data"]["active_run"]["running_batch_count"].is_number());
    assert!(body["data"]["active_run"]["active_enqueue_lease_count"].is_number());
    assert!(!body["data"]["active_run"]["slot_work"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn trs3_run_state_unauthenticated_returns_401() {
    let (app, _services) = build_app().await;

    let resp = app.oneshot(get_request("/api/teams/team-1/run-state")).await.unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "UNAUTHORIZED");
}

#[tokio::test]
async fn trs4_run_state_missing_team_returns_404() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/teams/team-missing/run-state", &token);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn trs5_run_state_rejects_cross_user_access() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (admin_token, admin_csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let data = create_team(&mut app, &services, &admin_token, &admin_csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let (other_token, _other_csrf) = setup_and_login(&mut app, &services, "other", "StrongP@ss2").await;
    let req = get_with_token(&format!("/api/teams/{team_id}/run-state"), &other_token);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "FORBIDDEN");
}

// TL-3: Each team contains full assistants info
#[tokio::test]
async fn tl3_teams_contain_full_agent_info() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    create_team(&mut app, &services, &token, &csrf).await;

    let req = get_with_token("/api/teams", &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let teams = json["data"].as_array().unwrap();
    let agent = &teams[0]["assistants"][0];
    assert!(agent["slot_id"].is_string());
    assert!(agent["name"].is_string());
    assert!(agent["role"].is_string());
    assert!(agent["conversation_id"].is_string());
    assert!(agent["backend"].is_string());
    assert!(agent["model"].is_string());
}

// TG-1: Get existing team
#[tokio::test]
async fn tg1_get_existing_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["id"], team_id);
    assert_eq!(json["data"]["name"], "Alpha");
}

// TG-2: Get nonexistent team returns 404
#[tokio::test]
async fn tg2_get_nonexistent_returns_404() {
    let (mut app, services) = build_app().await;
    let (token, _csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = get_with_token("/api/teams/nonexistent", &token);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// TD-1: Delete existing team
#[tokio::test]
async fn td1_delete_existing_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}"), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// TD-2: Delete then list confirms removal
#[tokio::test]
async fn td2_delete_then_list_empty() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}"), &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let req = get_with_token("/api/teams", &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());
}

// TD-6: Delete nonexistent team returns 404
#[tokio::test]
async fn td6_delete_nonexistent_returns_404() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = delete_with_token("/api/teams/nonexistent", &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// TR-1: Rename existing team
#[tokio::test]
async fn tr1_rename_existing_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/name"),
        json!({ "name": "New Name" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// TR-2: Rename then get confirms new name
#[tokio::test]
async fn tr2_rename_then_get_confirms_new_name() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/name"),
        json!({ "name": "New Name" }),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "New Name");
}

// TR-4: Rename nonexistent team returns 404
#[tokio::test]
async fn tr4_rename_nonexistent_returns_404() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "PATCH",
        "/api/teams/nonexistent/name",
        json!({ "name": "X" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// §2 Agent Management (AA-*, AR-*, AN-*)
// ===========================================================================

// AA-1: Add agent to team
#[tokio::test]
async fn aa1_add_agent_to_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let body = json!({
        "name": "New Agent",
        "role": "teammate",
        "model": "claude",
        "assistant_id": DEFAULT_TEAM_ASSISTANT_ID
    });
    let req = json_with_token("POST", &format!("/api/teams/{team_id}/agents"), body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "New Agent");
    assert!(json["data"]["conversation_id"].is_string());
}

// AA-2: After adding, agent count increases
#[tokio::test]
async fn aa2_add_agent_increases_count() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let body = json!({
        "name": "X",
        "role": "teammate",
        "model": "claude",
        "assistant_id": DEFAULT_TEAM_ASSISTANT_ID
    });
    let req = json_with_token("POST", &format!("/api/teams/{team_id}/agents"), body, &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["assistants"].as_array().unwrap().len(), 3);
}

// AA-4: Add agent to nonexistent team returns 404
#[tokio::test]
async fn aa4_add_agent_nonexistent_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let body = json!({
        "name": "X",
        "role": "teammate",
        "model": "claude",
        "assistant_id": DEFAULT_TEAM_ASSISTANT_ID
    });
    let req = json_with_token("POST", "/api/teams/nonexistent/agents", body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// AA-5: Missing required fields returns 400
#[tokio::test]
async fn aa5_add_agent_missing_fields() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let body = json!({ "role": "teammate" });
    let req = json_with_token("POST", &format!("/api/teams/{team_id}/agents"), body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// AR-1: Remove agent from team
#[tokio::test]
async fn ar1_remove_agent_from_team() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let slot_id = data["assistants"][1]["slot_id"].as_str().unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}/agents/{slot_id}"), &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// AR-2: After removal, agent not in team
#[tokio::test]
async fn ar2_after_removal_agent_gone() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let slot_id = data["assistants"][1]["slot_id"].as_str().unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}/agents/{slot_id}"), &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let assistants = json["data"]["assistants"].as_array().unwrap();
    assert_eq!(assistants.len(), 1);
    assert!(assistants.iter().all(|a| a["slot_id"] != slot_id));
}

// AR-4: Remove nonexistent agent returns 404
#[tokio::test]
async fn ar4_remove_nonexistent_agent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}/agents/nonexistent"), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// AN-1: Rename agent
#[tokio::test]
async fn an1_rename_agent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let slot_id = data["assistants"][1]["slot_id"].as_str().unwrap();

    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/agents/{slot_id}/name"),
        json!({ "name": "Senior Worker" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// AN-2: Rename then get confirms new name
#[tokio::test]
async fn an2_rename_then_get_confirms_name() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let slot_id = data["assistants"][1]["slot_id"].as_str().unwrap();

    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/agents/{slot_id}/name"),
        json!({ "name": "Senior Worker" }),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    let assistants = json["data"]["assistants"].as_array().unwrap();
    let agent = assistants.iter().find(|a| a["slot_id"] == slot_id).unwrap();
    assert_eq!(agent["name"], "Senior Worker");
}

// AN-3: Rename nonexistent agent returns 404
#[tokio::test]
async fn an3_rename_nonexistent_agent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/agents/nonexistent/name"),
        json!({ "name": "X" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// §3 Session Management (ES-*, SS-*)
// ===========================================================================

// ES-1: Ensure session
#[tokio::test]
async fn es1_ensure_session() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn es1b_team_mcp_list_assistants_matches_assistant_projection() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    mark_hermes_backend_team_mcp_stdio_capable(&services).await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let lead = &data["assistants"][0];
    let lead_conversation_id = lead["conversation_id"].as_str().unwrap();
    let lead_slot_id = lead["slot_id"].as_str().unwrap();

    let assistants_resp = app
        .clone()
        .oneshot(get_with_token("/api/assistants", &token))
        .await
        .unwrap();
    assert_eq!(assistants_resp.status(), StatusCode::OK);
    let assistants_body = body_json(assistants_resp).await;
    let mut expected_ids: Vec<String> = assistants_body["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|assistant| assistant["team_selectable"].as_bool().unwrap_or(false))
        .filter(|assistant| assistant["agent"].is_object())
        .map(|assistant| assistant["id"].as_str().unwrap().to_owned())
        .collect();
    expected_ids.sort();
    assert!(
        !expected_ids.is_empty(),
        "fixture must expose at least one team-selectable assistant via /api/assistants: {assistants_body}"
    );
    assert!(
        expected_ids.contains(&DEFAULT_TEAM_ASSISTANT_ID.to_owned()),
        "seeded team assistant must be team-selectable in assistant projection: {assistants_body}"
    );

    let ensure_req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    let ensure_resp = app.clone().oneshot(ensure_req).await.unwrap();
    assert_eq!(ensure_resp.status(), StatusCode::OK);

    let lead_conversation = services
        .conversation_repo
        .get(lead_conversation_id)
        .await
        .unwrap()
        .unwrap();
    let extra: Value = serde_json::from_str(&lead_conversation.extra).unwrap();
    let mcp_config: TeamMcpStdioConfig =
        serde_json::from_value(extra["team_mcp_stdio_config"].clone()).expect("team mcp config");
    assert_eq!(mcp_config.slot_id, lead_slot_id);

    let mut stream = mcp_connect(mcp_config.port, &mcp_config.token, &mcp_config.slot_id).await;
    let list_resp = mcp_call_tool(&mut stream, 2, "team_list_assistants", json!({})).await;
    assert!(
        !list_resp["result"]["isError"].as_bool().unwrap_or(false),
        "team_list_assistants failed: {list_resp}"
    );
    let list_body: Value = serde_json::from_str(mcp_text(&list_resp)).expect("team_list_assistants JSON");
    let mut runtime_ids: Vec<String> = list_body["assistants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|assistant| assistant["assistant_id"].as_str().unwrap().to_owned())
        .collect();
    runtime_ids.sort();

    assert_eq!(
        runtime_ids, expected_ids,
        "Team MCP runtime assistant list must match /api/assistants team_selectable projection"
    );

    let stop_req = delete_with_token(&format!("/api/teams/{team_id}/session"), &token, &csrf);
    let stop_resp = app.oneshot(stop_req).await.unwrap();
    assert_eq!(stop_resp.status(), StatusCode::OK);
}

// ES-2: Ensure session is idempotent
#[tokio::test]
async fn es2_ensure_session_idempotent() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ES-3: Ensure session for nonexistent team returns 404
#[tokio::test]
async fn es3_ensure_session_nonexistent() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/teams/nonexistent/session", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// SS-1: Stop session
#[tokio::test]
async fn ss1_stop_session() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}/session"), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// SS-3: Stop session without active is noop
#[tokio::test]
async fn ss3_stop_session_noop() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = delete_with_token(&format!("/api/teams/{team_id}/session"), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// §4 Message sending (SM-*, SA-*)
// ===========================================================================

// SM-1: Send message with active session
#[tokio::test]
async fn sm1_send_message_with_session() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    // Start session first
    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/messages"),
        json!({ "content": "Hello team" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn sm1b_team_send_persists_user_bubble_through_projection_adapter() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let lead_conversation_id = data["assistants"][0]["conversation_id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/messages"),
        json!({ "content": "Hello through adapter" }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let repo = aionui_db::SqliteConversationRepository::new(services.database.pool().clone());
    let messages = repo
        .list_messages_page(
            lead_conversation_id,
            &MessagePageParams {
                limit: 50,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap();
    assert!(messages.items.iter().any(|row| {
        row.position.as_deref() == Some("right")
            && row.status.as_deref() == Some("finish")
            && row.content.contains("Hello through adapter")
    }));
}

#[tokio::test]
async fn sm1c_team_owned_conversation_regular_send_is_forbidden() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let conversation_id = data["assistants"][0]["conversation_id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/conversations/{conversation_id}/messages"),
        json!({ "content": "must go through team api" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = body_json(resp).await;
    assert_eq!(body["code"], "FORBIDDEN");
    assert_eq!(body["error"], "Forbidden.");
}

#[tokio::test]
async fn sm1d_team_send_rejects_missing_csrf() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/teams/{team_id}/messages"))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"content":"x"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

// SM-4: Send message without session returns 404
#[tokio::test]
async fn sm4_send_message_no_session() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/teams/nonexistent/messages",
        json!({ "content": "Hello" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// SM-5: Missing content returns 400
#[tokio::test]
async fn sm5_send_message_missing_content() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/messages"),
        json!({}),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// SA-1: Send message to specific agent
#[tokio::test]
async fn sa1_send_message_to_agent() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    let slot_id = data["assistants"][1]["slot_id"].as_str().unwrap();

    // Start session first
    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/agents/{slot_id}/messages"),
        json!({ "content": "Do this" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// §5 Full lifecycle
// ===========================================================================

// Full CRUD lifecycle
#[tokio::test]
async fn full_team_lifecycle() {
    let (mut app, services) = build_app_with_mock_agents().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create
    let data = create_team(&mut app, &services, &token, &csrf).await;
    let team_id = data["id"].as_str().unwrap();
    assert_eq!(data["assistants"].as_array().unwrap().len(), 2);

    // Add agent
    let body = json!({
        "name": "Helper",
        "role": "teammate",
        "model": "claude",
        "assistant_id": DEFAULT_TEAM_ASSISTANT_ID
    });
    let req = json_with_token("POST", &format!("/api/teams/{team_id}/agents"), body, &token, &csrf);
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let added = body_json(resp).await;
    let new_slot = added["data"]["slot_id"].as_str().unwrap().to_owned();

    // Verify 3 assistants
    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["assistants"].as_array().unwrap().len(), 3);

    // Rename team
    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/name"),
        json!({ "name": "Renamed" }),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    // Rename agent
    let req = json_with_token(
        "PATCH",
        &format!("/api/teams/{team_id}/agents/{new_slot}/name"),
        json!({ "name": "Senior Helper" }),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    // Ensure session
    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/session"),
        json!({}),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    // Send message
    let req = json_with_token(
        "POST",
        &format!("/api/teams/{team_id}/messages"),
        json!({ "content": "Hello" }),
        &token,
        &csrf,
    );
    app.clone().oneshot(req).await.unwrap();

    // Stop session
    let req = delete_with_token(&format!("/api/teams/{team_id}/session"), &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    // Remove added agent
    let req = delete_with_token(&format!("/api/teams/{team_id}/agents/{new_slot}"), &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    // Verify 2 assistants remain
    let req = get_with_token(&format!("/api/teams/{team_id}"), &token);
    let resp = app.clone().oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["data"]["assistants"].as_array().unwrap().len(), 2);
    assert_eq!(json["data"]["name"], "Renamed");

    // Delete team
    let req = delete_with_token(&format!("/api/teams/{team_id}"), &token, &csrf);
    app.clone().oneshot(req).await.unwrap();

    // Verify empty
    let req = get_with_token("/api/teams", &token);
    let resp = app.oneshot(req).await.unwrap();
    let json = body_json(resp).await;
    assert!(json["data"].as_array().unwrap().is_empty());
}
