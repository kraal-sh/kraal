The two publication fixes are implemented and uncommitted on `loop-publication-fixes`, based on `37cd5b4748a4bf28f562700f9b4e3a8479e07cb7`.

[01-launch-failure.patch](01-launch-failure.patch) is the complete first change against that baseline. [02-notification-overflow.patch](02-notification-overflow.patch) follows it and includes its fixture migration. Both changes are already applied in this worktree. The patches were applied to an isolated baseline copy, compared byte-for-byte with the final source, and reversed in reverse delivery order. They are review/commit boundaries, not patches to apply again over the current worktree. The Git index was left untouched.

The first change touches `src/pane/pty.rs`, its export in `src/pane/mod.rs`, `src/server/panes.rs`, the server test fixture in `src/server/mod.rs`, and `tests/cli.rs`. `Pending::start` returns a must-use outcome: cancelled, running, or failure with pane ID, process hold, and error. It does not publish failure. The launcher worker publishes the returned failure normally; the loop fallback calls `pane_failed` directly. Each synchronous failure is therefore applied at the end of its own start, before subsequent pending starts and before the loop resumes queued events, including events already queued before the start. The process identity check rejects stale failures. Focus and empty-workspace cleanup remain at the enclosing handler boundary. Synchronous process creation itself remains synchronous.

The second change adds the private `src/server/queue.rs` construction module and `src/server/publication_tests.rs`. It updates the wait and output reconciliation in `src/server/mod.rs`; publisher types/calls in `src/pane/{mod,pty}.rs` and `src/server/{agents,fileman,persistence,worktrees}.rs`; fixtures in `src/pane/{archive,commands,fuzz,mod,pty,scrollback}.rs`, `src/server/{mod,panes,persistence}.rs`; and tests in `src/preview/tests.rs` and `tests/cli.rs`.

Successful readiness notifications still use the ordinary bounded queue. Only a full queue sets the relevant SeqCst bit and always attempts a nonblocking send to a separate capacity-one channel. Pane readiness and the existing Wake work are the only coalescible notifications. Payload events keep their blocking, ordered worker delivery. The loop uses Crossbeam selection over both channels and its existing deadline. A selected token remains local work across the ordinary event phase; token receipt precedes exchanging the bits with zero. Reconciliation finds current panes with buffered output, deduplicates them through `ready_set`, advances pending exits, and runs the existing Wake handler. The tick scan remains redundant recovery. No output bytes are moved by notification publication, no new polling interval is added, and ordinary successful pane notifications perform no dirty-bit or owner check.

The receiver is unique and cannot move to another thread. Construction records the current loop owner before restore or producers start. Clones share the queue domain; a mismatched sender is rejected at event-loop entry. Blocking publication on the owner thread returns `OwnThread(event)` with ownership intact, including when the queue has room. All production blocking publication sites were checked: after the launch repair, they execute on helper threads. The wrapper exposes no raw event sender. This enforces access to this queue, not a claim about unrelated channels or general handler liveness.

The 20 added unit tests all pass:

| Test | What it checks |
| --- | --- |
| `synchronous_launch_failure_does_not_wait_for_its_full_queue` | Actual fallback, broken interpreter, disconnected launcher, full capacities 1 and 4096, no receiving during the operation; failure before queued work; cleanup and focus only at the handler boundary. |
| `synchronous_launch_failure_survives_receiver_shutdown` | Actual fallback still suspends the failed pane after receiver shutdown. |
| `a_returned_failure_owns_the_failed_process_and_cannot_suspend_its_replacement` | Returned pane ID, error and hold; stale generation and removed pane ignored; cancelled pending start. |
| `successful_notifications_use_only_the_ordinary_queue` | Success preserves FIFO and creates neither dirty state nor token. |
| `both_notification_kinds_coalesce_but_payloads_do_not` | One token covers both kinds; 20 client payloads remain ordered and delivered exactly once. |
| `own_thread_errors_return_the_event_even_with_room_or_after_shutdown` | Immediate owner-thread rejection with payload ownership; disconnected worker publication returns its payload. |
| `dropping_a_receiver_releases_a_blocked_worker_with_its_payload` | Shutdown releases publication against a full queue while overflow is pending. |
| `bindings_are_distinct_even_when_recreated_on_the_same_thread` | Clone identity, old-domain isolation, recreated binding, mismatch rejection. |
| `all_dirty_and_token_interleavings_remain_reachable` | 900 deterministic schedules using the real atomics and Crossbeam channels: same/different dirty kinds, empty/full/stale token slot, producer stores/sends around consumer receipt/exchange, no rescuing producer or timer. |
| `a_producer_paused_after_the_dirty_store_still_sends_a_token` | Thread checkpoint across receipt/exchange; an already-set bit still sends; a stale token is harmless. |
| `an_idle_selector_stays_asleep_until_publication` | Linux thread context-switch count stays unchanged during a 700 ms quiet interval, then publication wakes it. |
| `rejected_final_output_is_parsed_once_without_a_tick_or_later_producer` | Forced rejection, real output buffer, server reconciliation and parser; final text appears exactly once. |
| `rejected_eof_finishes_an_exit_without_bytes_or_a_timer` | Rejected empty EOF completes a pending exit through reconciliation. |
| `overflow_and_ordinary_readiness_do_not_duplicate_unparsed_or_watched_output` | 400 ordered lines survive interrupted parsing; ready/watched bookkeeping and final tail are not duplicated. |
| `reconciliation_uses_current_panes_after_removal_or_restart` | Removed and replaced output is not applied to the current pane; replacement output appears once. |
| `a_selected_token_is_kept_across_handlers_and_selected_events_keep_fifo` | Selected event retention, ordinary FIFO, and retained token obligation across handlers. |
| `a_rejected_preview_wake_runs_the_existing_reload_handler` | Overflow starts the real preview reload handler without a tick and applies its returned job. |
| `the_reader_publishes_its_final_burst_and_empty_eof_through_overflow` | Actual `read_output` with a controlled pipe: rejected final burst followed by silence, and rejected EOF with no bytes; notification remains acknowledged and bytes are taken once. |
| `overflow_releases_a_reader_waiting_for_output_space` | Threshold-full output, another reader append, overflow reconciliation making room, exact bytes and EOF. |
| `a_preview_callback_returns_with_both_notification_queues_full` | Actual preview callback invoked under its callback mutex returns with both queues full and during receiver shutdown; callback thread is joined. |

The existing held-program test now checks typed cancellation/running/failure outcomes. CLI coverage extends `a_program_that_cannot_run_after_all_waits_for_enter` to repair the executable and retry it. Two new shared-harness tests check that an attached client draws a final burst before a silent pane ends, and that waiting for exit preserves 9000 numbered output lines and the last marker exactly once. These CLI tests are compatibility tests; deterministic forced rejection is covered by the unit tests.

All required commands used Rust 1.93.0 and `taskset -c 0-7,16-23`:

| Command (following the affinity prefix) | Result |
| --- | --- |
| `cargo +1.93.0 fmt --check` | Passed. |
| `cargo +1.93.0 clippy --all-targets -- -D warnings` | Passed. |
| `cargo +1.93.0 clippy --all-targets --target aarch64-apple-darwin -- -D warnings` | Passed. |
| `cargo +1.93.0 test --bin kraal` | 747 passed, 23 failed, 5 ignored. |

The unchanged baseline was tested with the same toolchain and affinity through a copied manifest inside this worktree: 727 passed, the identical 23 tests failed, and 5 were ignored. There are no newly failing tests. Failures are existing socket operations denied by the sandbox (including socket creation/options and the socket portion of `process::this_process`) and two existing runtime-directory/isolation assertions. Full outputs are in [unit-tests.log](unit-tests.log) and [baseline-unit-tests.log](baseline-unit-tests.log); compiler outputs are [clippy-linux.log](clippy-linux.log) and [clippy-macos.log](clippy-macos.log). `git diff --check` also passed.

Rust 1.88 remains the declared MSRV. No dependencies changed, and Clippy's MSRV checks pass; a Rust 1.88 build was not run. The test pipe API used here was stabilized in Rust 1.87. No production OS-specific primitive was introduced; the only new Linux-specific test is cfg-gated.

End-to-end tests and native macOS tests were not run in this sandbox. Baseline/candidate latency, throughput, startup and full-session idle-rate comparisons from section 7.2 remain for the maintainer; no claim of measured cost neutrality or of a measured <0.1 wake-up/second session is made. The quiet-selector test is a narrow check against added periodic waking, not that benchmark. The existing unbounded event drain and synchronous handler waits remain unchanged.

The document's two publication defects reproduce against this checkout. Its source revision and line numbers are older. One scope qualification matters: `timers` already calls `finish_exits`, and `next_deadline` includes pending-exit deadlines; exit handling also drains output. The tick's `has_output` scan is therefore not the only recovery path, and rejected EOF alone does not establish permanently unnoticed normal process exit. The new tests deliberately omit those fallback paths. The original wait was `recv_timeout`; this change uses selection in the same Crossbeam backend, not an OS wait primitive or client polling.
