//! `jira bulk transition`: the transition resolved per issue, ambiguity, the
//! preview rows and the handler over a mock Jira. Split from `bulk.rs` to keep
//! that file within the size guideline.

use super::tests::{jira_ctx, mount_search};
use super::*;
use atlassian_cli_output::{OutputFormat, OutputRenderer};
use serde_json::json;
use wiremock::matchers::{method, path as path_matcher};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn available(items: &[(&str, &str, &str)]) -> Vec<AvailableTransition> {
    items
        .iter()
        .map(|(id, name, to)| AvailableTransition {
            id: id.to_string(),
            name: name.to_string(),
            to: Some(TransitionTarget {
                name: to.to_string(),
            }),
        })
        .collect()
}

#[test]
fn a_name_is_matched_case_insensitively() {
    let list = available(&[("11", "Start work", "Analysis"), ("21", "Done", "Done")]);
    let request = TransitionRequest::ByName("start WORK".into());
    let resolved = resolve_transition(&list, &request).expect("name must resolve");
    assert_eq!(resolved.id, "11");
    assert_eq!(resolved.name, "Start work");
    assert_eq!(resolved.to_status.as_deref(), Some("Analysis"));
}

#[test]
fn a_transition_that_is_not_offered_does_not_resolve() {
    let list = available(&[("11", "Start work", "Analysis")]);
    let request = TransitionRequest::ByName("Done".into());
    assert_eq!(
        resolve_transition(&list, &request),
        Err(Unresolved::NotAvailable)
    );
}

#[test]
fn an_id_only_resolves_where_it_keeps_its_name() {
    let request = TransitionRequest::ById {
        id: "21".into(),
        name: "In Progress".into(),
    };
    let same = available(&[("21", "In Progress", "In Progress")]);
    let other_workflow = available(&[("21", "Done", "Done"), ("11", "In Progress", "X")]);
    let missing = available(&[("11", "In Progress", "X")]);

    assert_eq!(resolve_transition(&same, &request).unwrap().id, "21");
    assert_eq!(
        resolve_transition(&other_workflow, &request),
        Err(Unresolved::NotAvailable)
    );
    assert_eq!(
        resolve_transition(&missing, &request),
        Err(Unresolved::NotAvailable)
    );
}

#[test]
fn the_first_issue_decides_what_an_id_means() {
    let first = available(&[("21", "In Progress", "In Progress")]);
    assert_eq!(
        TransitionRequest::from_first_issue(&first, "21"),
        TransitionRequest::ById {
            id: "21".into(),
            name: "In Progress".into()
        }
    );
    assert_eq!(
        TransitionRequest::from_first_issue(&first, "in progress"),
        TransitionRequest::ByName("in progress".into())
    );
}

#[test]
fn a_name_match_is_unicode_aware() {
    let list = available(&[("11", "Café", "Open"), ("12", "Other", "Open")]);
    let request = TransitionRequest::from_first_issue(&list, "CAFÉ");
    assert_eq!(request, TransitionRequest::ByName("CAFÉ".into()));
    let resolved = resolve_transition(&list, &request).expect("CAFÉ must match Café");
    assert_eq!(resolved.id, "11");

    let pinned = TransitionRequest::ById {
        id: "11".into(),
        name: "CAFÉ".into(),
    };
    assert_eq!(resolve_transition(&list, &pinned).unwrap().id, "11");
}

/// A name the first issue does not offer stays a name, so it is only used
/// on issues that have a transition of exactly that name.
#[test]
fn an_unknown_name_stays_a_name() {
    let first = available(&[("11", "Start work", "Analysis")]);
    let request = TransitionRequest::from_first_issue(&first, "Reopen");
    assert_eq!(request, TransitionRequest::ByName("Reopen".into()));

    let elsewhere = available(&[("31", "Reopen", "To Do")]);
    assert_eq!(resolve_transition(&elsewhere, &request).unwrap().id, "31");
}

/// A number that is neither an ID nor a name on the first issue is not
/// looked for by name on the others: a transition literally named "21"
/// there must not be run by accident.
#[test]
fn an_unknown_number_is_applied_to_no_issue() {
    let first = available(&[("11", "Start work", "Analysis")]);
    let request = TransitionRequest::from_first_issue(&first, "21");
    assert!(
        matches!(request, TransitionRequest::Unresolvable(_)),
        "{request:?}"
    );

    let elsewhere = available(&[("21", "Done", "Done"), ("22", "21", "Done")]);
    assert_eq!(
        resolve_transition(&elsewhere, &request),
        Err(Unresolved::NotAvailable)
    );
    assert_eq!(
        resolve_transition(&first, &request),
        Err(Unresolved::NotAvailable)
    );
}

/// A number that is a name on the first issue (and not an ID) is a name.
#[test]
fn a_number_that_is_a_name_on_the_first_issue_is_a_name() {
    let first = available(&[("11", "2026", "Done")]);
    assert_eq!(
        TransitionRequest::from_first_issue(&first, "2026"),
        TransitionRequest::ByName("2026".into())
    );
}

/// The value is the ID of one transition and the name of another one: no
/// guess, nothing runs.
#[test]
fn an_id_that_is_also_another_transitions_name_is_ambiguous() {
    let first = available(&[("21", "Start", "In Progress"), ("22", "21", "Done")]);
    let request = TransitionRequest::from_first_issue(&first, "21");
    let TransitionRequest::Unresolvable(reason) = &request else {
        panic!("expected an ambiguous request, got {request:?}");
    };
    assert!(reason.contains("ambiguous"), "{reason}");
    assert_eq!(
        resolve_transition(&first, &request),
        Err(Unresolved::NotAvailable)
    );

    // The other ID is not affected.
    assert_eq!(
        TransitionRequest::from_first_issue(&first, "22"),
        TransitionRequest::ById {
            id: "22".into(),
            name: "21".into()
        }
    );
}

/// One transition whose ID and name are the same value is not a conflict.
#[test]
fn an_id_that_is_also_its_own_name_is_not_ambiguous() {
    let first = available(&[("21", "21", "Done")]);
    assert_eq!(
        TransitionRequest::from_first_issue(&first, "21"),
        TransitionRequest::ById {
            id: "21".into(),
            name: "21".into()
        }
    );
}

#[test]
fn an_unreadable_first_issue_keeps_ids_closed_and_names_open() {
    assert_eq!(
        TransitionRequest::for_unreadable_first_issue("Done"),
        TransitionRequest::ByName("Done".into())
    );
    assert!(matches!(
        TransitionRequest::for_unreadable_first_issue("21"),
        TransitionRequest::Unresolvable(_)
    ));
}

fn reversed(list: &[AvailableTransition]) -> Vec<AvailableTransition> {
    list.iter().rev().cloned().collect()
}

fn candidates(items: &[(&str, &str)]) -> Unresolved {
    Unresolved::Ambiguous(
        items
            .iter()
            .map(|(id, name)| (id.to_string(), name.to_string()))
            .collect(),
    )
}

/// `Done` and `DONE` both match `--transition Done`. Taking the first one
/// made the transition depend on the order of Jira's response.
#[test]
fn case_variants_of_a_name_are_ambiguous_in_either_order() {
    let list = available(&[("11", "Done", "Resolved"), ("21", "DONE", "Cancelled")]);
    let request = TransitionRequest::ByName("Done".into());
    let expected = Err(candidates(&[("11", "Done"), ("21", "DONE")]));

    assert_eq!(resolve_transition(&list, &request), expected);
    assert_eq!(resolve_transition(&reversed(&list), &request), expected);
}

#[test]
fn identical_names_are_ambiguous() {
    let list = available(&[("21", "Done", "Cancelled"), ("11", "Done", "Resolved")]);
    let request = TransitionRequest::ByName("done".into());
    assert_eq!(
        resolve_transition(&list, &request),
        Err(candidates(&[("11", "Done"), ("21", "Done")]))
    );
}

/// Names that only collide after Unicode lowercasing are ambiguous too:
/// `CAFÉ`/`Café`, and the Kelvin sign (U+212A), which lowercases to `k`.
#[test]
fn a_unicode_lowercase_collision_is_ambiguous_in_either_order() {
    let accents = available(&[("11", "Café", "Open"), ("12", "CAFÉ", "Closed")]);
    let request = TransitionRequest::ByName("café".into());
    let expected = Err(candidates(&[("11", "Café"), ("12", "CAFÉ")]));
    assert_eq!(resolve_transition(&accents, &request), expected);
    assert_eq!(resolve_transition(&reversed(&accents), &request), expected);

    let kelvin = available(&[("31", "Kanban", "Open"), ("32", "\u{212A}anban", "Closed")]);
    let request = TransitionRequest::ByName("kanban".into());
    let expected = Err(candidates(&[("31", "Kanban"), ("32", "\u{212A}anban")]));
    assert_eq!(resolve_transition(&kelvin, &request), expected);
    assert_eq!(resolve_transition(&reversed(&kelvin), &request), expected);
}

/// The ID still picks one of several transitions with the same name, on
/// the first issue and on the others, whatever the order.
#[test]
fn an_explicit_id_disambiguates_duplicate_names() {
    let first = available(&[("11", "Done", "Resolved"), ("21", "DONE", "Cancelled")]);
    let other = reversed(&first);

    for (requested, id) in [("11", "11"), ("21", "21")] {
        let request = TransitionRequest::from_first_issue(&first, requested);
        assert!(
            matches!(&request, TransitionRequest::ById { id: pinned, .. } if pinned == id),
            "{request:?}"
        );
        assert_eq!(resolve_transition(&first, &request).unwrap().id, id);
        assert_eq!(resolve_transition(&other, &request).unwrap().id, id);
    }
}

/// The error names every candidate, in the same order whatever Jira
/// returned, and the issue is marked ambiguous rather than not available.
#[test]
fn an_ambiguous_issue_lists_the_candidates_independent_of_order() {
    let list = available(&[("21", "DONE", "Cancelled"), ("11", "Done", "Resolved")]);
    let request = TransitionRequest::ByName("Done".into());

    let plan = IssuePlan::for_issue("B-1".into(), &list, &request);
    let again = IssuePlan::for_issue("B-1".into(), &reversed(&list), &request);

    let skipped = plan
        .outcome
        .expect_err("an ambiguous issue must not resolve");
    assert!(skipped.ambiguous);
    assert!(skipped.message.starts_with("B-1: "), "{}", skipped.message);
    assert!(skipped.message.contains("ambiguous"), "{}", skipped.message);
    assert!(
        skipped.message.contains(r#"11 "Done", 21 "DONE""#),
        "{}",
        skipped.message
    );
    assert_eq!(again.outcome.expect_err("same request"), skipped);
}

fn transition_json(id: &str, name: &str, to: &str) -> Value {
    json!({ "id": id, "name": name, "to": { "name": to } })
}

/// Serve the transitions of `key` and accept a transition POST for it.
async fn mount_issue(server: &MockServer, key: &str, transitions: &[Value]) {
    mount_issue_expecting(server, key, transitions, None).await;
}

/// Like `mount_issue`, with the number of transition POSTs the server must
/// receive by the time it is dropped (`Some(0)` for a read-only run).
async fn mount_issue_expecting(
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
    mount_transition_post(server, key, posts).await;
}

async fn mount_transition_post(server: &MockServer, key: &str, posts: Option<u64>) {
    let mock = Mock::given(method("POST"))
        .and(path_matcher(format!("/rest/api/3/issue/{key}/transitions")))
        .respond_with(ResponseTemplate::new(204));
    match posts {
        Some(expected) => mock.expect(expected).mount(server).await,
        None => mock.mount(server).await,
    }
}

/// `(issue key, transition id)` of every transition POST received.
async fn transition_posts(server: &MockServer) -> Vec<(String, String)> {
    let mut posts: Vec<(String, String)> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            r.method == wiremock::http::Method::POST && r.url.path().ends_with("/transitions")
        })
        .map(|r| {
            let key = r
                .url
                .path()
                .split('/')
                .nth_back(1)
                .unwrap_or("")
                .to_string();
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            let id = body["transition"]["id"].as_str().unwrap_or("").to_string();
            (key, id)
        })
        .collect();
    posts.sort();
    posts
}

/// Two projects, two workflows: ID 21 is "In Progress" in one and "Done" in
/// the other.
async fn two_workflow_server() -> MockServer {
    two_workflow_server_expecting(None).await
}

/// The same server for runs that must not write: it verifies on drop that
/// no transition POST arrived.
async fn read_only_two_workflow_server() -> MockServer {
    two_workflow_server_expecting(Some(0)).await
}

async fn two_workflow_server_expecting(posts: Option<u64>) -> MockServer {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1"]).await;
    mount_issue_expecting(
        &server,
        "A-1",
        &[
            transition_json("21", "In Progress", "In Progress"),
            transition_json("31", "Done", "Done"),
        ],
        posts,
    )
    .await;
    mount_issue_expecting(
        &server,
        "B-1",
        &[
            transition_json("21", "Done", "Done"),
            transition_json("11", "In Progress", "In Progress"),
        ],
        posts,
    )
    .await;
    server
}

/// The defect: the ID found on the first issue was sent to every issue, so
/// the second one would have been moved to "Done".
#[tokio::test]
async fn a_transition_name_is_resolved_on_each_issue() {
    let server = two_workflow_server().await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);

    bulk_transition(&ctx, "project in (A, B)", "In Progress", true, true, 2)
        .await
        .unwrap();

    assert_eq!(
        transition_posts(&server).await,
        vec![
            ("A-1".to_string(), "21".to_string()),
            ("B-1".to_string(), "11".to_string())
        ]
    );
}

#[tokio::test]
async fn an_id_is_not_sent_to_an_issue_where_it_means_something_else() {
    let server = two_workflow_server().await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);

    let err = bulk_transition(&ctx, "project in (A, B)", "21", true, true, 2)
        .await
        .expect_err("the second issue must be an error");

    assert!(format!("{err:#}").contains("1 of 2"), "{err:#}");
    assert_eq!(
        transition_posts(&server).await,
        vec![("A-1".to_string(), "21".to_string())],
        "ID 21 must not be sent to B-1"
    );
}

#[tokio::test]
async fn an_issue_without_the_transition_fails_and_the_others_still_run() {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1", "C-1"]).await;
    let done = transition_json("31", "Done", "Done");
    mount_issue(&server, "A-1", std::slice::from_ref(&done)).await;
    mount_issue(&server, "B-1", &[transition_json("11", "Reopen", "To Do")]).await;
    mount_issue(&server, "C-1", &[done]).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);

    let err = bulk_transition(&ctx, "project = X", "Done", true, true, 2)
        .await
        .expect_err("an unavailable transition must fail the command");

    assert!(format!("{err:#}").contains("1 of 3"), "{err:#}");
    assert_eq!(
        transition_posts(&server).await,
        vec![
            ("A-1".to_string(), "31".to_string()),
            ("C-1".to_string(), "31".to_string())
        ],
        "B-1 gets nothing, the others are transitioned"
    );
}

/// A lookup that fails for one issue is that issue's error, not the end of
/// the run.
#[tokio::test]
async fn a_failed_lookup_on_one_issue_does_not_stop_the_others() {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1", "C-1"]).await;
    let done = transition_json("31", "Done", "Done");
    mount_issue(&server, "A-1", std::slice::from_ref(&done)).await;
    mount_issue(&server, "C-1", &[done]).await;
    // No mock for B-1: its lookup answers 404.
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);

    let err = bulk_transition(&ctx, "project = X", "Done", true, true, 2)
        .await
        .expect_err("the failed lookup must fail the command");

    assert!(format!("{err:#}").contains("1 of 3"), "{err:#}");
    assert_eq!(transition_posts(&server).await.len(), 2);
}

/// Three issues where the first issue's transitions cannot be read.
async fn server_with_unreadable_first_issue() -> MockServer {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1", "C-1"]).await;
    // A-1 has no transitions mock: its lookup answers 404.
    let done = transition_json("21", "Done", "Done");
    mount_issue(&server, "B-1", std::slice::from_ref(&done)).await;
    mount_issue(&server, "C-1", &[done]).await;
    server
}

/// A name does not depend on the first issue, so the others still run.
#[tokio::test]
async fn an_unreadable_first_issue_does_not_stop_a_name_on_the_others() {
    let server = server_with_unreadable_first_issue().await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);

    let err = bulk_transition(&ctx, "project = X", "Done", true, true, 2)
        .await
        .expect_err("the first issue is an error");

    assert!(format!("{err:#}").contains("1 of 3"), "{err:#}");
    assert_eq!(
        transition_posts(&server).await,
        vec![
            ("B-1".to_string(), "21".to_string()),
            ("C-1".to_string(), "21".to_string())
        ]
    );
}

/// The preview of the same run still lists the other issues.
#[tokio::test]
async fn an_unreadable_first_issue_is_one_row_of_the_preview() {
    let server = server_with_unreadable_first_issue().await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);
    let keys: Vec<String> = ["A-1", "B-1", "C-1"].map(String::from).to_vec();

    let plans = plan_transitions(&ctx, &keys, "Done", 2).await.unwrap();

    assert!(plans[0].outcome.is_err());
    assert_eq!(plans[1].outcome.as_ref().unwrap().id, "21");
    assert_eq!(plans[2].outcome.as_ref().unwrap().id, "21");
}

/// An ID cannot be tied to a name without the first issue: nothing runs.
#[tokio::test]
async fn an_unreadable_first_issue_closes_an_id_for_every_issue() {
    let server = server_with_unreadable_first_issue().await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);

    let err = bulk_transition(&ctx, "project = X", "21", true, true, 2)
        .await
        .expect_err("an ID with an unknown meaning must fail");

    assert!(format!("{err:#}").contains("3 of 3"), "{err:#}");
    assert!(
        transition_posts(&server).await.is_empty(),
        "no POST may be sent"
    );
}

/// The value is an ID on the first issue and the name of another
/// transition there: nothing runs.
#[tokio::test]
async fn an_ambiguous_value_is_applied_to_no_issue() {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1"]).await;
    let ambiguous = [
        transition_json("21", "Start", "In Progress"),
        transition_json("22", "21", "Done"),
    ];
    mount_issue(&server, "A-1", &ambiguous).await;
    mount_issue(&server, "B-1", &ambiguous).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);

    let err = bulk_transition(&ctx, "project = X", "21", true, true, 2)
        .await
        .expect_err("an ambiguous value must fail");

    assert!(format!("{err:#}").contains("2 of 2"), "{err:#}");
    assert!(transition_posts(&server).await.is_empty());
}

/// A-1 and C-1 offer one "Done", B-1 offers "Done" and "DONE". Built in
/// both response orders for B-1. B-1 must receive no POST at all.
async fn server_with_an_ambiguous_issue(reverse: bool) -> MockServer {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1", "C-1"]).await;
    let mut duplicates = vec![
        transition_json("11", "Done", "Resolved"),
        transition_json("21", "DONE", "Cancelled"),
    ];
    if reverse {
        duplicates.reverse();
    }
    mount_issue(&server, "A-1", &[transition_json("31", "Done", "Done")]).await;
    mount_issue_expecting(&server, "B-1", &duplicates, Some(0)).await;
    mount_issue(&server, "C-1", &[transition_json("41", "done", "Done")]).await;
    server
}

/// The ambiguous issue gets nothing, the others are transitioned, and the
/// command fails, whatever order Jira lists B-1's transitions in.
#[tokio::test]
async fn an_ambiguous_name_skips_that_issue_and_runs_the_others() {
    for reverse in [false, true] {
        let server = server_with_an_ambiguous_issue(reverse).await;
        let renderer = OutputRenderer::new(OutputFormat::Json);
        let ctx = jira_ctx(&server, &renderer);

        let err = bulk_transition(&ctx, "project = X", "Done", true, true, 2)
            .await
            .expect_err("an ambiguous issue must fail the command");

        assert!(format!("{err:#}").contains("1 of 3"), "{err:#}");
        assert_eq!(
            transition_posts(&server).await,
            vec![
                ("A-1".to_string(), "31".to_string()),
                ("C-1".to_string(), "41".to_string())
            ],
            "B-1 must get no POST (reverse = {reverse})"
        );
    }
}

/// The first issue is the ambiguous one. A name does not depend on the
/// first issue, so the others still run.
#[tokio::test]
async fn an_ambiguous_first_issue_does_not_stop_a_name_on_the_others() {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1"]).await;
    mount_issue_expecting(
        &server,
        "A-1",
        &[
            transition_json("21", "DONE", "Cancelled"),
            transition_json("11", "Done", "Resolved"),
        ],
        Some(0),
    )
    .await;
    mount_issue(&server, "B-1", &[transition_json("31", "Done", "Done")]).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);

    let err = bulk_transition(&ctx, "project = X", "Done", true, true, 2)
        .await
        .expect_err("the ambiguous first issue is an error");

    assert!(format!("{err:#}").contains("1 of 2"), "{err:#}");
    assert_eq!(
        transition_posts(&server).await,
        vec![("B-1".to_string(), "31".to_string())]
    );
}

/// The ID picks one of the duplicates on every issue.
#[tokio::test]
async fn an_explicit_id_runs_where_the_name_is_ambiguous() {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1"]).await;
    let duplicates = [
        transition_json("11", "Done", "Resolved"),
        transition_json("21", "DONE", "Cancelled"),
    ];
    mount_issue(&server, "A-1", &duplicates).await;
    let reversed: Vec<Value> = duplicates.iter().rev().cloned().collect();
    mount_issue(&server, "B-1", &reversed).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);

    bulk_transition(&ctx, "project = X", "11", true, true, 2)
        .await
        .unwrap();

    assert_eq!(
        transition_posts(&server).await,
        vec![
            ("A-1".to_string(), "11".to_string()),
            ("B-1".to_string(), "11".to_string())
        ]
    );
}

/// The preview marks the ambiguous issue and sends nothing.
#[tokio::test]
async fn the_preview_marks_an_ambiguous_issue() {
    let server = server_with_an_ambiguous_issue(false).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);
    let keys: Vec<String> = ["A-1", "B-1", "C-1"].map(String::from).to_vec();

    let plans = plan_transitions(&ctx, &keys, "Done", 2).await.unwrap();
    let rows = serde_json::to_value(transition_rows(&plans)).unwrap();

    assert_eq!(rows[0]["action"], "would transition");
    assert_eq!(rows[1]["key"], "B-1");
    assert_eq!(rows[1]["action"], "ambiguous");
    assert_eq!(rows[2]["action"], "would transition");
    assert!(transition_posts(&server).await.is_empty());
}

/// Without `--execute` the command only previews: what would run on each
/// issue is resolved and shown, and nothing is sent.
#[tokio::test]
async fn transition_lists_by_default_and_sends_no_transition() {
    let server = read_only_two_workflow_server().await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);

    bulk_transition(&ctx, "project in (A, B)", "Done", false, false, 2)
        .await
        .unwrap();

    assert!(
        transition_posts(&server).await.is_empty(),
        "listing must not transition anything"
    );
}

#[test]
fn the_preview_shows_transition_and_target_status_per_issue() {
    let plans = vec![
        IssuePlan {
            key: "A-1".into(),
            outcome: Ok(ResolvedTransition {
                id: "31".into(),
                name: "Complete".into(),
                to_status: Some("Done".into()),
            }),
        },
        IssuePlan {
            key: "B-1".into(),
            outcome: Err(Skipped::not_available("B-1: not available".into())),
        },
        IssuePlan {
            key: "C-1".into(),
            outcome: Err(Skipped {
                ambiguous: true,
                message: "C-1: ambiguous".into(),
            }),
        },
    ];
    let rows = serde_json::to_value(transition_rows(&plans)).unwrap();
    assert_eq!(
        rows,
        json!([
            {"key": "A-1", "transition": "Complete", "to_status": "Done",
             "action": "would transition"},
            {"key": "B-1", "transition": "", "to_status": "", "action": "not available"},
            {"key": "C-1", "transition": "", "to_status": "", "action": "ambiguous"}
        ])
    );
}

/// The second issue answers last, so the results arrive out of order and
/// only the explicit sort keeps the plans in the order of the search.
#[tokio::test]
async fn plans_keep_the_order_of_the_search_result() {
    let server = MockServer::start().await;
    mount_search(&server, &["A-1", "B-1", "C-1"]).await;
    let done = transition_json("31", "Done", "Done");
    mount_issue_expecting(&server, "A-1", std::slice::from_ref(&done), Some(0)).await;
    mount_issue_expecting(&server, "C-1", std::slice::from_ref(&done), Some(0)).await;
    Mock::given(method("GET"))
        .and(path_matcher("/rest/api/3/issue/B-1/transitions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "transitions": [done] }))
                .set_delay(std::time::Duration::from_millis(300)),
        )
        .mount(&server)
        .await;
    mount_transition_post(&server, "B-1", Some(0)).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);
    let keys: Vec<String> = ["A-1", "B-1", "C-1"].map(String::from).to_vec();

    let plans = plan_transitions(&ctx, &keys, "Done", 3).await.unwrap();

    let order: Vec<&str> = plans.iter().map(|plan| plan.key.as_str()).collect();
    assert_eq!(order, ["A-1", "B-1", "C-1"]);
    assert!(plans.iter().all(|plan| plan.outcome.is_ok()));
}

#[tokio::test]
async fn the_target_status_is_resolved_per_issue() {
    let server = read_only_two_workflow_server().await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = jira_ctx(&server, &renderer);
    let keys = vec!["A-1".to_string(), "B-1".to_string()];

    let plans = plan_transitions(&ctx, &keys, "Done", 2).await.unwrap();

    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].outcome.as_ref().unwrap().id, "31");
    assert_eq!(plans[1].outcome.as_ref().unwrap().id, "21");
    assert_eq!(
        plans[1].outcome.as_ref().unwrap().to_status.as_deref(),
        Some("Done")
    );
}

#[test]
fn ambiguous_names_refuse_in_either_order_but_explicit_ids_resolve() {
    for (a, b, wanted) in [
        ("Done", "Done", "Done"),
        ("Done", "DONE", "done"),
        ("Café", "CAFÉ", "café"),
    ] {
        let mut available: Vec<AvailableTransition> =
            [("11", a, "Resolved"), ("21", b, "Cancelled")]
                .into_iter()
                .map(|(id, name, status)| AvailableTransition {
                    id: id.into(),
                    name: name.into(),
                    to: Some(TransitionTarget {
                        name: status.into(),
                    }),
                })
                .collect();
        for _ in 0..2 {
            let request = TransitionRequest::from_first_issue(&available, wanted);
            assert!(matches!(
                resolve_transition(&available, &request),
                Err(Unresolved::Ambiguous(_))
            ));
            let plan = IssuePlan::for_issue("B-1".into(), &available, &request);
            assert!(plan.outcome.unwrap_err().message.contains("ambiguous"));
            let by_id = TransitionRequest::from_first_issue(&available, "21");
            assert_eq!(resolve_transition(&available, &by_id).unwrap().id, "21");
            available.reverse();
        }
    }
}
