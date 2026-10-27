# `confluence bulk delete`: listing by default and a confirmation gate

Status: in review (PR #TBD).

## Problem

`confluence bulk delete --cql <query>` deleted every page the query matched as
soon as it ran. The search is a single request with `limit=1000`, so one
mistyped or too broad query could remove up to a thousand pages. Nothing asked
for confirmation, and running without a terminal (a script, a scheduled job)
deleted all the same. The only protection was the opt-in `--dry-run` flag, which
is the wrong way round for a destructive command: whoever forgot it lost pages.

`bb bulk delete-branches` and `bb bulk archive-repos` had already been changed
to list by default and to require `--execute` plus a typed confirmation, see
[07092026_bulk_delete_branches_safety.md](07092026_bulk_delete_branches_safety.md).
The Confluence command was the remaining bulk delete without that gate.

## Design notes

The design copies the Bitbucket commands on purpose, so the three bulk delete
commands behave alike.

- **Listing is the default.** Without `--execute` the command prints the
  matching pages (ID and title, as a table, or as JSON and the other formats
  like any list) and sends nothing but the search. A hint on stderr says how to
  delete, so a piped `-f json` listing stays machine-readable.
- **`--execute` deletes.** Only with it are pages deleted.
- **A typed confirmation.** With `--execute` the user types the number of
  matched pages (for example `42`) before the first delete. The prompt shows the
  count, says the pages move to the trash and that only the first search page
  (at most 1000 pages) is covered. It is built from the same search result that
  is then deleted, so the count shown is the count acted on, and nothing is
  deleted before the prompt is answered.
- **No terminal means refuse.** Without a terminal on stdin and without
  `--yes`, `confirm_destructive` returns an error and nothing is deleted. A
  script that never had a chance to answer must not delete.
- **`--yes` skips only the prompt.** It has no effect without `--execute`, so
  `--yes` alone still lists.
- **`--dry-run` is kept, hidden and deprecated.** It behaves like the new
  default. It conflicts with `--execute` in clap, so the pair is rejected
  instead of one silently overriding the other, and the dispatch code also
  computes `execute && !dry_run` as a second guard.
- **Nothing to delete means nothing to confirm.** An empty result prints the
  usual empty-list message (an empty array under `-f json`) and never prompts.
- **`confluence bulk add-labels` is unchanged.** Adding labels is additive and
  outside this change.

The confirmation is split out as `delete_pages_with`, which takes the
confirmation function as a parameter. Production passes `confirm_destructive`;
tests pass a closure that calls `confirm_destructive_on(.., false)` or records
what was asked, so no test depends on whether `cargo test` inherits a terminal.

For scripts the migration is mechanical: add `--execute --yes` where the old
call deleted. `docs/examples/confluence/bulk-cleanup.sh` does this after its own
typed confirmation, and takes its page count from the listing of the same
command.

## Limitations

- **One search page.** The search still requests `limit=1000` and does not
  follow pagination. With 1000 matches a run covers only the first 1000, and
  the command warns about this on stderr. Further matches need another run.
- **Trash, not purge.** The command calls `DELETE /wiki/api/v2/pages/{id}`,
  which Atlassian documents as moving a current page to the trash. This has not
  been checked against a live Confluence instance, so the wording rests on the
  API documentation.
- **Partial failures.** If some deletions fail, the executor reports how many
  failed. It does not list which pages were already deleted; the successes are
  only logged at `info` level. The Bitbucket commands render that record, this
  command does not yet.
- **Page IDs in paths.** The IDs returned by the search are interpolated into
  the delete path without encoding. This is the general path-building topic in
  [08092026_remaining_hardening.md](08092026_remaining_hardening.md) and is not
  changed here.
- **Concurrency of 0** is not validated, as before.

## Tests

Unit tests in `crates/cli/src/commands/confluence/bulk.rs`, over a wiremock
server:

- `listing_is_the_default_and_deletes_nothing`: no DELETE without `--execute`.
- `yes_alone_does_not_delete`
- `execute_without_a_terminal_or_yes_refuses`: the refusal of
  `confirm_destructive_on(.., false)`, and no DELETE.
- `the_confirmation_expects_the_match_count_and_states_the_scope`: the expected
  text is the count, the warning names the count, the trash and the 1000 page
  cap, and nothing is deleted when the confirmation is declined.
- `a_passed_confirmation_deletes_every_match`
- `execute_with_yes_deletes_every_match_without_asking`
- `no_matches_neither_asks_nor_deletes`

End-to-end tests in `crates/cli/tests/confluence_bulk_delete_e2e.rs`, which run
the binary with stdin closed against a mock server whose DELETE endpoint has an
explicit expected request count:

- `listing_is_the_default_and_deletes_nothing`
- `yes_without_execute_still_only_lists`
- `deprecated_dry_run_still_lists`
- `dry_run_and_execute_are_rejected_together`
- `execute_without_a_terminal_or_yes_refuses_and_deletes_nothing`
- `execute_with_yes_deletes_every_match`

`docs_examples.rs` and `docs_examples_scripts.rs` check that the README and the
example script still parse with the new flags.
