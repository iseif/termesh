# Tasks and problems

Press `F5` to choose a workspace task and `Shift+F5` to cancel the newest running task. Human runs
and exact, approved ACP terminal requests share the same task lifecycle, managed terminal, output
decoder, and bounded Problems list. Task programs and arguments are always structured argv values;
Termesh never interpolates a task into a shell string.

## Adapter discovery

Task discovery uses every project marker at the selected root and reads only bounded configuration
files through `FileSystemService`. It does not invoke a build tool, inspect nested project roots, or
require a toolchain on `PATH` merely to populate the picker.

- **Rust:** `Cargo.toml` contributes Check, Build, Test, and Clippy. Cargo runs with JSON diagnostic
  output so rendered compiler messages and precise primary spans reach the terminal and Problems.
- **Node:** `package.json` contributes one task per string-valued `scripts` entry, sorted by script
  name and capped at 128. A task runs `<manager> run <script>` as argv. Script names are data, even
  if they contain shell punctuation.
- **Python:** `pyproject.toml` contributes the conventional `pytest` task. Projects using another
  convention can declare the exact replacement command as an additional workspace task.
- **Maven:** `pom.xml` contributes `clean`, `compile`, `test`, `package`, and `verify`. Termesh uses
  `<root>/mvnw` (`mvnw.cmd` on Windows) when present, otherwise `mvn`.
- **Gradle:** `build.gradle` or `build.gradle.kts` contributes `build`, `test`, `clean`, `check`,
  and `assemble`. Termesh uses `<root>/gradlew` (`gradlew.bat` on Windows) when present, otherwise
  `gradle`.
- **Go:** the marker is detected, but `0.1.0` intentionally supplies no Go task adapter.

Maven and Gradle are probed independently, so a migration workspace containing both build systems
gets both catalogs. Java tasks run with the workspace root as their working directory; at a Maven
reactor or Gradle multi-project root, one selection therefore operates on every module. Per-module
catalogs are deferred. If the conventional list does not fit the project, declare the exact custom
command beside it with `[tasks.<id>]` in `.termesh/workspace.toml` (see the next section).

A malformed or script-free `package.json` contributes no Node tasks without removing Cargo or
Python tasks detected in the same workspace.

## Package-manager selection

Node discovery selects the first lockfile in this order:

1. `pnpm-lock.yaml` → `pnpm`
2. `yarn.lock` → `yarn`
3. `bun.lockb` → `bun`
4. `package-lock.json` → `npm`

With no recognized lockfile it defaults to `npm`. This avoids running npm in a pnpm, Yarn, or Bun
repository and accidentally creating a second lockfile.

## Workspace-declared tasks

Add tasks to `<workspace>/.termesh/workspace.toml`:

```toml
[tasks.smoke]
label = "Smoke"
program = "make"
args = ["smoke"]

[tasks.typecheck]
label = "Typecheck"
program = "npm"
args = ["run", "typecheck"]
```

Each entry requires a display `label`, a non-empty `program`, and an `args` array containing only
strings. Declared tasks are appended to adapter tasks with `workspace.`-prefixed ids, so
`tasks.smoke` becomes `workspace.smoke` and cannot collide with built-in `cargo.*`, `npm.*`,
`python.*`, `maven.*`, or `gradle.*` ids. A command or its arguments may not be supplied as one
shell string.

## Navigable problems

Cargo retains its JSON decoder. Every non-Cargo task uses one line-buffered text matcher for these
shapes:

```text
src/app.ts(12,5): error TS2304: Cannot find name 'foo'
src/app.js:12:5: warning: unused variable
src/main/java/com/example/App.java:12: error: cannot find symbol
  File "app.py", line 12, in handler
```

The Java shape has no column, so it defaults to column 1. A leading Maven `[ERROR]` or `[WARNING]`
prefix is removed before path matching and controls severity when present. The stricter
three-coordinate shape is attempted first so existing compiler output keeps its real column.

Safe relative paths resolve against the task working directory. Absolute paths remain absolute,
and relative paths containing parent traversal are refused. Complete task output reaches the
terminal byte-for-byte even when no problem shape is recognized. Each run retains at most 500
problems; `F8` and `Shift+F8` navigate safe locations.

Multi-line formats such as eslint's stylish output and Jest summaries are intentionally not
matched. They remain readable terminal output.
