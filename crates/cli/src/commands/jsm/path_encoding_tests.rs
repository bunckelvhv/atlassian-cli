//! Identifiers in mutating Jira Service Management requests are
//! percent-encoded, so a value containing `?`, `#` or `/` cannot change which
//! resource a request targets.
//!
//! Each test checks the path the mock server actually received.

use atlassian_cli_api::ApiClient;
use atlassian_cli_output::{OutputFormat, OutputRenderer};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::utils::JsmContext;

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
async fn deleting_feedback_encodes_the_request_key() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = JsmContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    // The response handling is not what is under test; the request target is.
    let _ = super::feedback::delete_feedback(&ctx, "SD-1?x").await;

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/rest/servicedeskapi/request/SD-1%3Fx/feedback".to_string()
        )]
    );
}

#[tokio::test]
async fn removing_participants_encodes_the_request_key() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = JsmContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    let _ = super::requests::remove_participant(&ctx, "SD-1#x", vec!["acc-1".to_string()]).await;

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/rest/servicedeskapi/request/SD-1%23x/participant".to_string()
        )]
    );
}

/// A value that cannot be one path segment is refused before any request.
#[tokio::test]
async fn identifiers_spanning_segments_are_refused_without_a_request() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = JsmContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    for key in ["SD-1/feedback", "..", ""] {
        let err = super::feedback::delete_feedback(&ctx, key)
            .await
            .expect_err("the request key must be refused");
        assert!(
            format!("{err:#}").contains("Invalid identifier"),
            "{key:?}: {err:#}"
        );
    }

    assert!(received(&server).await.is_empty());
}
