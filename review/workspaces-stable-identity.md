W4 gives saved workspaces an identity independent of their label, order, source path and the
numeric handles of a running server. The changes are uncommitted.

The implementation reuses the UUID generator already used by Accounts. Snapshots and workspace
list replies carry the identity; restore keeps it, including when a workspace cannot start.
Failed workspaces also keep their reason. Opening their source refuses to launch a replacement
layout. Saved name collisions survive restore, and ambiguous open/save requests refuse to pick
the first entry.

Migration uses version 1, revision 2, with additive fields in the existing `workspaces` array.
The 0.2.0 and 0.3.0 reader definitions were checked in their local git tags. Both can still read
the layout. An older writer discards workspace identities when it saves again; returning to
this version assigns new identities and makes another backup. There is no new selector or
protocol feature that an old server could silently ignore.

Before conversion, a private `SESSION.json.before-workspace-ids-UUID/` directory receives
copies of referenced, available scrollback and the exact original `state.json`. The latter is
written last to distinguish a complete backup from an interrupted one. Each conversion has its
own backup. The converted snapshot is published atomically before programs are restored. KDL,
KDL backups and account files are not rewritten by this migration; account references, source
paths, recorded KDL bytes and unknown JSON fields survive conversion.

A whole-state read, validation, backup or migration failure stops startup without replacing the
source. Startup success is announced after restore so the caller can receive that diagnostic.
A per-workspace restore failure retains the workspace and reports how to retry. Backups include
scrollback because live scrollback is later overwritten or pruned. They intentionally survive
`forget`; the documentation explains rollback, the loss of changes made after migration, and
manual removal when a backup is no longer needed.

Scope follows the delivery plan: keeping Closed workspaces remains W5, the common workspace
list and renaming without renaming a file remain W6, Delete recovery remains W7, and reorder
controls remain W8. This PR changes neither the existing rename action's file behavior nor
Close semantics, and adds no unfinished UI action. File-only list entries do not receive an
identity until opened. The updated pages are `docs/workspaces.md` and `docs/compatibility.md`.
KDL syntax and the examples are unchanged.

The new unit tests prove:

| Test | What it proves |
| --- | --- |
| `migration_only_adds_identities_and_keeps_order_sources_collisions_and_accounts` | Distinct IDs, preserved order and names, source and account metadata, unknown fields, and idempotent conversion |
| `releases_020_and_030_can_read_the_complete_migrated_workspace_array` | The shared released session/workspace schema still reads the full layout rather than an empty session |
| `migration_preserves_the_meaning_of_legacy_tab_names` | Revision 0 automatic tab labels migrate, while explicit names from revision 1 stay |
| `the_migration_backup_is_private_complete_and_never_replaced` | Exact original bytes, independent scrollback copies, private permissions, repeated conversion and rollback |
| `a_failed_backup_leaves_the_state_unchanged_and_can_be_retried` | Backup failure prevents publication, and fixing the cause permits retry; missing scrollback does not lose a layout |
| `unreadable_future_and_invalid_states_are_never_replaced` | Corrupt or unsupported state, missing/invalid/duplicate identities and dangling state links are preserved |
| `workspace_identity_is_optional_for_older_servers_and_clients` | Old replies remain accepted and existing protocol fields retain their meaning |
| `workspace_identity_survives_renaming_reordering_and_restoring` | Identity survives rename, source relocation, reorder, name collisions and changed runtime handles; a successful restore clears an old failure |
| `a_failed_workspace_keeps_its_identity_source_layout_and_reason_until_restored` | A workspace without restorable panes stays saved across retries |
| `failed_workspaces_keep_their_source_without_opening_a_second_copy` | A failed saved layout cannot be bypassed by opening its KDL or its personal name |
| `ambiguous_workspace_names_never_pick_the_first_saved_layout_or_file` | Ambiguous open/save requests preserve all entries instead of selecting an arbitrary one |

The existing account-binding test also checks that unsupported future revisions are left alone.
Two new tests in `tests/cli.rs` use `tests/common`: real legacy-state migration with snapshot
precedence and stable IDs across restarts, and startup failure with an unchanged source and a
successful retry after repair. The existing TUI rename test now checks identity before and after
rename and restart. The older-tab-label integration test uses revision 2 when constructing its
legacy fixture.

All requested commands use Rust 1.93.0 and the required CPU affinity:

| Command | Result |
| --- | --- |
| `taskset -c 8-13,24-29 cargo +1.93.0 fmt --check` | Passed |
| `taskset -c 8-13,24-29 cargo +1.93.0 clippy --all-targets -- -D warnings` | Passed |
| `taskset -c 8-13,24-29 cargo +1.93.0 clippy --all-targets --target aarch64-apple-darwin -- -D warnings` | Passed |
| `taskset -c 8-13,24-29 cargo +1.93.0 test --bin kraal` | 847 passed, 23 failed, 5 ignored |

All new unit tests passed, as did all 13 documentation tests in `src/cli/docs.rs`. The 23 failures
are existing tests of socket creation, process access and runtime-directory isolation in this
sandbox. Their diagnostics include `Operation not permitted`, rejected socket options and
runtime-directory checks falling back to the developer's forbidden directory. No restrictions
were bypassed. The complete failure names and diagnostics are in
[the unit-test log](workspaces-unit-tests.log); the other logs are
[formatting](workspaces-fmt.log), [Linux clippy](workspaces-clippy-linux.log) and
[macOS clippy](workspaces-clippy-macos.log).

The end-to-end, examples and real released-binary compatibility suites were not run. Both
clippy checks compile their targets. The maintainer still needs the full suite on Linux and
macOS; the macOS result here is a cross-target lint check, not a macOS execution. MSRV stays at
1.88, with no dependency changes, and was not separately compiled with that toolchain.
