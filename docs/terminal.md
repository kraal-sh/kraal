# Shell integration and images

What kraal adds to the terminal inside each pane: the commands your shell ran, and images.

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
