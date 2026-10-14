# Compatibility between versions

## Saved files and sessions

Kraal 0.4 migrates sessions and `accounts.toml` from **0.2.0** and **0.3.0** once,
keeping a copy before conversion. Existing workspace KDL and configuration files keep working.
Session migration preserves the personal layout, identities, source links, account references,
restore failures and available scrollback. Account migration happens on the next change to the
file, keeping comments, order and unknown keys. See [Workspace migration](workspaces.md#workspace-identities-and-migration)
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

Workspace Add and Rename use the attached terminal's existing input protocol. No new request or
saved-state field is needed. A workspace's name is independent of its source path; older versions
already read these separately. Renaming on a current server leaves every KDL file unchanged.
A session still running 0.2.0 or 0.3.0 retains that server's old rename behavior.

Delete, Undo and Deleted workspaces also use the existing attached-terminal input protocol.
Their retained layouts, deletion intents and source exclusions stay in `sessions-v2` with the
session's private state. Missing deletion fields in an earlier snapshot mean no deletions.
The 0.2.0 and 0.3.0 session paths remain untouched, so those releases cannot read a session
containing only deleted workspaces as an empty session or overwrite its recovery records.
An older running server continues its existing workspace behavior until restarted.

Move up and Move down use the existing attached-terminal input protocol too. The optional
`workspace_order` field in `sessions-v2` records saved identities and the paths of files not yet
imported. A snapshot without it keeps its saved sequence followed by scanned files. Importing
a file replaces its order reference with the workspace's identity, without changing its position.
The 0.2.0 and 0.3.0 state paths remain untouched. Their clients can use the actions when attached
to a current server; an older running server keeps its existing order behavior.

Edit pane uses the existing attached-terminal input protocol and the existing pane command,
directory, name and stopped-state fields in `sessions-v2`. It adds no wire request or saved-state
format. Old custom `rename_pane` bindings keep working; the default `ctrl+b P e` now opens the
form whose Name field also renames. An older running server keeps its existing pane actions.

Duplicate uses the same attached-terminal input protocol and existing saved-layout fields.
Its closed copy has a new identity, no source-file association, and stopped command panes without
conversation or scrollback references. No KDL, configuration or protocol format changes are
needed. Copies stay in `sessions-v2`; the 0.2.0 and 0.3.0 paths and migration backups are untouched.
Clients from those releases can use Duplicate on a current server. An older running server
keeps its existing actions until restarted.

Import and Export use new requests gated by the `workspace_files` capability. Older running
servers refuse them with the usual restart guidance. `kraal save` keeps its existing wire request,
name rules and account references, and now explains that it exports KDL. Current servers refuse
unconfirmed replacements, including unchanged sources; old servers retain their own behavior.
No new session format is introduced. Imported source bytes and export listing exclusions use
existing version 2 fields, leaving all legacy state paths untouched.

Add server uses the existing attached-terminal input protocol and the existing `servers.toml`
format. Saving is separate from SSH. The `auto_add` configuration key still reads as a boolean;
on a current server it controls discovery suggestions, which require explicit addition.
Previously saved servers, including the `discovered` group, keep working without migration.
Remote attach still marks an existing matching entry for agent watching, but suggests an
explicit save for an unknown connection. Older running servers keep their existing discovery
behavior until restarted. No session format or legacy state path changes are needed.

Review, Keep layout, Load from file and Undo load use the existing attached-terminal input
protocol. Optional `source_reviewed`, `pending_layout` and `layout_undo` fields in `sessions-v2`
retain the acknowledged source revision, prepared commands that wait for Enter, and the previous
local layout. Missing fields mean no acknowledgement, pending load or Undo. Source KDL,
configuration, protocol messages and the 0.2.0 and 0.3.0 state paths remain unchanged. Clients
from those releases can use these actions on a current server. An older running server keeps
its existing source-file behavior until restarted.

The tunnel form, list and result controls use the existing attached-terminal input protocol.
They keep the saved tunnel fields and wire state names (`connecting`, `up`, `down`) unchanged;
**Ready** is the UI label for `up`. The CLI still chooses an available port when `--local` is
omitted. No configuration, KDL or session format changes are needed, and legacy state paths
remain untouched. Older running servers keep their existing tunnel UI until restarted.

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
