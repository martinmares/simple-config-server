# simple-config-server

A small Rust service that behaves like a **read‑only Spring Cloud Config Server**, but with a few extras:

* multi‑tenant environments (`dev`, `test`, `ref`, `prod`, …),
* templating of text files using environment variables (`{{ VAR_NAME }}`),
* lightweight HTML UI for inspection and debugging,
* **Bearer JWT** and optional trusted proxy header authentication,
* additional endpoints for non‑Spring clients (raw assets).

It is designed to run inside Kubernetes, but works equally well as a simple binary on your laptop.

---

## 1. High‑level model

The server reads configuration from:

* one or more **Git repositories** (per environment),
* one or more **env files** (`KEY=VALUE` per line),
* optional **process environment** (real OS env vars).

For each logical environment (e.g. `dev`, `test`):

1. The server periodically fetches & resets a local Git workdir.
2. For each request, it:
   * chooses the right environment (`{env}` segment in the URL),
   * optionally chooses a Git **label** (`{label}` = branch or tag),
   * loads and templates YAML files (`application.yml`, `<application>.yml`, profile-specific variants),
   * flattens YAML into a Spring‑style JSON structure.

You can run it in two modes:

* **Single‑instance**: one Git config for everything (`git` at the root of `config.yaml`).
* **Multi‑tenant**: multiple environments under `environments:` in `config.yaml`.

### 1.1 Runtime env contract

`simple-config-server` reads **final `.env` files**.

- Decryption (`env.secured.json` -> dotenv) is out of scope for this service and should be done by initContainer/job/pipeline before startup.
- Recommended production setting is `env_from_process: false` for deterministic behavior.

---

## 2. Configuration (`config.yaml`)

### 2.1 Root structure

```yaml
server:
  bind: "0.0.0.0:8181"
  base_path: "/config"   # optional prefix for all routes

# optional global env sources
env_from_process: true          # take current process env as a base map
env_file: "/app/config/global.env"

# auth config
auth:
  bearer:
    enabled: true
    issuers:
      - name: "simple-idm-jwt"
        kind: "simple-idm-jwt"
        issuer: "https://sso.cloud-app.cz"
        jwks_url: "https://sso.cloud-app.cz/.well-known/jwks.json"
        audience: "simple-config-server"

  trusted_proxy:
    enabled: false

# Either single-instance:
git:
  repo_url: "file:///…/config-repo"
  branch: "main"                  # default branch
  branches: ["main", "release"]   # optional list of allowed labels
  workdir: "/var/lib/simple-config-server/workdir"
  subpath: "dev"                  # optional repo subdirectory
  refresh_interval_secs: 30

# Or multi-tenant:
# environments:
#   dev:
#     git: { ... }
#     env_file: "/app/config/dev.env"
#   test:
#     git: { ... }
#     env_file: "/app/config/test.env"
```

Rules:

* If **`environments`** is present → multi‑tenant mode.
* If **`environments`** is absent and **`git`** is present → single‑instance mode.
* If `env_from_process: true`, then all OS env vars are loaded into a **global env map**.
* If root‑level `env_file` is set, it is loaded and merged into the global map.
* For each environment (`environments.<name>.env_file`), that env file is loaded and overrides global keys.

The final **template env map for a given env** is:

1. process env (if `env_from_process=true`),
2. root `env_file` (if present),
3. per‑environment `env_file` (if present).

Later values override earlier ones.

### 2.2 Git config

`GitConfig` fields:

```yaml
git:
  repo_url: "file:///…/config-repo"
  branch: "main"                  # default branch used when no label given
  branches: ["main", "release"]   # optional list of allowed labels
  workdir: "/var/lib/simple-config-server/dev"
  subpath: "dev"                  # optional path inside the repo
  refresh_interval_secs: 30       # how often to git fetch/reset (seconds)
```

Notes:

* On startup and then every `refresh_interval_secs`, the server runs a `git fetch` and hard reset to the configured ref.
* Internally, `branches` is normalized so that:
  * the default `branch` is always present,
  * the default `branch` is always the first element.

If `branches` is empty, it is treated as `["<branch>"]`.

---

## 3. Spring‑compatible endpoints

All endpoints described here are **relative to `base_path`**. For the default `base_path: "/"` you call them as written. With `base_path: "/config"` you prefix every route with `/config`.

### 3.1 URL shapes

For an environment `env`, application `app`, profile `profile`, and optional git label `label`:

* Default label (uses `git.branch`):

  ```text
  GET /api/v1/tenants/{tenant}/envs/{env}/{app}/{profile}
  ```

* Explicit label (branch / tag / commit-ish):

  ```text
  GET /api/v1/tenants/{tenant}/envs/{env}/{app}/{profile}/{label}
  ```

Example:

```bash
curl -u myuser:mypassword   "http://localhost:8181/api/v1/tenants/default/envs/dev/config-client/default"

curl -u myuser:mypassword   "http://localhost:8181/api/v1/tenants/default/envs/dev/config-client/default/release"
```

### 3.2 YAML resolution & merge order

For each request the server looks for YAML files under the environment’s `git.subpath` in this order:

1. `<app>-<profile>.yml`
2. `<app>-<profile>.yaml`
3. `<app>.yml`
4. `<app>.yaml`
5. `application-<profile>.yml`
6. `application-<profile>.yaml`
7. `application.yml`
8. `application.yaml`

Each file is:

1. loaded from Git (respecting `{label}` if given),
2. treated as **text** and templated (see section 5),
3. parsed as YAML,
4. flattened into a map of dot‑separated keys (`foo.bar.baz`) → JSON values.

For each physical YAML file found you get an entry in `propertySources`, in **the same order as above** (highest‑priority first), e.g.:

```json
{
  "name": "app",
  "profiles": ["dev"],
  "label": "release",
  "version": "86b4bdfa0feaf6d376cab620318df1f00e528314",
  "state": "",
  "propertySources": [
    {
      "name": "file:///…/config-repo/dev/config-client-dev.yml",
      "source": {
        "demo.message": "Hello from RELEASE branch (dev profile)",
        "demo.number": 42
      }
    },
    {
      "name": "file:///…/config-repo/dev/application.yml",
      "source": {
        "logging.level.org.springframework.security.ldap": "TRACE",
        "ui.config.env": "dev"
      }
    }
  ]
}
```

If **no file matches**, the server mimics Spring Cloud Config and returns HTTP `200` with an empty `propertySources` array and `label` set appropriately.

### 3.3 Data types

After templating, YAML is parsed using `serde_yaml_ng`, so basic types are preserved:

* numbers stay numbers (`1`, `1.23`),
* booleans stay booleans (`true`, `false`),
* strings stay strings.

Flattening uses an [`IndexMap`](https://docs.rs/indexmap/) under the hood, so keys in each `source` map keep their original YAML order.

---

## 4. Extra endpoints for non‑Spring clients (assets)

In addition to the Spring‑compatible endpoints, each environment exposes helpers for **raw assets**.

Again, all routes are prefixed by `base_path` if configured.

### 4.1 Asset endpoints

* List all files (relative to `git.subpath`) from the default branch:

  ```text
  GET /api/v1/tenants/{tenant}/envs/{env}/assets
  ```

  Response:

  ```json
  {
    "files": [
      "application.yml",
      "config-client.yml",
      "user-management.yml"
    ],
    "items": [
      {
        "path": "application.yml",
        "kind": "file",
        "opaque": false,
        "templated": true
      },
      {
        "path": "assets.secured.json",
        "kind": "assets-secured-bundle",
        "opaque": true,
        "templated": false
      }
    ]
  }
  ```

* Get a single asset from the **default** label:

  ```text
  GET /api/v1/tenants/{tenant}/envs/{env}/assets/{path}
  ```

  Example:

  ```bash
curl -u myuser:mypassword     "http://localhost:8181/api/v1/tenants/default/envs/test/assets/application.yml"
  ```

* Get a single asset from an explicit label (branch / tag):

  ```text
  GET /api/v1/tenants/{tenant}/envs/{env}/assets/{label}/{path}
  ```

  Example:

  ```bash
curl -u myuser:mypassword     "http://localhost:8181/api/v1/tenants/default/envs/test/assets/release/application.yml"
  ```

Notes:

- text assets are template-expanded using the effective env map
- `assets.secured.json` and `assets.unsecured.json` are served as opaque JSON payloads without template expansion
- this allows clients or initContainers to download encrypted asset bundles and process them locally with `encjson-rs`
- the UI marks these files as `virtual fs` bundles to distinguish them from regular text assets

Semantics:

* The server resolves `{label}` against the `branches` list (and the default `branch`).
* Content type:
  * if the file contains a `0x00` byte or is not valid UTF‑8 → it is treated as **binary** and returned as `application/octet-stream` (or a guessed MIME type).
  * otherwise it is treated as **text**:
    * templating is applied (section 5),
    * MIME type is guessed by extension (`.yml`, `.yaml` → `text/yaml`; `.json` → `application/json`; default `text/plain`).

---

## 5. Templating

Any **text** file goes through a very small templating step:

* Pattern: `{{ VAR_NAME }}` (double curly braces, no extra syntax).
* Lookup: in the effective env map for the addressed environment.
* Missing variables are simply replaced by an empty string.

Example YAML in Git:

```yaml
demo:
  message: "Hello from {{ ENV_NAME }}!"
spring:
  datasource:
    url: "{{ DB_URL }}?currentSchema=app"
    username: "{{ DB_USER }}"
    password: "{{ DB_PASSWORD }}"
    hikari:
      maximumPoolSize: {{ DB_MAX_POOL_SIZE }}
```

With env:

```bash
DB_URL=jdbc:postgresql://localhost:5432/app-dev
DB_USER=demo_user
DB_PASSWORD=s3cr3t
DB_MAX_POOL_SIZE=30
ENV_NAME=local-dev
```

The templated YAML becomes:

```yaml
demo:
  message: "Hello from local-dev!"
spring:
  datasource:
    url: "jdbc:postgresql://localhost:5432/app-dev?currentSchema=app"
    username: "demo_user"
    password: "s3cr3t"
    hikari:
      maximumPoolSize: 30
```

After YAML parsing, `maximumPoolSize` will be a number, not a string.

> The server does **not** do any decryption.
> If you use encrypted env files (for example with `encjson-rs`), decrypt them before starting `simple-config-server` and/or render them into the `.env` files.

---

## 6. HTTP, base path & authentication

### 6.1 HTTP & base path

`http` section:

```yaml
server:
  bind: "0.0.0.0:8181"
  base_path: "/config"
```

Tenancy:

```yaml
tenancy:
  mode: "simple"   # simple | multi
  default_tenant: "default"
```

* `bind` – address and port to bind, e.g. `0.0.0.0:8080`.
* `base_path` – optional prefix. If set to `/config`, all routes are available under that prefix:

  * Spring:
    * `/config/api/v1/tenants/{tenant}/envs/{env}/{application}/{profile}`
    * `/config/api/v1/tenants/{tenant}/envs/{env}/{application}/{profile}/{label}`
  * Asset helpers:
    * `/config/api/v1/tenants/{tenant}/envs/{env}/assets`
    * `/config/api/v1/tenants/{tenant}/envs/{env}/assets/{path}`
    * `/config/api/v1/tenants/{tenant}/envs/{env}/assets/{label}/{path}`
  * Health:
    * `/config/healthz`
    * `/config/healthz/env`
  * UI:
    * `/config/ui`

If `base_path` is `/`, routes are exposed exactly as `/api/v1/tenants/{tenant}/envs/{env}/assets`, `/api/v1/tenants/{tenant}/envs/{env}/{application}/{profile}`, etc.

### 6.2 Authentication

There are two supported authentication mechanisms:

1. **Bearer JWT auth** through configured issuers.
2. **Trusted proxy headers** through `X-Auth-*` headers from a protected auth proxy.

Legacy Basic Auth and `X-Client-Id` authentication are not supported. If these
keys are present in `auth:`, configuration loading fails.

At least one auth mode must grant access for application endpoints. Health
endpoints remain unauthenticated.

#### 6.2.1 Trusted proxy headers

Use this mode only when the server is reachable exclusively through a trusted
reverse proxy that strips all client-supplied `X-Auth-*` headers before setting
trusted values.

```yaml
auth:
  trusted_proxy:
    enabled: true
```

Accepted headers:

```http
X-Auth-Subject: 97173b5f-6277-4aa7-b15e-a6c0b03cf0fd
X-Auth-User: mares
X-Auth-Email: mares@example.com
X-Auth-Groups: simple-config:tenant:default,simple-config:env:test,simple-config:scope:config:read
```

Supported group conventions:

* `simple-config:role:admin` – full access.
* `simple-config:tenant:<tenant>` or `simple-config:tenant:*`.
* `simple-config:env:<env>` or `simple-config:env:*`.
* `simple-config:scope:config:read` for Spring-style config endpoints.
* `simple-config:scope:files:read` for asset endpoints.
* `simple-config:ui` for `/ui` access.

#### 6.2.2 Bearer JWT auth

Use this mode for CLI, CI/CD, service-to-service clients, and Kubernetes /
OpenShift workloads.

```yaml
auth:
  bearer:
    enabled: true
    issuers:
      - name: "simple-idm-jwt"
        kind: "simple-idm-jwt"
        issuer: "https://sso.cloud-app.cz"
        jwks_url: "https://sso.cloud-app.cz/.well-known/jwks.json"
        audience: "simple-config-server"

      - name: "kube-sa-jwt"
        kind: "kube-sa-jwt"
        issuer: "https://openshift.example.com"
        discovery_url: "https://openshift.example.com/.well-known/openid-configuration"
        audience: "simple-config-server"
```

If `jwks_url` is omitted, the server uses `discovery_url`. If both are omitted,
it tries `{issuer}/.well-known/openid-configuration`.

The same group conventions as trusted proxy headers are supported:

* `simple-config:role:admin`
* `simple-config:tenant:<tenant>` / `simple-config:tenant:*`
* `simple-config:env:<env>` / `simple-config:env:*`
* `simple-config:scope:config:read`
* `simple-config:scope:files:read`
* `simple-config:ui`

OAuth scopes `config:read` and `files:read` are also accepted for API reads
when tenant and environment access is granted through groups.

For `kube-sa-jwt`, the server validates that the JWT `sub` matches the
Kubernetes `namespace` and `serviceaccount.name` claims.

#### 6.2.3 Auth precedence & defaults

The authorization logic is:

1. If Bearer JWT auth is enabled and the token grants access → **allow**.
2. Otherwise, if trusted proxy headers are enabled and `X-Auth-*` grants access → **allow**.
3. Otherwise → **401 Unauthorized**.

Health endpoints (`/healthz`, `/healthz/env`) are intentionally **not** protected and always return basic status information.

---

## 7. HTML UI (`/ui`)

A dark‑themed UI (Fomantic‑UI) is available at:

* `GET /ui` (or `${base_path}/ui` if you use a prefix).

It shows:

* a list of configured environments (`dev`, `test`, `ref`, `prod`, …),
* for the selected environment:
  * Repo URL
  * Default branch
  * Subpath
  * Workdir
  * Last commit hash
  * Commit date
* a tree of **assets** (files) under the Git subpath:
  * click a file to see the preview,
  * or trigger a **Spring config JSON preview** (simulates `/api/v1/tenants/{tenant}/envs/{env}/app/default` and shows the merged JSON).

The main preview areas have “copy to clipboard” icons.

Authentication:

* Bearer JWT users need `simple-config:ui` or `simple-config:role:admin`.
* Trusted proxy users need `simple-config:ui` or `simple-config:role:admin`.

---

## 8. Health endpoints

Health endpoints are useful for Kubernetes liveness/readiness probes and basic monitoring.

* Basic process health:

  ```text
  GET /healthz
  ```

  Response:

  ```json
  {
    "status": "UP",
    "startup_time": "2025-12-13T10:00:00Z"
  }
  ```

* Summary for all environments:

  ```text
  GET /healthz/env
  ```

  Response:

  ```json
  {
    "status": "UP",
    "startup_time": "2025-12-13T10:00:00Z",
    "environments": [
      {
        "env": "dev",
        "env_var_count": 42,
        "file_count": 18
      },
      {
        "env": "test",
        "env_var_count": 40,
        "file_count": 19
      }
    ]
  }
  ```

* Detail for a single environment:

  ```text
  GET /api/v1/tenants/{tenant}/envs/{env}/healthz
  ```

  Response:

  ```json
  {
    "status": "UP",
    "startup_time": "2025-12-13T10:00:00Z",
    "env": "dev",
    "env_var_count": 42,
    "file_count": 18
  }
  ```

All of the above are also available under `${base_path}` if configured (e.g. `/config/healthz`).

---

## 9. Example `config/assets` Git layout

A typical multi-environment runtime config/assets repo:

```text
<git_repo_root>/
  dev/
    application.yml
    apps/
      config-client.yml
      user-management.yml
    files/
      nginx/nginx.conf
      vector/vector.yaml
      scripts/start.sh
    secrets/
      assets.secured.json
  test/
    application.yml
    apps/
      config-client.yml
      user-management.yml
    files/
      nginx/nginx.conf
      vector/vector.yaml
  ref/
    application.yml
  prod/
    application.yml
```

Each environment in `config.yaml` points `git.subpath` to the corresponding subdirectory (`"dev"`, `"test"`, …).
Spring-compatible lookup still reads YAML files relative to that subpath. Raw asset endpoints expose the whole tree below it.

---

## 10. Spring Boot integration

The Spring side is standard **Config Client**:

1. Add Spring Cloud Config Client to your Spring Boot app.
2. Point it at `simple-config-server` using `spring.config.import`.

Example:

```bash
java -jar configclient.jar   --spring.profiles.active=dev   --spring.config.import="optional:configserver:http://myuser:mypass@localhost:8899/dev"
```

* The `/dev` segment selects the environment.
* Spring will first request `/api/v1/tenants/default/envs/dev/<app>/default`, then `/api/v1/tenants/default/envs/dev/<app>/dev`, etc.
* `simple-config-server` responds with the same JSON structure as the official Spring Cloud Config Server, including:
  * `name`
  * `profiles`
  * `label`
  * `version` (Git commit hash)
  * `propertySources` (list of file‑backed maps).

---

## 11. License

AGPLv3 License - see [LICENSE](LICENSE) file for details

## Author

Martin Mareš
