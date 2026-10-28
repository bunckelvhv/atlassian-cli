//! Exercise Confluence CQL pagination through the built CLI. The API can
//! return fewer results than requested in one response, with the next page
//! identified only by `_links.next`.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;
use wiremock::matchers::{method, path as path_matcher, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_atlassian-cli");

fn write_config(dir: &Path, base_url: &str) -> std::path::PathBuf {
    let config = dir.join("config.yaml");
    std::fs::write(
        &config,
        format!("default_profile: local\nprofiles:\n  local:\n    email: dev@example.com\n    base_url: {base_url}\n"),
    )
    .unwrap();
    config
}

fn run(config: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .arg("--config")
        .arg(config)
        .arg("--config-dir")
        .arg(config.parent().unwrap())
        .env("HOME", config.parent().unwrap())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ATLASSIAN_API_TOKEN")
        .env("ATLASSIAN_CLI_TOKEN_LOCAL", "fake-token")
        .args(args)
        .output()
        .unwrap()
}

fn result(id: usize) -> serde_json::Value {
    serde_json::json!({
        "id": id.to_string(),
        "title": format!("Page {id}"),
        "type": "page"
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cql_fetches_results_beyond_the_first_250() {
    let server = MockServer::start().await;
    let first: Vec<_> = (1..=250).map(result).collect();

    Mock::given(method("GET"))
        .and(path_matcher("/wiki/rest/api/content/search"))
        .and(query_param("cursor", "page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [result(251)]
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_matcher("/wiki/rest/api/content/search"))
        .and(query_param("cql", "type=page"))
        .and(query_param("limit", "250"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": first,
            "_links": {"next": "/rest/api/content/search?cql=type%3Dpage&limit=250&cursor=page-2"}
        })))
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());
    let out = run(
        &config,
        &[
            "confluence",
            "search",
            "cql",
            "type=page",
            "--limit",
            "300",
            "-f",
            "json",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 251);
    assert_eq!(rows[250]["id"], "251");

    let all = run(
        &config,
        &[
            "confluence",
            "search",
            "cql",
            "type=page",
            "--limit",
            "0",
            "-f",
            "json",
        ],
    );
    assert!(
        all.status.success(),
        "{}",
        String::from_utf8_lossy(&all.stderr)
    );
    let all_rows: serde_json::Value = serde_json::from_slice(&all.stdout).unwrap();
    assert_eq!(all_rows.as_array().unwrap().len(), 251);
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cql_limit_applies_to_the_total_and_warns_when_more_exist() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_matcher("/wiki/rest/api/content/search"))
        .and(query_param("cursor", "page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [result(3), result(4)]
        })))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_matcher("/wiki/rest/api/content/search"))
        .and(query_param("limit", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [result(1), result(2)],
            "_links": {"next": "/rest/api/content/search?cql=type%3Dpage&limit=3&cursor=page-2"}
        })))
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());
    let out = run(
        &config,
        &[
            "confluence",
            "search",
            "cql",
            "type=page",
            "--limit",
            "3",
            "-f",
            "json",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 3);
    assert_eq!(rows[2]["id"], "3");
    assert!(String::from_utf8_lossy(&out.stderr).contains("more match this query"));
}

fn search(config: &Path, limit: Option<&str>) -> std::process::Output {
    let mut args = vec!["confluence", "search", "cql", "type=page", "-f", "json"];
    if let Some(limit) = limit {
        args.extend(["--limit", limit]);
    }
    run(config, &args)
}

fn rows(out: &std::process::Output) -> Vec<serde_json::Value> {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuation_links_keep_the_cursor_and_normalize_wiki_once() {
    for base_suffix in ["", "/wiki"] {
        for link_form in ["rest", "wiki", "absolute"] {
            let server = MockServer::start().await;
            let suffix = "/rest/api/content/search?cursor=a%2Bb%26c%3D%25&limit=250";
            let next = match link_form {
                "rest" => suffix.to_string(),
                "wiki" => format!("/wiki{suffix}"),
                _ => format!("{}/wiki{suffix}", server.uri()),
            };
            Mock::given(method("GET"))
                .and(path_matcher("/wiki/rest/api/content/search"))
                .and(query_param("cql", "type=page"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [result(1)], "_links": {"next": next}
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_matcher("/wiki/rest/api/content/search"))
                .and(query_param("cursor", "a+b&c=%"))
                .and(query_param("limit", "250"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [result(2)]
                })))
                .expect(1)
                .mount(&server)
                .await;
            let dir = TempDir::new().unwrap();
            let config = write_config(dir.path(), &format!("{}{base_suffix}", server.uri()));
            let out = search(&config, Some("0"));
            assert_eq!(
                rows(&out)
                    .iter()
                    .map(|row| row["id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["1", "2"]
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
            assert!(!String::from_utf8_lossy(&out.stderr).contains("pagination stopped"));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blank_continuation_links_end_the_search() {
    for next in [
        serde_json::Value::Null,
        serde_json::json!(""),
        serde_json::json!(" \t\n"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/wiki/rest/api/content/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": [result(1)], "_links": {"next": next}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = TempDir::new().unwrap();
        let config = write_config(dir.path(), &server.uri());
        let out = search(&config, Some("0"));
        assert_eq!(rows(&out).len(), 1);
        assert!(!String::from_utf8_lossy(&out.stderr).contains("pagination stopped"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_origin_continuation_sends_no_request_to_that_origin() {
    let server = MockServer::start().await;
    let foreign = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [result(2)]
        })))
        .expect(0)
        .mount(&foreign)
        .await;
    Mock::given(method("GET"))
        .and(path_matcher("/wiki/rest/api/content/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [result(1)],
            "_links": {"next": format!("{}/wiki/rest/api/content/search?cursor=2", foreign.uri())}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());
    let out = search(&config, Some("0"));
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Failed to search with CQL"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(foreign.received_requests().await.unwrap().is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_limit_remains_25_results() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_matcher("/wiki/rest/api/content/search"))
        .and(query_param("limit", "25"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": (1..=25).map(result).collect::<Vec<_>>(),
            "_links": {"next": "/wiki/rest/api/content/search?cursor=2"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());
    let out = search(&config, None);
    assert_eq!(rows(&out).len(), 25);
    assert!(String::from_utf8_lossy(&out.stderr).contains("more match this query"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_request_budget_stops_at_50_and_reports_incomplete_results() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_matcher("/wiki/rest/api/content/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [result(1)],
            "_links": {"next": "/wiki/rest/api/content/search?cursor=repeat"}
        })))
        .expect(50)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());
    let out = search(&config, Some("0"));
    assert_eq!(rows(&out).len(), 50);
    assert!(String::from_utf8_lossy(&out.stderr).contains("pagination stopped before completion"));
    assert_eq!(server.received_requests().await.unwrap().len(), 50);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_page_with_a_cursor_reports_incomplete_results() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_matcher("/wiki/rest/api/content/search"))
        .and(query_param("cql", "type=page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [result(1)], "_links": {"next": "/rest/api/content/search?cursor=2"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_matcher("/wiki/rest/api/content/search"))
        .and(query_param("cursor", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [], "_links": {"next": "/rest/api/content/search?cursor=3"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());
    let out = search(&config, Some("0"));
    assert_eq!(rows(&out).len(), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("pagination stopped before completion"));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}
