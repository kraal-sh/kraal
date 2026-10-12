# Keys

Everything starts from one key, and the keys after it follow one rule. They can all be changed.

## The menu

One key to remember: **`ctrl+b`** opens a menu of commands. Press it, let go, and the menu
shows what each next key does; press that key (or click it).

<img src="../assets/menu.png" alt="The ctrl+b menu: one letter per theme, its capital for the theme's menu" width="720">

One rule: after `ctrl+b`, **a theme's letter runs its main action, and the same letter in
capitals opens the theme's menu**. `ctrl+b s` connects to a server; `ctrl+b S` opens the Servers
menu (upload, download, edit the list). Letters are taken for what they mean, so the rule holds on
any keyboard layout.

| Theme | `ctrl+b` + letter | `ctrl+b` + capital: the menu |
|---|---|---|
| servers | `s` connect (or a group in a grid) | `S`: `s` connect, `u` upload, `d` download, `e` edit the list |
| broadcast | `b` typing goes to the whole tab, `b` again stops | `B`: `b` this tab, `w` the whole workspace, `m` mark this pane, `s` the marked panes, `o` stop |
| workspaces | `w` choose one | `W`: `w` choose, `n` new, `←`/`→` previous/next, `e` rename, `s` save the layout, `r` restart the session (on the kraal installed now), `x` close |
| agents | `a` start one on an account | `A`: `a` start, `g` go to the agent needing you, `c` change account |
| tabs | `t` new tab | `T`: `t` new, `←`/`→` previous/next, `e` rename, `x` close |
| panes | `p` find a pane | `P`: `p` find, `n` new, `→`/`↓` split, `e` rename, `r` resize with the arrows, `f` floating, `x` close |
| view | `v` copy mode: scroll and select | `V`: `v` copy mode, `g` search the output, `o` links and paths, `c` copy the last command's output, `p` paste buffers, `f` floating panes, `l` sidebar, `j` message log |
| files | `f` file manager | `F`: `f` file manager, `p` preview a file, `u`/`d` upload/download |
| run | `r` tools where the pane is | `R`: `r` tools, `b` runbooks, `g` git and worktrees |

Inside every menu the same letters do the same things: `n` new, `x` close, `e` rename, arrows
previous and next, and the theme's letter its main action again.

| Other keys after `ctrl+b` | |
|---|---|
| `n` · `x` · `z` | new pane · close pane · zoom pane |
| `m` | mark this pane for broadcast |
| `o` | open a link, a path, a commit on screen (hints) |
| `g` (or `/`) | search the output |
| `=` | paste buffers: what you copied, to paste again (tmux's key) |
| arrows · `1`…`9` | focus the pane on that side · go to a tab |
| `space` | search every command by what it does (and see its keys) |
| `h` (or `?`) | help: every key, from your configuration |
| `d` | detach (programs keep running) |
| `ctrl+b` | type `ctrl+b` itself (whatever your prefix is, pressing it twice sends it) |

Without the prefix: `alt+arrows` move the focus (left/right at the edge go to the next tab),
`alt+n` opens a new pane, `alt+a` jumps to the next agent that needs you.

`[keys] preset = "classic"` switches to single-key commands (`%` `"` `c` `n` `p`
`o` `s` `=`…), still with the menu.

## Lists, fields and questions

In lists, type to filter, `↑↓` move, Enter chooses, **Tab shows what else can be done** with the
selected item (connect in a new tab, browse its files, run a command…), Esc closes. Text fields edit
like a shell line (arrows, `home`/`end`, `alt+←→`, `ctrl+w`, `ctrl+u`, `ctrl+k`). Questions show
two buttons, Cancel first: `y` answers yes, `n` or Esc no, Enter takes the selected button.
`ctrl+b V j` lists the messages kraal showed, warnings included.

## Your own keys

Every command and every menu can move, in `~/.config/kraal/config.toml`:

```toml
[keys]
prefix = ["ctrl+b"]          # several are fine: ["ctrl+b", "ctrl+space"]

[keys.leader]                # after the prefix; a space separates a sequence ("P right")
broadcast_toggle = ["y"]     # replaces the command's keys
rename_tab = []              # [] removes them
menu_servers = ["M"]         # a menu moves too: servers, broadcast, workspaces, agents, tabs,
                             # panes, view, files, run

[keys.direct]                # one key, without the prefix
new_pane = ["alt+n"]
```

`kraal config keys` prints every key, menu and mode, with the names to use here (`--markdown`
for a sheet to keep or print); `kraal config check` reports a key bound twice or a sequence that
hides another; `kraal config reload` applies the file to the running session (keys, theme,
sidebar, mouse, frames, tools, hints, hooks; new panes take a new shell or scrollback length).

## Mouse

The mouse works too: click to focus panes, tabs, workspaces and agents (a click on the workspace
name opens the list); drag borders to resize; drag floating panes by their title; wheel to scroll
back; drag to select and copy (double click selects a word, triple click a line); Ctrl+click opens
a link (OSC 8 links stay clickable, and plain URLs work too). Hold Shift to use your terminal's own
selection.

## Paste buffers

What you copy goes to your terminal's clipboard, and kraal keeps it too, as tmux does: a copy
mode selection, a mouse selection, `ctrl+b V c` (the last command's output), a hint's text, and
what programs copy. The newest copy is buffer 0, the one before it buffer 1, and so on; the same
text copied twice in a row is kept once. At most 50 numbered buffers and 32 MB stay, the oldest
going first; buffers set by name (`kraal buffer set notes`, see
[Scripting](scripting.md#paste-buffers)) stay until deleted.

`ctrl+b V p` (or `ctrl+b =`) lists them, newest first, with their first line, size and age; type
to filter. Enter pastes the selected one into the focused pane as your terminal would: bracketed
when the program asked for it, to the panes typing is broadcast to, and asked about first when
several lines would run at once in production ([Production](production.md)). Tab shows what else
can be done, each also on its own key:

| Key | |
|---|---|
| `enter` | paste into the pane |
| `ctrl+y` | copy to the clipboard again |
| `ctrl+o` | show in a preview pane (a window, never saved with the session) |
| `ctrl+x` | delete |

Buffers live in the server's memory: they go when it stops, and nothing of them is written to
disk, since they can hold secrets. `[buffers]` in the configuration changes the limits, or keeps
them across restarts in a file only you can read ([Configuration](configuration.md)).

## Keyboards and terminals

Keys reach programs unambiguously: kraal speaks the kitty keyboard protocol with terminals that
support it (Ghostty, kitty, WezTerm, foot…) and gives each program what it asked for, so
Shift+Enter, Ctrl+Enter or Esc arrive as such (Claude Code's Shift+Enter makes a new line), with
the keys' alternates and the text they type for programs that want them. Typed text, accents and
dead keys included, reaches programs as it was typed.

Shortcuts work on every keyboard layout. Letters are read by the character they type, wherever
the key is. The digit row is read by its place: on AZERTY, the key that types `&` is `1` after
the prefix, without Shift. On a non-Latin layout, a key stands for the letter at its place on a
US keyboard, so Ctrl+B is still the prefix on a Russian layout. A key the keymap binds by its
character keeps that meaning. Terminals without the protocol send characters only, and shortcuts
read them as such. Key release events are not passed on yet.
