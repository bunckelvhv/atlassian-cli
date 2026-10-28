//! Identifiers in mutating Confluence requests are percent-encoded, so a value
//! containing `?`, `#` or `/` cannot change which resource a request targets.
//!
//! Each test checks the path the mock server actually received.

use std::path::PathBuf;

use atlassian_cli_api::ApiClient;
use atlassian_cli_output::{OutputFormat, OutputRenderer};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::utils::ConfluenceContext;

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
async fn removing_a_label_encodes_the_page_id() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = ConfluenceContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    super::pages::remove_page_label(&ctx, "1?x", "draft")
        .await
        .unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/wiki/rest/api/content/1%3Fx/label".to_string(),
            Some("name=draft".to_string())
        )]
    );
}

#[tokio::test]
async fn deleting_a_page_encodes_the_page_id() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = ConfluenceContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };

    super::pages::delete_page(&ctx, "1?x", true).await.unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/wiki/api/v2/pages/1%3Fx".to_string(),
            None
        )]
    );
}

/// The multipart upload bypasses the central path guard, so the page id has to
/// be encoded at the call site.
#[tokio::test]
async fn an_upload_encodes_the_page_id() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = ConfluenceContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };
    let dir = tempfile::TempDir::new().unwrap();
    let file: PathBuf = dir.path().join("note.txt");
    std::fs::write(&file, "hello").unwrap();

    super::attachments::upload_attachment(&ctx, "1?x", &file, None)
        .await
        .unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "POST".to_string(),
            "/wiki/rest/api/content/1%3Fx/child/attachment".to_string(),
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
    let ctx = ConfluenceContext {
        client: ApiClient::new(server.uri()).unwrap(),
        renderer: &renderer,
    };
    let dir = tempfile::TempDir::new().unwrap();
    let file: PathBuf = dir.path().join("note.txt");
    std::fs::write(&file, "hello").unwrap();

    for page_id in ["1/label", "..", ""] {
        let err = super::pages::remove_page_label(&ctx, page_id, "draft")
            .await
            .expect_err("the page id must be refused");
        assert!(
            format!("{err:#}").contains("Invalid identifier"),
            "{page_id:?}: {err:#}"
        );

        let err = super::attachments::upload_attachment(&ctx, page_id, &file, None)
            .await
            .expect_err("the upload must be refused");
        assert!(
            format!("{err:#}").contains("Invalid identifier"),
            "{page_id:?}: {err:#}"
        );
    }

    assert!(received(&server).await.is_empty());
}
