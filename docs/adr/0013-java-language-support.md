# 13. Java support uses wrapper-owned JDT LS launch and conventional build tasks

Date: 2026-08-11

## Status
Accepted

## Context

Phase 09 adds Java as the fifth supported language: Eclipse JDT LS for language intelligence,
Maven and Gradle tasks, and navigable Java compiler failures. It exercises the polyglot seams from
Phase 08 without changing them. ADR-0011's long-lived sessions, `LspServerId` routing, live-buffer
document text, `LspState`-owned wire versions, transaction-safe server edits, and mandatory replies
to inbound requests remain fixed. ADR-0012's extension routing, lazy first-document startup,
root-only project detection, synchronous configuration-reading task discovery, and declared task
escape hatch also remain fixed. `crates/lsp` continues to depend on exactly `termesh-core` and
`serde_json`, and no test may require a JDK, JDT LS, Maven, or Gradle.

Earlier planning assumed that Termesh would discover the Eclipse equinox launcher, select an
operating-system configuration directory, and manage a persistent JDT LS workspace-data directory.
That is unnecessary when JDT LS is invoked through the launcher script supplied by Eclipse JDT LS
and common package managers. The launcher already owns those details. ADR-0012's context described
launcher discovery as anticipated Phase 09 work but did not decide its implementation; this ADR
records the narrower decision before production work begins.

Java also exposes three assumptions that were previously harmless. It is the first project kind
with several root markers, so the current marker scan can report the same kind more than once. Its
project kind does not identify its build system because Maven and Gradle can coexist. Finally,
ADR-0012 Section 3 explicitly deferred the question of asynchronous task discovery to this phase,
where real Gradle enumeration would add seconds of process work and daemon startup to workspace
open.

JDT LS vendor notifications remain wire-boundary details translated in
`crates/lsp/src/protocol.rs`; no Java-specific protocol type may reach the editor, UI, or model.
This phase deliberately excludes `workspace/executeCommand`-backed actions such as organize imports
and generating getters or setters because they add new client protocol surface. It also excludes
JUnit test running, which belongs to the Phase 11 test explorer. Phase 09 adds Java context for the
agent, not Java tools.

## Options considered

### A. Use the `jdtls` wrapper, deduplicate detection, probe build files, and keep conventional tasks

Treat Java as another static language recipe, make project-kind detection set-like while preserving
marker priority, let the Java task adapter inspect Maven and Gradle build files independently, and
offer a bounded conventional task catalog. Prefer project wrappers when they exist and retain
`.termesh/workspace.toml` declared tasks for project-specific commands.

### B. Launch Eclipse equinox directly and model Maven and Gradle as separate project kinds

Termesh could discover a versioned launcher jar, resolve installation roots, choose per-platform
configuration directories, and own JDT LS workspace data. Separate Maven and Gradle kinds could
then select separate adapters. This duplicates work already done by the launcher, requires
filesystem and persistent-cache support at the language boundary, and misrepresents repositories
that contain both build systems.

### C. Invoke build tools to enumerate their complete task graphs asynchronously

A worker could run Maven or Gradle and replace the task catalog when enumeration completes. This
would discover plugins and custom tasks, but Gradle enumeration commonly costs five to thirty
seconds and may warm a daemon on every workspace open. It adds process execution and an asynchronous
catalog lifecycle before a concrete project has shown that the existing conventional and declared
task mechanisms are insufficient.

We take Option A.

## Decision

### 1. Java launches through a `jdtls` launcher script on `PATH`

The built-in Java recipe invokes the single command `jdtls`. Termesh does not glob for an equinox
launcher jar, resolve `JDTLS_HOME`, branch over operating-system `-configuration` directories, or
choose and manage a `-data` directory. The wrapper owns all of those launch and persistence details.

This keeps `recipe_for("java")` a static row like the four existing recipes, preserves the rule that
`crates/lsp` needs no filesystem access, and requires no persistent-cache addition to
`platform::paths`. A user with an unpacked Eclipse JDT LS tarball but no launcher wrapper must set
`[lsp.java].command` to a suitable launcher command. That is the same documented override available
to every other language recipe, not missing automatic discovery.

### 2. Project-kind detection deduplicates kinds in marker-priority order

Several root markers may map to one `ProjectKind`. Detection lists each kind once and preserves the
first occurrence from the marker table; it does not sort the result. The primary `kind` remains the
first entry in `kinds`, preserving ADR-0012's contract and established marker priority.

Java is the first kind with more than one marker: `pom.xml`, `build.gradle`, and
`build.gradle.kts`. Without this guard, a mixed or migrating repository can report Java repeatedly,
render duplicate labels, and configure several recipes that claim the same `.java` documents while
only the first can start.

### 3. Build system is a separate axis from `ProjectKind`

`ProjectKind::Java` means that the root contains a Java project; it does not mean Maven or Gradle.
The kind selects the Java adapter, and that adapter probes for `pom.xml`, `build.gradle`, and
`build.gradle.kts` to choose which tasks to offer. A root containing both build systems offers both
catalogs. This mirrors the Node adapter, which reads `package.json` rather than encoding package
manager or script details in `ProjectKind`.

### 4. Project wrappers are preferred and resolved as absolute paths

Maven tasks use the project wrapper when present and otherwise use `mvn`; Gradle tasks use the
project wrapper when present and otherwise use `gradle`. Wrapper programs are absolute paths under
the workspace root, so their meaning does not depend on process working-directory interpretation.
On Windows the platform-specific wrapper names are used: `mvnw.cmd` and `gradlew.bat`.

Wrappers pin the tool version expected by the repository. This is the same class of decision as
Phase 08's lockfile-driven package-manager selection: it prevents silently running a different
system version while remaining invisible when configured correctly.

### 5. Asynchronous task discovery is not introduced yet

ADR-0012 Section 3's deferred async-discovery question resolves as "not yet." Maven and Gradle use
bounded conventional task lists. Real Gradle enumeration would cost five to thirty seconds plus a
daemon warmup on workspace open, while the conventional list covers the tasks most people run and
`[tasks.*]` already lets a workspace declare exact custom commands.

The reconsideration point is no longer a phase number. Async enumeration should be reconsidered
when a concrete project demonstrates useful tasks that the conventional list cannot express, so
the required lifecycle can be designed against evidence rather than anticipated breadth.

## Consequences

**Java stays a narrow addition to the polyglot architecture.** The language recipe is static,
`crates/lsp` retains its complete two-dependency set, and `platform::paths` gains no cache or JDT LS
installation concept. Tarball-only installations require an explicit command override.

**Detection remains deterministic and becomes safe for aliases.** Multiple markers can identify
one kind without duplicating status, recipes, tasks, or agent context. First-occurrence ordering
continues to choose the primary project kind.

**Mixed Java builds are represented honestly.** Maven and Gradle can coexist and contribute tasks
independently. Running from the workspace root means a multi-module reactor operates on all modules;
per-module task catalogs remain out of scope.

**Project-pinned tools win without shell ambiguity.** Absolute wrapper paths preserve the project's
chosen Maven or Gradle version across platforms. Repositories without wrappers continue to use the
corresponding tool on `PATH`.

**Workspace open stays immediate and deterministic.** Task discovery reads bounded configuration
and invokes no build tool. The catalog will omit unconventional build tasks unless the workspace
declares them, an accepted cost with an existing escape hatch and an evidence-based reconsideration
condition.

**The excluded protocol and test surfaces remain deferred.** `workspace/executeCommand` actions,
JUnit test execution, equinox discovery, per-module tasks, Go, and Dart are not authorized by this
decision. This ADR was approved before Phase 09 implementation began.
