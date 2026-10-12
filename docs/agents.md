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

For exact states, install kraal's hooks in the agent:

```sh
kraal integration install claude   # adds hooks to ~/.claude/settings.json (backup kept)
kraal integration install gemini
kraal integration install all      # Claude Code, Codex, and every agent below used here
kraal integration status
```

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
agent whose hooks report is followed even under a wrapper kraal does not recognise
(`bash -c 'opencode; …'`): until its hooks say the session ended, or, for OpenCode and Kilo,
whose plugins cannot say it, until the process the plugin runs in ends.
States are shown as coloured dots, with the state written
next to them in the sidebar and in pane frames: yellow working, red blocked (waiting for you), green
done (not seen yet), `○` idle. A blocked or finished agent you are not
looking at triggers a desktop notification through your terminal (OSC 777/9) or `notify-send`.

The agents of kraal sessions on your servers are listed too, after the server's name, and
`alt+a` goes to them as well: see [Agents on your servers](servers.md#agents-on-your-servers).
`kraal agents` lists them with their server, and their changes of state are `agent.*` events with
a `server` field, for scripts and hooks: see [Scripting](scripting.md).

When the server restarts, panes that ran an agent with a known session resume it
(`claude --resume <id>`, `codex resume <id>`). A prompt given on the command line is never sent
again, and a one-shot run (`claude -p`, `codex exec`) is not resumed. When kraal cannot tell an
option's value from a prompt (an option it does not know), the pane shows the command and waits:
Enter runs it as shown. The same goes for Enter on an ended agent and for an account switch.

## Worktrees

Panes in a git repository show their branch in their frame (`⎇ main*? ↑2`: `*` changes, `?`
untracked files, commits ahead).

The agent picker (`ctrl+b a`) offers to start an agent **in a new worktree**: kraal creates a branch
`kraal/<name>` and a directory next to the repository (`<repo>.worktrees/<name>`), and starts the
agent there in its own tab. Several agents can work on the same repository without stepping on each
other. `ctrl+b R g` in that tab reviews the changes (your pager, delta included), commits them, opens
a pull request (`gh`), merges into the base branch or removes the worktree. Removing says first
what would be lost (uncommitted changes, commits the base does not have, ignored files of its
own) and which panes it closes. Account rules written for the repository also apply in its
worktrees.

```sh
kraal wt new fix-login --agent claude   # the same from the command line
kraal wt list                      # with each worktree's number
kraal wt rm fix-login              # refuses if it would lose changes, unmerged commits or ignored files
kraal wt rm fix-login --force      # removes anyway
kraal wt prune                     # removes those merged into their base and clean (--dry-run)
kraal wt prune --discard-ignored   # also those whose only leftovers are ignored files
```

**Preparation.** A new worktree has no `.env` and no `node_modules`. A repository says how to
prepare its worktrees in `.kraal/worktree.toml`, read from the commit the worktree starts from:

```toml
copy = [".env*", "config/master.key"]   # from the main checkout, when they are there
setup = ["npm ci", "npm run db:migrate"]
```

`copy` takes paths in the repository, with `*` and `?` in the last part; a directory is copied
whole, without the links in it. A file already in the worktree stays as it is, and nothing is
copied through a link out of the repository, nor written through one. `setup` runs in the new
worktree, in the agent's pane, before the agent starts; the first command that fails stops the
setup, and the agent then waits for Enter. These commands come from the repository: like its
runbooks, they run only once you have approved that version of the file (kraal shows them and
asks again when the file changes), with control and invisible characters written out (`\u{1b}`,
`\r`), so what you approve is what runs. `kraal wt new` asks in the terminal; `--yes` approves,
`--no-setup` skips the preparation (the file is not even read). Your own `copy` and `setup`, for
every repository, go in `[worktrees]` in the config; they run without asking, after the
repository's.

**Ports.** Each worktree has a number in its repository (1, 2…), kept with the worktree whatever
it checks out (another branch, a detached HEAD) and reused once the worktree is gone. Panes opened
in it get `$KRAAL_WORKTREE` (its name), `$KRAAL_WORKTREE_INDEX` and `$KRAAL_PORT_OFFSET` (the
number times `port_step`, 10 by default), and `$PORT` when `[worktrees]` sets `port_base`, even
when opened with `kraal run --cwd` in a worktree the session has not seen yet, so the same dev
server can run in several worktrees at once:
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

Each account is a separate config directory for an agent (`CLAUDE_CONFIG_DIR` for Claude Code,
`CODEX_HOME` for Codex), logged in with the agent's own login. kraal never reads or stores
credentials; it chooses which directory a pane starts with and reads the usage the agents report.

```sh
kraal accounts add claude pro          # new directory, then `claude auth login` in it
kraal accounts add codex work          # new directory, then `codex login` in it
kraal accounts                         # every account: logged in?, 5-hour and weekly usage
kraal agent claude                     # start Claude on the account the rules pick
kraal agent codex --account work --right
kraal accounts limits --threshold 85 --mode ask
kraal accounts rename claude:pro work  # rules and usage follow, the directory stays
```

Your existing setup is the `default` account of each agent. New Claude accounts share
`CLAUDE.md`, `settings.json`, skills, plugins, memory and conversations with it, so a conversation
can continue on another account; Codex accounts share nothing, as Codex keeps databases in its home.

The agent picker (`ctrl+b a`) lists every account; Tab on one shows what can be done with it: start
it, log in again, rename it or remove it. Renaming changes the rules that name the account and
keeps its usage; its directory stays where it is, and agents running on it go on, shown under the
new name. A rule for every agent that also names another agent's account of the old or the new
name stays theirs: the renamed account gets a rule of its own just before it. `default`, the
agent's own home, keeps its name. The sidebar lists every account at the bottom, with its usage
once some is seen.

**Usage.** Claude Code passes the 5-hour and weekly usage to its status line;
`kraal integration install claude` puts a small status line in front of yours that records it,
then runs yours unchanged. Codex accounts are asked through Codex's own app-server every few
minutes (`kraal accounts refresh` asks now). Limit messages on screen ("You've hit your session
limit") count too.

**Rules and thresholds** live in `~/.config/kraal/accounts.toml` (`kraal accounts edit`):

```toml
[limits]
threshold = 90        # percent of the 5-hour or weekly window
mode = "ask"          # notify | ask | auto

[[rule]]
workspace = "pro"     # or: path = "~/Dev/client"
agent = "claude"
accounts = ["pro", "default"]   # in order of preference
```

A new agent starts on the first account of the matching rule that is under its threshold; when
the matching rule names no account that exists any more, no account is used there (rather than
any). The name `default` is the account in the agent's own home (`~/.claude`, `~/.codex`). When
the account of a running agent reaches its threshold, kraal notifies you (`notify`), offers to
continue the conversation on another account once the agent finished its turn (`ask`), or does it
by itself at the end of the turn (`auto`). `auto` only switches when the agent's own hooks said
its turn ended (`kraal integration install`); without them, kraal asks instead, since restarting
an agent mid-turn would lose its work. A Claude conversation continues on the other account; a
Codex one cannot (its history stays in its account's directory), so switching starts a new one.
Workspace files can say `agent="claude"` (rules apply) or `agent="claude:pro"`.

`auto` is off by default. Providers set usage limits per subscription; check their terms (for
Anthropic, the Usage Policy's section on bypassing product restrictions) before letting a tool
move your work between subscriptions automatically.
