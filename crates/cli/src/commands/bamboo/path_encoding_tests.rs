//! Identifiers in mutating Bamboo requests are percent-encoded, so a value
//! containing `?`, `#` or `/` cannot change which resource a request targets.
//!
//! Each test checks the path the mock server actually received.

use atlassian_cli_api::ApiClient;
use atlassian_cli_output::{OutputFormat, OutputRenderer};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::utils::BambooContext;

async fn mock_all_methods(server: &MockServer) {
    for verb in ["POST", "PUT", "DELETE"] {
        Mock::given(method(verb))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
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
async fn removing_a_label_encodes_the_build_key_and_the_label() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = BambooContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    super::builds::remove_label(&ctx, "PLAN-1?x", "needs review")
        .await
        .unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/rest/api/latest/result/PLAN-1%3Fx/label/needs%20review".to_string()
        )]
    );
}

#[tokio::test]
async fn disabling_a_plan_encodes_the_key() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = BambooContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    super::plans::disable_plan(&ctx, "PROJ-PLAN#x")
        .await
        .unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/rest/api/latest/plan/PROJ-PLAN%23x/disable".to_string()
        )]
    );
}

/// A value that cannot be one path segment is refused before any request.
#[tokio::test]
async fn identifiers_spanning_segments_are_refused_without_a_request() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = BambooContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    for key in ["PLAN-1/label", "..", ""] {
        let err = super::builds::remove_label(&ctx, key, "lbl")
            .await
            .expect_err("the build key must be refused");
        assert!(
            format!("{err:#}").contains("Invalid identifier"),
            "{key:?}: {err:#}"
        );
    }

    assert!(received(&server).await.is_empty());
}
