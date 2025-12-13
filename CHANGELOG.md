# Changelog

All notable changes to this project will be documented in this file.

The format is inspired by [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [1.0.0] - 2025-12-13

### Added

- **Spring Cloud Config compatible API**:
  - `GET /{env}/{application}/{profile}`
  - `GET /{env}/{application}/{profile}/{label}`
  - Returns `name`, `profiles`, `label`, `version` (Git commit hash), and `propertySources[]` in the same shape as the official Spring Cloud Config Server.
- **YAML resolution logic** with Spring-like priority:
  - `<application>-<profile>.yml`
  - `<application>-<profile>.yaml`
  - `<application>.yml`
  - `<application>.yaml`
  - `application-<profile>.yml`
  - `application-<profile>.yaml`
  - `application.yml`
  - `application.yaml`
- **Templating engine** for all text assets:
  - Simple `{{ VAR_NAME }}` substitution.
  - Variables taken from a merged env map (process env + root env_file + per-env env_file).
  - Works for YAML and any other text file; missing variables become empty strings.
- **Multi-tenant environment model**:
  - `config.yaml` supports a top-level `environments` map.
  - Each environment has its own Git config (`repo_url`, `branch`, `branches`, `workdir`, `subpath`).
  - Each environment can have its own `env_file`.
- **Single-instance mode**:
  - If `environments` is omitted, the server falls back to a single global `git` config.
- **Env helpers** for non-Spring clients:
  - `GET /{env}/env` – effective env map as JSON.
  - `GET /{env}/env/export` – env as shell `export` statements.
- **Asset API**:
  - `GET /{env}/assets` – list all files under the Git subpath (default branch).
  - `GET /{env}/assets/{path}` – download a single file from the default label.
  - `GET /{env}/assets/{label}/{path}` – download a single file from the given label (branch/tag).
  - Auto-detects text vs. binary:
    - binary → `application/octet-stream` (or guessed MIME),
    - text → templated and served as UTF‑8 with a sensible MIME type based on extension.
- **Branch / label support**:
  - `GitConfig` now supports `branch` (default) and `branches` (allowed labels).
  - Labels are resolved as `origin/<label>` unless they already contain a `/`.
  - Git fetch logic updated to fetch all branches into `refs/remotes/origin/*`, even if the original clone was single-branch.
- **Environment-wide health endpoints**:
  - `GET /healthz` – basic process health (`status`, `startup_time`).
  - `GET /healthz/env` – status for all environments (env name, env var count, file count).
  - `GET /healthz/env/{env}` – status for a single environment.
- **HTML UI** (`/ui`):
  - Dark theme, responsive layout.
  - Fixed environment list (always visible when scrolling).
  - Shows per-environment:
    - Git repo URL,
    - default branch,
    - subpath,
    - workdir,
    - last commit hash,
    - commit date.
  - Shows effective env:
    - as JSON,
    - as shell exports.
  - Asset browser:
    - lists files under the Git subpath,
    - clicking a file shows the **templated content**.
  - Spring config JSON preview:
    - simulates the Spring call `/{env}/{application}/default`,
    - displays the merged JSON in a dedicated panel.
  - Copy-to-clipboard buttons for all main text areas.
- **Authentication**:
  - Optional **Basic Auth** using `AUTH_USERNAME` and `AUTH_PASSWORD` env vars.
  - Optional **X‑Client‑Id auth** configured in `config.yaml`:
    - `header_name` (e.g. `x-client-id`),
    - multiple clients with:
      - `id`,
      - `description`,
      - `environments` (list or `["*"]`),
      - `scopes` (`config:read`, `files:read`, `env:read`),
      - `ui_access` (boolean).
  - Auth precedence:
    - valid Basic Auth → grants full access (independent of X‑Client‑Id),
    - else X‑Client‑Id is evaluated against env + scopes,
    - else open access (if both systems are disabled) or `401`.
- **Base path support**:
  - All endpoints can be prefixed with `http.base_path` (e.g. `/config`), which is transparently applied to Spring, env, asset, health, and UI routes.
- **Ordering guarantees**:
  - Per-file property maps use `IndexMap` internally so keys preserve the same order as in the original YAML.
  - `propertySources` list preserves the same priority order as Spring’s resolution logic.
- **Clippy- and fmt-clean codebase**:
  - `cargo fmt` and `cargo clippy -- -D warnings` both pass on the 1.0.0 codebase.

### Changed

- Renamed the project to **simple-config-server** (older internal names like `secure-config-server` are no longer used).
- Switched fully from `/files` to `/assets` in the public HTTP API for consistency:
  - `/env/assets`,
  - `/env/assets/{path}`,
  - `/env/assets/{label}/{path}`.
- Updated configuration model:
  - deprecated old single `branch`‑only semantics, in favour of `branch` + `branches[]`,
  - clarified semantics for single-instance vs. multi-tenant modes,
  - aligned README with the final structure of `config.yaml`.
- Tightened UI access semantics:
  - `/ui` requires either valid Basic Auth credentials or a client configured with `ui_access: true` (if X‑Client‑Id auth is enabled).

### Removed

- Legacy assumptions from early prototypes (e.g. Spring Cloud Config Server as a hard dependency).
- Old `/file/…` and `/files/…` routes in favour of the unified `/assets` naming.

