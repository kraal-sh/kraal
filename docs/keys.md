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
| workspaces | `w` choose one | `W`: `w` choose, `n` new, `←`/`→` previous/next, `e` rename, `s` save the layout, `r` restart the session (on the kraal installed now), `q` quit, `x` close |
| agents | `a` start one on an account | `A`: `a` start, `g` go to the agent needing you, `c` change account |
| tabs | `t` new tab | `T`: `t` new, `←`/`→` previous/next, `e` rename, `x` close |
| panes | `p` find a pane | `P`: `p` find, `n` new, `→`/`↓` split, `e` rename, `r` resize with the arrows, `f` floating, `l` next layout, `s` then an arrow stack with the pane on that side, `u` unstack, `x` close |
| view | `v` copy mode: scroll and select | `V`: `v` copy mode, `g` search the output, `o` links and paths, `c` copy the last command's output, `p` paste buffers, `f` floating panes, `l` sidebar, `k` bottom bar, `j` message log |
| files | `f` file manager | `F`: `f` file manager, `p` preview a file, `u`/`d` upload/download |
| run | `r` tools where the pane is | `R`: `r` tools, `b` runbooks, `g` git and worktrees |

Inside every menu the same letters do the same things: `n` new, `x` close, `e` rename, arrows
previous and next, and the theme's letter (or Enter) its main action again. Backspace goes back
to the menu before, Esc closes.

| Other keys after `ctrl+b` | |
|---|---|
| `n` · `x` · `z` | new pane · close pane · zoom pane |
| `m` | mark this pane for broadcast |
| `o` | open a link, a path, a commit on screen (hints) |
| `g` (or `/`) | search the output |
| `=` | paste buffers: what you copied, to paste again |
| arrows · `1`…`9` | focus the pane on that side · go to a tab |
| `space` | search every command by what it does (and see its keys) |
| `h` (or `?`) | help: every key, from your configuration |
| `d` | detach (programs keep running) |
| `q` | quit (programs stop; the layout is saved for the next `kraal`), after asking |
| `ctrl+b` | type `ctrl+b` itself (whatever your prefix is, pressing it twice sends it) |

Without the prefix: `alt+arrows` move the focus (left/right at the edge go to the next tab),
`alt+n` opens a new pane, `alt+a` jumps to the next agent that needs you.

`[keys] preset = "classic"` switches to single-key commands (`%` `"` `c` `n` `p`
`o` `s` `=`…), still with the menu; there `d` or `q` detaches and `Q` quits. The other pages give
the keys of the default preset: with another preset or keys of your own, the menu, the help
(`ctrl+b h` here) and `kraal config keys` show yours.

## The bottom bar

The last row shows what can be done in the focused pane, the most useful first, each with the
keys that do it in your keymap, and the menu key at the right end:

| Focused pane | |
|---|---|
| on a server (its name in red in production) | its files `ctrl+b f`, the tools there `ctrl+b r`, download `ctrl+b S d`, upload `ctrl+b S u` |
| an agent (its name and state) | switch its account `ctrl+b A c` (one kraal started), review its worktree `ctrl+b R g` (when it works in one), the next agent needing you `alt+a`, a new agent `ctrl+b a` |
| in a git repository | git and worktrees `ctrl+b R g`, tools `ctrl+b r`, new pane `alt+n`, split `ctrl+b P →`, start an agent `ctrl+b a` |
| a shell | new pane, split, connect to a server `ctrl+b s`, start an agent |
| a preview | its own keys: `↑↓` scroll, `r` reload, `q` close |
| the file manager | new pane, split (its own keys are in its footer) |

An agent counts before its server, and a server before a repository. A click on an action runs
it, and a click on the menu key opens the menu. When the terminal is narrow, actions give way
from the right; in copy mode, a list or a question, the bar keeps only what the pane is.
`ctrl+b V k` hides the bar, or shows it again, for the session; `[bar] keys = "off"` in the
configuration hides it for good. Without it, the panes take the row and the menu key goes to the
top bar.

## Stacks and layouts

Several panes can share one place as a **stack**: one is open, the others are folded to the top
line of their frame, one row each (name, branch, agent state, PROD badge, as on any frame), those
before it above and those after it below. `ctrl+b P s` then an arrow stacks the focused pane with
the pane on that side; `ctrl+b P u` takes it out again. A folded pane opens when it gets the focus,
whatever brings it there: `alt+↑`/`alt+↓` (they go through the stack first), a click on its row,
the pane finder, `alt+a`. Opening one resizes nothing: every pane of a stack has the size of the
open one. A stack resizes as one tile; `n` from a stacked pane adds a pane to its stack, a split
splits the whole stack's place.

`ctrl+b P l` arranges the tab's panes in the next layout, in their order and with the focus where
it was: even columns, even rows, the first pane on the left and the others in a column on the
right, the same with the others stacked, the first pane on top and the others in a row below, a
grid. The first pane takes three fifths where it is the main one, the top bar names the layout
(`Layout: even rows`), and a layout that would look like one before it is skipped (with three
panes, the grid is the even columns). The tab keeps its layout as panes come and go, until you
resize, split or stack by hand.

In the classic preset: `E` the next layout, `M` then an arrow (or `hjkl`) to stack, `u` to
unstack.

## Copy mode, search and resizing

`ctrl+b v` enters copy mode, where the keys move in the pane's text, its scrollback included:

| Copy mode | |
|---|---|
| arrows, `h` `j` `k` `l` | move |
| `w` · `b` · `e` | next word · previous word · end of word |
| `0` · `$` (or `home` · `end`) | start · end of the line |
| `g` · `G` | top · bottom |
| `pageup` · `pagedown` | a page up · down (`ctrl+u` · `ctrl+d`: half a page) |
| `v` · `V` · `ctrl+v` | select characters · lines · a block |
| `y` (or Enter) | copy the selection and leave |
| `/` · `n` · `N` | search · next match · previous match |
| `p` · `P` | the prompt above · below, with a shell that marks its commands ([Shell commands](terminal.md#shell-commands)) |
| `o` · `O` | select the output of the command at the cursor · the whole command |
| `q` (or Esc) | leave |

`ctrl+b g` searches the output: type the text, `↑↓` go to an older or newer match, Enter keeps
the match and goes on in copy mode, Esc cancels. `ctrl+b P r` resizes: the arrows (or `hjkl`)
move the focused pane's border, Enter or Esc ends it.

An account login pane has its own ending: Enter closes it after failure or cancellation,
`r` explicitly tries the login again, and `c` checks sign-in again after an unsuccessful check.
A successful, verified sign-in closes the pane automatically.

## Lists, fields and questions

In lists, type to filter, `↑↓` move, Enter chooses, **Tab shows what else can be done** with the
selected item (connect in a new tab, browse its files, run a command…), each also on a key of its
own that the list shows, Esc closes. The footer says what Enter does on the selected line;
only actions that apply to that line are offered. When there are no other actions, Tab moves
to the next line. Text fields edit like a shell line (arrows, `home`/`end`,
`alt+←→`, `ctrl+w`, `ctrl+u`, `ctrl+k`). Questions show two buttons, Cancel first: `y` answers
yes, `n` or Esc no, Enter takes the selected button (`←→` or Tab change it). A question longer
than its box says how many lines are below and scrolls: `↑↓` (or `j` `k`) by a line, `pageup`
`pagedown` (or `space`) by what the box shows, `home` (or `g`) back to the top. What a question
quotes, like the commands a runbook would run, keeps its spacing; a line too long for the box
goes on in the rows below it, each marked `↪` in the margin.
`ctrl+b V j` lists the messages kraal showed, warnings included.

## Your own keys

Every command and every menu can move, in `~/.config/kraal/config.toml`
([an example](../examples/config/keys-classic.toml), on the classic preset):

```toml config
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

`kraal config keys` prints every key, menu and mode, with the names to use here
(`kraal config keys --markdown` for a sheet to keep or print): each key of a command has its line,
and a command you gave other keys stays where it was among the others, as in the menu and the
help. `kraal config check` reports a key bound twice or a sequence that hides another;
`kraal config reload` applies the file to the running session (keys, theme, sidebar, bottom bar,
mouse, frames, tools, hints, hooks; new panes take a new shell or scrollback length).

## Mouse

The mouse works too: click to focus panes, tabs, workspaces and agents (a click on the workspace
name opens the list, a click on a folded pane's row opens it), or to run an action of the bottom
bar; drag borders to resize; drag floating panes by their title; wheel to scroll
back; drag to select and copy (double click selects a word, triple click a line); Ctrl+click opens
a link (OSC 8 links stay clickable, and plain URLs work too), or, on the top line of a pane's
frame, marks the pane for broadcast. Hold Shift to use your terminal's own selection.
`mouse = false` in the configuration leaves the mouse to your terminal.

## Paste buffers

What you copy goes to your terminal's clipboard, and kraal keeps it too: a copy mode selection, a
mouse selection, `ctrl+b V c` (the last command's output), a hint's text, and what programs copy.
The newest copy is buffer 0, the one before it buffer 1, and so on; the same text copied twice in
a row is kept once. At most 50 numbered buffers and 32 MB stay, the oldest going first; buffers
set by name (`kraal buffer set notes`, see [Scripting](scripting.md#paste-buffers)) stay until
deleted.

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
