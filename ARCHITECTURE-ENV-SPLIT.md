# Target Architecture Notes

This note captures the agreed direction for splitting responsibilities around deployment model, env sources, runtime assets, and bootstrap.

## Core Split

The system is split into three source domains:

1. `tsm-deploy-model`
   - Desired deployment state.
   - Input for `kube_build_app`.
   - Contains app model, replicas, resources, mounts, probes, and deployment-time metadata.
   - Does not contain runtime assets.
   - Does not contain `env.secured.json`, `env.unsecured.json`, or `.env`.

2. `tsm-env-sources`
   - Source of truth for runtime environment variables.
   - Today this is primarily:
     - `<env>/env.unsecured.json`
     - `<env>/env.secured.json`
   - Produces resolved runtime outputs such as `.env`.
   - In the future this source may be backed by Vault or another authority.

3. `tsm-config-assets`
   - Source of truth for runtime files and application config.
   - Includes Spring config, text config files, scripts, and optionally encrypted asset bundles.
   - This is the domain served by `simple-config-server`.

## Runtime Roles

### `kube_build_app`

- Renders manifests from `tsm-deploy-model`.
- Must move away from direct responsibility for `encjson` and env decryption.
- Short-term backward compatibility is acceptable.
- Long-term it should describe bootstrap flow, not resolve env secrets itself.

### `simple-config-server`

- Serves data from `tsm-config-assets`.
- Main responsibility is runtime config and asset delivery.
- It should not be the primary producer of `.env`.
- It may serve `assets.secured.json` as an opaque JSON asset.
- It does not need to decrypt secret bundles when acting only as a delivery server.

### `simple-init`

- Must stay simple.
- Acts as process supervisor / executor only.
- Starts services using already prepared local files and env inputs.
- Does not own env resolution.
- Does not own `encjson` or key handling.

### `simple-secrets-server`

- Owns the domain logic around `env.secured.json` and `env.unsecured.json`.
- This is the correct home for env-source logic.
- It should evolve into a workspace with:
  - core library
  - CLI resolver/export tool
  - server/UI
- Web UI remains useful and should be kept.

## Env Resolution

Resolved env output should be produced by a dedicated resolver utility.

Working concept:

- `env-source-resolver` is a CLI-oriented producer of resolved env outputs.
- It reads `tsm-env-sources`.
- It merges unsecured and secured values.
- It decrypts secured values using `encjson-rs`.
- It produces `.env`, JSON, or YAML exports.

This resolver should come from the same domain as `simple-secrets-server`, ideally as a workspace crate sharing core logic.

## Bootstrap Flow

Reference startup flow:

1. `kube_build_app` renders manifests from `tsm-deploy-model`.
2. Pod starts with shared writable volume(s).
3. Init container runs env resolution:
   - reads `tsm-env-sources`
   - uses `env-source-resolver`
   - writes `/work/env/app.env`
4. Init container fetches runtime config/assets:
   - typically from `simple-config-server`
   - writes files to `/work/config` or `/work/assets`
5. If encrypted asset bundles are used:
   - fetch `assets.secured.json`
   - use `encjson-rs` in bootstrap layer
   - export files into local runtime directories
6. Main container starts.
7. `simple-init` reads local files and starts processes.

## Keys And `encjson-rs`

Target rule: only the bootstrap / env-resolution layer should need keys.

That means:

- `simple-init`: no
- `simple-config-server`: no, when serving opaque bundle files
- `kube_build_app`: ideally no
- env resolver / bootstrap init container: yes

The bootstrap layer may use:

- local keydir
- mounted bootstrap secret
- workload identity
- or another external authority

But the rest of the runtime stack should not need to know how decryption happens.

## Secret Asset Bundles

Encrypted asset bundles remain valid in this architecture.

- `assets.secured.json` is a text JSON container.
- It may contain binary payloads encoded inside the bundle.
- `simple-config-server` may serve it as a regular asset with JSON content type.
- Decryption and export of its contents happen in the pod bootstrap layer, not in `simple-config-server`.

This allows JKS and similar binary files to travel via the asset domain without making `simple-config-server` a secret expander.

## Repo Naming

Current preferred naming direction:

- `tsm-deploy-model`
- `tsm-env-sources`
- `tsm-config-assets`

Existing generated manifests repo can continue using `tsm-deploy`.

## Practical Conclusion

- `env.secured.json` and `env.unsecured.json` belong to the env-source domain.
- `assets.secured.json` belongs to the runtime asset domain.
- `simple-secrets-server` should become the authoritative implementation of env-source behavior.
- `simple-init` should remain a local runtime executor, not a downloader or secret resolver.
