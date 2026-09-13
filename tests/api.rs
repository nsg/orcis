use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use orcis::{
    api::{AppState, router},
    store::Store,
};
use serde_json::{Value, json};
use tower::ServiceExt;

fn app(token: Option<&str>) -> Router {
    router(AppState::new(Store::in_memory(), token.map(str::to_owned)))
}

fn get(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
}

fn json_request(method: &str, path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn body_json(response: axum::response::Response) -> Value {
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn create_task(app: &Router, body: Value) -> Value {
    let response = app
        .clone()
        .oneshot(json_request("POST", "/tasks", body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    body_json(response).await
}

#[tokio::test]
async fn index_and_health_are_json() {
    let app = app(None);
    let response = app.clone().oneshot(get("/")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let index = body_json(response).await;
    assert_eq!(index["name"], "orcis");
    assert!(index["endpoints"].as_array().unwrap().len() >= 15);

    let response = app.oneshot(get("/healthz")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await, json!({ "status": "ok" }));
}

#[tokio::test]
async fn agent_docs_are_public_markdown_and_cover_the_index() {
    let unprotected = app(None);
    let response = unprotected.clone().oneshot(get("/docs.md")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/markdown")
    );
    let docs = body_text(response).await;
    assert!(docs.starts_with("# orcis"));

    let response = unprotected.clone().oneshot(get("/")).await.unwrap();
    let index = body_json(response).await;
    for endpoint in index["endpoints"].as_array().unwrap() {
        let method = endpoint["method"].as_str().unwrap();
        let path = endpoint["path"].as_str().unwrap();
        assert!(
            docs.contains(&format!("`{method} {path}`")),
            "docs missing endpoint `{method} {path}`"
        );
    }

    let response = unprotected
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/docs.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        body_json(response).await,
        json!({"error": "method not allowed"})
    );

    let protected = app(Some("secret"));
    let response = protected.clone().oneshot(get("/docs.md")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = protected
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/docs.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(response).await, json!({"error": "unauthorized"}));
}

#[tokio::test]
async fn task_crud_filters_and_json_errors() {
    let app = app(None);
    let first = create_task(
        &app,
        json!({
            "title": "first",
            "description": "before",
            "priority": 2,
            "requires": ["rust", "rust"],
            "metadata": {"phase": 1}
        }),
    )
    .await;
    let first_id = first["id"].as_str().unwrap();
    assert_eq!(first["requires"], json!(["rust"]));
    assert_eq!(first["version"], 1);
    assert_eq!(first["ready"], true);

    let second = create_task(
        &app,
        json!({"title": "second", "priority": 1, "requires": ["rust", "api"]}),
    )
    .await;
    let second_id = second["id"].as_str().unwrap();

    let response = app
        .clone()
        .oneshot(get(&format!("/tasks/{first_id}")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["title"], "first");

    let response = app
        .clone()
        .oneshot(get(
            "/tasks?status=todo&status=failed&ready=true&requires=rust",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await["tasks"].as_array().unwrap().len(),
        2
    );

    let response = app
        .clone()
        .oneshot(get("/tasks?requires=rust&requires=api"))
        .await
        .unwrap();
    let list = body_json(response).await;
    assert_eq!(list["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(list["tasks"][0]["id"], second_id);

    let response = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/tasks/{first_id}"),
            json!({"title": "updated", "description": "after", "metadata": null}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let patched = body_json(response).await;
    assert_eq!(patched["title"], "updated");
    assert_eq!(patched["description"], "after");
    assert_eq!(patched["metadata"], Value::Null);
    assert_eq!(patched["version"], 2);

    let malformed = Request::builder()
        .method("POST")
        .uri("/tasks")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{"))
        .unwrap();
    let response = app.clone().oneshot(malformed).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await.get("error").is_some());

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/tasks",
            json!({"title": "bad", "unknown": true}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await.get("error").is_some());

    let response = app.clone().oneshot(get("/tasks/not-a-uuid")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(body_json(response).await.get("error").is_some());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/tasks/{first_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.headers().get(header::CONTENT_TYPE).is_none());

    let response = app
        .clone()
        .oneshot(get(&format!("/tasks/{first_id}")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = app
        .oneshot(get("/tasks/00000000-0000-4000-8000-000000000000"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn claim_next_uses_literal_route_and_claims_distinct_tasks() {
    let app = app(None);
    create_task(
        &app,
        json!({"title": "low", "priority": 1, "requires": ["rust"]}),
    )
    .await;
    let high = create_task(
        &app,
        json!({"title": "high", "priority": 10, "requires": ["rust"]}),
    )
    .await;

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/tasks/claim",
            json!({"agent": "one", "capabilities": ["rust"]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let first = body_json(response).await;
    assert_eq!(first["id"], high["id"]);
    assert_eq!(first["claimed_by"], "one");

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/tasks/claim",
            json!({"agent": "two", "capabilities": ["rust"]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let second = body_json(response).await;
    assert_ne!(first["id"], second["id"]);

    let response = app
        .oneshot(json_request(
            "POST",
            "/tasks/claim",
            json!({"agent": "three", "capabilities": ["rust"]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response.headers().get(header::CONTENT_TYPE).is_none());
}

#[tokio::test]
async fn dependency_chain_blocks_then_unlocks() {
    let app = app(None);
    let a = create_task(&app, json!({"title": "A"})).await;
    let a_id = a["id"].as_str().unwrap();
    let b = create_task(&app, json!({"title": "B", "depends_on": [a_id]})).await;
    let b_id = b["id"].as_str().unwrap();

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/tasks/{b_id}/claim"),
            json!({"agent": "worker"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        body_json(response).await["error"]
            .as_str()
            .unwrap()
            .contains(a_id)
    );

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/tasks/{a_id}/claim"),
            json!({"agent": "worker"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/tasks/{a_id}/complete"),
            json!({"agent": "worker", "result": {"done": true}}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(json_request(
            "POST",
            &format!("/tasks/{b_id}/claim"),
            json!({"agent": "worker"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn auth_exempts_health_and_agent_docs_only() {
    let app = app(Some("secret"));
    let response = app.clone().oneshot(get("/tasks")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(response).await, json!({"error": "unauthorized"}));

    let wrong = Request::builder()
        .uri("/tasks")
        .header(header::AUTHORIZATION, "Bearer wrong")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(wrong).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = app.clone().oneshot(get("/healthz")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(response).await, json!({"error": "unauthorized"}));

    let correct = Request::builder()
        .uri("/tasks")
        .header(header::AUTHORIZATION, "Bearer secret")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(correct).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn cancel_and_retry_accept_no_body_and_validate_present_body() {
    let app = app(None);
    let task = create_task(&app, json!({"title": "cancel me"})).await;
    let id = task["id"].as_str().unwrap();

    let cancel = Request::builder()
        .method("POST")
        .uri(format!("/tasks/{id}/cancel"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(cancel).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let retry = Request::builder()
        .method("POST")
        .uri(format!("/tasks/{id}/retry"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(retry).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/tasks/{id}/cancel"),
            json!({"unknown": true}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await.get("error").is_some());

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/tasks/{id}/cancel"),
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/tasks/{id}/retry"),
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(json_request(
            "POST",
            &format!("/tasks/{id}/cancel"),
            json!({"padding": "x".repeat(3 * 1024 * 1024)}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body_json(response).await.get("error").is_some());
}

#[tokio::test]
async fn patch_distinguishes_null_from_missing() {
    let app = app(None);
    let task = create_task(&app, json!({"title": "patch null"})).await;
    let id = task["id"].as_str().unwrap();

    let response = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/tasks/{id}"),
            json!({"title": null}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(response).await.get("error").is_some());

    let response = app
        .oneshot(json_request(
            "PATCH",
            &format!("/tasks/{id}"),
            json!({"metadata": null}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["metadata"], Value::Null);
}

#[tokio::test]
async fn json_rejections_preserve_content_type_and_body_limit_statuses() {
    let app = app(None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks")
                .body(Body::from(r#"{"title":"missing content type"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(body_json(response).await.get("error").is_some());

    let response = app
        .oneshot(json_request(
            "POST",
            "/tasks",
            json!({"title": "x".repeat(3 * 1024 * 1024)}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body_json(response).await.get("error").is_some());
}

#[tokio::test]
async fn malformed_task_paths_and_method_errors_are_json() {
    let app = app(None);
    let response = app.clone().oneshot(get("/tasks/%FF")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(body_json(response).await.get("error").is_some());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tasks/not-a-uuid/claim")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(body_json(response).await.get("error").is_some());

    let response = app.oneshot(get("/tasks/claim")).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(body_json(response).await.get("error").is_some());
}

#[tokio::test]
async fn claimed_by_filter_release_and_fail_work_over_http() {
    let app = app(None);
    let first = create_task(&app, json!({"title": "first owner"})).await;
    let second = create_task(&app, json!({"title": "second owner"})).await;
    let first_id = first["id"].as_str().unwrap();
    let second_id = second["id"].as_str().unwrap();

    for (id, agent) in [(first_id, "one"), (second_id, "two")] {
        let response = app
            .clone()
            .oneshot(json_request(
                "POST",
                &format!("/tasks/{id}/claim"),
                json!({"agent": agent}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let response = app
        .clone()
        .oneshot(get("/tasks?claimed_by=one"))
        .await
        .unwrap();
    let list = body_json(response).await;
    assert_eq!(list["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(list["tasks"][0]["id"], first_id);

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/tasks/{first_id}/release"),
            json!({"agent": "wrong"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        body_json(response).await["error"]
            .as_str()
            .unwrap()
            .contains("wrong")
    );

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/tasks/{first_id}/release"),
            json!({"agent": "one"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["status"], "todo");

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/tasks/{second_id}/fail"),
            json!({"agent": "wrong"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let response = app
        .oneshot(json_request(
            "POST",
            &format!("/tasks/{second_id}/fail"),
            json!({"agent": "two", "result": {"reason": "test"}}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let failed = body_json(response).await;
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["result"], json!({"reason": "test"}));
}

#[tokio::test]
async fn dependency_conflicts_list_dependents_and_reject_cycles() {
    let app = app(None);
    let parent = create_task(&app, json!({"title": "parent"})).await;
    let parent_id = parent["id"].as_str().unwrap();
    let child = create_task(&app, json!({"title": "child", "depends_on": [parent_id]})).await;
    let child_id = child["id"].as_str().unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/tasks/{parent_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        body_json(response).await["error"]
            .as_str()
            .unwrap()
            .contains(child_id)
    );

    let response = app
        .oneshot(json_request(
            "PATCH",
            &format!("/tasks/{parent_id}"),
            json!({"depends_on": [child_id]}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        body_json(response).await["error"]
            .as_str()
            .unwrap()
            .contains("dependency cycle")
    );
}

#[tokio::test]
async fn task_list_sorts_by_priority_created_at_and_id() {
    let app = app(None);
    let mut created = Vec::new();
    for (title, priority) in [("low", 0), ("high old", 10), ("high new", 10), ("mid", 5)] {
        if title == "high new" {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        created.push(create_task(&app, json!({"title": title, "priority": priority})).await);
    }
    created.sort_by(|left, right| {
        right["priority"]
            .as_i64()
            .cmp(&left["priority"].as_i64())
            .then_with(|| {
                left["created_at"]
                    .as_str()
                    .cmp(&right["created_at"].as_str())
            })
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    let expected: Vec<_> = created.iter().map(|task| task["id"].clone()).collect();

    let response = app.oneshot(get("/tasks")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let actual: Vec<_> = body["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].clone())
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(body["tasks"][0]["title"], "high old");
    assert_eq!(body["tasks"][1]["title"], "high new");
}
