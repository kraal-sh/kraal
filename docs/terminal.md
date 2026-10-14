# Shell integration and images

What kraal adds to the terminal inside each pane: the commands your shell ran, and images.

## Clipboard

kraal sends text copies to the attached client's terminal using OSC 52. With
`kraal attach --host db`, that terminal is on the machine where you ran the attach command.
SSH carries the copy back from the server. kraal does not run a native clipboard tool on
that machine.

- A mouse selection, copy mode, a hint, the last command's output, or copying a paste buffer
  goes to the client that requested it.
- A program's OSC 52 goes to one attached client: the most recently used client viewing that
  pane's workspace, even if another pane there has focus. It is never broadcast to all clients.
- Without a client viewing that workspace, a program's copy only enters the session's paste
  buffers. It is not sent later on attach.

**copy sent to terminal** means kraal sent the request toward that terminal. OSC 52 writes
have no acknowledgement, so kraal cannot confirm that the system clipboard changed. The
terminal may refuse the request or impose a size limit. **copy not sent: try again** means the
client's output queue could not accept it. If buffer limits allow it, the text remains in
[Paste buffers](keys.md#paste-buffers), where `ctrl+y` sends it again. With `buffers.limit = 0`,
or when the byte budget is exhausted (including by named buffers), the copy may not be kept.

### What the terminal must allow

| Terminal or outer multiplexer | Requirement |
|---|---|
| Ghostty | `clipboard-write = allow`, the default. `ask` requires approval; `deny` blocks writes. See [Ghostty's clipboard setting](https://ghostty.org/docs/config/reference#clipboard-write). |
| iTerm2 | Enable **Settings > General > Selection > Applications in terminal may access clipboard**. See [iTerm2's settings](https://iterm2.com/documentation-preferences-general.html#selection). |
| macOS Terminal.app | No native OSC 52 clipboard support ([Terminal.app workaround project](https://github.com/roy2220/osc52pty)). Use Ghostty or iTerm2 for this copy path, or select visible text with the terminal's own selection and Copy action. |
| tmux outside the client | Set `set -s set-clipboard on` and ensure tmux has the `Ms` capability for its outer terminal. `external` does not accept clipboard requests from applications inside tmux. The outer terminal must also allow writes. See [tmux's clipboard guide](https://github.com/tmux/tmux/wiki/Clipboard). |
| A local kraal outside the remote tab | The outer kraal treats the remote copy as a program's OSC 52: it keeps a paste buffer and forwards to its most recently used client viewing that local workspace. That client's terminal must allow writes too. Use the remote agent tab from the sidebar or `alt+a`; plain attach inside a kraal pane refuses nesting. |

Allowing OSC 52 writes also allows programs in your panes to replace the clipboard. Reading
it is a separate permission and is not needed for copying. kraal does not read it to check
whether a write worked. See [the OSC 52 protocol](https://ghostty.org/docs/vt/osc/52).

If a copy does not arrive, check the receiving terminal's write permission and any outer
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
