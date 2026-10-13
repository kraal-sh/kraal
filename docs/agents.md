# Coding agents

kraal knows which panes run an agent and what each one is doing, gives each agent its own worktree, and spreads the work over several subscriptions.

## States

kraal recognises 30 coding agents by their process, run directly or through Node or Python as
npm and pip install them: Claude Code (`claude`), Codex (`codex`), OpenCode (`opencode`), Gemini
CLI (`gemini`), Qwen Code (`qwen`), GitHub Copilot CLI (`copilot`), Cursor CLI (`cursor-agent`
or `agent`), Factory's droid (`droid`), Augment (`auggie`), Kiro CLI (`kiro-cli`, `q chat`),
Kimi Code (`kimi`), Kilo (`kilo`), Cline (`cline`), Continue (`cn`), Amp (`amp`), Aider
(`aider`), Goose (`goose`), Crush (`crush`), pi (`pi`), Grok (`grok`), Codebuff (`codebuff`),
Qoder (`qoder`), CodeBuddy (`codebuddy`), Junie (`junie`), LLxprt Code (`llxprt`), Kode
(`kode`), OpenHands (`openhands`), Mistral Vibe (`vibe`), Letta Code (`letta`) and Rovo Dev
(`acli rovodev run`). It infers their state from the screen: the interrupt hint an agent shows
while it works, the permission prompt or question it shows while it waits for you.

For exact states, install kraal's hooks in the agent. For Claude Code, this adds hooks to
`~/.claude/settings.json` and keeps a backup:

```sh
kraal integration install claude
```

| Command | |
|---|---|
| `kraal integration install gemini` | the same for another agent of the table below |
| `kraal integration install all` | Claude Code, Codex, and every agent below used here |
| `kraal integration status` | says what is installed, for the agents and for [your shells](terminal.md#shell-commands) |
| `kraal integration uninstall claude` | takes kraal's entries out again |

| Agent | `install …` | What kraal changes |
|---|---|---|
| Claude Code | `claude` | hooks and status line in `~/.claude/settings.json` |
| Gemini CLI | `gemini` | hooks in `~/.gemini/settings.json` |
| Qwen Code | `qwen` | hooks in `~/.qwen/settings.json` |
| GitHub Copilot CLI | `copilot` | a file of its own, `~/.copilot/hooks/kraal.json` |
| OpenCode 1 | `opencode` | a plugin of its own, `~/.config/opencode/plugins/kraal.js` |
| Kilo | `kilo` | a plugin of its own, `~/.config/kilo/plugin/kraal.js` |
| droid | `droid` | `~/.factory/hooks.json` (the hooks of `settings.json` when they are kept there) |
| CodeBuddy | `codebuddy` | hooks in `~/.codebuddy/settings.json` |
| Kimi Code | `kimi` | `[[hooks]]` tables in `~/.kimi-code/config.toml` |
| pi (0.84.4 or later) | `pi` | an extension of its own, `~/.pi/agent/extensions/kraal.ts` |
| Codex | `codex` | `notify` in `~/.codex/config.toml`: the end of each turn |
| Cursor CLI | `cursor` | `~/.cursor/hooks.json`: the start and end of each turn |
| Augment | `auggie` | hooks in `~/.augment/settings.json` that run a script of kraal's own, `~/.augment/kraal-hook.sh`: tool calls and the end of each turn |

Codex's, Cursor's and Augment's hooks say nothing when the agent asks you something, and Gemini
CLI's nothing when you decline a tool or cancel one: that still comes from the screen. The other
agents have no hook that tells their state; theirs always comes from the screen. A turn that
finished stays done until you look at the pane, even when the agent then says it waits for a
prompt. A file kraal edits is copied next to it first (`.kraal-bak`), only kraal's entries are
added, and `kraal integration uninstall <agent>` takes out those alone. A file of kraal's own is
written and removed whole; one of the same name that kraal cannot read, or that is not kraal's,
is left alone, and one you edited is copied next to it before it changes or goes. Agents read
their hooks when a session starts, and Gemini CLI runs them only in folders you trust. `all`
covers Claude Code and Codex, and each other agent whose directory exists (`~/.gemini`,
`~/.copilot`, …).

The hooks do nothing outside kraal. Some agents run another agent's hooks (Cursor and Continue
run Claude Code's): kraal ignores a hook for an agent other than the one running in its pane. An
agent whose hooks tell every state (those of the table but Gemini CLI, Codex, Cursor CLI and
Augment) is followed even under a wrapper kraal does not recognise (`bash -c 'opencode; …'`):
until its hooks say the session ended, or, for OpenCode and Kilo, whose plugins cannot say it,
until the process the plugin runs in ends.
States are shown as coloured dots, with the state written
next to them in the sidebar and in pane frames: yellow working, red blocked (waiting for you), green
done (not seen yet), `○` idle. A blocked or finished agent you are not
looking at triggers a desktop notification through your terminal (OSC 777/9) or `notify-send`.

The agents of kraal sessions on your servers are listed too, after the server's name, and
`alt+a` goes to them as well: see [Agents on your servers](servers.md#agents-on-your-servers).
`kraal agents` lists them with their server, and their changes of state are `agent.*` events with
a `server` field, for scripts and hooks: see [Scripting](scripting.md).

When the server restarts, panes that ran Claude Code or Codex with a known session resume it
(`claude --resume <id>`, `codex resume <id>`), with the options they were started with. A prompt
given on the command line is never sent again: the pane then shows the command that resumes,
without the prompt, and waits for Enter. A one-shot run (`claude -p`, `codex exec`) is not
resumed: it waits for Enter as any other command does. When kraal cannot tell an option's value
from a prompt (an option it does not know), the pane also shows the command and waits: Enter runs
it as shown. The same goes for Enter on an ended agent and for an account switch.

## Worktrees

Panes in a git repository show their branch in their frame (`⎇ main*? ↑2`: `*` changes, `?`
untracked files, commits ahead).

In the agent picker (`ctrl+b a`), select an account and press Tab, then **Start in worktree**: kraal creates a branch
`kraal/<name>` and a directory next to the repository (`<repo>.worktrees/<name>`), and starts the
agent there in its own tab. Several agents can work on the same repository without stepping on each
other. `ctrl+b R g` in that tab reviews the changes (your pager, delta included), commits them, opens
a pull request (`gh`), merges into the base branch or removes the worktree. Removing says first
what would be lost (uncommitted changes, commits the base does not have, ignored files of its
own) and which panes it closes. Account rules written for the repository also apply in its
worktrees.

| Command | |
|---|---|
| `kraal wt new fix-login --agent claude` | the same from the command line (without `--agent`, the worktree alone); its last line is the id of the agent's pane |
| `kraal wt new fix-login --base main` | from that branch instead of the one checked out |
| `kraal wt setup` | shows the repository's `.kraal/worktree.toml` as a new worktree would get it (`--base main`: as that branch has it) |
| `kraal wt trust` | approves that file as it is now, for new worktrees to be prepared from it (see Preparation below) |
| `kraal wt list` | this repository's worktrees, with each one's number |
| `kraal wt rm fix-login` | removes one with its branch; refuses if it would lose changes, unmerged commits or ignored files, or while panes work in it |
| `kraal wt rm fix-login --discard-ignored` | removes it with ignored files of its own |
| `kraal wt rm fix-login --force` | removes anyway, closing the panes that work in it |
| `kraal wt prune` | removes those merged into their base and clean |
| `kraal wt prune --dry-run` | only says which ones would go |
| `kraal wt prune --discard-ignored` | also those whose only leftovers are ignored files |

A worktree's name takes letters, digits, `-`, `_` and `.`. The agent kraal starts in one is
Claude Code or Codex, the two it has accounts for; any other runs there once you start it.

**Preparation.** A new worktree has no `.env` and no `node_modules`. A repository says how to
prepare its worktrees in `.kraal/worktree.toml`, read from the commit the worktree starts from:

```toml worktree
copy = [".env*", "config/master.key"]   # from the main checkout, when they are there
setup = ["npm ci", "npm run db:migrate"]
```

`copy` takes paths in the repository, with `*` and `?` in the last part; a directory is copied
whole, without the links in it. A file already in the worktree stays as it is, and nothing is
copied through a link out of the repository, nor written through one. `setup` runs in the new
worktree, in the agent's pane, before the agent starts; the first command that fails stops the
setup, and the agent then waits for Enter. With `kraal wt new`, the setup runs in the terminal
you typed it in: a command that fails there fails `kraal wt new`, the worktree stays, and the
agent is not started.

This file comes with the repository, and someone else may have written it. Like a repository's
[runbooks](runbooks.md#repository-runbooks), kraal acts on it only once you have approved that
version of it: it copies none of the files it names and runs none of its commands before, and it
needs your approval again when the file changes. Approving is an act of its own:

- The agent picker shows what the file copies and every line of every command it runs, and asks
  whether to trust it, before the worktree is made. A long one
  [scrolls](keys.md#lists-fields-and-questions), and Cancel is the button selected.
- In a terminal, `kraal wt new fix-login` shows the same and asks, before anything is made.
- `kraal wt trust` approves the file as it is now, without a question: for scripts, and once you
  have read it (`kraal wt setup`).

Whatever kraal shows of the file has its control and invisible characters written out (`\u{1b}`,
`\r`), so what you approve is what runs. `--yes` asks nothing and approves nothing: with it, or
without a terminal, a file that is new or changed is refused, nothing is made, and the message
names the commands above. `--no-setup` makes the worktree without any preparation (the file is
not even read). Your own `copy` and `setup`, for every repository, go in `[worktrees]` in the
config; they need no approval, and run after the repository's.

To copy: a repository's [worktree.toml](../examples/repository/.kraal/worktree.toml), your own
[`[worktrees]`](../examples/config/worktrees.toml), and
[agent-in-worktree.sh](../examples/scripts/agent-in-worktree.sh), which starts an agent in a new
worktree and tells you when it needs you.

**Ports.** Each worktree has a number in its repository (1, 2…), kept with the worktree whatever
it checks out (another branch, a detached HEAD) and reused once the worktree is gone. Panes opened
in it get `$KRAAL_WORKTREE` (its name), `$KRAAL_WORKTREE_INDEX` and `$KRAAL_PORT_OFFSET` (the
number times `port_step`, 10 by default), and `$PORT` when `[worktrees]` sets `port_base`, even
when `kraal run` opens them with `--cwd` in a worktree the session has not seen yet, so the same
dev server can run in several worktrees at once:
`npm run dev -- --port $((5173 + KRAAL_PORT_OFFSET))`. The sidebar's PORTS section names the
worktree a port is served from.

**Cleanup.** `ctrl+b R g` › Remove merged worktrees, from any checkout of the repository, removes
in one go the worktrees whose work is all in their base, after saying which panes it closes and
why the others stay. `kraal wt prune` does the same, keeping the worktrees panes of the session
work in. A worktree is merged when all of these hold:

- it has no uncommitted change and no untracked file;
- each commit of its branch (and of a detached HEAD in it) is in the history of its base, or, for
  a branch merged by squash, rebased or cherry-picked, merging the branch into its base would
  change nothing: every change the branch brings is in the base, the same. A change the base made
  differently since, or undid, means the branch is not merged, and stays. Update the base first
  (`git pull` on it) for a pull request merged on the server to count. This takes git 2.38 or
  later; with an older git, only commits in the base's history count;
- it has no ignored file (`.gitignore`) of its own: build output, a local database, a `.env` you
  changed. One the main checkout has, the same, at the same place (a file `copy` put there) is
  no loss. Remove merged worktrees lists such files and deletes them once you agree; `kraal wt
  prune` keeps such a worktree unless given `--discard-ignored`.

The branch is then deleted only at the commit that was checked, and only while its base is
where it was: a commit made meanwhile (in another terminal) keeps the branch. Neither ever forces
a removal.

## Accounts

Each account has a name and a sign-in home for an agent (`CLAUDE_CONFIG_DIR` for Claude Code,
`CODEX_HOME` for Codex). kraal never reads or stores
credentials; it chooses which directory a pane starts with and reads the usage the agents report.

Accounts are for Claude Code and Codex. A second Claude subscription, named `pro`, in a new
directory where kraal then runs `claude auth login`:

```sh
kraal accounts add claude pro
```

| Command | |
|---|---|
| `kraal accounts add codex work` | the same for Codex, with `codex login` |
| `kraal accounts add claude team --home ~/claude-team --no-login --threshold 80` | in a directory you choose (`~/.local/share/kraal/accounts/claude-team` otherwise), without logging in now, with a threshold of its own |
| `kraal accounts` | every account: sign-in status, 5-hour and weekly usage (`kraal accounts list --json` for scripts) |
| `kraal accounts review claude` | identifies an existing sign-in marked Review, without another login |
| `kraal accounts login claude:pro` | logs an account in again |
| `kraal accounts rename claude:pro work` | renames it: rules and usage follow, the directory stays |
| `kraal accounts rm claude:work` | forgets it; its directory is kept |
| `kraal accounts limits --threshold 85 --mode ask` | sets the threshold and what happens there (below) |
| `kraal agent claude` | starts Claude Code on the account the rules pick, in a new pane whose id it prints (in this terminal outside kraal) |
| `kraal agent codex --account work --right` | on that account, in a split on the right (`--down`, `--floating` and `--tab` place it too; `--here` runs it in this terminal even inside kraal) |
| `kraal agent claude -- --model opus` | what follows `--` goes to the agent |

When the agent verifies its existing sign-in in `~/.claude` or `~/.codex`, kraal adopts it as
**Personal**, or uses a public identity returned by the agent, such as Claude's email. You can
rename it like any other account. No login is started and no credentials are copied. An absent
agent or an unsigned home adds no account. **Could not verify** offers **Check again**, without
starting a login; on the command line, run `kraal accounts list` again.

Accounts opens immediately. Discovery checks Claude and Codex independently in the background;
each result appears as soon as it finishes. **Checking sign-in** is still pending: wait for
**Review** before choosing which account it is. Opening a verified account or its Review does
not run another status command. Reopening Accounts checks unsigned standard homes again, so
a sign-in completed outside kraal can be found; an uncertain result waits for **Check again**.

If named accounts already exist, the existing sign-in appears as **Existing Claude account ·
Review** (or Codex). Open it in Accounts (`ctrl+b A l`), click Review in the sidebar, or use
`kraal accounts review claude`. **Which account is this?** lists your named accounts and
**A different account**. Choose one, then confirm **Use account**. Cancel is selected first.
Running agents keep their process and home. New launches use the selected named account's
home. The account appears once, and its latest usage observation is used without adding usage
from two homes. No home or credential is deleted. Choosing **A different account** asks for its name and keeps the
existing home as a separate named account. The command-line review keeps its proposed name. The answer is saved, so Review does not return.
Two names for the same canonical home are joined automatically; an email alone never joins
accounts.

`kraal accounts add` also adopts an unlisted verified sign-in when there are no named accounts.
An explicit `--home` with a verified sign-in is adopted without login, even without `--no-login`.
Adding another name for a listed home resolves to that same account. Use Rename to change its
visible name. Old `default` references in commands, rules and saved sessions continue to resolve.

New Claude accounts share
`CLAUDE.md`, `settings.json`, skills, plugins, memory and conversations with it, so a conversation
can continue on another account; Codex accounts share nothing, as Codex keeps databases in its home.
An agent kraal starts on an account gets `$KRAAL_ACCOUNT` (`claude:pro`) and an internal
`$KRAAL_ACCOUNT_ID` that stays the same when renamed. It does not inherit the API keys of
your environment (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`…), which would bill another account.

Open **Accounts** from the Agents menu (`ctrl+b A l`), or click the sidebar's **ACCOUNTS**
heading. Each account has two lines: `Claude · work` and `57% used · 2 panes`. Unknown usage
says **Usage unavailable**, a login still to verify says **Sign-in required**, and expired
observations say **Last checked ...**. Enter or one click opens details with the usage windows,
when they were checked and their resets. Zero panes are omitted. In a narrow sidebar, **No usage**
and **Checked ...** keep the second line readable; clicking either line opens those details.
Tab opens **Rename**, **Sign in**, and **Remove** (`ctrl+r`, `ctrl+l`, `ctrl+x`). Remove asks first, with Cancel selected, and explains that conversations and the
agent's own sign-in are kept.
Enter or Esc in details returns to the list with that account selected. Background account
checks keep the list's search and selected row while updating its entries.

**+ Add account** opens one form, also reached directly with `ctrl+b A n`. It stays visible when
filtering the list. **Agent** starts with the focused pane's agent, otherwise the last choice;
Left/Right changes it. When an existing sign-in needs review, a one-line explanation and the
buttons **Use existing account**, **Sign in to another account**, and **Cancel** come before
any name field. **Use existing account** opens Review; it does not adopt the sign-in. Choosing
**Sign in to another account** opens the name field for a new sign-in. A name for an existing
sign-in uses the public identity reported by the agent when available and not already taken.
Otherwise **Name** is empty with the hint **e.g. Work or Personal**. Enter on an empty name asks
for a name and changes nothing. No numbered Personal name is proposed by the form.
Tab moves between fields and buttons; arrows choose a button. Buttons stay together, with a
border and a `>` marker on the selected action. A missing agent says **Not installed** and explains
how to retry. An error keeps both fields; long errors wrap and scroll with PageUp/PageDown or
the wheel. Enter or one click on a button submits it. Successful addition returns to Accounts,
selects the new account and says **Account added**. These screens also work at 80x24 without
the sidebar.
Esc or **Cancel** in the form returns to Accounts. While **Checking sign-in...** is shown,
the form accepts cancellation but no second submission. A cancelled check cannot later replace
the screen you are using with its result.

The launch picker (`ctrl+b a`) lists installed agents' accounts that do not require sign-in.
Enter or one click starts the selected account. Tab offers **Start in new tab** (`ctrl+t`),
**Start in worktree** (`ctrl+w`) when the focused pane is in a local repository, and **Account
details** (`ctrl+d`). Worktree launches keep the selected account. Account management is in
Accounts.

The name is a label: renaming keeps the same account, rules and usage. Its directory stays where
it is, and running agents go on under the new name. Removing an account keeps the label and
launch environment of panes that use it. Adding another account with that name does not attach
those panes, saved sessions, workspace bindings, rules or cached usage to the replacement.
A workspace bound to a removed account asks you to choose an account again.

Click the account name in a pane's frame, the bottom bar's **switch account**, or press
`ctrl+b A c` to open **Change account** for that pane. It lists accounts of the same agent.
In that picker, Enter switches to the selected account; launch, login,
rename and remove actions are not offered. The pane's account is marked `current`: selecting
it closes the picker and leaves the agent running as it is, without restarting it. An exited
pane stays exited too. Another name for the same account directory also leaves it unchanged.

Each destination says **Continue conversation**, **New conversation**, **Sign-in required**, or
**Could not verify**. Sign-in checks run while the picker is open and again when you choose an
account. An account that needs sign-in offers **Sign in**; the current agent keeps running.
After signing in, choose the account again. If verification fails, the current pane is unchanged;
**Check again** retries it. A failure shown while another dialog is open can be retried from
**Change account**.

With reliable hooks, a change requested during work says **Switch after this turn**. Open
`ctrl+b A c` again and choose **Cancel account change** to cancel it. Without a reliable turn
boundary, kraal asks **Stop and switch?**, with **Cancel** selected. Only one change can be
pending per pane: every attached client sees it and can cancel it before the agent stops.
Sign-in and the conversation are checked again after waiting, before stopping anything.

A Claude conversation must have a readable transcript on the current account. If it is missing,
empty, unreadable, or has diverged from the destination's copy, kraal refuses and says why, leaving the
agent running. This also applies before Claude has written its first transcript: let it finish
a turn before changing accounts. Codex changes start a **New conversation**, stated in both the
picker and the confirmation; the original history stays in the original account's directory.
Kraal needs its conversation id and saved history before stopping Codex. If the id is not yet
known, finish a turn with kraal's integration enabled, then try again.

After the replacement starts, **Using <name> · Undo** offers a clickable return. The previous
account is also kept in the pane's actions: open **Find a pane** (`ctrl+b p`), select the pane,
press Tab and choose **Undo account change**. This remains available after the message expires
and after restoring a saved session. Claude returns with the latest conversation, subject to
the same transcript, sign-in and interruption checks. Codex reopens the original conversation
in its account and keeps the newer conversation's files in the other account. Undo does not
revert changes the agent made to your working files. A successful Undo clears that return action;
a later account change replaces it with the most recent source.

A failure after stopping triggers one attempt to resume the previous account. Kraal confirms
success only after observing the replacement agent running, and says **Resumed on <name>** if
recovery succeeds. If recovery also fails, the pane keeps the previous command and Enter retries
it. The message only says the conversation is saved when its history can be verified.

Claude and Codex started by hand in a shell can also change accounts when kraal can identify
the foreground process, account home, saved conversation and exact resume arguments. A session
selector or a hook from that process supplies the conversation id. An opaque wrapper, credential
override, unknown conversation or ambiguous command instead shows **Can't switch this process.**
and offers **Open with another account**. That opens another pane and leaves the original running.
Shells with other jobs are not stopped for an account change.

Sign-in from Accounts opens a dedicated login pane. Opening the same account's login again
in that session focuses the existing pane, including through another account name for the same
directory. After the login exits successfully, kraal checks with `claude auth status` or
`codex login status`, in that account's environment. A confirmed sign-in closes the pane and
says that the account is signed in. When adding, it returns to the new row in Accounts. A failed or cancelled sign-in stays open: Enter closes it,
and `r` is the separate **Try again** action. While a check runs, Enter also closes the pane;
its late result is ignored. `c`, **Check again**, retries an unsuccessful check without running
the login again. A check waits at most ten seconds and discards the agent's output.

A lock in the account directory also prevents another kraal session or `kraal accounts login`
from starting the same login while it is running or being checked.

The account entry is kept in `accounts.toml` if sign-in fails, is cancelled, or cannot be
verified. Until verification succeeds it shows **Sign-in required** in Accounts and the sidebar,
even if it has usage from before. Login panes and their scrollback are never saved, including
when saving a workspace layout. Their output, titles, terminal notifications and clipboard
requests are not copied into kraal's messages, notifications or logs.

`kraal accounts` uses the same check: **signed in**, **sign-in required**, **not installed**,
or **could not check sign-in**. A timeout or unexpected failure is **could not check sign-in**,
not a missing agent. JSON keeps `logged_in` (`true`, `false`, or `null`) and adds `status`:
`signed_in`, `not_signed_in`, `not_installed`, or `could_not_check`.
The command-line add and login commands also verify sign-in after the login succeeds.

**Usage.** Claude Code passes the 5-hour and weekly usage to its status line;
`kraal integration install claude` puts a small status line in front of yours that records it,
then runs yours unchanged. Codex accounts are asked through Codex's own app-server every few
minutes (`kraal accounts refresh` asks now). Limit messages on screen ("You've hit your session
limit") count too.

**Rules and thresholds** live in `~/.config/kraal/accounts.toml` (`kraal accounts edit`):

```toml accounts
[limits]
threshold = 90        # percent of the 5-hour or weekly window
mode = "ask"          # notify | ask | auto

[[rule]]
workspace = "pro"     # or: path = "~/Dev/client"
agent = "claude"
accounts = ["pro", "Personal"]  # in order of preference
```

The file also holds the accounts themselves, one `[[account]]` each (`name`, `agent`, `home`,
an internal `id`, and a `threshold` of its own if it has one), which `kraal accounts add` writes. A login that has
not been verified also has `sign_in_required = true`; successful verification removes that flag.
Pending reviews have `review = true` and may keep a `public_identity` supplied by the agent.
Former names, identities and homes are stored in `[[account.references]]` under their owner,
including notes and unknown keys. Old files with separate `linked_to` account entries are
compacted on their next rewrite, with the original kept in `accounts.toml.kraal-bak`. Once a
review is resolved there is one `[[account]]` per account in the list. References whose account
was removed have no owner; they are kept in `[[forgotten]]` to prevent rediscovery and identity
reuse, not as account entries. Keep these generated fields. No home or credential is removed.
Older kraal reads the ordinary `name`, `agent` and `home` fields and ignores these reference
fields. It cannot resolve a former name from them and may rediscover a standard home joined to
another home. See [Account compatibility](compatibility.md#account-identities).
kraal edits what it changes there and leaves the rest as you wrote it, including comments, order
and unknown keys. A new file includes an explanatory header and omits empty rules and an empty `[claude]`
table. When kraal next rewrites an older file, it removes an empty top-level `rule = []` and an
empty `[claude]` table, keeping their comments. A `[claude]` table with a saved `statusline`
command or other settings stays, as do existing rules. A file that cannot be parsed is left
untouched. Older files without identifiers work unchanged. On the next rewrite, kraal adds
identifiers and binds existing rules to them, keeping a first backup as `accounts.toml.kraal-bak`.
Before changing accounts, it also binds readable closed sessions and workspace definitions in
the configured directories. Rules accept bare names and `agent:name`; kraal writes resolved
references as `agent:@id`. Workspace files keep the readable `agent="claude:name"` and add
`account_id` when bound or saved. These are generated details: keep them when editing a label;
use names when writing a new rule or choosing a different account in a workspace, removing that
pane's old `account_id`. No command or picker requires an identifier.
[examples/accounts.toml](../examples/accounts.toml) has limits and two rules, by workspace and by
directory.

The first rule that matches decides. A new agent starts on the first account of that rule whose
known usage is under its threshold. Otherwise it tries the first account with unknown usage,
or, when all have known usage over the threshold, the one whose usage resets first. When
the matching rule names no account that exists any more, no account is used there (rather than
any). Without a matching rule, every confirmed account of the agent may be used, in list order. When
the account of a running agent reaches its threshold, kraal notifies you (`notify`) with
**<name> is at NN% used · Change account**. Click **Change account** in kraal's notification to
open that pane's account picker, even when another pane has focus. In `ask` mode it offers to
continue the conversation on another account once the agent finished its turn. In `auto` mode
it does so by itself at the end of the turn. `auto` only switches when the agent's own hooks said
its turn ended, which Claude Code's do (`kraal integration install claude`) and Codex's cannot;
otherwise kraal asks instead, since restarting an agent mid-turn would lose its work. Limits are
watched for agents kraal started on an account and for shell launches it can associate safely. A Claude conversation continues on the other account; a
Codex one cannot (its history stays in its account's directory), so switching starts a new one.
Threshold alternatives need known usage below their threshold; unknown usage is not zero.
Accounts marked as needing sign-in are excluded. An automatic change also checks sign-in and
the conversation before stopping anything, and needs confirmation for a new Codex conversation.
A failed automatic attempt is not repeated on each check: choose an account manually to retry,
or wait until usage falls below the threshold and reaches it again.
Workspace files can say `agent="claude"` (rules apply) or `agent="claude:pro"`.

`auto` is off by default. Providers set usage limits per subscription; check their terms (for
Anthropic, the Usage Policy's section on bypassing product restrictions) before letting a tool
move your work between subscriptions automatically.
