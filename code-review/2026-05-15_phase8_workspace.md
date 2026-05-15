# Code Review — Phase 8 (PR #7)

**Date:** 2026-05-15T17:07:47Z
**Branch:** phase8
**Reviewed by:** Claude (review command)
**Scope:** PR #7 — Dockerfile, GitHub Actions CI/CD, example configs, deployment/ops/protocol docs
**Files reviewed:**
- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`
- `docker/Dockerfile`
- `docker/docker-compose.example.yml`
- `examples/config.minimal.toml`
- `examples/config.production.toml`
- `examples/docker-compose.with-postgres.yml`
- `docs/deployment.md`
- `docs/operations.md`
- `docs/webhook-protocol.md`
- `CLAUDE.md`

> No Rust source changed in this PR — most of the checklist sections (Async/Tokio, sqlx, Repo trait, HMAC, etc.) are inapplicable. Findings focus on Docker/CI/docs.

---

## Findings

### Finding 1 — Compose healthchecks use `wget`, which is not in the runtime image

| Field | Value |
|-------|-------|
| **File:Line** | `docker/docker-compose.example.yml:48`, `examples/docker-compose.with-postgres.yml:53` |
| **Severity** | Critical |
| **Category** | Correctness |

**Problem**

Both example compose files run `wget -qO- http://localhost:9090/health || exit 1` as the dispatcher healthcheck, but the runtime stage in `docker/Dockerfile` builds `FROM debian:bookworm-slim` and only installs `ca-certificates`. `wget` is **not** present in `bookworm-slim`. Every healthcheck will exit with `command not found`, so Docker will report the dispatcher unhealthy and `depends_on: condition: service_healthy` chains downstream will never see it up. Both example configurations are unusable as written.

**Context** (surrounding code as it exists today)

```yaml
# docker/docker-compose.example.yml lines 47-52
healthcheck:
  test: ["CMD-SHELL", "wget -qO- http://localhost:9090/health || exit 1"]
  interval: 10s
  timeout: 5s
  retries: 5
  start_period: 15s
```

```dockerfile
# docker/Dockerfile lines 47-49
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*
```

**Recommended fix**

Pick one of these two approaches and apply it consistently:

Option A — add a static healthcheck binary to the image (preferred, smallest):

```dockerfile
# docker/Dockerfile — replace the apt install line
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates wget && \
    rm -rf /var/lib/apt/lists/*
```

Option B — add a dispatcher subcommand `healthcheck` that does `GET /health` in-process, and use it:

```yaml
# docker/docker-compose.example.yml lines 47-52
healthcheck:
  test: ["CMD", "/app/outbox-dispatcher", "healthcheck"]
  interval: 10s
  timeout: 5s
  retries: 5
  start_period: 15s
```

**Why this fix**

Option A is one extra apt package (~600 KB) and keeps the compose file as-is. Option B is hermetic — no external binary required — but needs a small Rust addition. Either way, the current configuration ships broken.

---

### Finding 2 — Missing `.dockerignore`; build context includes `target/`, `.git/`, `.idea/`

| Field | Value |
|-------|-------|
| **File:Line** | `docker/Dockerfile` (repo root has no `.dockerignore`) |
| **Severity** | High |
| **Category** | Performance / Correctness |

**Problem**

There is no `.dockerignore` at the workspace root. `docker build` therefore packages and sends `target/` (hundreds of MB of Rust build artefacts), `.git/`, `.idea/`, and any locally-mounted `.env.toml` to the Docker daemon as build context. This dramatically slows local and CI image builds, bloats Buildx cache layers, and risks shipping `.env.toml` into the image if it ever ends up tracked or copied by `COPY . .`-style instructions (the current Dockerfile is careful, but a future change could regress).

**Context**

The Dockerfile explicitly copies only the manifests, then `crates/`, `migrations/`, `envs/`, and `.sqlx/` — so today nothing leaks. The issue is purely build-time waste and a footgun for future edits. No `.dockerignore` is checked in:

```
$ ls -la /Users/vova/work/workspace/volmyr/github/outbox-dispatcher/.dockerignore
ls: …/.dockerignore: No such file or directory
```

**Recommended fix**

Add `.dockerignore` at the workspace root:

```gitignore
# Build artefacts
target/
**/target/

# VCS / IDE / OS
.git/
.gitignore
.idea/
.vscode/
.DS_Store

# Local secrets — never bake into images
.env
.env.toml

# Docs and examples are not needed inside the image
docs/
examples/
code-review/
GEMINI.md
README.md
CLAUDE.md

# Test fixtures from testcontainers / cargo
**/*.log
```

**Why this fix**

Mirrors the `.gitignore` for non-source-of-truth files plus excludes anything the runtime image does not need. Shrinks the build context from ~hundreds-of-MB to single-digit MB, and removes the risk of any future `COPY . .` accidentally including secrets.

---

### Finding 3 — Dockerfile dependency-warmup layer swallows real errors with `|| true`

| Field | Value |
|-------|-------|
| **File:Line** | `docker/Dockerfile:29` |
| **Severity** | High |
| **Category** | Correctness |

**Problem**

The dependency-only build step uses `2>/dev/null || true`. If the workspace fails to resolve (e.g. a broken `Cargo.toml`, a missing dependency, an incompatible toolchain), the failure is silently discarded. The subsequent real build then succeeds-or-fails on its own, but you lose all error output from the warmup attempt and lose the entire point of separating dependency compilation from source compilation: when the cache is missing, both steps now compile everything from scratch with no signal that the warmup didn't actually warm anything. It also masks misconfigurations of the stub `main.rs` that should otherwise be caught immediately.

**Context** (surrounding code as it exists today)

```dockerfile
# docker/Dockerfile lines 19-29
RUN mkdir -p crates/core/src \
             crates/http-callback/src \
             crates/admin-api/src \
             crates/bin/src && \
    echo "pub fn _stub() {}" > crates/core/src/lib.rs && \
    echo "pub fn _stub() {}" > crates/http-callback/src/lib.rs && \
    echo "pub fn _stub() {}" > crates/admin-api/src/lib.rs && \
    echo "fn main() {}"      > crates/bin/src/main.rs

RUN cargo build --release --bin outbox-dispatcher 2>/dev/null || true
```

**Recommended fix**

Compile only the dependency graph (no error-suppression):

```dockerfile
# docker/Dockerfile — replace line 29
# Warm the dep graph. The stubs declare no extern crates, so we ask cargo to
# fetch and compile dependencies only (not the workspace binary itself).
RUN cargo fetch --locked && \
    cargo build --release --workspace --all-targets --offline || \
    (echo "warm-up build failed — investigate before relying on layer cache" && exit 1)
```

Or, more reliably, use `cargo chef` (a small first-stage that emits a dependency-only recipe), which is the idiomatic way to cache Rust workspace dependencies in Docker.

**Why this fix**

Silent `|| true` defeats the layer-cache optimisation and hides real problems. If the warmup genuinely cannot work for some workspace shape, the build should fail loudly so the operator can fix the Dockerfile, not paper over it.

---

### Finding 4 — Release workflow does not gate on CI

| Field | Value |
|-------|-------|
| **File:Line** | `.github/workflows/release.yml:1-19` |
| **Severity** | Medium |
| **Category** | Correctness |

**Problem**

`release.yml` runs on `push tags v*.*.*`. There is no dependency on the `CI` workflow having passed on the underlying commit. A maintainer can therefore tag a commit whose `cargo clippy -- -D warnings`, unit tests, or integration tests are red, and the workflow will happily publish broken binaries and a Docker image to GHCR.

**Context**

```yaml
# .github/workflows/release.yml lines 1-7
name: Release

on:
  push:
    tags:
      - "v[0-9]+.[0-9]+.[0-9]+"
```

**Recommended fix**

Add a CI gate that runs the same checks before any artefact is built:

```yaml
# .github/workflows/release.yml — insert a job that runs first
jobs:
  ci:
    name: Verify CI on tagged commit
    uses: ./.github/workflows/ci.yml

  build-binaries:
    name: Build ${{ matrix.target }}
    needs: ci
    runs-on: ${{ matrix.os }}
    # … existing matrix …

  docker:
    name: Docker Image
    needs: ci
    runs-on: ubuntu-latest
    # … existing steps …
```

To use `workflow_call`, also add this to `ci.yml`:

```yaml
# .github/workflows/ci.yml — replace the existing `on:` block
on:
  push:
    branches: [main]
  pull_request:
    branches: [main]
  workflow_call:
```

**Why this fix**

Tags are cheap to create but releases are visible to users. Forcing the same checks the PR ran prevents accidental regressions from shipping.

---

### Finding 5 — `cross` is installed from an unpinned git ref

| Field | Value |
|-------|-------|
| **File:Line** | `.github/workflows/release.yml:55` |
| **Severity** | Medium |
| **Category** | Correctness |

**Problem**

```yaml
- name: Install cross (for cross-compilation)
  if: matrix.cross
  run: cargo install cross --git https://github.com/cross-rs/cross
```

Installing from `main` is non-reproducible: a future cross release with a breaking change (Docker base image swap, env var rename) silently breaks the next release build, and the prior release's binaries cannot be rebuilt bit-for-bit.

**Context**

`release.yml:53-55` (shown above).

**Recommended fix**

Pin to a known-good tag and use a prebuilt binary installer so the step is fast and deterministic:

```yaml
- name: Install cross (for cross-compilation)
  if: matrix.cross
  uses: taiki-e/install-action@v2
  with:
    tool: cross@0.2.5
```

**Why this fix**

`taiki-e/install-action` fetches a prebuilt cross binary in seconds instead of compiling from source (~minutes), and the pinned version means tomorrow's release builds the same way today's did.

---

### Finding 6 — Cloud Run docs suggest `PORT=8080`, but the dispatcher does not read `PORT`

| Field | Value |
|-------|-------|
| **File:Line** | `docs/deployment.md:149` |
| **Severity** | Medium |
| **Category** | Config / Documentation |

**Problem**

The Cloud Run section ends with:

> Set `PORT=8080` or adjust `admin.bind` to match what Cloud Run expects.

The dispatcher loads its admin listener from `admin.bind` (`crates/core/src/config.rs:425-445` plus `crates/bin/src/main.rs:247`). It does **not** consult a `PORT` env var. A Cloud Run deployer following the doc literally — exporting `PORT=8080` — will get a service that ignores the variable and binds to whatever `admin.bind` says (default `0.0.0.0:9090`), causing Cloud Run's health probe on `:8080` to fail and the revision to be marked unhealthy.

**Context**

```markdown
<!-- docs/deployment.md lines 137-149 -->
[admin]
bind = "0.0.0.0:8080"      # Cloud Run routes traffic to $PORT (default 8080)
```

```

Set `PORT=8080` or adjust `admin.bind` to match what Cloud Run expects.
```

**Recommended fix**

Drop the `PORT=` claim and document only the supported lever, or wire a `PORT` override into config loading. Doc-only fix:

```markdown
Cloud Run injects a `$PORT` env var (default `8080`). `outbox-dispatcher` does
not read `$PORT` directly — set `admin.bind = "0.0.0.0:8080"` (or pass
`APP__ADMIN__BIND=0.0.0.0:8080`) so the admin listener matches.
```

Code-only fix (if you want the doc claim to be true): in `AppConfig::load` add

```rust
.set_override_option(
    "admin.bind",
    std::env::var("PORT").ok().filter(|s| !s.trim().is_empty())
        .map(|p| format!("0.0.0.0:{p}")),
)?
```

**Why this fix**

Either the doc matches the code or the code matches the doc. As-is, the most prominent Cloud Run guidance is wrong.

---

### Finding 7 — Compose comment references `examples/config.production.yaml`, but the file is `.toml`

| Field | Value |
|-------|-------|
| **File:Line** | `docker/docker-compose.example.yml:44` |
| **Severity** | Low |
| **Category** | Documentation |

**Problem**

The mount comment names the wrong filename, sending users to a path that does not exist:

```yaml
# Mount your production config (see examples/config.production.yaml).
```

The actual file is `examples/config.production.toml`. The dispatcher only consumes TOML config — there is no YAML loader.

**Context**

```yaml
# docker/docker-compose.example.yml lines 43-46
volumes:
  # Mount your production config (see examples/config.production.yaml).
  # The base envs/ baked into the image are used as fallback defaults.
  - ./config.prod.toml:/app/envs/app_config_prod.toml:ro
```

**Recommended fix**

```yaml
volumes:
  # Mount your production config (see examples/config.production.toml).
  # The base envs/ baked into the image are used as fallback defaults.
  - ./config.prod.toml:/app/envs/app_config_prod.toml:ro
```

**Why this fix**

A user copying the file name out of the comment lands on a 404. One-character fix.

---

### Finding 8 — Dockerfile declares no `HEALTHCHECK`

| Field | Value |
|-------|-------|
| **File:Line** | `docker/Dockerfile:42-69` |
| **Severity** | Low |
| **Category** | Config |

**Problem**

The runtime stage exposes `9090` (admin) but does not include a `HEALTHCHECK` instruction. Users who run the image with `docker run` (no compose, no Kubernetes) get no liveness signal — the image always reports `running`, never `unhealthy`, even when the dispatcher is wedged. This pushes the responsibility entirely onto the orchestrator.

**Context**

```dockerfile
# docker/Dockerfile lines 60-69
USER dispatcher

# Admin API
EXPOSE 9090
# Prometheus metrics
EXPOSE 9091

ENV APP_ENV=prod

ENTRYPOINT ["/app/outbox-dispatcher"]
```

**Recommended fix**

Once Finding 1 is resolved (so the test command actually exists in the image):

```dockerfile
HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=3 \
    CMD wget -qO- http://localhost:9090/health || exit 1
```

Or, with a built-in subcommand:

```dockerfile
HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=3 \
    CMD ["/app/outbox-dispatcher", "healthcheck"]
```

**Why this fix**

A baseline image-level healthcheck means `docker ps` / orchestrator-agnostic deployments get accurate status without each consumer having to re-declare one.

---

## Summary

| # | Title | File:Line | Severity | Category | Status | Notes |
|---|-------|-----------|----------|----------|--------|-------|
| 1 | Compose healthchecks use `wget`, missing from `bookworm-slim` | `docker/docker-compose.example.yml:48`, `examples/docker-compose.with-postgres.yml:53` | Critical | Correctness | TODO | |
| 2 | Missing `.dockerignore`; build context includes `target/`, `.git/` | repo root | High | Performance | TODO | |
| 3 | Dockerfile warmup swallows errors with `\|\| true` | `docker/Dockerfile:29` | High | Correctness | TODO | |
| 4 | Release workflow does not gate on CI | `.github/workflows/release.yml:1-19` | Medium | Correctness | TODO | |
| 5 | `cross` installed from unpinned git ref | `.github/workflows/release.yml:55` | Medium | Correctness | TODO | |
| 6 | Cloud Run docs claim `PORT=8080` is respected, but the binary ignores it | `docs/deployment.md:149` | Medium | Config | TODO | |
| 7 | Compose comment names `.yaml` file that doesn't exist | `docker/docker-compose.example.yml:44` | Low | Documentation | TODO | |
| 8 | Dockerfile declares no `HEALTHCHECK` | `docker/Dockerfile:42-69` | Low | Config | TODO | |

> **Instructions for the implementing LLM:**
> - Change `TODO` to `DONE` once a finding is fully addressed.
> - If a finding is intentionally skipped or cannot be applied, change `TODO` to `SKIPPED` and add a short reason in the **Notes** column.
> - Do not delete rows — the table is the authoritative implementation log.
> - After applying fixes, run the mandatory post-change steps from CLAUDE.md (`cargo fmt`, `cargo check`, `cargo clippy -- -D warnings`, `cargo test`). For doc/CI/Docker-only fixes that do not touch Rust source, the Rust commands are no-ops but should still pass cleanly.
