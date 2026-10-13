Accounts now have one management screen, reached from Agents > Accounts (`ctrl+b A l`), the sidebar heading, or an account's details. `ctrl+b A n` and the single active `+ Add account` entry open the same two-field form. The sidebar and Accounts use the same two-line labels and usage summaries. Unknown usage is never displayed as zero; expired observations say when they were last checked.

Enter or a single click opens details. Tab offers Rename, Sign in and Remove. Removal asks first, selects Cancel initially, and states that conversations, native sign-in and running panes are kept. Rename returns to the selected account. The launch picker contains startable accounts, with only new-tab, applicable worktree and account-details actions. Worktree launches retain the selected account rather than choosing again from rules.

The form starts with the focused pane's agent, otherwise the last choice of that attached client, and an available name. Missing programs are explained before account creation. Existing verified sign-ins awaiting adoption are offered first. Checks cannot be submitted twice; cancellation ignores late results. Errors retain both fields, wrap and scroll. A successful addition selects its row in Accounts and shows `Account added`.

The existing merged sign-in lifecycle is retained. A failed or cancelled native login leaves a recoverable account marked `Sign-in required`. If saving succeeded but opening sign-in failed, the form explains that Esc returns to Accounts and Sign in retries. This change introduces no account-file, saved-session or protocol fields. Existing account identities, legacy names, home links and pane references remain in use. CLI account output is unchanged.

The focused implementation is in `src/server/account_ui.rs`; existing picker, sidebar, keymap and worktree code routes into it. `docs/agents.md` and `docs/keys.md` describe the resulting behavior. The CLI examples did not need changes.

| Unit coverage | What it proves |
| --- | --- |
| `account_summaries_distinguish_missing_expired_and_required_usage` | Known percentages, missing measurements, expired windows and required sign-in remain distinct. |
| `accounts_list_always_has_one_active_add_row_and_selects_by_identity` | Identity selection, shared wording, pending reviews, and one visible undimmed add entry even after filtering. |
| `add_form_defaults_and_missing_program_keep_values_without_creating_an_account` | Available default name, agent selection, retained input, installation advice and return to Accounts. |
| `form_errors_wrap_and_scroll_in_full_at_both_sizes_and_buttons_click_once` | Every line of a long error is readable at 80x24 and 124x36; one click submits or cancels. |
| `existing_sign_in_is_the_first_form_action_and_details_keep_unknown_usage` | Existing-account action order, unknown usage in details and access to account actions. |
| `checking_an_existing_sign_in_accepts_cancel_but_never_a_second_submission` | A checking form rejects duplicate submission, advertises its actual state and accepts cancellation. |
| `one_click_on_either_line_opens_an_unselected_account_and_blank_rows_do_nothing` | Both account lines activate immediately; blank space cannot activate another item. |
| `remove_first_asks_and_enter_cancels_without_touching_accounts` | Removal explains retention and requires an affirmative answer. |
| `sidebar_accounts_share_two_lines_and_keep_the_list_and_add_accessible` | Both sidebar lines target the same account; the heading and add entry remain when account rows must give way. |
| Updated picker-action, footer and documentation tests | Launch and management actions are separated, existing switching actions remain, and documented keys match the menu. |

End-to-end coverage was written and compiled, but was not executed in this sandbox. All of it uses `tests/common` and fake agents:

| End-to-end coverage | What it checks |
| --- | --- |
| `accounts_add_form_finishes_in_the_list_and_actions_keep_live_conversations` | Both terminal sizes, focused-agent prefill, duplicate-name retention, one native login, automatic selection and completion message, sidebar and picker clicks, launch actions, rename, confirmed removal and a surviving conversation. |
| `the_add_form_names_an_existing_sign_in_without_running_login` | Existing-account action order, one pending check, cancellation without a late rename, and adoption with the requested name without native login. |
| `a_missing_agent_keeps_the_add_form_and_creates_nothing` | Installation guidance, retained name, no account or login pane, remembered agent choice and no add rows in the launch picker. |
| Updated login, review, rename, picker and worktree tests | Existing scenarios use Accounts; a worktree keeps the selected alpha account even when a rule prefers beta. |

Every Cargo command used `taskset -c 0-7,16-23` and Rust 1.93.0.

| Required check | Result |
| --- | --- |
| `cargo +1.93.0 fmt --check` | Passed. |
| `cargo +1.93.0 clippy --all-targets -- -D warnings` | Passed. |
| `cargo +1.93.0 clippy --all-targets --target aarch64-apple-darwin -- -D warnings` | Passed. |
| `cargo +1.93.0 test --bin kraal` | 812 passed, 23 failed, 5 ignored. |
| `git diff --check` | Passed. |

The 23 failure names exactly match the existing `review/baseline-unit-tests.log`: restricted socket operations and runtime-directory isolation assertions. That is a comparison with the repository's recorded baseline, not a newly run baseline checkout. All changed unit tests and the documentation checks pass. Current outputs are in `accounts-fmt.log`, `accounts-clippy-linux.log`, `accounts-clippy-macos.log` and `accounts-unit-tests.log` beside this report.

The maintainer still needs the full end-to-end, examples and compatibility suites on Linux and native macOS. Rust 1.88 was not separately built; the declared MSRV remains 1.88 and Clippy's MSRV checks pass. No new platform-specific production code or dependency was introduced. No kraal binary was run manually and no changes were committed or staged.
