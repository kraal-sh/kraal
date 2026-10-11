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
startup file, active only in kraal's panes, when you ask:

```sh
kraal integration install zsh      # or bash (4.4 and later), fish; a backup is kept
kraal integration show zsh         # the lines, for a startup file you keep yourself
kraal integration uninstall zsh
```

A startup file that is a link (dotfiles kept in a repository) stays a link: the file it points to
changes, and the backup goes next to the link.

## Images

Programs that draw images in the terminal work in panes, locally or over SSH: the kitty graphics
protocol (kitty's `icat`, Yazi, timg, chafa…) and sixel (img2sixel, gnuplot, matplotlib
backends…). kraal keeps each image and draws it in the pane like text: it scrolls with the
output, is cut by the pane's frame, hides under a floating pane, and shows on every attached
terminal that can draw it.

- Your terminal must draw kitty graphics over Unicode placeholders: Ghostty and kitty do. kraal
  asks each terminal when it attaches; on one that cannot (WezTerm and Konsole draw images, but
  not this way), the image's cells stay blank, and programs in panes are told no images can be
  shown, as a terminal without them would.
- Programs send the image data itself. Reading an image from a file or shared memory is refused
  (the program may run on another machine than kraal), and programs then send the data instead.
- `[graphics] memory_mb` (256 by default) bounds the memory all panes keep for images; the oldest
  go first. `enabled = false` turns images off: their sequences then reach the emulator, which
  ignores them.
- Not supported: animations, images under the text (negative z-index), offsets within a cell.
