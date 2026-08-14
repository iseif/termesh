# Configuration

Termesh layers user configuration over compiled defaults. A missing file is normal. A malformed
file reports the file, line when available, problem, and fallback inside the app; it never makes
the editor unusable. Use the `config.reload` action from the command palette to reread both files
without restarting.

## File locations

| Platform | Directory |
|---|---|
| Linux and macOS | `$XDG_CONFIG_HOME/termesh/`, or `~/.config/termesh/` when `XDG_CONFIG_HOME` is unset |
| Windows | `%APPDATA%\termesh\` |

The global settings file is `config.toml`; key overrides are in `keymap.toml`. Agent definitions
remain in `agents.toml`, and project-specific tasks, language overrides, and command permissions
remain in `<project>/.termesh/workspace.toml`.

## `config.toml`

Every key is optional. Values not named in the file keep their compiled default.

| Key | Type | Default | Effect |
|---|---|---|---|
| `version` | unsigned integer | `1` | Configuration schema version. Omitting it means version 1. |
| `theme` | string enum | `"dark"` | Selects the compiled palette. `"dark"` is the only named theme in 0.1.0; detected colour depth still selects its true-colour, 256/16-colour, or no-colour rendering. |
| `shell` | string or absent | platform default | Executable used for new human terminal tabs. The string is a program path, not a shell command line. |
| `tab_width` | integer `1..=16` | `4` | Display width of tab stops. Out-of-range values are clamped and diagnosed. |
| `soft_wrap` | boolean | `true` | Reserved and parsed, but not applied in 0.1.0; setting it produces a diagnostic explaining the deferral. |
| `autosave` | `"off"` or `{ debounced = { seconds = N } }` | `{ debounced = { seconds = 2 } }` | Controls crash-recovery draft mirroring. It never saves over the user's file. |
| `exclusions` | array of strings | `[]` | Additional gitignore-style patterns hidden from the explorer, file discovery, and agent workspace snapshot. |

Example:

```toml
version = 1
theme = "dark"
shell = "/bin/zsh"
tab_width = 4
soft_wrap = true
autosave = { debounced = { seconds = 2 } }
exclusions = [".cache/", "generated/**"]
```

To disable crash-recovery drafts:

```toml
autosave = "off"
```

## `keymap.toml`

The top-level keys are `version` plus five context tables. Each table maps a quoted chord to a
stable action ID. User bindings overlay the complete default keymap; they do not replace it.

| Key or table | Default | Effect |
|---|---|---|
| `version` | `1` | Keymap schema version. Omitting it means version 1. |
| `[global]` | empty overlay | Applies regardless of focused pane unless a focused context shadows the chord. |
| `[project]` | empty overlay | Applies while the Project explorer is focused. |
| `[editor]` | empty overlay | Applies while an editor buffer is focused. |
| `[terminal]` | empty overlay | Applies to client-owned terminal routes and terminal copy mode. Normal terminal input otherwise goes directly to the PTY. |
| `[agent]` | empty overlay | Applies while the Agent pane is focused. |

Use F11 (`help.show`) to see registered actions and live bindings. Action IDs are the values shown
in the command palette and help, such as `git.show`, `lsp.format`, and `help.show`.

```toml
version = 1

[global]
"alt+g" = "git.show"
"f11" = "help.show"

[editor]
"alt+g" = "lsp.format"
```

Within one context, a later binding for a chord replaces the compiled binding for that context.
A focused context wins over `[global]`. An unknown context, action, key, or modifier is diagnosed;
valid sibling bindings still load. If the TOML itself is malformed, the whole overlay is skipped
and the compiled keymap remains intact.

## Chord grammar

A chord is case-insensitive for modifier and named-key spelling, with `+` between parts and exactly
one key last: `ctrl+shift+p`, `alt+enter`, `f12`, or `shift+pageup`. Modifiers may appear in any
order. Printable character keys are case-sensitive: `a` and `A` are distinct terminal inputs.

| Part | Accepted names |
|---|---|
| Modifiers | `ctrl`, `alt`, `shift` |
| Navigation | `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown` |
| Editing/control | `enter`, `esc` or `escape`, `tab`, `backtab`, `backspace`, `delete` or `del`, `space` |
| Function keys | `f` followed by an unsigned byte-sized number, normally `f1` through `f12` |
| Character | One Unicode scalar value, for example `p`, `/`, or `A` |

Legacy terminal input cannot distinguish every keyboard combination. The loader rejects `Ctrl+I`
(`Tab`), `Ctrl+M`/`Ctrl+J` (`Enter`), `Ctrl+H` (`Backspace`), `Ctrl+[` (`Esc`), the NUL family
``Ctrl+` ``, `Ctrl+@`, and `Ctrl+Space`, and `Ctrl+Shift+letter` (the Shift bit is lost). An `Alt`
modifier makes a distinct escape sequence, so combinations such as `Alt+I` remain valid. Some
terminal or operating-system settings may still consume function or Alt keys before the app sees
them; keep a function-key route for portability.

## Migration contract

No `version` means schema 1. The current version loads as written. An older version is migrated in
memory and is rewritten only if the app later writes that owned state for another reason. A newer
version loads fields this build understands and reports that newer fields may be ignored. Unknown
keys are reported and left untouched on disk. Reading configuration never rewrites it, and a parse
failure always falls back to compiled settings or the compiled keymap plus a diagnostic
(ADR-0014 §2–§3).
