# Language servers

Termesh ships language-server recipes for Rust, TypeScript/JavaScript, Python, and Java. Servers
are not bundled: the editor remains usable when one is absent, and a missing server affects only
the language it owns.

## Built-in recipes

| Root marker | Language | Command | Claimed documents |
|---|---|---|---|
| `Cargo.toml` | Rust | `rust-analyzer` | `*.rs` |
| `package.json` | TypeScript/JavaScript | `typescript-language-server --stdio` | `*.ts`, `*.tsx`, `*.js`, `*.jsx`, `*.mjs`, `*.cjs` |
| `pyproject.toml` | Python | `pyright-langserver --stdio` | `*.py`, `*.pyi` |
| `pom.xml`, `build.gradle`, or `build.gradle.kts` | Java | `jdtls` | `*.java` |

Detection checks every marker at the selected workspace root. It does not search nested
directories for project roots. Several Java build files still produce one Java recipe, while a
root containing Java and another project kind configures both. A Go root remains a supported
editor workspace with no shipped language recipe.

## Lazy sessions

Opening a workspace resolves its recipes but starts no process. A server starts only when the
first open document has an extension that recipe claims; opening a README starts none. Opening a
Rust file and then a TypeScript file in the same workspace starts two independent sessions, and
each receives only its own documents.

This moves configuration failures to first use. If a server is missing or its command fails, the
notification and status name the affected language immediately. Other live sessions keep working.
The status bar follows the session that owns the active buffer, while agent context lists every
detected language and every live session, including idle sides of a polyglot workspace.

## Install the servers

With a rustup-managed Rust toolchain:

```bash
rustup component add rust-analyzer
rust-analyzer --version
```

For TypeScript and JavaScript:

```bash
npm install --global typescript typescript-language-server
typescript-language-server --version
```

For Python:

```bash
npm install --global pyright
pyright-langserver --version
```

For Java, install Eclipse JDT LS so its `jdtls` launcher script is either on `PATH` or named in a
workspace override:

- **macOS:** `brew install jdtls`.
- **Linux:** use a distribution package that exposes `jdtls`, use `brew install jdtls` with
  Linuxbrew, or download an official build from the
  [Eclipse JDT LS download area](https://download.eclipse.org/jdtls/milestones/). For an unpacked
  archive, point `[lsp.java].command` at its `bin/jdtls` script as shown below.
- **Windows:** unpack an official build and invoke its Python launcher through the workspace
  override, or install a package that puts an equivalent `jdtls` wrapper on `PATH`.

JDT LS needs a **Java 17 or newer runtime to run the language server itself**, regardless of the
Java version the project targets. Ensure that runtime is selected by the environment Termesh
inherits (`JAVA_HOME` and `PATH`). A project may still compile to an older Java target.

The executable must be discoverable on the `PATH` inherited by Termesh unless the workspace
supplies an override. Installation is never required to build or test Termesh.

## Override a command

Commands are argv arrays in `<workspace>/.termesh/workspace.toml`:

```toml
[lsp.rust]
command = ["/absolute/path/to/rust-analyzer"]

[lsp.node]
command = ["/opt/node/bin/typescript-language-server", "--stdio"]

[lsp.java]
command = ["/opt/jdtls/bin/jdtls"]
```

Project configuration uses the detected language label, so the TypeScript recipe is overridden
under `[lsp.node]`.

To use a Python language server installed in a workspace virtualenv, point the Python recipe at
that executable. For example, a virtualenv containing basedpyright can use:

```toml
[lsp.python]
command = ["/workspace/.venv/bin/basedpyright-langserver", "--stdio"]
```

On Windows, use the corresponding `.venv\\Scripts\\...exe` path. Termesh does not auto-detect a
virtualenv; interpreter selection and analysis settings remain the language server's
configuration (for pyright, commonly `pyrightconfig.json`).

An unpacked JDT LS tarball is deliberately handled by this override rather than by equinox-jar
discovery. On Windows, where the launcher is commonly invoked through Python, use an argv array
such as:

```toml
[lsp.java]
command = ["py", "C:\\Tools\\jdtls\\bin\\jdtls"]
```

The launcher owns equinox-jar selection, the operating-system configuration directory, and JDT
LS workspace data. Termesh does not resolve `JDTLS_HOME`, glob launcher jars, select `config_*`, or
manage `-data`.

Arguments stay in separate array entries and are never parsed as a shell string. A malformed
override is reported with the settings path and reason, then Termesh falls back to built-in
recipes. An override changes only the process command; document routing remains recipe-defined.

## Status, restart, and logs

`LSP starting` means the active document's process is starting or initializing.
`LSP <message> <percent>%` is server-reported indexing progress. Unavailable and failed states
include the language name when several sessions exist.

Java cold starts are visibly longer than the other recipes because JDT LS imports the Maven or
Gradle model and builds its index. A small project may settle in a few seconds; a cold Spring
workspace commonly takes roughly 30–60 seconds. `LSP Importing…` or `LSP Indexing…` means work is
still progressing rather than hung. Saving `pom.xml`, `build.gradle`, `build.gradle.kts`,
`settings.gradle`, or `settings.gradle.kts` asks the live Java session to reload that project
configuration without a manual restart.

`Code: Restart Language Server` restarts every session that has actually started, using the recipe
resolved when the workspace opened. If all recipes are still idle, it reports that no language
server has started yet. Diagnostics and symbols from replaced processes are dropped, and their
open documents are resent from live buffers with fresh wire versions.

A server that exits unexpectedly is relaunched with exponential backoff, capped at three
consecutive attempts. Server stderr is captured in a per-session log bounded to the newest 200
lines; this is the diagnostic record for a `jdtls` wrapper that refuses to start, most often
because it selected an unsuitable Java runtime. There is no dedicated raw-log pane yet, so the app
surfaces the actionable startup, transport, handshake, or exit failure; when more detail is
needed, run the exact configured wrapper argv in a Termesh terminal to see the same stderr.

## Features and actions

Only claimed files are synchronized, always from live buffer text rather than disk. Diagnostics
decorate the editor and merge with task output in Problems; `F8` and `Shift+F8` navigate the list.
The `Code:` action family includes definition, references, hover, completion, document and
workspace symbols, rename, code actions, formatting, and restart.

Completion, formatting, rename, and code-action edits enter through ordinary version-checked
buffer transactions. They remain dirty for review and save. Diagnostics and active-document
symbols also feed bounded agent context; Termesh does not expose language actions as ACP tools.

## Headless demos

These demos require neither a language server nor a TTY:

```bash
cargo run --bin termesh -- --dump-frame --lsp-demo .
cargo run --bin termesh -- --dump-frame --polyglot-demo .
cargo run --bin termesh -- --dump-frame --java-demo .
```

The first renders a synthetic Rust diagnostic and hover. The second detects a synthetic Rust and
TypeScript workspace, routes two lazily created sessions, and displays discovered npm tasks without
spawning a process. The Java frame shows a JDT diagnostic, Maven import progress, and conventional
Maven tasks. None of the demos starts a language server or build tool.
