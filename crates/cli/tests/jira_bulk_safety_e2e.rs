//! `jira bulk transition`, `assign` and `label`: preview by default, a typed
//! confirmation for `--execute`, and a transition resolved per issue.
//!
//! These commands used to change every issue a JQL query matched as soon as
//! they were run, and `bulk transition` sent the transition ID found on the
//! first issue to all of them. They drive the built binary against a mock Jira.
//! The binary's stdin is not a terminal here, which is what a script or a cron
//! job looks like: without `--yes` it has no way to confirm and must refuse.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{method, path as path_matcher};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_atlassian-cli");

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

fn run(config: &Path, args: &[&str]) -> Output {
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
        .expect("failed to run the CLI")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn transition(id: &str, name: &str, to: &str) -> Value {
    json!({ "id": id, "name": name, "to": { "name": to } })
}

async fn mount_search(server: &MockServer, keys: &[&str]) {
    let issues: Vec<Value> = keys.iter().map(|k| json!({ "key": k })).collect();
    Mock::given(method("POST"))
        .and(path_matcher("/rest/api/3/search/jql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "issues": issues })))
        .mount(server)
        .await;
}

async fn mount_transitions(server: &MockServer, key: &str, transitions: &[Value]) {
    mount_transitions_expecting(server, key, transitions, None).await;
}

/// Serve the transitions of `key` and accept a transition POST for it. With
/// `Some(n)` the server verifies on drop that exactly `n` POSTs arrived
/// (`Some(0)` for a run that must not write).
async fn mount_transitions_expecting(
    server: &MockServer,
    key: &str,
    transitions: &[Value],
    posts: Option<u64>,
) {
    Mock::given(method("GET"))
        .and(path_matcher(format!("/rest/api/3/issue/{key}/transitions")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "transitions": transitions })),
        )
        .mount(server)
        .await;
    let post = Mock::given(method("POST"))
        .and(path_matcher(format!("/rest/api/3/issue/{key}/transitions")))
        .respond_with(ResponseTemplate::new(204));
    match posts {
        Some(expected) => post.expect(expected).mount(server).await,
        None => post.mount(server).await,
    }
}

/// Two workflows in one result: ID 21 starts work in A-1 and finishes in B-1.
async fn two_workflow_server() -> MockServer {
    two_workflow_server_expecting(None).await
}

/// The same server for runs that must not write.
async fn read_only_two_workflow_server() -> MockServer {
    two_workflow_server_expecting(Some(0)).await
}

async fn two_workflow_server_expecting(posts: Option<u64>) -> MockServer {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1"]).await;
    mount_transitions_expecting(
        &server,
        "A-1",
        &[
            transition("21", "Start progress", "In Progress"),
            transition("31", "Finish", "Done"),
        ],
        posts,
    )
    .await;
    mount_transitions_expecting(
        &server,
        "B-1",
        &[
            transition("21", "Finish", "Done"),
            transition("11", "Start progress", "In Progress"),
        ],
        posts,
    )
    .await;
    server
}

/// Two issues that accept an assignee change and a label update.
async fn writable_server() -> MockServer {
    writable_server_expecting(None).await
}

/// The same server for runs that must not write: it verifies on drop that no
/// PUT arrived.
async fn read_only_server() -> MockServer {
    writable_server_expecting(Some(0)).await
}

async fn mount_put(server: &MockServer, request_path: String, puts: Option<u64>) {
    let mock = Mock::given(method("PUT"))
        .and(path_matcher(request_path))
        .respond_with(ResponseTemplate::new(204));
    match puts {
        Some(expected) => mock.expect(expected).mount(server).await,
        None => mock.mount(server).await,
    }
}

async fn writable_server_expecting(puts: Option<u64>) -> MockServer {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1"]).await;
    for key in ["A-1", "B-1"] {
        mount_put(&server, format!("/rest/api/3/issue/{key}/assignee"), puts).await;
        Mock::given(method("GET"))
            .and(path_matcher(format!("/rest/api/3/issue/{key}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "fields": { "labels": ["old"] } })),
            )
            .mount(&server)
            .await;
        mount_put(&server, format!("/rest/api/3/issue/{key}"), puts).await;
    }
    server
}

/// Path and JSON body of every write received. The search that selects the
/// issues is a POST as well, and is not a write.
async fn writes(server: &MockServer) -> Vec<(String, Value)> {
    let mut writes: Vec<(String, Value)> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            (r.method == wiremock::http::Method::POST || r.method == wiremock::http::Method::PUT)
                && r.url.path() != "/rest/api/3/search/jql"
        })
        .map(|r| {
            (
                r.url.path().to_string(),
                serde_json::from_slice(&r.body).unwrap_or(Value::Null),
            )
        })
        .collect();
    writes.sort_by(|a, b| a.0.cmp(&b.0));
    writes
}

fn transition_id_of(body: &Value) -> &str {
    body["transition"]["id"].as_str().unwrap_or("")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_lists_by_default_with_the_status_each_issue_would_reach() {
    let server = read_only_two_workflow_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "transition",
            "--jql",
            "project in (A, B)",
            "--transition",
            "Finish",
            "-f",
            "json",
        ],
    );

    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let rows: Value = serde_json::from_str(&stdout(&out)).expect("preview is a JSON list");
    assert_eq!(
        rows,
        json!([
            {"key": "A-1", "transition": "Finish", "to_status": "Done",
             "action": "would transition"},
            {"key": "B-1", "transition": "Finish", "to_status": "Done",
             "action": "would transition"}
        ])
    );
    assert!(
        stderr(&out).contains("--execute"),
        "must say how to apply: {}",
        stderr(&out)
    );
    assert!(writes(&server).await.is_empty(), "listing must not write");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_preview_reports_issues_where_it_is_not_available() {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1"]).await;
    mount_transitions_expecting(
        &server,
        "A-1",
        &[transition("31", "Finish", "Done")],
        Some(0),
    )
    .await;
    mount_transitions_expecting(
        &server,
        "B-1",
        &[transition("11", "Reopen", "To Do")],
        Some(0),
    )
    .await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "transition",
            "--jql",
            "project in (A, B)",
            "--transition",
            "Finish",
            "-f",
            "json",
        ],
    );

    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let rows: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(rows[0]["action"], "would transition");
    assert_eq!(rows[1]["key"], "B-1");
    assert_eq!(rows[1]["action"], "not available");
    assert!(writes(&server).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_execute_without_yes_and_without_a_terminal_is_refused() {
    let server = read_only_two_workflow_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "transition",
            "--jql",
            "project in (A, B)",
            "--transition",
            "Finish",
            "--execute",
        ],
    );

    assert!(
        !out.status.success(),
        "must refuse without a way to confirm"
    );
    let err = stderr(&out);
    assert!(err.contains("--yes"), "must say how to proceed: {err}");
    assert!(
        err.contains("About to transition 2 of 2"),
        "must state the count: {err}"
    );
    assert!(writes(&server).await.is_empty(), "nothing may be sent");
}

/// The defect: the ID found on the first issue was sent to every issue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_by_name_runs_the_matching_transition_on_each_issue() {
    let server = two_workflow_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "transition",
            "--jql",
            "project in (A, B)",
            "--transition",
            "start PROGRESS",
            "--execute",
            "--yes",
        ],
    );

    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let writes = writes(&server).await;
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert_eq!(writes[0].0, "/rest/api/3/issue/A-1/transitions");
    assert_eq!(transition_id_of(&writes[0].1), "21");
    assert_eq!(writes[1].0, "/rest/api/3/issue/B-1/transitions");
    assert_eq!(transition_id_of(&writes[1].1), "11");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_by_id_is_an_error_where_the_id_means_something_else() {
    let server = two_workflow_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "transition",
            "--jql",
            "project in (A, B)",
            "--transition",
            "21",
            "--execute",
            "--yes",
        ],
    );

    assert!(!out.status.success(), "B-1 must make the command fail");
    let err = stderr(&out);
    assert!(err.contains("B-1"), "the error names the issue: {err}");
    let writes = writes(&server).await;
    assert_eq!(writes.len(), 1, "only A-1 is transitioned: {writes:?}");
    assert_eq!(writes[0].0, "/rest/api/3/issue/A-1/transitions");
    assert_eq!(transition_id_of(&writes[0].1), "21");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transition_unavailable_on_one_issue_fails_but_the_others_are_transitioned() {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1", "C-1"]).await;
    mount_transitions(&server, "A-1", &[transition("31", "Finish", "Done")]).await;
    mount_transitions(&server, "B-1", &[transition("11", "Reopen", "To Do")]).await;
    mount_transitions(&server, "C-1", &[transition("41", "finish", "Done")]).await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "transition",
            "--jql",
            "project = X",
            "--transition",
            "Finish",
            "--execute",
            "--yes",
        ],
    );

    assert!(
        !out.status.success(),
        "an unavailable transition is an error"
    );
    let err = stderr(&out);
    assert!(err.contains("B-1"), "{err}");
    assert!(
        err.contains("Finish"),
        "names the requested transition: {err}"
    );
    let paths: Vec<String> = writes(&server).await.into_iter().map(|w| w.0).collect();
    assert_eq!(
        paths,
        vec![
            "/rest/api/3/issue/A-1/transitions",
            "/rest/api/3/issue/C-1/transitions"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assign_lists_by_default() {
    let server = read_only_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "assign",
            "--jql",
            "project = X",
            "--assignee",
            "acc-1",
            "-f",
            "json",
        ],
    );

    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let rows: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(rows.as_array().map(Vec::len), Some(2));
    assert_eq!(rows[0]["key"], "A-1");
    assert_eq!(rows[0]["assignee"], "acc-1");
    assert!(writes(&server).await.is_empty(), "listing must not write");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assign_execute_without_yes_and_without_a_terminal_is_refused() {
    let server = read_only_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "assign",
            "--jql",
            "project = X",
            "--assignee",
            "acc-1",
            "--execute",
        ],
    );

    assert!(!out.status.success());
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));
    assert!(writes(&server).await.is_empty(), "nothing may be sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assign_execute_with_yes_assigns_every_issue() {
    let server = writable_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "assign",
            "--jql",
            "project = X",
            "--assignee",
            "acc-1",
            "--execute",
            "--yes",
        ],
    );

    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let writes = writes(&server).await;
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert_eq!(writes[0].0, "/rest/api/3/issue/A-1/assignee");
    assert_eq!(writes[0].1, json!({ "accountId": "acc-1" }));
    assert_eq!(writes[1].0, "/rest/api/3/issue/B-1/assignee");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn label_lists_by_default() {
    let server = read_only_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "label",
            "--jql",
            "project = X",
            "--action",
            "set",
            "--labels",
            "a,b",
            "-f",
            "json",
        ],
    );

    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let rows: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(rows.as_array().map(Vec::len), Some(2));
    assert_eq!(rows[0]["labels"], "a,b");
    assert_eq!(rows[0]["action"], "would set labels");
    assert!(writes(&server).await.is_empty(), "listing must not write");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn label_execute_without_yes_and_without_a_terminal_is_refused() {
    let server = read_only_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "label",
            "--jql",
            "project = X",
            "--action",
            "add",
            "--labels",
            "a",
            "--execute",
        ],
    );

    assert!(!out.status.success());
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));
    assert!(writes(&server).await.is_empty(), "nothing may be sent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn label_execute_with_yes_writes_the_labels() {
    let server = writable_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "label",
            "--jql",
            "project = X",
            "--action",
            "add",
            "--labels",
            "new",
            "--execute",
            "--yes",
        ],
    );

    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let writes = writes(&server).await;
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert_eq!(
        writes[0].1,
        json!({ "fields": { "labels": ["old", "new"] } })
    );
}

/// `set` without labels used to send `labels: []` for every match.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn label_without_labels_is_rejected_for_every_action() {
    let server = read_only_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    for action in ["add", "remove", "set"] {
        let missing = run(
            &config,
            &[
                "jira",
                "bulk",
                "label",
                "--jql",
                "project = X",
                "--action",
                action,
                "--execute",
                "--yes",
            ],
        );
        assert!(!missing.status.success(), "{action}: --labels is required");
        assert!(
            stderr(&missing).contains("--labels"),
            "{action}: {}",
            stderr(&missing)
        );

        let empty = run(
            &config,
            &[
                "jira",
                "bulk",
                "label",
                "--jql",
                "project = X",
                "--action",
                action,
                "--labels",
                "",
                "--execute",
                "--yes",
            ],
        );
        assert!(!empty.status.success(), "{action}: empty --labels");
    }

    assert!(
        writes(&server).await.is_empty(),
        "no label may be written without a label"
    );
}

/// The old flag stays accepted so scripts keep working, and it still means
/// "change nothing".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_deprecated_dry_run_flag_still_lists_only() {
    let server = read_only_two_workflow_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "transition",
            "--jql",
            "project in (A, B)",
            "--transition",
            "Finish",
            "--dry-run",
        ],
    );

    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(writes(&server).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_cannot_be_combined_with_execute() {
    let server = read_only_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let cases: [&[&str]; 3] = [
        &["transition", "--transition", "Finish"],
        &["assign", "--assignee", "acc-1"],
        &["label", "--action", "add", "--labels", "a"],
    ];
    for sub in cases {
        let mut args = vec!["jira", "bulk"];
        args.extend(sub.iter().copied());
        args.extend(["--jql", "project = X", "--dry-run", "--execute", "--yes"]);
        let out = run(&config, &args);
        assert_eq!(out.status.code(), Some(2), "{sub:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("cannot be used with"),
            "{sub:?} must fail on the conflict itself: {}",
            stderr(&out)
        );
    }
    assert!(writes(&server).await.is_empty());
}

/// Nothing matches: `-f json` gets an empty list, the same as a non-empty
/// preview gets a list, not prose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_match_renders_an_empty_json_list_for_every_command() {
    let server = MockServer::start().await;
    mount_search(&server, &[]).await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let cases: [&[&str]; 3] = [
        &["transition", "--transition", "Finish"],
        &["assign", "--assignee", "acc-1"],
        &["label", "--action", "add", "--labels", "a"],
    ];
    for sub in cases {
        let mut args = vec!["jira", "bulk"];
        args.extend(sub.iter().copied());
        args.extend(["--jql", "project = X", "-f", "json"]);
        let out = run(&config, &args);
        assert!(out.status.success(), "{sub:?}: {}", stderr(&out));
        let parsed: Value = serde_json::from_str(&stdout(&out))
            .unwrap_or_else(|e| panic!("{sub:?}: not JSON ({e}): {}", stdout(&out)));
        assert_eq!(parsed, json!([]), "{sub:?}");
    }
    assert!(writes(&server).await.is_empty());
}

/// Three issues where the first one's transitions cannot be read.
async fn server_with_unreadable_first_issue(posts: Option<u64>) -> MockServer {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1", "C-1"]).await;
    // A-1 has no transitions mock, so its lookup answers 404.
    mount_transitions_expecting(&server, "B-1", &[transition("21", "Done", "Done")], posts).await;
    mount_transitions_expecting(&server, "C-1", &[transition("21", "Done", "Done")], posts).await;
    server
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_first_issue_does_not_stop_a_transition_name() {
    let server = server_with_unreadable_first_issue(None).await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "transition",
            "--jql",
            "project = X",
            "--transition",
            "Done",
            "--execute",
            "--yes",
        ],
    );

    assert!(!out.status.success(), "the first issue is an error");
    assert!(stderr(&out).contains("A-1"), "{}", stderr(&out));
    let paths: Vec<String> = writes(&server).await.into_iter().map(|w| w.0).collect();
    assert_eq!(
        paths,
        vec![
            "/rest/api/3/issue/B-1/transitions",
            "/rest/api/3/issue/C-1/transitions"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_first_issue_sends_no_transition_for_an_id() {
    let server = server_with_unreadable_first_issue(Some(0)).await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "transition",
            "--jql",
            "project = X",
            "--transition",
            "21",
            "--execute",
            "--yes",
        ],
    );

    assert!(
        !out.status.success(),
        "an ID with no known meaning must fail"
    );
    assert!(writes(&server).await.is_empty(), "no POST at all");
}

/// "21" is the ID of one transition and the name of another on the first
/// issue: nothing runs and the error says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ambiguous_transition_value_sends_nothing() {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1"]).await;
    let ambiguous = [
        transition("21", "Start", "In Progress"),
        transition("22", "21", "Done"),
    ];
    mount_transitions_expecting(&server, "A-1", &ambiguous, Some(0)).await;
    mount_transitions_expecting(&server, "B-1", &ambiguous, Some(0)).await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "transition",
            "--jql",
            "project in (A, B)",
            "--transition",
            "21",
            "--execute",
            "--yes",
        ],
    );

    assert!(!out.status.success());
    assert!(stderr(&out).contains("ambiguous"), "{}", stderr(&out));
    assert!(writes(&server).await.is_empty(), "no POST at all");
}

/// `--yes` only skips the prompt. It does not turn a listing into a change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yes_alone_does_not_execute() {
    let server = read_only_server().await;
    let dir = TempDir::new().unwrap();
    let config = write_config(dir.path(), &server.uri());

    let out = run(
        &config,
        &[
            "jira",
            "bulk",
            "assign",
            "--jql",
            "project = X",
            "--assignee",
            "acc-1",
            "--yes",
        ],
    );

    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(writes(&server).await.is_empty());
}
