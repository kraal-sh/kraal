Account changes now belong to the pane. Clicking its account label, choosing the bottom bar's
switch account entry, and pressing `ctrl+b A c` open the same Change account flow. The existing
sign-in checks, end-of-turn waiting, interruption confirmation and per-pane operation reservation
remain shared. A notify threshold displays `<name> is at NN% used · Change account`; the clickable
action addresses the pane that reached the threshold, independently of focus.

A started replacement offers `Using <name> · Undo`. Find a pane, Tab, Undo account change keeps
the return available after the message expires. The optional `account_undo` saved with the pane
also survives session restoration. Claude transfers the latest conversation back under the same
checks. Codex resumes the original conversation in its original account and retains the newer
history. A successful Undo clears the return; another forward change replaces it. No file edits
made by the agent are reverted.

The source launch remains available until the replacement has produced output and has been
observed as a running agent for at least one second. A spawn error, early exit, or absence of a
usable start within ten seconds triggers one recovery attempt. Recovery uses the previous
account and conversation, and reports `Resumed on <name>` only after its own usable start.
A second failure leaves the previous command ready for Enter, without retrying in a loop.
The saved-conversation promise is conditional on verifying the history. Startup observation is
process and output evidence, not a provider readiness API. A later failure after that observation
is an ordinary agent exit.

Shell launches are eligible only when the foreground process, account home, conversation and
resume arguments can be established. The checks include process start time, hook ancestry or an
explicit session selector, readable history, credential overrides, the executable and PATH,
and other jobs in the shell. Observations are cached for rendering and re-read before stopping.
Opaque or changed processes offer `Can't switch this process.` and `Open with another account`.
The latter opens another pane. Codex needs an identifiable saved conversation before a switch;
without its id, the next step is to finish a turn with the integration enabled and try again.
History verification follows the JSONL session metadata used by the
[upstream Codex rollout fixture](https://github.com/openai/codex/blob/main/codex-rs/app-server/tests/common/rollout.rs).
Unrecognized or oversized metadata is refused conservatively.

No accounts.toml schema, protocol request or CLI option was added. The optional saved-pane field
defaults to absent for old sessions, and older readers can ignore it. The existing input protocol
continues to carry keyboard and mouse actions for older attached clients. Notification actions
are clickable in kraal's interface; terminal and system notifications carry the same action text
without introducing a platform-specific desktop callback. Linux process inspection and the
existing macOS sysctl/libproc path both compile. Rust's declared MSRV remains 1.88.

The behavior is documented in `docs/agents.md`, `docs/keys.md`, `docs/compatibility.md`, and the
comments in `examples/accounts.toml`.

All 16 new unit tests passed in the final run:

| Test | What it proves |
| --- | --- |
| `a_process_account_uses_its_own_home_and_refuses_credential_overrides` | Association uses the process's home, rejects relative or unknown homes and credential overrides. |
| `codex_resume_requires_readable_matching_session_metadata` | Missing, malformed, mismatched and invalid session references cannot authorize a resume. |
| `command_environment_distinguishes_an_unreadable_process` | Unreadable process information is distinct from an empty environment. |
| `observed_commands_require_exact_interactive_arguments` | Known interactive commands are accepted; wrappers, ambiguous options, one-shot runs and unsafe selectors are refused. |
| `undo_is_saved_only_with_a_started_change_and_survives_resuming_the_pane` | Preparing a replacement does not announce success; the return survives serialization and resume, old JSON still loads, and threshold state resets for the new account. |
| `claude_undo_carries_the_latest_transcript_and_refuses_divergence` | Undo copies the newest history back and refuses divergent transcripts before stopping. |
| `codex_undo_resumes_the_original_id_and_keeps_both_histories` | A new Codex run does not inherit the old id, and Undo resumes the original without removing either history. |
| `a_failed_undo_sign_in_keeps_the_return_action_and_running_account` | Failed verification preserves both the running pane and its Undo. |
| `failed_start_restores_the_source_and_a_failed_recovery_keeps_one_retry` | Recovery uses the original account and resume command; a second failure does not loop. |
| `removing_the_destination_after_the_stop_resumes_the_source` | A destination removed between stopping and launching is not started. |
| `a_live_detected_process_is_rechecked_before_any_stop` | A real fake-agent process is associated, and a changed process identity is refused without signalling it. |
| `a_process_that_changes_during_verification_offers_another_pane` | An invalidated observation produces the actionable fallback and preserves the original pane. |
| `an_unassociated_process_offers_a_new_pane_without_changing_the_old_one` | Missing evidence never authorizes a stop. |
| `notify_threshold_has_a_change_action_without_stopping_or_requiring_an_alternative` | The rounded percentage and pane action appear once per crossing, including without an eligible automatic alternative. |
| `account_frame_hit_uses_the_drawn_cells_and_drops_hidden_details` | Hit regions match rendered Unicode labels at 80 and 124 columns, including floating and marked panes; hidden labels do not receive clicks. |
| `notification_actions_stay_visible_when_truncated_and_expire_with_the_message` | Action labels survive truncation, clicks cover their cells, and replaced or expired messages lose their action. |

The existing pane startup-output test also now checks that respawn clears startup evidence.
Existing tests for sign-in, reliable turn boundaries, confirmation defaults, concurrent changes,
stale checks, transcript failures, identities and documentation all passed.

Seven end-to-end tests were added in `tests/cli.rs`, using the shared `tests/common` harness and
fake agents. They were compiled by both Clippy runs but were not executed:

| Test | What it is intended to verify |
| --- | --- |
| `frame_and_bottom_bar_open_the_same_change_and_claude_undo_continues_the_latest_history` | Mouse entry points and one-click Claude Undo at 80x24 and 124x36, including history written after switching. |
| `codex_undo_from_pane_actions_reopens_the_original_and_keeps_the_new_conversation` | The pane action, confirmation, original resume id and unchanged newer history. |
| `a_replacement_that_exits_before_starting_resumes_the_source_once` | An early exit recovers automatically; failure of both launches leaves one retryable pane. |
| `a_notify_threshold_opens_change_for_the_pane_that_reached_it` | A notification targets the affected pane while another pane has focus. |
| `a_shell_agent_can_switch_only_with_an_exact_account_and_saved_conversation` | Typed Claude and Codex launches can use the same switch engine. |
| `an_ambiguous_shell_agent_offers_another_pane_without_stopping_it` | Missing ids, credential overrides, wrappers and other shell jobs take the safe fallback. |
| `switching_waits_for_the_replacement_to_produce_output_before_offering_undo` | A spawned but silent replacement does not announce success. |

The existing prompt-preservation and failed-transfer end-to-end test now expects automatic
source recovery rather than another Enter. The fake Codex records conversation metadata and
reports its id through the real hook path.

Every Cargo command used `taskset -c 8-13,24-29` and Rust 1.93.0. Final results:

| Command after the affinity prefix | Result |
| --- | --- |
| `cargo +1.93.0 fmt --check` | Passed, exit 0. [Log](account-switch-fmt.log). |
| `cargo +1.93.0 clippy --all-targets -- -D warnings` | Passed, exit 0. [Log](account-switch-clippy-linux.log). |
| `cargo +1.93.0 clippy --all-targets --target aarch64-apple-darwin -- -D warnings` | Passed, exit 0. [Log](account-switch-clippy-macos.log). |
| `cargo +1.93.0 test --bin kraal` | 833 passed, 23 failed, 5 ignored, exit 101. [Full log](account-switch-unit.log). |

The 23 failures are in socket/terminal/client/network and runtime-directory tests. The log
records denied socket operations (`Operation not permitted`), assertions around those operations,
the runtime path ownership check, and the isolation assertion that the fallback runtime path is
the developer's. None is a new account-switch test. The unit suite is therefore not reported as
passing. All 13 `src/cli/docs.rs` checks passed, including the TOML examples and quoted keys.

The end-to-end suite, `tests/examples.rs`, cross-version compatibility suite, and macOS runtime
tests were not run in this sandbox. No 1.88 toolchain run was requested or performed. The
maintainer still needs the full Linux and macOS runs and the 0.2.0/0.3.0 compatibility run.
Changes are uncommitted. No kraal binary was run by hand.
