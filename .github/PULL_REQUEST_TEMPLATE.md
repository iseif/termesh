## What & why

## Phase / crate

## Checklist
- [ ] `cargo fmt --all` clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo test --workspace` passes
- [ ] New service surfaces have a fake in `test-support`
- [ ] Widgets go through service traits (no direct FS/PTY/LSP/ACP)
- [ ] UI change includes a screenshot / recording
- [ ] Load-bearing change (registry / transactions / ACP) has an ADR
