//! End-to-end tests for `confluence bulk delete`, driving the real binary
//! against a mock Confluence.
//!
//! The command used to delete every page a CQL query matched as soon as it
//! ran, with no prompt and no terminal check. It now lists by default, and
//! `--execute` needs the match count typed back or `--yes`. The mock's DELETE
//! endpoint carries an explicit expected count, verified when the server is
//! dropped, so a regression that deletes where it should not fails the test.

use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;
use wiremock::matchers::{method, path as path_matcher, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_atlassian-cli");
const CQL: &str = "space = OLD AND type = page";

/// Write a config pointing at `base_url`. HTTP is allowed because `ApiClient`
/// exempts localhost from its HTTPS requirement.
fn write_config(dir: &Path, base_url: &str) -> std::path::PathBuf {
    let config_path = dir.join("config.yaml");
    std::fs::write(
        &config_path,
        format!(
            "default_profile: local\nprofiles:\n  local:\n    email: dev@example.com\n    base_url: {base_url}\n"
        ),
    )
    .unwrap();
    config_path
}

/// Run the CLI with stdin closed, so no terminal is available to confirm on.
fn run(config: &Path, args: &[&str]) -> std::process::Output {
    let dir = config.parent().unwrap_or_else(|| Path::new("."));
    Command::new(BIN)
        .arg("--config")
        .arg(config)
        // Keep the CLI away from the developer's real configuration: `--config`
        // moves only the config file, so without this the credential lookup
        // still reaches $HOME.
        .env("HOME", dir)
        .env("ATLASSIAN_CLI_CONFIG_DIR", dir)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ATLASSIAN_API_TOKEN")
        .env_remove("ATLASSIAN_BITBUCKET_TOKEN")
        .env_remove("BITBUCKET_TOKEN")
        .env("ATLASSIAN_CLI_TOKEN_LOCAL", "fake-token")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run the CLI")
}

async fn mock_search(server: &MockServer, pages: &[(&str, &str)]) {
    let results: Vec<_> = pages
        .iter()
        .map(|(id, title)| serde_json::json!({ "content": { "id": id, "title": title } }))
        .collect();

    Mock::given(method("GET"))
        .and(path_matcher("/wiki/rest/api/content/search"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "results": results })),
        )
        .expect(1)
        .mount(server)
        .await;
}

/// Accept page deletions, and require exactly `expected` of them.
async fn mock_delete(server: &MockServer, expected: u64) {
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/wiki/api/v2/pages/[^/]+$"))
        .respond_with(ResponseTemplate::new(204))
        .expect(expected)
        .mount(server)
        .await;
}

async fn delete_paths(server: &MockServer) -> Vec<String> {
    let mut paths: Vec<String> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::DELETE)
        .map(|r| r.url.path().to_string())
        .collect();
    paths.sort();
    paths
}

/// Without `--execute` the pages are listed with their ids and titles, and no
/// DELETE is sent.
#[tokio::test]
async fn listing_is_the_default_and_deletes_nothing() {
    let server = MockServer::start().await;
    mock_search(&server, &[("101", "Old roadmap"), ("102", "Old notes")]).await;
    mock_delete(&server, 0).await;

    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &["-f", "json", "confluence", "bulk", "delete", "--cql", CQL],
    );

    assert!(
        out.status.success(),
        "listing failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listed: Vec<serde_json::Value> =
        serde_json::from_slice(&out.stdout).expect("stdout should be a JSON array");
    let summary: Vec<(&str, &str, &str)> = listed
        .iter()
        .map(|row| {
            (
                row["id"].as_str().unwrap_or_default(),
                row["title"].as_str().unwrap_or_default(),
                row["action"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("101", "Old roadmap", "would delete"),
            ("102", "Old notes", "would delete")
        ]
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--execute"),
        "the listing should say how to delete: {stderr}"
    );
    assert!(delete_paths(&server).await.is_empty());
}

/// A search hit without a title is still listed, by ID alone.
#[tokio::test]
async fn a_page_without_a_title_is_listed_by_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_matcher("/wiki/rest/api/content/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [
                { "content": { "id": "201" } },
                { "content": { "id": "202", "title": null } }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    mock_delete(&server, 0).await;

    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &["-f", "json", "confluence", "bulk", "delete", "--cql", CQL],
    );

    assert!(
        out.status.success(),
        "listing failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listed: Vec<serde_json::Value> =
        serde_json::from_slice(&out.stdout).expect("stdout should be a JSON array");
    let ids: Vec<&str> = listed
        .iter()
        .map(|row| row["id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(ids, vec!["201", "202"]);
    assert!(listed.iter().all(|row| row["title"] == ""));
    assert!(delete_paths(&server).await.is_empty());
}

/// `--yes` only skips the typed confirmation. It does not enable deletion.
#[tokio::test]
async fn yes_without_execute_still_only_lists() {
    let server = MockServer::start().await;
    mock_search(&server, &[("101", "Old roadmap")]).await;
    mock_delete(&server, 0).await;

    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &["confluence", "bulk", "delete", "--cql", CQL, "--yes"],
    );

    assert!(out.status.success());
    assert!(delete_paths(&server).await.is_empty());
}

/// The old flag is hidden but still accepted, and behaves like the listing.
#[tokio::test]
async fn deprecated_dry_run_still_lists() {
    let server = MockServer::start().await;
    mock_search(&server, &[("101", "Old roadmap")]).await;
    mock_delete(&server, 0).await;

    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &["confluence", "bulk", "delete", "--cql", CQL, "--dry-run"],
    );

    assert!(
        out.status.success(),
        "--dry-run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("101"));
    assert!(delete_paths(&server).await.is_empty());
}

/// `--dry-run` would otherwise silently override `--execute`, so clap rejects
/// the pair before anything is requested.
#[tokio::test]
async fn dry_run_and_execute_are_rejected_together() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "confluence",
            "bulk",
            "delete",
            "--cql",
            CQL,
            "--dry-run",
            "--execute",
        ],
    );

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be used with"),
        "expected a clap conflict error: {stderr}"
    );
}

/// A script that never had a chance to answer must not delete: without a
/// terminal and without `--yes` the command refuses.
#[tokio::test]
async fn execute_without_a_terminal_or_yes_refuses_and_deletes_nothing() {
    let server = MockServer::start().await;
    mock_search(&server, &[("101", "Old roadmap"), ("102", "Old notes")]).await;
    mock_delete(&server, 0).await;

    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &["confluence", "bulk", "delete", "--cql", CQL, "--execute"],
    );

    assert!(!out.status.success(), "must fail without a terminal");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Refusing to continue"), "{stderr}");
    assert!(stderr.contains("--yes"), "{stderr}");
    assert!(delete_paths(&server).await.is_empty());
}

#[tokio::test]
async fn execute_with_yes_deletes_every_match() {
    let server = MockServer::start().await;
    mock_search(&server, &[("101", "Old roadmap"), ("102", "Old notes")]).await;
    mock_delete(&server, 2).await;

    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "confluence",
            "bulk",
            "delete",
            "--cql",
            CQL,
            "--execute",
            "--yes",
        ],
    );

    assert!(
        out.status.success(),
        "deletion failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        delete_paths(&server).await,
        vec!["/wiki/api/v2/pages/101", "/wiki/api/v2/pages/102"]
    );
}

/// One DELETE fails. The other pages are still deleted, and the command exits
/// non-zero instead of reporting success.
#[tokio::test]
async fn a_failed_delete_fails_the_command_and_the_others_still_run() {
    let server = MockServer::start().await;
    mock_search(
        &server,
        &[
            ("101", "Old roadmap"),
            ("102", "Locked"),
            ("103", "Old notes"),
        ],
    )
    .await;
    Mock::given(method("DELETE"))
        .and(path_matcher("/wiki/api/v2/pages/102"))
        .respond_with(ResponseTemplate::new(404))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    mock_delete(&server, 2).await;

    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "confluence",
            "bulk",
            "delete",
            "--cql",
            CQL,
            "--execute",
            "--yes",
        ],
    );

    assert!(
        !out.status.success(),
        "a failed delete must fail the command: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        delete_paths(&server).await,
        vec![
            "/wiki/api/v2/pages/101",
            "/wiki/api/v2/pages/102",
            "/wiki/api/v2/pages/103"
        ]
    );
}
