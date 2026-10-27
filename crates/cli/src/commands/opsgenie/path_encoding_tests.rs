//! Identifiers in mutating Opsgenie requests are percent-encoded, so a value
//! containing `?`, `#` or `/` cannot change which resource a request targets.
//!
//! Each test checks the path the mock server actually received.

use atlassian_cli_api::ApiClient;
use atlassian_cli_output::{OutputFormat, OutputRenderer};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::utils::OpsgenieContext;

async fn mock_all_methods(server: &MockServer) {
    for verb in ["GET", "POST", "PUT", "DELETE"] {
        Mock::given(method(verb))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(server)
            .await;
    }
}

fn context<'a>(server: &MockServer, renderer: &'a OutputRenderer) -> OpsgenieContext<'a> {
    OpsgenieContext {
        client: ApiClient::new(format!("{}/v2/", server.uri())).unwrap(),
        renderer,
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
async fn removing_a_team_member_encodes_the_team() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = context(&server, &renderer);

    super::teams::remove_member(&ctx, "T?x", "user-1")
        .await
        .unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/v2/teams/T%3Fx/members/user-1".to_string()
        )]
    );
}

#[tokio::test]
async fn removing_a_team_member_encodes_the_user() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = context(&server, &renderer);

    super::teams::remove_member(&ctx, "T", "u#1").await.unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "DELETE".to_string(),
            "/v2/teams/T/members/u%231".to_string()
        )]
    );
}

#[tokio::test]
async fn deleting_an_alert_encodes_the_identifier() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = context(&server, &renderer);

    super::alerts::delete_alert(&ctx, "a?x").await.unwrap();

    assert_eq!(
        received(&server).await,
        vec![("DELETE".to_string(), "/v2/alerts/a%3Fx".to_string())]
    );
}

/// `heartbeat get a/ping` used to reach the ping endpoint of heartbeat `a`.
#[tokio::test]
async fn getting_a_heartbeat_cannot_reach_the_ping_endpoint() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = context(&server, &renderer);

    let err = super::heartbeats::get_heartbeat(&ctx, "a/ping")
        .await
        .expect_err("a name spanning segments must be refused");
    assert!(format!("{err:#}").contains("Invalid identifier"), "{err:#}");

    assert!(received(&server).await.is_empty());
}

#[tokio::test]
async fn pinging_a_heartbeat_encodes_the_name() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = context(&server, &renderer);

    super::heartbeats::ping_heartbeat(&ctx, "nightly job?x")
        .await
        .unwrap();

    assert_eq!(
        received(&server).await,
        vec![(
            "GET".to_string(),
            "/v2/heartbeats/nightly%20job%3Fx/ping".to_string()
        )]
    );
}

/// A value that cannot be one path segment is refused before any request.
#[tokio::test]
async fn identifiers_spanning_segments_are_refused_without_a_request() {
    let server = MockServer::start().await;
    mock_all_methods(&server).await;
    let renderer = OutputRenderer::new(OutputFormat::Json);
    let ctx = context(&server, &renderer);

    for team in ["T/members", "..", ""] {
        let err = super::teams::remove_member(&ctx, team, "user-1")
            .await
            .expect_err("the team must be refused");
        assert!(
            format!("{err:#}").contains("Invalid identifier"),
            "{team:?}: {err:#}"
        );
    }

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
        let ctx = context(&server, &renderer);

        super::teams::delete_team(&ctx, value).await.unwrap();

        assert_eq!(
            received(&server).await,
            vec![("DELETE".to_string(), format!("/v2/teams/{encoded}"))],
            "{value:?}"
        );
    }
}
