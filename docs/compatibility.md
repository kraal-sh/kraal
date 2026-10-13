# Compatibility between versions

Updating kraal never forces a restart. A session keeps running the kraal that started it, and the
kraal installed since works with it: attaching, every command (`kraal run`, `send`, `read`,
`agents`, `events`…), scripts and agent hooks. The other way round works too: an older kraal (one
not updated yet, or the one on a server you attach to with `--host`) works with a session that a
newer kraal started.

This holds between any two releases from **0.2.0** on, and every change to kraal is tested against
0.2.0 and the latest release, in both directions, before it is merged. Sessions of 0.1 work too
but are not tested, and restarting one loses what its panes showed (0.1 does not save it).

## A session on an older kraal

What the session's kraal cannot do yet is refused before anything happens, with the reason and
the way out:

```
$ kraal buffer list
kraal: session main runs kraal 0.2.0, which cannot keep paste buffers: `kraal -s main restart` updates it (its layout and scrollback are kept)
```

`kraal ls` shows the kraal each session runs, and kraal says it when you detach from a session on
an older one. `kraal restart` restarts the session on the kraal installed now (see
[Persistence](workspaces.md#persistence)), and `kraal update` offers it for each session it leaves
on an older kraal ([Updates](install.md#updates)).

A few things do less instead of failing:

- `kraal agents` lists the session's own agents, without those of your servers' sessions, when the
  session cannot list those.
- When you restart a session that runs 0.2.0, the terminals attached to it end instead of waiting
  for the next server: run `kraal` again there. From later releases on, they come back by
  themselves.
- A session that runs 0.3.0 or older drops a `kraal events` reader that is too slow, or that reads
  nothing for ten seconds while events wait for it, without telling it: the stream ends as it
  does with the session ([Events](scripting.md#events)).

One thing an older kraal does not take part in: kraal now holds a session where it saves it, so
that a kraal which looks in another runtime directory does not start a second server on the same
files ([One server for what a session saves](workspaces.md#one-server-for-what-a-session-saves)).
Kraal 0.3.0 holds a session in its runtime directory only, and 0.2.0 not at all:

- **0.3.0, same runtime directory.** Both take the lock of that directory: neither starts nor
  forgets a session the other holds.
- **0.2.0, same runtime directory.** It takes no lock, so only a socket that answers keeps a
  second server or a `kraal forget` away, on both sides: a session whose server listens is not
  started again nor forgotten. A 0.2.0 started at the very instant another kraal starts the same
  session can still become a second server of it. And on macOS a server too busy to take a
  connection (its queue is full) looks like one that is gone, where Linux tells the two apart.
- **Another runtime directory.** Only two kraal that both hold the session where it is saved keep
  each other out. A 0.3.0 or 0.2.0 started with another `$XDG_RUNTIME_DIR` than the session's
  server starts a second server on its files, as it always did.
- **A restart.** `kraal restart` keeps the session from `kraal forget` for the moment it has no
  server. A `forget` of 0.3.0 or older does not know that: run at that very moment, it still
  deletes what the session saved.

## Workspace file replacements

The session format and workspace KDL remain readable by 0.2.0 and 0.3.0. A new optional
`definition_source` field records the contents last loaded or saved; older versions ignore it
and still read all the workspaces. Old snapshots without this field still restore their layout
and links, but require confirmation before replacing an existing source.

The save request has not changed. Older clients of a current server receive the same collision
and external-edit refusals as current clients. A server running 0.2.0 or 0.3.0 still uses its old
save behavior and cannot provide these protections. File locks coordinate current writers;
older servers and external editors do not take part. See [Workspace saves](workspaces.md#saving-without-replacing-someone-elses-layout)
for concurrent editor writes and recovery.

## Workspace identities

Snapshot revision 2 adds workspace identities and optional restore errors within the existing
version 1 `workspaces` array. Kraal 0.2.0 and 0.3.0 still read every workspace and its layout;
they do not interpret this state as an empty session. They ignore the new metadata and lose it
if they save the session again. On returning to this version, missing identities are assigned
again, with a separate migration backup.

Workspace list replies add an optional `id`. Old clients ignore it, and current clients accept
replies without it from old servers. Workspace selection still uses names; no request silently
asks an old server to select by an identity it cannot understand.

The conversion keeps an exact copy of the original snapshot and its available scrollback before
writing anything. See [Workspace identities and migration](workspaces.md#workspace-identities-and-migration)
for rollback and failed-state recovery. Whole-session restore failures now stop startup and leave
the original in place, rather than starting a new session that could overwrite it.

## Closed and empty workspaces

Saved sessions keep the version 1 `workspaces` array and revision 2. Optional `closed`, `empty`
and `close_pending` fields describe the new lifecycle; older snapshots without them still load.
Closed layouts stay in the ordinary array, including their scrollback references. An empty
workspace carries a shell fallback in `tabs`, which current kraal ignores when `empty` is true.
This prevents 0.2.0 and 0.3.0 from discarding its record as an unrestorable empty layout.

An older server ignores these fields: it reopens closed workspaces, gives empty workspaces a
shell, and cannot finish an interrupted Close. Saving from that older server loses the new
lifecycle metadata. The session is never read as empty merely because all its workspaces were
closed or empty. Use the current version to retain those states and complete crash recovery.

Workspace list replies keep the existing `open` boolean and counts. Closed saved workspaces
also have the optional stable `id`; file-only entries omit it. Old clients can reopen them by
name with the existing open request, and attached clients use the server's Close and Retry UI.
No new client capability is required.

## Account identities

Accounts and saved sessions from 0.2.0 and 0.3.0 still load. Account names remain accepted by
commands and older clients. New saved panes and protocol replies carry an optional account
identity alongside the readable label. Selecting by identity requires the `account_ids` server
feature, checked before sending, so an older server cannot silently select a reused name.

The next change to `accounts.toml` adds missing identities and binds rules, with the original
file kept in `accounts.toml.kraal-bak`. Comments, order and unknown keys stay. An unreadable file
is never replaced. Rules keep readable names in `accounts` for older servers and authoritative
references in `account_ids` for this version. A session still running an older binary continues
its old name-based behavior until restarted on this version; it does not protect against name
reuse. The backup can be restored to return to the original file.

Verified standard sign-ins become ordinary named accounts. Old `default` references still resolve,
including after a review links that home to a named account. Names for the same canonical home
resolve to one identity. The file keeps one
`[[account]]` per account after review. Its `[[account.references]]` fields retain old names,
identities and homes; panes keep their launch home when resumed. Usage uses the newest
observation across those references, never a sum. The next rewrite migrates old `linked_to`
entries into these fields, keeping their comments, notes and unknown keys. No credentials or
homes are removed. A forgotten reference has no owning account, so it is stored separately in
`[[forgotten]]` to prevent rediscovery and reuse of its identity.

Kraal 0.2.0 and 0.3.0 still read every ordinary account's `name`, `agent`, `home` and threshold.
They do not understand the reference fields: a field cannot teach an already released binary
to resolve a former name. On an older server, use the current account name. If the standard home
was joined to a different home, an older kraal can still show its implicit `default`; if the
standard home itself was named, it suppresses that implicit account and the former `default`
name is unavailable there. Old clients talking to a current server can still use these former
names. Rules saved by this version keep current readable names for older readers. Session and
workspace formats are unchanged; older readers still load their panes. These limits of older
account lookup do not justify extra `[[account]]` rows in the shared file.

A saved pane may also carry an optional `account_undo` with the previous launch and conversation.
Older saved panes still load without it. Older kraal versions ignore that return action and
continue to read the pane's ordinary account and session fields. Account switching uses the
existing attached-terminal input protocol, so older clients can use the same interface.

## For contributors: how the protocol changes

The client and the server exchange JSON messages (`src/protocol.rs`, whose comments hold the
details), and every change to them keeps both directions working:

- A new field is optional, with a default where it is read; a reader ignores the fields it does
  not know. A field never changes type or meaning, and a name is never reused.
- A new request is refused by name by a server that does not know it, and the client turns that
  into the message above: `Request::needs` names the feature it takes, which the server lists in
  its answer to a ping and to an attach. When an older server would take the request and quietly
  ignore a new option (`kraal edit --sudo`), the feature is checked before the request is sent.
- A server sends a new message type only to attached clients, a new kind of answer only to a new
  request, and a new value of an existing enum (an agent state) only to clients that list
  `lenient` among their features. Every message type, answer kind and enum value kraal does not
  know reads as a fallback (unknown, skipped), never as an error.
- What a restart saves must be readable by the next release, which restores it.
- The protocol version changes only for what these rules cannot express: releases of different
  protocol versions refuse each other, and `kraal restart` and `kraal kill` still stop any server.

## The cross-version test

`tests/compat.sh` downloads 0.2.0 and the latest release from GitHub, checks them against the
release's checksums, and runs `tests/compat.rs` with each: this kraal drives a session of theirs
(panes, agents and their hooks, run, send, read, keys, wait, events, attach and detach, what it
cannot do, kill, and a restart onto this kraal), and their commands drive a session of this kraal.
CI runs it on Linux and macOS (attaching in a terminal is tested on Linux only).

| Command | |
|---|---|
| `tests/compat.sh` | 0.2.0 and the latest release |
| `tests/compat.sh v0.2.0` | one release |
| `tests/compat.sh --build v0.2.0` | built from its tag instead (`git fetch --tags` first) |

Raising the oldest supported release means changing `OLDEST` in `tests/compat.sh` and this page;
`cargo test` fails while the two differ.
