# Compatibility between versions

## Saved files and sessions

Kraal 0.4 migrates sessions and `accounts.toml` from **0.2.0** and **0.3.0** once,
keeping a copy before conversion. This migration belongs to 0.4 and is removed in **0.5.0**.
Existing workspace KDL and configuration files keep working.

The unreleased `[keys.prefixed]` table is ignored with an unknown-key warning; use
`[keys.leader]` for bindings after the prefix. The unreleased session format `version: 1,
revision: 2` is refused with `unsupported state revision 2; use a newer kraal`, leaving the
saved session untouched. Both supported releases wrote revision 1.

Each save writes the current state format only. Kraal does not also write a downgraded copy
for older versions. Migration backups and the untouched legacy session are recovery copies,
not synchronized views of the current session.
Session migration preserves the personal layout, identities, source links, account references,
restore failures and available scrollback. Account migration happens on the next change to the
file, keeping comments, order and unknown keys. Before account changes, saved pane references
are bound to account identities, including layout Undo in kept and deleted workspaces. Unknown
session fields are preserved. See [Workspace migration](workspaces.md#workspace-identities-and-migration)
and [Accounts](agents.md#accounts).

A kraal older than the saved state does not read that state. Starting with 0.4, sessions use
`sessions-v2/<session>.json` and `scrollback-v2/<session>/` in the kraal state directory.
Kraal 0.2.0 and 0.3.0 never open these paths. Going back to either finds the session as it was
at upgrade, or no saved session if there was none before. Its saves and `kraal forget` cannot
overwrite the newer session or its scrollback. Changes made after upgrade do not appear in the
older kraal; changes made while downgraded do not replace the newer state when returning to 0.4.

From 0.5.0, state older than 0.4 will be left untouched and refused with this next step:
`run kraal 0.4 once first`. The one-way session and account migration is removed in 0.5.0.
A state from a newer format is also left untouched, with a message to use a newer kraal.

## Clients and servers

Clients and servers of different versions keep working together in both directions, from
0.2.0 on. Updating kraal does not force a running session to restart. Attaching, commands,
scripts and agent hooks continue to use the running server. This protocol contract is unchanged.

A feature an older server cannot provide is refused before the action, with the reason and
how to restart onto the installed version:

```
$ kraal buffer list
kraal: session main runs kraal 0.2.0, which cannot keep paste buffers: `kraal -s main restart` updates it (its layout and scrollback are kept)
```

`kraal ls` shows the server version. `kraal restart` saves and restores the session using the
installed binary ([Persistence](workspaces.md#persistence)). An older server continues its own
behavior until restarted: a new client cannot add file protections or lifecycle behavior to it.
A restart from 0.2.0 ends attached terminals; attach again afterwards. Later clients wait for
the replacement server.

The running server determines the behavior. On a current server, attached clients, including
0.2.0 and 0.3.0 clients, can use the current lists, forms and actions through the existing
terminal-input protocol. This includes workspace Close, Delete, Restore, Duplicate, ordering,
source review and pane editing, as well as account, server and tunnel forms. Renaming a workspace
leaves KDL files unchanged. Closed layouts and recovery records stay in the current session
state; the legacy session is not rewritten to resemble them.

CLI features that need newer requests are checked separately:

| Feature | Compatibility behavior |
|---|---|
| `kraal import` and `kraal export` | Require the server's `workspace_files` capability; older servers refuse with restart guidance. |
| Workspace Create, Rename, Close, Delete, Restore and deleted listing | Require `workspace_lifecycle` when talking to a running server. |
| `kraal save` | Keeps its existing request, name rules and explicit account references. Current servers treat it as legacy KDL export and refuse unconfirmed replacements. |
| `kraal open`, workspace listing, pane Close and Rename | Keep their existing requests and meanings. Open does not switch an attached terminal's view. |

Existing `rename_pane` key bindings still rename; the default `ctrl+b P e` opens Edit pane.
The existing `[servers] auto_add` boolean now controls discovery suggestions on a current server;
only explicit Add saves an unknown server. Previously saved entries remain usable. Tunnels keep
the wire states `connecting`, `up` and `down`; the current UI labels `up` as **Ready**. Their CLI
still chooses an available local port when `--local` is omitted.

New optional fields in the current session format retain deleted workspaces, source review,
layout Undo and workspace order. Missing fields retain their older defaults. These additions
never put current state into the 0.2.0 or 0.3.0 paths. See [Workspaces](workspaces.md) and
[Accounts](agents.md#accounts) for the retained state and its limits.

Clipboard messages and results use the optional `clipboard` capability in both peers' lists.
A current server sends either a structured copy to a capable client or legacy OSC 52 bytes to
an older client, never both. A new client applies native policy only after the server advertises
the capability; with an older server it retains the legacy terminal path. Clipboard `off`
requires the capability and otherwise refuses attach with restart guidance. The optional
`[clipboard]` settings and `clipboard_native` server field default to native user copies with
terminal fallback and no native program permission. Existing configurations keep working.
Clipboard request IDs, pending results and policies add no saved-session fields; version 2
state and the 0.2.0 and 0.3.0 legacy paths remain unchanged.

Attachment offers, start, chunk, acknowledgement, finish, cancel, local-path and result/status
and result dismissal messages require the `attachments` capability in both peers' lists. Older clients keep their
existing input behavior; choosing Attach files on a current server tells them to update the
client and attach again. Older servers keep their existing palette until restarted. No new
message is sent to an older server. Unknown messages retain the existing fallback behavior.
The optional `[attachments]` configuration defaults preserve existing configurations. Files and
result records use a separate `attachments-v1` directory. No saved-session, workspace KDL or
legacy state format changes are introduced.

Automatic pasted-path recognition additionally requires `pasted_paths` in both peers' lists.
Reservation requests, prepared replies and original-text submission use that capability.
An optional `pasted` field on attachment results defaults to false for older peers. Clients
without the capability retain ordinary paste bytes; no new requests reach an older server.
The laptop's `paste-files.toml` permissions are separate from configuration, workspace and
session formats. Existing 0.2.0 and 0.3.0 files and downgrade isolation remain unchanged.

Clipboard image offers additionally require `clipboard_images` and `attachments` in both
peers' lists. The new action tells older clients to update; older servers retain their existing
palette. Image data uses the existing attachment messages. Local image batches also use that
transport to create private session files. No configuration, workspace or saved-state format
changes are needed, and existing configurations add no default image shortcut.

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
- The protocol version changes only for what these rules cannot express: releases of different
  protocol versions refuse each other, and `kraal restart` and `kraal kill` still stop any server.

## The cross-version test

`tests/compat.sh` downloads 0.2.0 and 0.3.0 and checks their release checksums before running
`tests/compat.rs` against each real binary. The tests cover clients and servers in both
directions, migration with retained copies, and downgrade isolation, including saves and
Forget by the older binary. They also check that old workspace and configuration files keep
working. CI runs these tests on Linux and macOS; terminal attachment is tested on Linux only.

| Command | Releases |
|---|---|
| `tests/compat.sh` | 0.2.0 and 0.3.0 |
| `tests/compat.sh v0.2.0` | 0.2.0 only |
| `tests/compat.sh --build v0.2.0` | Built from its tag (`git fetch --tags` first) |

The releases in `RELEASES` in `tests/compat.sh` are checked against this page by the unit tests.
