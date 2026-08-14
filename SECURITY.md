# Security Policy

## Reporting a vulnerability
Please **do not** open a public issue for security problems. Email **seif.ibrahim@iseif.dev** or use GitHub private vulnerability reporting. We aim to acknowledge within 72 hours.

## Areas of particular concern
- **Agent tool execution.** Commands requested by an ACP agent are permission-gated and shown as argv arrays; we never interpolate agent output into a shell string. Report any bypass.
- **Permission escalation** across workspace boundaries (out-of-workspace reads/writes an agent could trigger).
- **Untrusted project content** influencing the editor, LSP, or agent context.

## Supported versions
Pre-alpha: only `main` is supported. A formal support policy accompanies the `0.1.0` beta.
