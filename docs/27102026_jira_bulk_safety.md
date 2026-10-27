# `jira bulk`: confirmed changes and per-issue transitions

Status: in review (PR #TBD).

## Problem

Two defects in `jira bulk transition`, `jira bulk assign` and `jira bulk label`
(`crates/cli/src/commands/jira/bulk.rs`).

**No safety rails.** Each command changed every issue a JQL query matched as
soon as it was run. `--dry-run` was opt-in, and a run without a terminal wrote
just the same. A wrong JQL clause, or a shell variable that expanded to
nothing, was enough to change up to 1000 issues. `label --action set` made it
worse: `--labels` was optional, and `set` without labels sent `labels: []`, which
clears every label on every match. The Bitbucket bulk commands had already been
given a preview and a confirmation (see
[07092026_bulk_delete_branches_safety.md](07092026_bulk_delete_branches_safety.md)),
the Jira ones had not.

**One transition ID for every issue.** `bulk transition` fetched the available
transitions of the first matching issue, resolved the name or ID there, and sent
the resulting ID to all issues. Transition IDs are only unique within one
workflow: the same ID can be "In Progress" in one project and "Done" in another.
A query that matched issues from several workflows therefore either ran a
different transition than the one asked for, or failed on the issues where the
ID does not exist.

## What changed for users

- `transition`, `assign` and `label` list the matching issues by default and
  change nothing. `transition` shows, per issue, the transition that would run and
  the status it leads to, or `not available`.
- `--execute` applies the change. It asks for the number of matching issues to be
  typed back, or takes `--yes` to skip the prompt. Without a terminal and without
  `--yes` it refuses.
- `--yes` only skips the prompt. Without `--execute` it changes nothing.
- `--dry-run` is hidden and deprecated. It conflicts with `--execute` and means
  "list only".
- `--labels` is required and needs at least one non-empty value for `add`,
  `remove` and `set`. Clearing all labels through bulk is no longer possible.
- `transition` resolves the transition on every issue separately (details below).
- `jira bulk import` and `jira bulk export` are unchanged.

Scripts that relied on the old behaviour must add `--execute --yes`.

## Design notes

### Preview, confirmation, `--yes`

The model is `bb bulk delete-branches`: listing is the default, `--execute`
performs the change, and `confirm_destructive` in
`crates/cli/src/commands/common.rs` gates it. The expected typed value is the
number of matching issues. The confirmation happens after the issues are found
(and, for `transition`, after the transition is resolved) but before the first
write, so the count shown is the count that is acted on and no partial change
precedes the prompt. The prompt and the notes go to stderr, so a piped `-f json`
listing stays machine-readable. As in Bitbucket, a missing terminal is a hard
error, not a hang and not a silent write.

`--dry-run` is kept so existing invocations still parse. As with
`bb bulk delete-branches`, `execute && !dry_run` means the flag can never start
a change, and clap rejects it next to `--execute`.

### Labels

`bulk_label` calls `normalize_labels` before it searches: values are trimmed,
empty ones are dropped, and an empty result is an error for every action. clap
additionally marks `--labels` as required, so the omission is reported as a usage
error. A mistyped or empty variable can no longer turn `set` into "clear all
labels". Clearing labels in bulk was never a documented use, and doing it by
omission is the hazard being removed.

### No match

When the query matches nothing, the three commands render an empty list through
`render_list_or_empty`, as a non-empty preview renders its rows. The table format
prints "No issues found matching query", and `-f json` prints `[]`.

### Transitions resolved per issue

`plan_transitions` reads the transitions of every matching issue and resolves
the request on each one. The decision is a pure function,
`resolve_transition(available, request)`, which returns the transition ID, its
name and the target status (`to.name`), or `None`.

- **A name** is compared case-insensitively on each issue, Unicode aware
  (`CAFÉ` matches `café`).
- **An ID** is a value that equals a transition ID on the first issue
  (`TransitionRequest::from_first_issue`). It is pinned to the name it has there.
  On every other issue it is only used if the ID exists with the same name.
  Otherwise that issue is an error.
- **Precedence on the first issue.** An ID match is checked first. If the same
  value is also the name of a *different* transition there, it is ambiguous and
  applied to no issue, with an "ambiguous" error per issue. If it is the name of
  the same transition (ID and name are equal), it is an ID. Without an ID match,
  a name match means a name, matched on every issue.
- **No match on the first issue.** A value made only of digits is meant as an ID.
  A number that is neither an ID nor a name there is applied to no issue, so a
  transition that happens to be named like that number on a later issue is never
  run by accident. Any other value is kept as a name and resolves only on issues
  that have a transition of exactly that name.
- **No fallback.** An issue that does not offer the transition gets no request.

The first issue is read first because it fixes what an ID means. The others are
read concurrently with the `--concurrency` setting. A failed read never aborts
the run, it is that issue's error. If the first issue cannot be read, a name is
still resolved independently on every other issue, while a number is applied to
no issue, because its meaning cannot be established. When a request is applied to
no issue, the other issues are not read at all.

The same plan feeds the preview, the confirmation and the execution, so the
transition shown is the transition sent, and the confirmation names how many
issues will be skipped. Only the resolved transitions are sent. Issues without
one are reported with their key and the requested transition, issues that were
transitioned stay transitioned, and the command exits with an error when any
issue was not transitioned. Failures are printed per issue on stderr, because the
executor's own summary only counts them.

The issue key is interpolated into the request path as before, as in the rest of
the Jira command layer. Encoding path segments is a separate, broader change; see
[08092026_remaining_hardening.md](08092026_remaining_hardening.md). The keys come
from the search response, not from the command line.

## Limitations

- **The transition is resolved before the prompt, not at write time.** An
  issue whose status changes while the prompt waits can reject the request. That
  issue is reported as a failure and nothing else is affected.
- **An ID is tied to the first issue.** If the first issue does not offer the ID,
  or cannot be read, a number is applied to no issue. Use the transition name for
  queries that span workflows, or order the query so a suitable issue comes
  first.
- **Two transitions with the same name on one issue.** The first one in Jira's
  list is used, as before. Jira does not promise an order.
- **Search is capped at 1000 issues, as before.** The count in the prompt is the
  count of issues returned, not necessarily everything the JQL matches.
- **The typed value is the match count,** not an identifying name. It protects
  against a prompt answered by reflex, and `--yes` removes it.
- Nothing here was exercised against a live Jira instance. The behaviour is
  covered by mocked HTTP only.

## Tests

Resolution, in `jira/bulk.rs` (no HTTP):

- `a_name_is_matched_case_insensitively`
- `a_name_match_is_unicode_aware`
- `a_transition_that_is_not_offered_does_not_resolve`
- `an_id_only_resolves_where_it_keeps_its_name`
- `the_first_issue_decides_what_an_id_means`
- `an_unknown_name_stays_a_name`
- `an_unknown_number_is_applied_to_no_issue`
- `a_number_that_is_a_name_on_the_first_issue_is_a_name`
- `an_id_that_is_also_another_transitions_name_is_ambiguous`
- `an_id_that_is_also_its_own_name_is_not_ambiguous`
- `an_unreadable_first_issue_keeps_ids_closed_and_names_open`
- `normalize_labels_trims_and_drops_empty_values`
- `the_preview_shows_transition_and_target_status_per_issue`

The handlers over a `wiremock` server, in `jira/bulk.rs`. Two workflows, where ID
`21` is "In Progress" once and "Done" once. The runs that must not write mount
their mutation mocks with `expect(0)`, which wiremock verifies when the server is
dropped:

- `a_transition_name_is_resolved_on_each_issue`: each issue gets its own ID.
- `an_id_is_not_sent_to_an_issue_where_it_means_something_else`
- `an_issue_without_the_transition_fails_and_the_others_still_run`
- `a_failed_lookup_on_one_issue_does_not_stop_the_others`
- `an_unreadable_first_issue_does_not_stop_a_name_on_the_others`
- `an_unreadable_first_issue_is_one_row_of_the_preview`
- `an_unreadable_first_issue_closes_an_id_for_every_issue`
- `an_ambiguous_value_is_applied_to_no_issue`
- `plans_keep_the_order_of_the_search_result`: three issues, the second answers
  late, so the order only holds because of the explicit sort.
- `the_target_status_is_resolved_per_issue`
- `transition_lists_by_default_and_sends_no_transition`
- `assign_lists_by_default_and_changes_nothing`
- `assign_with_execute_and_yes_assigns_every_issue`
- `label_lists_by_default_and_changes_nothing`
- `label_set_with_execute_and_yes_writes_the_given_labels`
- `label_without_labels_is_an_error_and_sends_nothing`
- `no_match_is_a_no_op_for_every_command`

The built binary against a mock Jira, in `crates/cli/tests/jira_bulk_safety_e2e.rs`.
The binary has no terminal on stdin there, which is what a script looks like:

- `transition_lists_by_default_with_the_status_each_issue_would_reach`
- `transition_preview_reports_issues_where_it_is_not_available`
- `transition_execute_without_yes_and_without_a_terminal_is_refused`
- `transition_by_name_runs_the_matching_transition_on_each_issue`
- `transition_by_id_is_an_error_where_the_id_means_something_else`
- `transition_unavailable_on_one_issue_fails_but_the_others_are_transitioned`
- `an_unreadable_first_issue_does_not_stop_a_transition_name`
- `an_unreadable_first_issue_sends_no_transition_for_an_id`
- `an_ambiguous_transition_value_sends_nothing`
- `assign_lists_by_default`, `assign_execute_without_yes_and_without_a_terminal_is_refused`,
  `assign_execute_with_yes_assigns_every_issue`
- `label_lists_by_default`, `label_execute_without_yes_and_without_a_terminal_is_refused`,
  `label_execute_with_yes_writes_the_labels`
- `label_without_labels_is_rejected_for_every_action`
- `no_match_renders_an_empty_json_list_for_every_command`
- `the_deprecated_dry_run_flag_still_lists_only`
- `dry_run_cannot_be_combined_with_execute`: also checks the clap message
- `yes_alone_does_not_execute`

The README and the example scripts under `docs/examples/jira/` are checked by
`docs_examples.rs` and `docs_examples_scripts.rs`, which only verify that the
command lines parse.

The typed prompt itself (`confirm_destructive`) is covered by the tests in
`commands/common.rs`. A positive answer typed on a terminal is not covered by a
test, as for `bb bulk`.
