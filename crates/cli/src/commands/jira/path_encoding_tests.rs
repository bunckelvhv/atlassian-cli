//! Identifiers in mutating Jira requests are percent-encoded, so a value
//! containing `?`, `#` or `/` cannot change which resource a request targets.
//!
//! Each test checks the path the mock server actually received.

use atlassian_cli_api::ApiClient;
use atlassian_cli_output::{OutputFormat, OutputRenderer};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::utils::JiraContext;

async fn mock_all_methods(server: &MockServer) {
    for verb in ["POST", "PUT", "DELETE"] {
        Mock::given(method(verb))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(server)
            .await;
    }
}

/// Method, path and query of every request the server received.
async fn received(server: &MockServer) -> Vec<(String, String, Option<String>)> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            (
                r.method.to_string(),
                r.url.path().to_string(),
                r.url.query().map(str::to_string),
            )
        })
        .collect()
}

#[tokio::test]
async fn removing_a_role_actor_encodes_the_project() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = JiraContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    super::fields_workflows::remove_role_actor(&ctx, "P?x", "10002", "acc-1")
        .await
        .unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/rest/api/3/project/P%3Fx/role/10002".to_string(),
            Some("user=acc-1".to_string())
        )]
    );
}

/// The user is a query value: it is encoded, so it cannot add parameters.
#[tokio::test]
async fn removing_a_role_actor_encodes_the_user_query_value() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = JiraContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    super::fields_workflows::remove_role_actor(&ctx, "P", "10002", "a&b=c")
        .await
        .unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/rest/api/3/project/P/role/10002".to_string(),
            Some("user=a%26b%3Dc".to_string())
        )]
    );
}

#[tokio::test]
async fn deleting_a_comment_encodes_the_issue_key() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = JiraContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    super::issues::delete_comment(&ctx, "P-1?x", "10100")
        .await
        .unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/rest/api/3/issue/P-1%3Fx/comment/10100".to_string(),
            None
        )]
    );
}

#[tokio::test]
async fn removing_a_watcher_encodes_the_key_and_the_account_id() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = JiraContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    super::issues::remove_watcher(&ctx, "P-1?x", "5b:1&x=2")
        .await
        .unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/rest/api/3/issue/P-1%3Fx/watchers".to_string(),
            Some("accountId=5b%3A1%26x%3D2".to_string())
        )]
    );
}

#[tokio::test]
async fn deleting_an_issue_with_a_hash_targets_that_literal_key() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = JiraContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    super::issues::delete_issue(&ctx, "P-1#x", true)
        .await
        .unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/rest/api/3/issue/P-1%23x".to_string(),
            None
        )]
    );
}

/// A value that cannot be one path segment is refused before any request.
#[tokio::test]
async fn identifiers_spanning_segments_are_refused_without_a_request() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = JiraContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    for key in ["P-1/comment", "..", ""] {
        let err = super::issues::delete_comment(&ctx, key, "10100")
            .await
            .expect_err("the key must be refused");
        assert!(
            format!("{err:#}").contains("Invalid identifier"),
            "{key:?}: {err:#}"
        );
    }
    let err = super::fields_workflows::remove_role_actor(&ctx, "P", "../x", "acc")
        .await
        .expect_err("the role id must be refused");
    assert!(format!("{err:#}").contains("Invalid identifier"), "{err:#}");

    assert!(received(&server).await.is_empty());
}

/// Values that are legal in real identifiers reach the server as documented
/// escapes, and an existing `%2F` is sent literally, not decoded.
#[tokio::test]
async fn compatibility_values_are_encoded_as_documented() {
    let cases = [
        ("user@example.test", "user%40example.test"),
        ("5b:1", "5b%3A1"),
        ("{abc-123}", "%7Babc-123%7D"),
        ("função", "fun%C3%A7%C3%A3o"),
        ("%2F", "%252F"),
    ];

    for (value, encoded) in cases {
        let server = MockServer::start().await;
        mock_all_methods(&server).await;
        let renderer = OutputRenderer::new(OutputFormat::Json);
        let ctx = JiraContext {
            client: ApiClient::new(server.uri()).unwrap(),
            renderer: &renderer,
        };

        super::issues::delete_comment(&ctx, value, "10100")
            .await
            .unwrap();

        assert_eq!(
            received(&server).await,
            vec![(
                "DELETE".to_string(),
                format!("/rest/api/3/issue/{encoded}/comment/10100"),
                None
            )],
            "{value:?}"
        );
    }
}

const COMMENT_IDENTIFIERS: &[(&str, &str)] = &[
    ("1?x=1", "1%3Fx%3D1"),
    ("DEV-1#note", "DEV-1%23note"),
    ("%2F", "%252F"),
    ("%2e%2e", "%252e%252e"),
];

#[tokio::test]
async fn comment_add_encodes_the_issue_key() {
    for (key, encoded) in COMMENT_IDENTIFIERS {
        let server = MockServer::start().await;
        mock_all_methods(&server).await;
        let renderer = OutputRenderer::new(OutputFormat::Json);
        let ctx = JiraContext {
            client: ApiClient::new(server.uri()).unwrap(),
            renderer: &renderer,
        };

        super::issues::add_comment(&ctx, key, "note", None)
            .await
            .unwrap();

        assert_eq!(
            received(&server).await,
            vec![(
                "POST".into(),
                format!("/rest/api/3/issue/{encoded}/comment"),
                None,
            )],
        );
    }
}

#[tokio::test]
async fn comment_update_encodes_both_identifiers_and_keeps_visibility() {
    use super::issues::VisibilityChange;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, path};

    for (value, encoded) in COMMENT_IDENTIFIERS {
        for kind in ["role", "group"] {
            let server = MockServer::start().await;
            let comment_path = format!("/rest/api/3/issue/{encoded}/comment/{encoded}");
            let visibility = json!({
                "type": kind, "value": "Developers", "identifier": "restricted-1"
            });
            Mock::given(method("GET"))
                .and(path(&comment_path))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "visibility": visibility
                })))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("PUT"))
                .and(path(&comment_path))
                .and(body_partial_json(json!({ "visibility": visibility })))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .expect(1)
                .mount(&server)
                .await;
            let renderer = OutputRenderer::new(OutputFormat::Json);
            let ctx = JiraContext {
                client: ApiClient::new(server.uri()).unwrap(),
                renderer: &renderer,
            };

            super::issues::update_comment(&ctx, value, value, "note", &VisibilityChange::Keep)
                .await
                .unwrap();

            assert_eq!(
                received(&server).await,
                vec![
                    ("GET".into(), comment_path.clone(), None),
                    ("PUT".into(), comment_path, None),
                ],
            );
        }
    }
}

#[tokio::test]
async fn comment_delete_encodes_both_identifiers() {
    for (value, encoded) in COMMENT_IDENTIFIERS {
        let server = MockServer::start().await;
        mock_all_methods(&server).await;
        let renderer = OutputRenderer::new(OutputFormat::Json);
        let ctx = JiraContext {
            client: ApiClient::new(server.uri()).unwrap(),
            renderer: &renderer,
        };

        super::issues::delete_comment(&ctx, value, value)
            .await
            .unwrap();

        assert_eq!(
            received(&server).await,
            vec![(
                "DELETE".into(),
                format!("/rest/api/3/issue/{encoded}/comment/{encoded}"),
                None,
            )],
        );
    }
}

#[tokio::test]
async fn invalid_comment_identifiers_send_no_requests() {
    use super::issues::{
        add_comment, delete_comment, update_comment, CommentVisibility, VisibilityChange,
    };
    use atlassian_cli_api::error::ApiError;

    let server = MockServer::start().await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = JiraContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    for invalid in ["", ".", "..", "DEV-1/..", "1/comment"] {
        let err = add_comment(&ctx, invalid, "note", None).await.unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ApiError>(),
            Some(ApiError::InvalidIdentifier { .. })
        ));
        for (key, comment_id) in [(invalid, "10500"), ("DEV-1", invalid)] {
            let err = delete_comment(&ctx, key, comment_id).await.unwrap_err();
            assert!(matches!(
                err.downcast_ref::<ApiError>(),
                Some(ApiError::InvalidIdentifier { .. })
            ));
            for change in [
                VisibilityChange::Keep,
                VisibilityChange::Public,
                VisibilityChange::Set(CommentVisibility::Role("Developers".into())),
            ] {
                let err = update_comment(&ctx, key, comment_id, "note", &change)
                    .await
                    .unwrap_err();
                assert!(matches!(
                    err.downcast_ref::<ApiError>(),
                    Some(ApiError::InvalidIdentifier { .. })
                ));
            }
        }
    }
    assert!(received(&server).await.is_empty());
}
