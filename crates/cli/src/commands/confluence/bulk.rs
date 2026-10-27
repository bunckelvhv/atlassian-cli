use anyhow::{Context, Result};
use atlassian_cli_bulk::BulkExecutor;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

use super::utils::ConfluenceContext;
use crate::commands::common::{confirm_destructive, render_success, ExportFormat, MutationResult};

/// Page size of the CQL search behind the bulk commands, and so the most pages
/// one run can act on. Pagination is not followed.
const MAX_SEARCH_RESULTS: usize = 1000;

#[derive(Deserialize)]
struct PageRef {
    id: String,
    #[serde(default)]
    title: Option<String>,
}

/// Bulk delete pages.
///
/// Listing is the default: without `execute` the matching pages are printed and
/// nothing is sent. With `execute`, the match count has to be typed back unless
/// `assume_yes` is set, and without a terminal the command refuses instead of
/// deleting. The confirmation comes after the search and before the first
/// delete, so the count shown is the count that is deleted.
pub async fn bulk_delete_pages(
    ctx: &ConfluenceContext<'_>,
    cql: &str,
    execute: bool,
    assume_yes: bool,
    concurrency: usize,
) -> Result<()> {
    delete_pages_with(
        ctx,
        cql,
        execute,
        assume_yes,
        concurrency,
        confirm_destructive,
    )
    .await
}

/// The body of [`bulk_delete_pages`], with the confirmation as a parameter so a
/// test can run it without a terminal on stdin.
async fn delete_pages_with<F>(
    ctx: &ConfluenceContext<'_>,
    cql: &str,
    execute: bool,
    assume_yes: bool,
    concurrency: usize,
    confirm: F,
) -> Result<()>
where
    F: FnOnce(&str, &str) -> Result<()>,
{
    #[derive(Serialize)]
    struct PageRow {
        id: String,
        title: String,
        action: &'static str,
    }

    let pages = search_pages(ctx, cql).await?;

    if pages.is_empty() {
        return ctx
            .renderer
            .render_list_or_empty::<PageRow>(&[], "No pages found matching query");
    }

    if !execute {
        let rows: Vec<PageRow> = pages
            .into_iter()
            .map(|page| PageRow {
                id: page.id,
                title: page.title.unwrap_or_default(),
                action: "would delete",
            })
            .collect();
        // stderr, so a piped `-f json` listing stays machine-readable.
        eprintln!(
            "Listing only. {} page(s) match. Pass --execute to move them to the trash.",
            rows.len()
        );
        return ctx.renderer.render_list(&rows);
    }

    // Confirm before the first delete, not after some of them have happened.
    if !assume_yes {
        confirm(
            &pages.len().to_string(),
            &format!(
                "About to move {} page(s) to the trash.\n\
                 Only the first search page (at most {MAX_SEARCH_RESULTS} pages) is covered; \
                 further matches need another run.\n\
                 Re-run without --execute to list them first.",
                pages.len()
            ),
        )?;
    }

    let page_ids: Vec<String> = pages.into_iter().map(|page| page.id).collect();
    let executor = BulkExecutor::new(concurrency, false);
    let client = ctx.client.clone();

    executor
        .run(page_ids, move |id| {
            let client = client.clone();
            async move {
                let _: Value = client
                    .delete(&format!("/wiki/api/v2/pages/{}", id))
                    .await
                    .with_context(|| format!("Failed to delete page {}", id))?;
                tracing::info!(%id, "Page deleted successfully");
                Ok(())
            }
        })
        .await?;

    render_success(
        ctx.renderer,
        "✅ Bulk delete completed",
        &MutationResult::new("Bulk delete completed"),
    )
}

// Bulk add labels
pub async fn bulk_add_labels(
    ctx: &ConfluenceContext<'_>,
    cql: &str,
    labels: Vec<String>,
    dry_run: bool,
    concurrency: usize,
) -> Result<()> {
    let page_ids = search_page_ids(ctx, cql).await?;

    if page_ids.is_empty() {
        println!("No pages found matching query");
        return Ok(());
    }

    println!("Found {} pages to label", page_ids.len());

    if dry_run {
        println!("🔍 Dry run mode - no changes will be made:");
        for id in &page_ids {
            println!("  Would add labels {:?} to page {}", labels, id);
        }
        return Ok(());
    }

    let executor = BulkExecutor::new(concurrency, dry_run);
    let client = ctx.client.clone();

    executor
        .run(page_ids, move |id| {
            let client = client.clone();
            let labels = labels.clone();
            async move {
                let label_objects: Vec<_> = labels
                    .iter()
                    .map(|l| json!({"prefix": "global", "name": l}))
                    .collect();

                let _: Value = client
                    .post(
                        &format!("/wiki/rest/api/content/{}/label", id),
                        &label_objects,
                    )
                    .await
                    .with_context(|| format!("Failed to add labels to page {}", id))?;

                tracing::info!(%id, "Labels added successfully");
                Ok(())
            }
        })
        .await?;

    render_success(
        ctx.renderer,
        "✅ Bulk label operation completed",
        &MutationResult::new("Bulk label operation completed"),
    )
}

// Bulk export pages
pub async fn bulk_export_pages(
    ctx: &ConfluenceContext<'_>,
    cql: &str,
    output: &PathBuf,
    format: ExportFormat,
) -> Result<()> {
    #[derive(Deserialize)]
    struct SearchResponse {
        results: Vec<SearchResult>,
    }

    #[derive(Deserialize)]
    struct SearchResult {
        content: Value,
    }

    let query_string = format!(
        "cql={}",
        atlassian_cli_api::query::encode_query_component(cql)
    );

    let response: SearchResponse = ctx
        .client
        .get(&format!(
            "/wiki/rest/api/content/search?{}&expand=body.storage",
            query_string
        ))
        .await
        .context("Failed to search pages")?;

    if response.results.is_empty() {
        println!("No pages found matching query");
        return Ok(());
    }

    println!("Found {} pages to export", response.results.len());

    let pages: Vec<Value> = response.results.into_iter().map(|r| r.content).collect();

    match format {
        ExportFormat::Json => {
            let json_str = serde_json::to_string_pretty(&pages)?;
            fs::write(output, json_str)?;
        }
        ExportFormat::Csv => {
            let mut wtr = csv::Writer::from_path(output)?;

            // Write header
            wtr.write_record(["id", "title", "type", "space"])?;

            // Write rows
            for page in &pages {
                let id = page.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let title = page.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let page_type = page.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let space = page
                    .get("space")
                    .and_then(|s| s.get("key"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                wtr.write_record([id, title, page_type, space])?;
            }

            wtr.flush()?;
        }
    }

    render_success(
        ctx.renderer,
        &format!("✅ Exported {} pages to {}", pages.len(), output.display()),
        &MutationResult::new(format!(
            "Exported {} pages to {}",
            pages.len(),
            output.display()
        )),
    )
}

// Helper function to search for page IDs using CQL
async fn search_page_ids(ctx: &ConfluenceContext<'_>, cql: &str) -> Result<Vec<String>> {
    Ok(search_pages(ctx, cql)
        .await?
        .into_iter()
        .map(|page| page.id)
        .collect())
}

// Helper function to search for pages using CQL
// Note: Currently limited to MAX_SEARCH_RESULTS. TODO: Implement cursor-based pagination for larger result sets
async fn search_pages(ctx: &ConfluenceContext<'_>, cql: &str) -> Result<Vec<PageRef>> {
    #[derive(Deserialize)]
    struct SearchResponse {
        results: Vec<SearchResult>,
    }

    #[derive(Deserialize)]
    struct SearchResult {
        content: PageRef,
    }

    let query_string = format!(
        "cql={}&limit={}",
        atlassian_cli_api::query::encode_query_component(cql),
        MAX_SEARCH_RESULTS
    );

    let response: SearchResponse = ctx
        .client
        .get(&format!("/wiki/rest/api/content/search?{}", query_string))
        .await
        .context("Failed to search pages")?;

    if response.results.len() >= MAX_SEARCH_RESULTS {
        tracing::warn!(
            "Search returned maximum results ({}). Some pages may be excluded. Consider using more specific CQL.",
            MAX_SEARCH_RESULTS
        );
        eprintln!(
            "⚠️  Results limited to {}. Some pages may be excluded. Use more specific CQL query.",
            MAX_SEARCH_RESULTS
        );
    }

    Ok(response.results.into_iter().map(|r| r.content).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::common::confirm_destructive_on;
    use atlassian_cli_api::ApiClient;
    use atlassian_cli_output::{OutputFormat, OutputRenderer};
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Serve a search result containing `pages` as (id, title), and accept any
    /// page DELETE. The DELETE mock is what the tests count requests against.
    async fn server_with_pages(pages: &[(&str, &str)]) -> MockServer {
        let server = MockServer::start().await;
        let results: Vec<_> = pages
            .iter()
            .map(|(id, title)| json!({ "content": { "id": id, "title": title } }))
            .collect();

        Mock::given(method("GET"))
            .and(path("/wiki/rest/api/content/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "results": results })))
            .mount(&server)
            .await;

        Mock::given(method("DELETE"))
            .and(path_regex(r"^/wiki/api/v2/pages/[^/]+$"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        server
    }

    fn ctx_for(server: &MockServer) -> ConfluenceContext<'static> {
        // Leaked so the context can be `'static`, which is acceptable in a test
        // process.
        let renderer: &'static OutputRenderer =
            Box::leak(Box::new(OutputRenderer::new(OutputFormat::Json)));
        ConfluenceContext {
            client: ApiClient::new(server.uri()).unwrap(),
            renderer,
        }
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

    /// The test that matters most. If deletion becomes the default again, or the
    /// `execute` guard is dropped, this fails.
    #[tokio::test]
    async fn listing_is_the_default_and_deletes_nothing() {
        let server = server_with_pages(&[("1", "Alpha"), ("2", "Beta")]).await;
        let ctx = ctx_for(&server);

        delete_pages_with(&ctx, "type = page", false, false, 2, |_, _| {
            anyhow::bail!("listing must not ask for a confirmation")
        })
        .await
        .unwrap();

        assert!(
            delete_paths(&server).await.is_empty(),
            "listing must not issue any DELETE"
        );
    }

    /// `--yes` has no effect without `--execute`.
    #[tokio::test]
    async fn yes_alone_does_not_delete() {
        let server = server_with_pages(&[("1", "Alpha")]).await;
        let ctx = ctx_for(&server);

        delete_pages_with(&ctx, "type = page", false, true, 2, |_, _| Ok(()))
            .await
            .unwrap();

        assert!(delete_paths(&server).await.is_empty());
    }

    /// A scheduled job that never had a chance to answer must not delete.
    #[tokio::test]
    async fn execute_without_a_terminal_or_yes_refuses() {
        let server = server_with_pages(&[("1", "Alpha"), ("2", "Beta")]).await;
        let ctx = ctx_for(&server);

        let err = delete_pages_with(&ctx, "type = page", true, false, 2, |expected, warning| {
            confirm_destructive_on(expected, warning, false)
        })
        .await
        .expect_err("must refuse when nothing can answer");

        let message = format!("{err:#}");
        assert!(message.contains("Refusing to continue"), "{message}");
        assert!(message.contains("--yes"), "{message}");
        assert!(delete_paths(&server).await.is_empty());
    }

    #[tokio::test]
    async fn the_confirmation_expects_the_match_count_and_states_the_scope() {
        let server = server_with_pages(&[("1", "Alpha"), ("2", "Beta"), ("3", "Gamma")]).await;
        let ctx = ctx_for(&server);

        let mut asked: Option<(String, String)> = None;
        let err = delete_pages_with(&ctx, "type = page", true, false, 2, |expected, warning| {
            asked = Some((expected.to_string(), warning.to_string()));
            anyhow::bail!("declined")
        })
        .await
        .expect_err("a declined confirmation must abort");

        assert!(format!("{err:#}").contains("declined"));
        let (expected, warning) = asked.expect("the confirmation must have been asked");
        assert_eq!(expected, "3");
        assert!(warning.contains('3'), "{warning}");
        assert!(warning.contains("trash"), "{warning}");
        assert!(warning.contains("1000"), "{warning}");
        assert!(
            delete_paths(&server).await.is_empty(),
            "nothing may be deleted before the confirmation passes"
        );
    }

    #[tokio::test]
    async fn a_passed_confirmation_deletes_every_match() {
        let server = server_with_pages(&[("1", "Alpha"), ("2", "Beta")]).await;
        let ctx = ctx_for(&server);

        delete_pages_with(&ctx, "type = page", true, false, 2, |_, _| Ok(()))
            .await
            .unwrap();

        assert_eq!(
            delete_paths(&server).await,
            vec!["/wiki/api/v2/pages/1", "/wiki/api/v2/pages/2"]
        );
    }

    #[tokio::test]
    async fn execute_with_yes_deletes_every_match_without_asking() {
        let server = server_with_pages(&[("1", "Alpha"), ("2", "Beta"), ("3", "Gamma")]).await;
        let ctx = ctx_for(&server);

        delete_pages_with(&ctx, "type = page", true, true, 2, |_, _| {
            anyhow::bail!("--yes must skip the confirmation")
        })
        .await
        .unwrap();

        assert_eq!(
            delete_paths(&server).await,
            vec![
                "/wiki/api/v2/pages/1",
                "/wiki/api/v2/pages/2",
                "/wiki/api/v2/pages/3"
            ]
        );
    }

    /// With nothing matched there is nothing to confirm, and no terminal is
    /// needed to learn that.
    #[tokio::test]
    async fn no_matches_neither_asks_nor_deletes() {
        let server = server_with_pages(&[]).await;
        let ctx = ctx_for(&server);

        delete_pages_with(&ctx, "type = page", true, false, 2, |_, _| {
            anyhow::bail!("there is nothing to confirm")
        })
        .await
        .unwrap();

        assert!(delete_paths(&server).await.is_empty());
    }
}
