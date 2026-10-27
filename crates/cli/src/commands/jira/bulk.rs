use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use atlassian_cli_api::ApiClient;
use atlassian_cli_bulk::BulkExecutor;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::adf::markdown_to_adf;
use super::issues::check_field_collisions;
use super::utils::JiraContext;
use crate::commands::common::{render_success, ExportFormat, MutationResult};

/// Build the POST body for a single bulk-import row. Pure, testable.
pub(crate) fn build_bulk_payload(project: &str, issue: &ImportIssue) -> Result<Value> {
    let empty = HashMap::new();
    let custom = issue.custom_fields.as_ref().unwrap_or(&empty);

    check_field_collisions(
        custom,
        &[
            ("project", "file project"),
            ("issuetype", "row.issue_type"),
            ("summary", "row.summary"),
        ],
        &[
            (
                "description",
                "row.description",
                issue.description.is_some(),
            ),
            ("assignee", "row.assignee", issue.assignee.is_some()),
            ("priority", "row.priority", issue.priority.is_some()),
            ("labels", "row.labels", !issue.labels.is_empty()),
        ],
    )?;

    let mut fields = json!({
        "project": { "key": project },
        "issuetype": { "name": issue.issue_type },
        "summary": issue.summary,
    });

    if let Some(desc) = &issue.description {
        fields["description"] = markdown_to_adf(desc);
    }
    if let Some(assignee) = &issue.assignee {
        fields["assignee"] = json!({ "id": assignee });
    }
    if let Some(priority) = &issue.priority {
        fields["priority"] = json!({ "name": priority });
    }
    if !issue.labels.is_empty() {
        fields["labels"] = json!(issue.labels);
    }
    for (key, value) in custom {
        fields[key] = value.clone();
    }

    Ok(json!({ "fields": fields }))
}

// Bulk transition issues
pub async fn bulk_transition(
    ctx: &JiraContext<'_>,
    jql: &str,
    transition: &str,
    dry_run: bool,
    concurrency: usize,
) -> Result<()> {
    // Search for issues
    let issue_keys = search_issue_keys(ctx, jql).await?;

    if issue_keys.is_empty() {
        println!("No issues found matching query");
        return Ok(());
    }

    eprintln!("Found {} issues to transition", issue_keys.len());

    // Transition IDs are only unique within one workflow, so the transition is
    // resolved against each issue's own list, never taken from the first issue.
    let plans = plan_transitions(ctx, &issue_keys, transition, concurrency).await?;

    if dry_run {
        let rows = transition_rows(&plans);
        ctx.renderer.render_list(&rows)?;
        eprintln!("Dry run: no changes were made.");
        return Ok(());
    }

    apply_transitions(ctx, plans, concurrency).await?;

    render_success(
        ctx.renderer,
        "✅ Bulk transition completed",
        &MutationResult::new("Bulk transition completed"),
    )
}

// Bulk assign issues
pub async fn bulk_assign(
    ctx: &JiraContext<'_>,
    jql: &str,
    assignee: &str,
    dry_run: bool,
    concurrency: usize,
) -> Result<()> {
    let issue_keys = search_issue_keys(ctx, jql).await?;

    if issue_keys.is_empty() {
        println!("No issues found matching query");
        return Ok(());
    }

    println!("Found {} issues to assign", issue_keys.len());

    if dry_run {
        println!("🔍 Dry run mode - no changes will be made:");
        for key in &issue_keys {
            println!("  Would assign {} to {}", key, assignee);
        }
        return Ok(());
    }

    let executor = BulkExecutor::new(concurrency, dry_run);
    let client = ctx.client.clone();
    let assignee = assignee.to_string();

    executor
        .run(issue_keys, move |key| {
            let client = client.clone();
            let assignee = assignee.clone();
            async move {
                let payload = json!({ "accountId": assignee });
                let _: Value = client
                    .put(&format!("/rest/api/3/issue/{key}/assignee"), &payload)
                    .await
                    .with_context(|| format!("Failed to assign issue {key}"))?;
                tracing::info!(%key, %assignee, "Assigned successfully");
                Ok(())
            }
        })
        .await?;

    render_success(
        ctx.renderer,
        "✅ Bulk assign completed",
        &MutationResult::new("Bulk assign completed"),
    )
}

// Bulk label operations
pub async fn bulk_label(
    ctx: &JiraContext<'_>,
    jql: &str,
    action: LabelAction,
    labels: Vec<String>,
    dry_run: bool,
    concurrency: usize,
) -> Result<()> {
    let issue_keys = search_issue_keys(ctx, jql).await?;

    if issue_keys.is_empty() {
        println!("No issues found matching query");
        return Ok(());
    }

    println!("Found {} issues to label", issue_keys.len());

    if dry_run {
        println!("🔍 Dry run mode - no changes will be made:");
        for key in &issue_keys {
            println!("  Would {:?} labels {:?} on {}", action, labels, key);
        }
        return Ok(());
    }

    let executor = BulkExecutor::new(concurrency, dry_run);
    let client = ctx.client.clone();

    executor
        .run(issue_keys, move |key| {
            let client = client.clone();
            let labels = labels.clone();
            let action = action.clone();
            async move {
                // Get current labels
                let issue: IssueWithLabels = client
                    .get(&format!("/rest/api/3/issue/{key}?fields=labels"))
                    .await
                    .with_context(|| format!("Failed to get issue {key}"))?;

                let new_labels = match action {
                    LabelAction::Add => {
                        let mut current = issue.fields.labels;
                        for label in labels {
                            if !current.contains(&label) {
                                current.push(label);
                            }
                        }
                        current
                    }
                    LabelAction::Remove => issue
                        .fields
                        .labels
                        .into_iter()
                        .filter(|l| !labels.contains(l))
                        .collect(),
                    LabelAction::Set => labels,
                };

                let payload = json!({ "fields": { "labels": new_labels } });
                let _: Value = client
                    .put(&format!("/rest/api/3/issue/{key}"), &payload)
                    .await
                    .with_context(|| format!("Failed to update labels for {key}"))?;

                tracing::info!(%key, "Labels updated successfully");
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

// Bulk export issues
pub async fn bulk_export(
    ctx: &JiraContext<'_>,
    jql: &str,
    output: &PathBuf,
    format: ExportFormat,
    fields: Vec<String>,
) -> Result<()> {
    // Search for issues with specified fields
    let field_list: Vec<String> = if fields.is_empty() {
        vec!["*all".to_string()]
    } else {
        fields
    };

    #[derive(Deserialize)]
    struct SearchResponse {
        issues: Vec<Value>,
    }

    let payload = json!({
        "jql": jql,
        "maxResults": 1000,
        "fields": field_list,
    });

    let response: SearchResponse = ctx
        .client
        .post("/rest/api/3/search/jql", &payload)
        .await
        .context("Failed to search issues")?;

    if response.issues.is_empty() {
        println!("No issues found matching query");
        return Ok(());
    }

    println!("Found {} issues to export", response.issues.len());

    match format {
        ExportFormat::Json => {
            let json_str = serde_json::to_string_pretty(&response.issues)?;
            fs::write(output, json_str)?;
        }
        ExportFormat::Csv => {
            // Extract common fields for CSV
            let mut wtr = csv::Writer::from_path(output)?;

            // Write header
            wtr.write_record([
                "key", "summary", "status", "assignee", "reporter", "created",
            ])?;

            // Write rows
            for issue in &response.issues {
                let key = issue.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let summary = issue
                    .get("fields")
                    .and_then(|f| f.get("summary"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let status = issue
                    .get("fields")
                    .and_then(|f| f.get("status"))
                    .and_then(|s| s.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let assignee = issue
                    .get("fields")
                    .and_then(|f| f.get("assignee"))
                    .and_then(|a| a.get("displayName"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let reporter = issue
                    .get("fields")
                    .and_then(|f| f.get("reporter"))
                    .and_then(|r| r.get("displayName"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let created = issue
                    .get("fields")
                    .and_then(|f| f.get("created"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                wtr.write_record([key, summary, status, assignee, reporter, created])?;
            }

            wtr.flush()?;
        }
    }

    println!(
        "✅ Exported {} issues to {}",
        response.issues.len(),
        output.display()
    );
    Ok(())
}

// Bulk import issues
pub async fn bulk_import(
    ctx: &JiraContext<'_>,
    file: &PathBuf,
    project: &str,
    dry_run: bool,
    concurrency: usize,
) -> Result<()> {
    let content = fs::read_to_string(file)?;
    let issues: Vec<ImportIssue> = serde_json::from_str(&content)?;

    if issues.is_empty() {
        println!("No issues to import from file");
        return Ok(());
    }

    println!("Found {} issues to import", issues.len());

    if dry_run {
        println!("🔍 Dry run mode - no changes will be made:");
        for (idx, issue) in issues.iter().enumerate() {
            println!("  Would create: {} - {}", idx + 1, issue.summary);
        }
        return Ok(());
    }

    let executor = BulkExecutor::new(concurrency, dry_run);
    let client = ctx.client.clone();
    let project = project.to_string();

    executor
        .run(issues, move |issue| {
            let client = client.clone();
            let project = project.clone();
            async move {
                let payload = build_bulk_payload(&project, &issue)?;

                let response: CreateResponse = client
                    .post("/rest/api/3/issue", &payload)
                    .await
                    .context("Failed to create issue")?;

                tracing::info!(key = %response.key, "Issue created successfully");
                Ok(())
            }
        })
        .await?;

    render_success(
        ctx.renderer,
        "✅ Bulk import completed",
        &MutationResult::new("Bulk import completed"),
    )
}

// Helper functions

async fn search_issue_keys(ctx: &JiraContext<'_>, jql: &str) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct SearchResponse {
        issues: Vec<Issue>,
    }

    #[derive(Deserialize)]
    struct Issue {
        key: String,
    }

    let payload = json!({
        "jql": jql,
        "maxResults": 1000,
        "fields": ["key"],
    });

    let response: SearchResponse = ctx
        .client
        .post("/rest/api/3/search/jql", &payload)
        .await
        .context("Failed to search issues")?;

    Ok(response.issues.into_iter().map(|i| i.key).collect())
}

/// A transition Jira offers for one issue (`GET .../issue/{key}/transitions`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AvailableTransition {
    pub id: String,
    pub name: String,
    /// The status the issue lands in. Absent on some older instances.
    #[serde(default)]
    pub to: Option<TransitionTarget>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TransitionTarget {
    pub name: String,
}

/// What `--transition` means, fixed once so every issue is held to the same
/// meaning.
///
/// Transition IDs are only unique within one workflow: the same ID can be
/// "In Progress" in one project and "Done" in another. A name is the stable
/// handle across workflows, so an ID is only honoured on issues where it still
/// carries the name it had on the first issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TransitionRequest {
    /// Matched case-insensitively by name on each issue.
    ByName(String),
    /// An ID, pinned to the name it has on the first issue.
    ById { id: String, name: String },
}

impl TransitionRequest {
    /// Classify `requested` against the transitions of the first issue.
    ///
    /// A name match wins. Otherwise an ID match pins the ID to its name. If
    /// neither matches, the value is kept as a name: it then resolves nowhere
    /// that does not have a transition of exactly that name, so nothing is
    /// executed on a guess.
    pub(crate) fn from_first_issue(first: &[AvailableTransition], requested: &str) -> Self {
        if first.iter().any(|t| t.name.eq_ignore_ascii_case(requested)) {
            return Self::ByName(requested.to_string());
        }
        match first.iter().find(|t| t.id == requested) {
            Some(t) => Self::ById {
                id: t.id.clone(),
                name: t.name.clone(),
            },
            None => Self::ByName(requested.to_string()),
        }
    }
}

/// The transition that would run on one issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedTransition {
    pub id: String,
    pub name: String,
    /// Name of the status the issue ends up in, when Jira reports it.
    pub to_status: Option<String>,
}

/// Find the transition for one issue, or `None` when it is not available there.
///
/// There is no fallback: an issue that does not offer the requested transition
/// is left alone rather than given a similar one.
pub(crate) fn resolve_transition(
    available: &[AvailableTransition],
    request: &TransitionRequest,
) -> Option<ResolvedTransition> {
    let found = match request {
        TransitionRequest::ByName(name) => {
            available.iter().find(|t| t.name.eq_ignore_ascii_case(name))
        }
        TransitionRequest::ById { id, name } => available
            .iter()
            .find(|t| t.id == *id && t.name.eq_ignore_ascii_case(name)),
    }?;

    Some(ResolvedTransition {
        id: found.id.clone(),
        name: found.name.clone(),
        to_status: found.to.as_ref().map(|to| to.name.clone()),
    })
}

fn unavailable_message(key: &str, request: &TransitionRequest) -> String {
    match request {
        TransitionRequest::ByName(name) => {
            format!("{key}: transition {name:?} is not available on this issue")
        }
        TransitionRequest::ById { id, name } => format!(
            "{key}: transition ID {id} ({name:?} on the first issue) is not available on this \
             issue, the ID is missing or belongs to a different transition here"
        ),
    }
}

/// The outcome of resolving the requested transition for one issue.
#[derive(Debug)]
pub(crate) struct IssuePlan {
    pub key: String,
    /// The transition to run, or why there is none.
    pub outcome: Result<ResolvedTransition, String>,
}

impl IssuePlan {
    fn for_issue(
        key: String,
        available: &[AvailableTransition],
        request: &TransitionRequest,
    ) -> Self {
        let outcome = resolve_transition(available, request)
            .ok_or_else(|| unavailable_message(&key, request));
        Self { key, outcome }
    }
}

async fn fetch_transitions(client: &ApiClient, key: &str) -> Result<Vec<AvailableTransition>> {
    #[derive(Deserialize)]
    struct TransitionsResponse {
        transitions: Vec<AvailableTransition>,
    }

    let available: TransitionsResponse = client
        .get(&format!("/rest/api/3/issue/{key}/transitions"))
        .await
        .with_context(|| format!("Failed to get transitions for {key}"))?;
    Ok(available.transitions)
}

/// Resolve the requested transition for every issue, in the order of `issue_keys`.
///
/// Only reads. The first issue decides what an ID means (see
/// [`TransitionRequest`]), so its transitions are fetched before the rest, which
/// are fetched concurrently. A failure to read the first issue aborts, because
/// nothing can be resolved without it. A failure on any other issue becomes
/// that issue's outcome and does not stop the others.
async fn plan_transitions(
    ctx: &JiraContext<'_>,
    issue_keys: &[String],
    requested: &str,
    concurrency: usize,
) -> Result<Vec<IssuePlan>> {
    let Some((first_key, rest)) = issue_keys.split_first() else {
        return Ok(Vec::new());
    };

    let first_available = fetch_transitions(&ctx.client, first_key).await?;
    let request = TransitionRequest::from_first_issue(&first_available, requested);

    let mut plans = Vec::with_capacity(issue_keys.len());
    plans.push(IssuePlan::for_issue(
        first_key.clone(),
        &first_available,
        &request,
    ));

    let client = ctx.client.clone();
    let job_request = request.clone();
    let items: Vec<(usize, String)> = rest.iter().cloned().enumerate().collect();

    let result = BulkExecutor::new(concurrency, false)
        .with_progress(false)
        .execute_with_results(items, move |(idx, key): (usize, String)| {
            let client = client.clone();
            let request = job_request.clone();
            async move {
                let plan = match fetch_transitions(&client, &key).await {
                    Ok(available) => IssuePlan::for_issue(key, &available, &request),
                    Err(err) => IssuePlan {
                        key,
                        outcome: Err(format!("{err:#}")),
                    },
                };
                Ok((idx, plan))
            }
        })
        .await?;

    if let Some((_, err)) = result.failed.into_iter().next() {
        return Err(err);
    }

    let mut resolved = result.successful;
    resolved.sort_by_key(|(idx, _)| *idx);
    plans.extend(resolved.into_iter().map(|(_, plan)| plan));
    Ok(plans)
}

#[derive(Serialize)]
struct TransitionRow<'a> {
    key: &'a str,
    transition: &'a str,
    to_status: &'a str,
    action: &'a str,
}

/// One row per issue showing what would run, or that nothing would.
fn transition_rows(plans: &[IssuePlan]) -> Vec<TransitionRow<'_>> {
    plans
        .iter()
        .map(|plan| match &plan.outcome {
            Ok(resolved) => TransitionRow {
                key: plan.key.as_str(),
                transition: resolved.name.as_str(),
                to_status: resolved.to_status.as_deref().unwrap_or(""),
                action: "would transition",
            },
            Err(_) => TransitionRow {
                key: plan.key.as_str(),
                transition: "",
                to_status: "",
                action: "not available",
            },
        })
        .collect()
}

/// Run the resolved transitions. Issues without one are reported as errors.
///
/// Every failure is printed with the issue key, issues that were transitioned
/// stay transitioned, and the call fails when any issue was not.
async fn apply_transitions(
    ctx: &JiraContext<'_>,
    plans: Vec<IssuePlan>,
    concurrency: usize,
) -> Result<()> {
    let mut problems: Vec<String> = Vec::new();
    let mut jobs: Vec<(String, ResolvedTransition)> = Vec::new();
    for plan in plans {
        match plan.outcome {
            Ok(resolved) => jobs.push((plan.key, resolved)),
            Err(message) => problems.push(message),
        }
    }
    let total = jobs.len() + problems.len();

    let client = ctx.client.clone();
    let result = BulkExecutor::new(concurrency, false)
        .execute_with_results(
            jobs,
            move |(key, resolved): (String, ResolvedTransition)| {
                let client = client.clone();
                async move {
                    let payload = json!({ "transition": { "id": resolved.id } });
                    let _: Value = client
                        .post(&format!("/rest/api/3/issue/{key}/transitions"), &payload)
                        .await
                        .with_context(|| {
                            format!("Failed to transition issue {key} via {:?}", resolved.name)
                        })?;
                    tracing::info!(%key, transition = %resolved.name, "Transitioned successfully");
                    Ok(key)
                }
            },
        )
        .await?;

    let transitioned = result.success_count();
    problems.extend(result.failed.iter().map(|(_, err)| format!("{err:#}")));

    if !problems.is_empty() {
        eprintln!("Not transitioned:");
        for problem in &problems {
            eprintln!("  {problem}");
        }
        bail!(
            "{} of {total} issue(s) were not transitioned ({transitioned} transitioned)",
            problems.len()
        );
    }

    Ok(())
}

// Data structures

#[derive(Debug, Clone)]
pub enum LabelAction {
    Add,
    Remove,
    Set,
}

#[derive(Deserialize)]
struct IssueWithLabels {
    fields: LabelsField,
}

#[derive(Deserialize)]
struct LabelsField {
    labels: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ImportIssue {
    pub summary: String,
    pub issue_type: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub custom_fields: Option<HashMap<String, Value>>,
}

#[derive(Deserialize)]
struct CreateResponse {
    key: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use atlassian_cli_output::{OutputFormat, OutputRenderer};
    use serde_json::json;
    use wiremock::matchers::{method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn test_import_issue_deserialize_with_custom_fields() {
        let json_str = r#"{
            "summary": "Test ticket",
            "issue_type": "Task",
            "custom_fields": {
                "customfield_10001": {"value": "Alpha"},
                "customfield_10002": [{"value": "Beta"}]
            }
        }"#;
        let issue: ImportIssue = serde_json::from_str(json_str).unwrap();
        assert_eq!(issue.summary, "Test ticket");
        let cf = issue.custom_fields.unwrap();
        assert_eq!(cf["customfield_10001"], json!({"value": "Alpha"}));
        assert_eq!(cf["customfield_10002"], json!([{"value": "Beta"}]));
    }

    #[test]
    fn test_import_issue_deserialize_without_custom_fields() {
        let json_str = r#"{
            "summary": "Basic ticket",
            "issue_type": "Bug"
        }"#;
        let issue: ImportIssue = serde_json::from_str(json_str).unwrap();
        assert_eq!(issue.summary, "Basic ticket");
        assert!(issue.custom_fields.is_none());
    }

    #[test]
    fn test_import_issue_deserialize_empty_custom_fields() {
        let json_str = r#"{
            "summary": "Empty customs",
            "issue_type": "Story",
            "custom_fields": {}
        }"#;
        let issue: ImportIssue = serde_json::from_str(json_str).unwrap();
        assert!(issue.custom_fields.unwrap().is_empty());
    }

    #[test]
    fn test_import_issue_full_roundtrip() {
        let json_str = r#"{
            "summary": "Full ticket",
            "issue_type": "Task",
            "description": "Some description",
            "assignee": "user123",
            "priority": "High",
            "labels": ["backend", "urgent"],
            "custom_fields": {
                "customfield_10001": "plain string",
                "customfield_10002": 42
            }
        }"#;
        let issue: ImportIssue = serde_json::from_str(json_str).unwrap();
        assert_eq!(issue.summary, "Full ticket");
        assert_eq!(issue.description.as_deref(), Some("Some description"));
        assert_eq!(issue.assignee.as_deref(), Some("user123"));
        assert_eq!(issue.priority.as_deref(), Some("High"));
        assert_eq!(issue.labels, vec!["backend", "urgent"]);
        let cf = issue.custom_fields.unwrap();
        assert_eq!(cf["customfield_10001"], json!("plain string"));
        assert_eq!(cf["customfield_10002"], json!(42));
    }

    fn minimal_issue() -> ImportIssue {
        ImportIssue {
            summary: "s".to_string(),
            issue_type: "Task".to_string(),
            description: None,
            assignee: None,
            priority: None,
            labels: vec![],
            custom_fields: None,
        }
    }

    #[test]
    fn build_bulk_payload_merges_typed_and_custom() {
        let mut issue = minimal_issue();
        issue.description = Some("d".into());
        issue.labels = vec!["backend".into()];
        issue.custom_fields = Some(
            [("customfield_10010".to_string(), json!({"value": "x"}))]
                .into_iter()
                .collect(),
        );
        let payload = build_bulk_payload("PROJ", &issue).unwrap();
        let fields = &payload["fields"];
        assert_eq!(fields["project"]["key"], "PROJ");
        assert_eq!(fields["issuetype"]["name"], "Task");
        assert_eq!(fields["summary"], "s");
        assert_eq!(fields["labels"], json!(["backend"]));
        assert_eq!(fields["customfield_10010"], json!({"value": "x"}));
        assert_eq!(fields["description"]["type"], "doc");
    }

    #[test]
    fn build_bulk_payload_rejects_mandatory_collision() {
        let mut issue = minimal_issue();
        issue.custom_fields = Some(
            [("summary".to_string(), json!("raw"))]
                .into_iter()
                .collect(),
        );
        let err = build_bulk_payload("PROJ", &issue).unwrap_err().to_string();
        assert!(err.contains("reserved"), "got: {err}");
        assert!(err.contains("row.summary"), "got: {err}");
    }

    #[test]
    fn build_bulk_payload_rejects_labels_collision_when_both_set() {
        let mut issue = minimal_issue();
        issue.labels = vec!["a".into()];
        issue.custom_fields = Some([("labels".to_string(), json!(["b"]))].into_iter().collect());
        let err = build_bulk_payload("PROJ", &issue).unwrap_err().to_string();
        assert!(err.contains("collides"), "got: {err}");
        assert!(err.contains("row.labels"), "got: {err}");
    }

    #[test]
    fn build_bulk_payload_allows_raw_only_labels() {
        let mut issue = minimal_issue();
        issue.custom_fields = Some(
            [("labels".to_string(), json!(["b", "c"]))]
                .into_iter()
                .collect(),
        );
        let payload = build_bulk_payload("PROJ", &issue).unwrap();
        assert_eq!(payload["fields"]["labels"], json!(["b", "c"]));
    }

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
        assert_eq!(resolve_transition(&list, &request), None);
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
        assert_eq!(resolve_transition(&other_workflow, &request), None);
        assert_eq!(resolve_transition(&missing, &request), None);
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

    /// A value the first issue does not know stays a name, so it cannot be
    /// executed as an ID on some other issue that happens to offer it.
    #[test]
    fn an_unknown_value_stays_a_name() {
        let first = available(&[("11", "Start work", "Analysis")]);
        let request = TransitionRequest::from_first_issue(&first, "21");
        assert_eq!(request, TransitionRequest::ByName("21".into()));

        let elsewhere = available(&[("21", "Done", "Done")]);
        assert_eq!(resolve_transition(&elsewhere, &request), None);
    }

    fn transition_json(id: &str, name: &str, to: &str) -> Value {
        json!({ "id": id, "name": name, "to": { "name": to } })
    }

    async fn mount_search(server: &MockServer, keys: &[&str]) {
        let issues: Vec<_> = keys.iter().map(|k| json!({ "key": k })).collect();
        Mock::given(method("POST"))
            .and(path_matcher("/rest/api/3/search/jql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "issues": issues })))
            .mount(server)
            .await;
    }

    /// Serve the transitions of `key` and accept a transition POST for it.
    async fn mount_issue(server: &MockServer, key: &str, transitions: &[Value]) {
        Mock::given(method("GET"))
            .and(path_matcher(format!("/rest/api/3/issue/{key}/transitions")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "transitions": transitions })),
            )
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path_matcher(format!("/rest/api/3/issue/{key}/transitions")))
            .respond_with(ResponseTemplate::new(204))
            .mount(server)
            .await;
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
        let server = MockServer::start().await;
        mount_search(&server, &["A-1", "B-1"]).await;
        mount_issue(
            &server,
            "A-1",
            &[
                transition_json("21", "In Progress", "In Progress"),
                transition_json("31", "Done", "Done"),
            ],
        )
        .await;
        mount_issue(
            &server,
            "B-1",
            &[
                transition_json("21", "Done", "Done"),
                transition_json("11", "In Progress", "In Progress"),
            ],
        )
        .await;
        server
    }

    fn jira_ctx<'a>(server: &MockServer, renderer: &'a OutputRenderer) -> JiraContext<'a> {
        JiraContext {
            client: ApiClient::new(server.uri()).unwrap(),
            renderer,
        }
    }

    /// The defect: the ID found on the first issue was sent to every issue, so
    /// the second one would have been moved to "Done".
    #[tokio::test]
    async fn a_transition_name_is_resolved_on_each_issue() {
        let server = two_workflow_server().await;
        let renderer = OutputRenderer::new(OutputFormat::Json);
        let ctx = jira_ctx(&server, &renderer);

        bulk_transition(&ctx, "project in (A, B)", "In Progress", false, 2)
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

        let err = bulk_transition(&ctx, "project in (A, B)", "21", false, 2)
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
        mount_issue(&server, "A-1", &[done.clone()]).await;
        mount_issue(&server, "B-1", &[transition_json("11", "Reopen", "To Do")]).await;
        mount_issue(&server, "C-1", &[done]).await;
        let renderer = OutputRenderer::new(OutputFormat::Json);
        let ctx = jira_ctx(&server, &renderer);

        let err = bulk_transition(&ctx, "project = X", "Done", false, 2)
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
        mount_issue(&server, "A-1", &[done.clone()]).await;
        mount_issue(&server, "C-1", &[done]).await;
        // No mock for B-1: its lookup answers 404.
        let renderer = OutputRenderer::new(OutputFormat::Json);
        let ctx = jira_ctx(&server, &renderer);

        let err = bulk_transition(&ctx, "project = X", "Done", false, 2)
            .await
            .expect_err("the failed lookup must fail the command");

        assert!(format!("{err:#}").contains("1 of 3"), "{err:#}");
        assert_eq!(transition_posts(&server).await.len(), 2);
    }

    #[tokio::test]
    async fn dry_run_resolves_per_issue_and_sends_no_transition() {
        let server = two_workflow_server().await;
        let renderer = OutputRenderer::new(OutputFormat::Json);
        let ctx = jira_ctx(&server, &renderer);

        bulk_transition(&ctx, "project in (A, B)", "Done", true, 2)
            .await
            .unwrap();

        assert!(
            transition_posts(&server).await.is_empty(),
            "a dry run must not transition anything"
        );
    }

    #[tokio::test]
    async fn plans_keep_the_order_of_the_search_result() {
        let server = two_workflow_server().await;
        let renderer = OutputRenderer::new(OutputFormat::Json);
        let ctx = jira_ctx(&server, &renderer);
        let keys = vec!["A-1".to_string(), "B-1".to_string()];

        let plans = plan_transitions(&ctx, &keys, "Done", 2).await.unwrap();

        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].key, "A-1");
        assert_eq!(plans[0].outcome.as_ref().unwrap().id, "31");
        assert_eq!(plans[1].key, "B-1");
        assert_eq!(plans[1].outcome.as_ref().unwrap().id, "21");
        assert_eq!(
            plans[1].outcome.as_ref().unwrap().to_status.as_deref(),
            Some("Done")
        );
    }
}
