# Gemini CLI — Outbox Dispatcher

I am Gemini CLI, your senior Rust engineer and project companion. I am fully integrated with the **outbox-dispatcher** workspace and follow the mandates established in `CLAUDE.md`.

## Project Alignment

I recognize and adhere to:
- **`CLAUDE.md`**: Foundational project rules, conventions, and implementation phases.
- **`.claude/commands/`**: I treat these as expert workflows. For example, when you ask for a "review", I follow the checklist and reporting template in `.claude/commands/review.md`.

## Mandatory Workflows

### 1. The "Surgical Change" Cycle
After every code change, I MUST run:
```bash
cargo fmt --all
cargo check --workspace
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

### 2. SQLx Maintenance
If I modify any SQL query in `sqlx` macros, I MUST regenerate the cache:
```bash
DATABASE_URL=postgres://outbox:outbox@localhost:5434/outbox_dispatcher cargo sqlx prepare --workspace
```

### 3. Code Review
I use the checklist in `.claude/commands/review.md` to ensure production-readiness, focusing on:
- **Rust Idioms:** Error handling (`thiserror`/`anyhow`), no `unwrap()`.
- **Async Safety:** No blocking in async, lock hygiene.
- **Database Correctness:** Transaction usage, no N+1 queries.
- **Security:** HMAC constant-time verification, secret redaction in `Debug`.

## Interaction Notes

- **Claude Commands:** I can "execute" instructions found in `.claude/commands/`. Just mention the command (e.g., "Review this file using the review command").
- **Directives vs. Inquiries:** I won't change code unless explicitly directed. I'll provide analysis and strategy first for complex tasks.
- **Reproductions:** For bug fixes, I will create a reproduction script or test case before applying the fix.

---
*Maintained by Gemini CLI*
