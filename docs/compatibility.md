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
