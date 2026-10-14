# Shell integration and images

What kraal adds to the terminal inside each pane: the commands your shell ran, and images.

## Clipboard

With a current client and server, kraal sends a structured copy to the attaching machine.
A user copy tries that machine's native clipboard first, then falls back to OSC 52 through
its terminal. With `kraal attach --host db`, the native tool runs where you launched attach.
A copy uses one successful path, never both native and OSC 52.

- A mouse selection, copy mode, a hint, the last command's output, or copying a paste buffer
  goes to the client that requested it.
- A program's OSC 52 goes to one attached client: the most recently used client viewing that
  pane's workspace, even if another pane there has focus. It is never broadcast to all clients.
  By default it still goes through the terminal's write permission, without native access.
- Without a client viewing that workspace, a program's copy only enters the session's paste
  buffers. It is not sent later on attach.

The attaching machine's `config.toml` controls its policy:

```toml config
[clipboard]
mode = "auto"             # native for user copies, then terminal fallback
max_bytes = 1048576        # 1 MiB of UTF-8 text, without truncation
```

`mode = "terminal"` uses OSC 52 for every copy. `mode = "off"` disables external writes,
while keeping internal paste buffers subject to their own limits. Settings apply on the next
attach. To let programs in one remote session use native tools too, set
`clipboard_native = true` on that server in the attaching machine's `servers.toml`
([Servers](servers.md#the-inventory)). Remove it or set it to false and attach again to revoke.
This permission applies to all programs of that server's session, including its SSH panes.
It bypasses the terminal's OSC 52 write permission. A different user or an overridden port
has no permission unless its own saved connection grants it.

When the client itself runs under SSH, in a VS Code remote environment, or under WSL,
`auto` uses the terminal even when native program copies are allowed. Its native clipboard
may belong to an intermediate machine. `DISPLAY` alone does not prove locality. Local tmux
or kraal nesting does not disable native copies.

Native tools are `pbcopy` on macOS; on Linux, `wl-copy` when `WAYLAND_DISPLAY` is set, then
`xclip -selection clipboard -in` or `xsel --clipboard --input` when `DISPLAY` is set.
Tools must be installed and able to reach the graphical session. A missing or failing tool
falls back to the next tool, then OSC 52. Native work runs off the input and display threads,
with a two-second deadline per tool, including writing its stdin. Persistent selection owners
are left running after handoff and detach, so Wayland does not lose the copied text.

**copied to laptop clipboard** means the native tool reported success. A persistent tool that
does not exit instead reports **copy handed to native clipboard owner** after startup; kraal
cannot confirm that tool's eventual write. Neither message promises that another application
will not replace the clipboard later. **copy sent to terminal via OSC 52** means the request
was written to the terminal. OSC 52 writes have no acknowledgement: kraal cannot confirm that
the system clipboard changed, and never reads it to check. The first native backend failure
per connection is recorded in Messages, without an alert on every selection.

Each side limits external copies to `clipboard.max_bytes` (1 MiB by default, configurable
from 1 byte through 8 MiB). Both limits must allow the text. Oversized copies are refused,
never truncated. Each client has at most one outstanding server request, a bounded worker
queue, and at most ten attempts per second. **copy busy: try again** means the previous copy
is still pending or a new copy came too soon. An unavailable result after ten seconds is
reported without resending the copy. Disconnect cancels unfinished work, with no replay.

**copy not sent: try again** means the client's output queue could not accept it. If buffer
limits allow it, the text remains in [Paste buffers](keys.md#paste-buffers), where `ctrl+y`
sends it again. With `buffers.limit = 0`, or when the byte budget is exhausted (including
by named buffers), the copy may not be kept.

Older clients receive only the legacy OSC 52 request and say **copy sent to terminal**.
A current client attached to an older server also keeps that server's legacy path, without
native copying or client size enforcement. `off` refuses such an attachment with restart
guidance, since an old server cannot separate copies from display bytes. No session or
workspace format changes are needed. See [Compatibility](compatibility.md#clients-and-servers).

### What the terminal fallback must allow

| Terminal or outer multiplexer | Requirement |
|---|---|
| Ghostty | `clipboard-write = allow`, the default. `ask` requires approval; `deny` blocks writes. See [Ghostty's clipboard setting](https://ghostty.org/docs/config/reference#clipboard-write). |
| iTerm2 | Enable **Settings > General > Selection > Applications in terminal may access clipboard**. See [iTerm2's settings](https://iterm2.com/documentation-preferences-general.html#selection). |
| macOS Terminal.app | No native OSC 52 clipboard support ([Terminal.app workaround project](https://github.com/roy2220/osc52pty)). Native user copies work through `pbcopy`; for program copies, grant native permission to that saved server or use the terminal's own selection and Copy action. |
| tmux outside the client | Set `set -s set-clipboard on` and ensure tmux has the `Ms` capability for its outer terminal. `external` does not accept clipboard requests from applications inside tmux. The outer terminal must also allow writes. See [tmux's clipboard guide](https://github.com/tmux/tmux/wiki/Clipboard). |
| A local kraal outside the remote tab | The outer kraal treats the remote copy as a program's OSC 52: it keeps a paste buffer and forwards to its most recently used client viewing that local workspace. That client's terminal must allow writes too. Use the remote agent tab from the sidebar or `alt+a`; plain attach inside a kraal pane refuses nesting. |

Allowing OSC 52 writes also allows programs in your panes to replace the clipboard. Reading
it is a separate permission and is not needed for copying. kraal does not read it to check
whether a write worked. See [the OSC 52 protocol](https://ghostty.org/docs/vt/osc/52).

If a fallback copy does not arrive, check the receiving terminal's write permission and any outer
multiplexer first. In Ghostty with its default permission, check whether the program actually
uses OSC 52 and which attached client was last used. You can still paste a kept copy inside
kraal with `ctrl+b =`, or use the terminal's own selection (hold Shift) and Copy action.

### Programs that write the server's clipboard

An agent's own copy command may call `wl-copy`, `xclip`, `xsel`, or `pbcopy` instead of emitting
OSC 52. Such a write never enters kraal's terminal stream or paste buffers. It targets the
graphical session that tool can access where it runs. On a server with its own desktop, that
can change the server's clipboard while leaving your laptop's clipboard untouched. Without an
accessible graphical session it may fail; kraal cannot see that result as a clipboard operation.
`DISPLAY` can also name a forwarded X11 display, so it does not by itself prove which machine's
clipboard is used.

Panes inherit `DISPLAY` and `WAYLAND_DISPLAY`, when set, from the environment of the kraal
server process. This is the environment that started that server, not a fresh environment
from each attaching client. Attaching through `--host` does not replace it with your laptop's
variables. Pane environment overrides still apply.

These variables should remain inherited: panes also launch graphical applications, and the
same session can have local and remote clients. Removing them globally would break valid
local use and would not reliably make programs choose OSC 52. For remote copying, configure
the program to use OSC 52. If its backend selection depends on these variables, launch just
that program with `env -u WAYLAND_DISPLAY -u DISPLAY program` and check its documented fallback;
unsetting them alone does not guarantee terminal copying, and does not disable macOS `pbcopy`.

## Shell commands

A shell can mark where each prompt, command line and output starts, and how each command ended
(the OSC 133 marks). kraal then knows the commands run in a pane:

- in copy mode, `p` jumps to the prompt above and `P` to the one below; `o` selects the output of
  the command at the cursor, `O` the whole command with its prompt;
- `ctrl+b V c` copies the output of the pane's last command (the one running, or the last that
  ran);
- a command that failed shows its exit status in the pane's frame (`✗ 2`, with how long it ran
  when that was 10 seconds or more), until the next one runs;
- a command that ran 30 seconds or more and ends in a pane you are not looking at is announced
  like an agent that finished, and waits in `alt+a`'s queue (`[notifications] long_command`, in
  seconds; 0 turns it off);
- each command that ends is a `command.finished` event.

A new pane width, or a program that scrolls only part of the screen, moves lines in ways kraal
cannot follow: the earlier prompts can no longer be jumped to, but how each command ended stays.

Shells do not send the marks by themselves (fish 4 does). kraal adds a few lines to your shell's
startup file, active only in kraal's panes, when you ask (here for zsh; `bash`, 4.4 and later,
and `fish` work the same):

```sh
kraal integration install zsh
```

| Command | |
|---|---|
| `kraal integration show zsh` | prints the lines, for a startup file you keep yourself |
| `kraal integration status zsh` | says whether they are installed |
| `kraal integration uninstall zsh` | takes them out again |

The startup file is copied next to itself first (`.kraal-bak`); fish gets a file of kraal's own
(`~/.config/fish/conf.d/kraal.fish`), so there is nothing to copy. A startup file that is a link
(dotfiles kept in a repository) stays a link: the file it points to changes, and the backup goes
next to the link.

### Over SSH

The same lines work on a server, where kraal need not be installed: paste what
`kraal integration show <shell>` prints into the shell's startup file there (`~/.zshrc`,
`~/.bashrc`, or `~/.config/fish/conf.d/kraal.fish`). There, the shell marks its commands as in a
local pane, and reports at each prompt the Kubernetes, Terraform and AWS contexts it works on (see
[Contexts](tools.md#contexts)).

SSH does not pass kraal's variables on, so an interactive shell started over SSH
(`SSH_CONNECTION` set, on a terminal) asks its terminal whether it is kraal: `OSC 7701 ; ?`, then
a request for the terminal's attributes (DA1), which every terminal answers. kraal answers the
first; other terminals ignore it, and the lines then do nothing more. The question costs one
round trip when the shell starts (two seconds at most if nothing answers), and keys typed during
it are lost. Outside SSH and kraal, the lines do nothing at all.

At each prompt the shell sends
`OSC 7701 ; context ; k8s=<context> ; ns=<namespace> ; tf=<workspace> ; aws=<profile> BEL`: an
empty value is none, `?` is what the shell could not read. It reads the kubeconfig's current
context with `awk` (one process per prompt, only when there is a kubeconfig), never runs kubectl
or terraform, and sends `?` instead of a name with other characters than letters, digits and
`-_.:/@+`. It reads YAML as kubectl, kubectx and the clouds' tools write it, comments and a
context written on one line (`context: {namespace: web}`) included; a kubeconfig written another
way (JSON, anchors and merge keys, braces over several lines) is `?`, never a context without its
namespace.

Some shells do not report: one inside another multiplexer on the server, one started by `sudo -i` or
`su -`, one on a server without these lines reached from the first. When such a shell marks its
prompts (fish 4 does by itself), kraal forgets what the shell before it reported; otherwise that
stays until the next report or the end of the session.

## Images

Programs that draw images in the terminal work in panes, locally or over SSH: the kitty graphics
protocol (kitty's `icat`, Yazi, timg, chafa…) and sixel (img2sixel, gnuplot, matplotlib
backends…). kraal keeps each image and draws it in the pane like text: it scrolls with the
output, is cut by the pane's frame, hides under a floating pane, and shows on every attached
terminal that can draw it. An image preview (`kraal preview photo.jpg`, `pane preview=`) is
shown the same way.

kraal asks each terminal what it draws when it attaches, and sends each the images in the best
way it has:

| Your terminal | Images |
| --- | --- |
| Ghostty, kitty | kitty graphics over Unicode placeholders: the terminal moves and cuts them itself |
| One whose device attributes report sixel: foot, WezTerm, Konsole, iTerm2, mlterm, Windows Terminal 1.22 or later, xterm started with `-ti vt340` | sixel, drawn by kraal over the image's cells |
| Others, such as Alacritty or Terminal.app | none: the image's cells stay blank, and programs in panes are told no images can be shown, as a terminal without them would (unless another terminal attached to the session draws them); previews draw their image with half blocks |

With sixel:

- kraal draws the part of each image the screen shows, with up to 255 colours, when it appears,
  changes or is uncovered; nothing is sent while it stays as it is. A part that goes is erased.
- An image that moves (output scrolling under it, a pane scrolled back) is erased at once, and
  drawn again where it is once it has stayed put for a tenth of a second.
- The terminal must tell its cell size when asked (`CSI 16 t`). After the window is resized,
  images come back once it has told it again.
- The window's last row shows no image (a sixel image there scrolls some terminals), and the
  bottom of each part stops at a whole band of six pixel rows, up to five rows short.
- Drawing a large image again costs the server a few milliseconds (12 to 17 for one filling a
  2000×1200 pixel window) and sends 1 to 2 MB. Scaling an image and choosing its colours is done
  once per size, off kraal's main thread.

And everywhere:

- Programs send the image data itself. Reading an image from a file or shared memory is refused
  (the program may run on another machine than kraal), and programs then send the data instead.
- Decoding an image is done off kraal's main thread, a pane's images one after the other: a
  large one takes its place in the pane at once, what follows it goes on, and it shows when it is
  decoded (a fraction of a second for the largest).
- `[graphics] memory_mb` (256 by default) bounds the memory all panes keep for images, the data
  of those still to decode included. A pane that adds an image past it loses its own oldest
  images, never another pane's, and the image is refused when it has none left to give up. A
  preview whose image does not fit in what is left draws it with half blocks. The
  images ready to draw as sixel take at most a quarter of that again. `enabled = false` turns
  images off: their sequences then reach the emulator, which ignores them, and previews draw their
  image with half blocks. Both settings are read when the session starts.
- Not supported: animations, images under the text (negative z-index), offsets within a cell.

## Attachments

Open the command palette (`ctrl+b space`) and choose **Attach files**. Enter existing absolute
paths on your laptop, separated by spaces. Quote paths containing spaces, for example
`'/home/me/notes client.txt' /home/me/capture.png`. Press Enter to send their contents and submit
the resulting paths to the pane named in the form. Ctrl+C cancels. The form is local: a server
message alone cannot read files. Local `file:///...` URIs are accepted with percent decoding;
remote URI authorities, directories, symlinks and special files are refused.
Use `kraal cp` for directories and larger transfers.

With `attach --host`, files travel through the existing attach connection, in 64 KiB chunks
with only one unacknowledged chunk. The target pane is reserved when the action opens. Changing
focus does not redirect the paths. Broadcasting asks whether to attach only to that pane.
A local session submits the original paths without copying files. No Enter is added, and the
usual production paste protections still apply. No path is submitted unless the entire batch
arrives. Ordinary pasted text is unchanged. Clipboard images are not read by this action.

### Pasted laptop paths

With `attach --host`, a complete bracketed paste containing only existing absolute laptop paths
opens a local choice. The target, file count, total size and names appear before any file is
opened for transfer. At 124 columns:

```text
SERVER / main / Claude | 2 local files, 3.2 MiB: capture.png, notes client.txt
Send the files and paste their remote paths?
[>Send<] [Always for SERVER] [Paste the text] [Cancel]
```

At 80 columns:

```text
SERVER / main / Claude
2 local files, 3.2 MiB: capture.png, notes client.txt
Send the files and paste their remote paths?
[>Send<] [Always here] [Text] [Cancel]
```

Tab or the arrow keys select a choice, Enter activates it, and clicking a choice does the same.
Ctrl+C or Esc cancels. **Send** sends this batch once. **Always** remembers permission on the
laptop for this SSH destination, including its connection arguments. Subsequent eligible pastes,
including on another attach, send without asking. Broadcasting still asks to send only to the
named pane. The choice is never permission for the server to read laptop files on its own.

Permissions are in `~/.config/kraal/paste-files.toml` (or
`$XDG_CONFIG_HOME/kraal/paste-files.toml`). Remove the matching `[[server]]` entry to revoke it,
or remove that file to revoke all permissions. Revocation applies to the next paste without
reattaching. An unreadable or malformed file never grants permission. The file holds destination
names and SSH arguments, not file contents or pasted text.

**Paste the text** (or **Text**) preserves the original bytes, including quotes and escapes.
**Cancel** sends no file and pastes nothing. Spaces can be quoted or escaped with backslashes.
Local `file:///...` URIs are percent decoded; remote authorities are refused. An unquoted name
with spaces is recognized only when it has one interpretation. Commands, sentences, missing
paths, ambiguous lists, variables, globs and shell substitutions remain ordinary text. Kraal
executes none of them to recognize a path. Recognition buffers at most 64 KiB; larger pastes
stream unchanged. A local session always pastes paths unchanged, including into an SSH pane.
Use **Upload to this pane's server** or `kraal cp` for that topology.

For a longer transfer, the choice is replaced after 200 ms by progress, for example:

```text
Sending to SERVER: 2.1 / 3.2 MiB (66%) [Cancel: Ctrl+C]
```

A short transfer goes directly to the result, without flashing progress. Files stay in their
original order, and all remote paths are submitted together to the reserved pane after the
whole batch succeeds. A failure retains the original text and offers **Retry**, **Paste the
text**, and **Close**. Retry reserves the current pane and shows the choice again; Text pastes
the original text into the current pane. After a connection loss, inspect the recovered result
and the pane before retrying: finalized files are kept, and kraal never retries automatically.
Directories, links and special files have an explicit refusal pointing to `kraal cp`.

Without bracketed paste or with an older peer, text continues unchanged. Use **Attach files**
when the terminal cannot mark pastes. Empty pastes never read clipboard images.

### Transfer lifetime and limits

Sending shows progress after 200 ms, with Ctrl+C to cancel. Typing waits in a bounded queue
(up to 64 KiB, for at most 60 seconds including the form) and follows the paths on success.
Closing the pane, cancelling, exceeding the queue or timing out discards the queued typing
and incomplete files, with a clear result. After failure, further typing is held out until
Ctrl+C dismisses the result, so the tail of a large paste cannot reach the pane. If focus changed, the paths still go to the reserved
pane but queued typing is discarded. Use **Attach files** to retry, selecting the files again. Dismiss a failure with Ctrl+C first.
Results remain on the client until Ctrl+C dismisses them or another attachment opens.

The defaults are 64 MiB per file, 128 MiB per batch, 32 files and 1 GiB per session. All four
are [configurable](configuration.md#attachments). No limit silently truncates content. The
session quota includes small result records, with 4 KiB reserved per in-progress batch for its
result. At most 4096 completed batches are retained to bound bookkeeping even for empty files.

The server chooses every destination under
`~/.local/state/kraal/attachments-v1/<session>/` (or `$XDG_STATE_HOME/kraal/attachments-v1/`).
Directories are private (0700) and files are 0600. Each file gets its own numbered subdirectory,
so equal names never overwrite each other. Ordinary filenames are retained and shell quoted;
control characters, separators and oversized names use an `attachment-N` alias visible in
the pasted path. Processes running under the same Unix account can read these files.

Completed files survive detaching and server restarts. `kraal forget` removes them with the
session. **Purge attachments** in the palette asks before removing all of the session's
attachments; previously pasted paths then stop working. Cancel active attachments first.
Startup removes incomplete batches left by a crash. It also removes orphan session directories
older than seven days, only after checking under the session lock that no server or saved
session owns them. Active and saved sessions are never purged just for their age.

A disconnect cancels an incomplete batch. After reconnect, the client queries the last batch's
result without resending bytes or pasting again. The server retains a result beside each
completed batch. If a crash prevents knowing whether the paste happened, the result says so:
inspect the pane before retrying. There is no byte resume.
