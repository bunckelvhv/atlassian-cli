//! Identifiers in mutating Bitbucket requests are percent-encoded, so a value
//! containing `?`, `#` or `/` cannot change which resource a request targets.
//!
//! Each test checks the path the mock server actually received, not just the
//! string a helper returned.

use atlassian_cli_api::ApiClient;
use atlassian_cli_output::{OutputFormat, OutputRenderer};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::utils::BitbucketContext;

async fn mock_all_methods(server: &MockServer) {
    for verb in ["POST", "PUT", "DELETE"] {
        Mock::given(method(verb))
            .respond_with(ResponseTemplate::new(204))
            .mount(server)
            .await;
    }
}

/// Method and path of every request the server received.
async fn received(server: &MockServer) -> Vec<(String, String)> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.method.to_string(), r.url.path().to_string()))
        .collect()
}

#[tokio::test]
async fn reopen_comment_encodes_a_question_mark_in_the_slug() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = BitbucketContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
        is_bearer: false,
    };

    super::pullrequests::reopen_pr_comment(&ctx, "w", "r?x", 7, 9)
        .await
        .unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/2.0/repositories/w/r%3Fx/pullrequests/7/comments/9/resolve".to_string()
        )]
    );
}

#[tokio::test]
async fn reopen_comment_encodes_a_question_mark_in_the_workspace() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = BitbucketContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
        is_bearer: false,
    };

    super::pullrequests::reopen_pr_comment(&ctx, "w?x", "r", 7, 9)
        .await
        .unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/2.0/repositories/w%3Fx/r/pullrequests/7/comments/9/resolve".to_string()
        )]
    );
}

#[tokio::test]
async fn deleting_a_repository_encodes_the_workspace() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = BitbucketContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
        is_bearer: false,
    };

    // The response shape is not what is under test; the request target is.
    let _ = super::repos::delete_repo(&ctx, "w?x", "r", true).await;

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/2.0/repositories/w%3Fx/r".to_string()
        )]
    );
}

#[tokio::test]
async fn creating_a_webhook_encodes_the_slug() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "uuid": "{u}", "url": "https://example.test/h", "active": true, "events": []
        })))
        .mount(&server)
        .await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = BitbucketContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
        is_bearer: false,
    };

    let _ = super::webhooks::create_webhook(
        &ctx,
        "w",
        "r#x",
        "https://example.test/h",
        None,
        vec!["repo:push".to_string()],
        true,
    )
    .await;

    assert_eq!(
        received(&server).await,
        vec![(
            "POST".to_string(),
            "/2.0/repositories/w/r%23x/hooks".to_string()
        )]
    );
}

/// A value that cannot be one path segment is refused before any request.
#[tokio::test]
async fn identifiers_spanning_segments_are_refused_without_a_request() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = BitbucketContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
        is_bearer: false,
    };

    for slug in ["r/x", "..", ".", ""] {
        let err = super::pullrequests::reopen_pr_comment(&ctx, "w", slug, 7, 9)
            .await
            .expect_err("the slug must be refused");
        assert!(
            format!("{err:#}").contains("Invalid identifier"),
            "{slug:?}: {err:#}"
        );
    }

    assert!(received(&server).await.is_empty());
}
