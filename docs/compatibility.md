# Compatibility between versions

## Saved files and sessions

Kraal 0.4 migrates sessions and `accounts.toml` from **0.2.0** and **0.3.0** once,
leaving the old session and scrollback files untouched. The accounts file is backed up before
conversion. This migration belongs to 0.4 and is removed in **0.5.0**.
Existing workspace KDL and configuration files keep working.

The unreleased `[keys.prefixed]` table is ignored with an unknown-key warning; use
`[keys.leader]` for bindings after the prefix. The unreleased session format `version: 1,
revision: 2` is refused with `unsupported state revision 2; use a newer kraal`, leaving the
saved session untouched. Both supported releases wrote revision 1.

Each save writes the current state format only. Kraal does not also write a downgraded copy
for older versions. The untouched legacy session and the accounts backup are recovery copies,
not synchronized views of the current session.
Session migration preserves the personal layout, identities, source links, account references,
restore failures and available scrollback. When an old session has no recorded source contents,
migration records the source file as it is then, without replacing the personal layout. This
baseline survives restart, so local layout edits do not report **File changed**, but later file
edits do. Missing or unreadable sources leave the saved layout available with a source error.
Account migration happens on the next change to the file, keeping comments, order and unknown keys.
Before account changes, the 0.4 migration copies
legacy usage to identity-based caches, binds saved pane references (including layout Undo in kept
and deleted workspaces), and adds account identities to readable KDL files in configured workspace
directories. Unknown session fields and KDL formatting are preserved. If this preparation fails,
the account change is not written; fix the reported file or directory and retry.
These disk adapters are removed in 0.5.0. Resolving identities during ordinary account and workspace
operations remains, including retained references and homes after renaming or removing an account.
See [Workspace migration](workspaces.md#workspace-identities-and-migration) and [Accounts](agents.md#accounts).

A kraal older than the saved state does not read that state. Starting with 0.4, sessions use
`sessions-v2/<session>.json` and `scrollback-v2/<session>/` in the kraal state directory.
Kraal 0.2.0 and 0.3.0 never open these paths. Going back to either finds the session as it was
at upgrade, or no saved session if there was none before. Its saves and `kraal forget` cannot
overwrite the newer session or its scrollback. Changes made after upgrade do not appear in the
older kraal; changes made while downgraded do not replace the newer state when returning to 0.4.

For recovery or a return to 0.2.0 or 0.3.0, stop the session first. The original files are
`~/.local/state/kraal/sessions/<session>.json` and
`~/.local/state/kraal/scrollback/<session>/` (under `$XDG_STATE_HOME/kraal` when set).
Kraal 0.4 never rewrites or removes them, including on account changes, Close, Delete, Undo,
or `kraal forget`. No additional session backup is made. An older kraal can still change
these files; they are not a history of changes made after upgrading.

`kraal forget` removes current state and scrollback. When legacy state exists, it first records
`sessions-v2/<session>.forgotten` so the next start does not import that state again. To recover
that legacy session in 0.4, while the session is stopped, move aside any current
`sessions-v2/<session>.json` and `scrollback-v2/<session>/`, then remove the `.forgotten` marker
if present. The next start imports the original files again.

`accounts.toml` is rewritten in place, so its original remains in `accounts.toml.kraal-bak`
beside it. Restore that backup before using the old account configuration with an older kraal.

From 0.5.0, state older than 0.4 will be left untouched and refused with this next step:
`run kraal 0.4 once first`. The one-way session and account migration is removed in 0.5.0.
A state from a newer format is also left untouched, with a message to use a newer kraal.

## Clients and servers

This version uses protocol 4 for laptop listings. Kraal 0.4.0 used protocol 3;
Kraal 0.2.0 and 0.3.0 use protocol 2.
Different protocol versions refuse attachment and ordinary commands. There are no older-peer
adapters or optional feature negotiation. A refusal names both protocol versions and one next step:

- A newer local client with an old server: `kraal -s main restart`.
- A newer client using `attach --host box`: `kraal remote-install box` (with `-s NAME` for a named session).
- An older client with a newer server: update this machine with `kraal update`.

The older client must be updated, since restarting through that binary would downgrade the server.
The current client refuses before changing the terminal, and a mismatched attachment never
changes the session. Updating the binary does not
stop a running session automatically. Existing attached terminals keep using their server until
it stops; a newly installed client needs the matching protocol before attaching.

The upgrade control path remains: `kraal ls` and the update notice identify running releases;
`kraal kill` saves and stops a 0.2.0 or 0.3.0 server; `kraal restart` saves, stops and restores it
with the installed binary. Old socket locations remain discoverable for this purpose and to
avoid starting a second server for the same session. Restart lists the programs it will stop
and asks before stopping them. Layout and available scrollback survive through the migration
above. Restart refuses if newer saved state or a Forget marker would hide the old running
session. See [Persistence](workspaces.md#persistence).

A restart from 0.2.0 ends its attached terminals. Attach again with the current client.
A 0.3.0 client waiting for a restarted server receives the protocol refusal and must be updated.

Workspaces are saved automatically. KDL is imported and exported explicitly with `kraal import`
and `kraal export`; `kraal save` and its historical `SaveWorkspace` request are removed.
Clipboard, file attachments, pasted paths and clipboard images introduced in protocol 3 remain
part of protocol 4.
The client applies its clipboard policy, including the terminal OSC 52 fallback where configured.
The server no longer emits OSC 52 as an adapter for old clients.

## Laptop listings on Linux and macOS

Laptop browsing uses the same attachment connection on both systems, with consent drawn by
the laptop client. Each directory component is opened without following symbolic links.
On macOS, the system may also ask for access when listing Documents or Desktop. Declining
system access leaves a listing error; grant access on the laptop and press `r` to retry.
The default scope excludes `~/Library`. Laptop file copying and filesystem-specific staging
are not available in this version.

Laptop panels save as the session machine's home directory, with no laptop paths or listings
in the saved session. The session format and its downgrade isolation remain unchanged.

## For contributors: how the protocol changes

The client and server exchange JSON messages in `src/protocol.rs`. Change `PROTOCOL_VERSION`
when the wire contract changes. Peers must agree on that generation before interactive use.
Do not add capabilities or adapters for older peers. Keep the protocol 2 ping, pane listing and
stop requests used to upgrade released servers, and their response readers, until their upgrade
path is removed. Saved-file migration is separate and remains in 0.4.

## The cross-version test

`tests/compat.sh` downloads 0.2.0 and 0.3.0 and checks their release checksums before running
`tests/compat.rs` against each real binary. The tests cover attachment refusal in both
directions, stopping and restarting old servers with their layout and scrollback, migration with
untouched original files, and downgrade isolation, including saves and
Forget by the older binary. They also check that old workspace and configuration files keep
working. CI runs these tests, including terminal refusal, on Linux and macOS.

| Command | Releases |
|---|---|
| `tests/compat.sh` | 0.2.0 and 0.3.0 |
| `tests/compat.sh v0.2.0` | 0.2.0 only |
| `tests/compat.sh --build v0.2.0` | Built from its tag (`git fetch --tags` first) |

The releases in `RELEASES` in `tests/compat.sh` are checked against this page by the unit tests.
