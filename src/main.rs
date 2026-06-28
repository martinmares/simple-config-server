use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use axum::{
    Json, Router,
    extract::{OriginalUri, Path as AxumPath, State},
    http::{
        HeaderMap, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE, HeaderName, WWW_AUTHENTICATE},
    },
    response::{Html, IntoResponse, Response},
    routing::get,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::{SecondsFormat, Utc};
use clap::Parser;
use indexmap::IndexMap;
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use mime_guess::MimeGuess;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Number as JsonNumber, Value as JsonValue};
use serde_yaml_ng::Value as YamlValue;
use thiserror::Error;
use tokio::{
    net::TcpListener,
    process::Command,
    sync::RwLock,
    time::{Duration, sleep},
};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

/// ---------- CLI & configuration ----------

#[derive(Parser, Debug)]
#[command(
    name = "simple-config-server",
    version,
    about = "Secure, template-aware config server (Spring Cloud Config compatible)"
)]
struct Cli {
    /// Path to configuration file (YAML)
    #[arg(short, long, value_name = "FILE", default_value = "config.yaml")]
    config: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
struct GitConfig {
    repo_url: String,
    /// Default branch used when no label is provided (e.g. "main")
    #[serde(default = "default_branch_name")]
    branch: String,
    /// Optional list of allowed branches/labels (e.g. ["main", "release"])
    #[serde(default)]
    branches: Vec<String>,
    workdir: PathBuf,
    #[serde(default)]
    subpath: Option<PathBuf>,
    #[serde(default = "default_refresh_interval")]
    refresh_interval_secs: u64,
}

fn default_branch_name() -> String {
    "main".to_string()
}

fn default_refresh_interval() -> u64 {
    30
}

impl GitConfig {
    /// Ensure that `branches` always contains at least the default `branch`,
    /// and that `branch` is the first element in the list.
    fn normalize_branches(&mut self) {
        if self.branches.is_empty() {
            if !self.branch.is_empty() {
                self.branches.push(self.branch.clone());
            }
        } else if !self.branches.iter().any(|b| b == &self.branch) {
            self.branches.insert(0, self.branch.clone());
        } else {
            // Move existing default branch to the front if it's not already
            if let Some(pos) = self.branches.iter().position(|b| b == &self.branch)
                && pos != 0
            {
                let val = self.branches.remove(pos);
                self.branches.insert(0, val);
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ServerConfig {
    bind: String,
    #[serde(default = "default_base_path")]
    base_path: String,
}

#[derive(Debug, Clone, Deserialize)]
struct HttpConfig {
    bind_addr: String,
    #[serde(default = "default_base_path")]
    base_path: String,
}

fn default_base_path() -> String {
    "/".to_string()
}

/// Root configuration supports:
/// - single instance: `git` + optional global env
/// - multi-tenant: `environments` + optional global env
#[derive(Debug, Clone, Deserialize, Default)]
struct ClientIdClientConfig {
    /// Public identifier passed in the header (e.g. "x-client-id")
    id: String,
    /// Optional human readable description
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    /// Allowed tenants, e.g. ["acme"] or ["*"] for all
    #[serde(default)]
    tenants: Vec<String>,
    /// Allowed environments, e.g. ["dev", "test"] or ["*"] for all
    #[serde(default)]
    environments: Vec<String>,
    /// Granted scopes, e.g. ["config:read", "files:read"]
    #[serde(default)]
    scopes: Vec<String>,
    /// Whether this client may access the HTML UI
    #[serde(default)]
    ui_access: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ClientIdAuthConfig {
    /// Turn on header based auth
    #[serde(default)]
    enabled: bool,
    /// Header name to read the client id from (default "x-client-id")
    #[serde(default = "default_client_id_header_name")]
    header_name: String,
    /// List of allowed clients
    #[serde(default)]
    clients: Vec<ClientIdClientConfig>,
}

fn default_client_id_header_name() -> String {
    "x-client-id".to_string()
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RootAuthConfig {
    /// Configuration for X-Client-Id style auth
    #[serde(default)]
    client_id: ClientIdAuthConfig,
    /// Trust X-Auth-* headers from a protected reverse proxy
    #[serde(default)]
    trusted_proxy: TrustedProxyAuthConfig,
    /// Authorization: Bearer JWT auth
    #[serde(default)]
    bearer: BearerAuthConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct TrustedProxyAuthConfig {
    /// Turn on trusted proxy header auth
    #[serde(default)]
    enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct BearerAuthConfig {
    /// Turn on Authorization: Bearer JWT auth
    #[serde(default)]
    enabled: bool,
    /// Configured trusted JWT issuers
    #[serde(default)]
    issuers: Vec<BearerIssuerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct BearerIssuerConfig {
    /// Stable policy-facing name, e.g. simple-idm-jwt or kube-sa-jwt
    name: String,
    /// Issuer kind: simple-idm-jwt | kube-sa-jwt
    kind: String,
    /// Expected JWT iss claim
    issuer: String,
    /// Expected audience. If omitted, aud validation is disabled.
    #[serde(default)]
    audience: Option<String>,
    /// Explicit JWKS URL
    #[serde(default)]
    jwks_url: Option<String>,
    /// OIDC discovery URL used when jwks_url is not set
    #[serde(default)]
    discovery_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RootConfig {
    #[serde(default)]
    server: Option<ServerConfig>,
    #[serde(default)]
    http: Option<HttpConfig>,

    #[serde(default)]
    tenancy: TenancyConfig,

    /// Load process environment into template variables
    #[serde(default)]
    env_from_process: bool,

    /// Optional global env file (KEY=VALUE per line)
    #[serde(default)]
    env_file: Option<String>,

    /// Single-instance mode
    #[serde(default)]
    git: Option<GitConfig>,

    /// Multi-tenant mode (legacy: environments only)
    #[serde(default)]
    environments: HashMap<String, EnvDefinition>,

    /// Multi-tenant mode: tenants -> environments
    #[serde(default)]
    tenants: HashMap<String, TenantDefinition>,

    /// Authentication / authorization configuration
    #[serde(default)]
    auth: RootAuthConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct TenancyConfig {
    #[serde(default = "default_tenancy_mode")]
    mode: String, // simple | multi
    #[serde(default = "default_tenant_name")]
    default_tenant: String,
}

fn default_tenancy_mode() -> String {
    "simple".to_string()
}

fn default_tenant_name() -> String {
    "default".to_string()
}

#[derive(Debug, Clone, Deserialize)]
struct EnvDefinition {
    git: GitConfig,
    #[serde(default)]
    env_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TenantDefinition {
    #[serde(default)]
    environments: HashMap<String, EnvDefinition>,
}

#[derive(Debug, Clone)]
struct EnvState {
    tenant: String,
    name: String,
    git: GitConfig,
    env_map: Arc<HashMap<String, String>>,
}

#[derive(Clone)]
struct ClientIdClient {
    id: String,
    tenants: Vec<String>,
    environments: Vec<String>,
    scopes: Vec<String>,
    ui_access: bool,
}

#[derive(Clone)]
struct ClientIdAuth {
    enabled: bool,
    header_name: HeaderName,
    clients: HashMap<String, ClientIdClient>,
}

#[derive(Clone)]
struct BearerAuth {
    enabled: bool,
    issuers: Arc<RwLock<Vec<BearerIssuer>>>,
}

#[derive(Clone)]
struct BearerIssuer {
    name: String,
    kind: BearerIssuerKind,
    issuer: String,
    audience: Option<String>,
    jwks_url: String,
    jwks: HashMap<String, DecodingKey>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BearerIssuerKind {
    SimpleIdmJwt,
    KubeSaJwt,
}

impl BearerIssuerKind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "simple-idm-jwt" => Some(Self::SimpleIdmJwt),
            "kube-sa-jwt" => Some(Self::KubeSaJwt),
            _ => None,
        }
    }
}

impl ClientIdAuth {
    fn from_config(cfg: &ClientIdAuthConfig) -> Self {
        let mut clients_map = HashMap::new();

        for c in &cfg.clients {
            let envs = if c.environments.is_empty() {
                vec!["*".to_string()]
            } else {
                c.environments.clone()
            };
            let tenants = if c.tenants.is_empty() {
                vec!["*".to_string()]
            } else {
                c.tenants.clone()
            };

            let client = ClientIdClient {
                id: c.id.clone(),
                tenants,
                environments: envs,
                scopes: c.scopes.clone(),
                ui_access: c.ui_access,
            };

            clients_map.insert(client.id.clone(), client);
        }

        let header_name = HeaderName::from_bytes(cfg.header_name.as_bytes())
            .unwrap_or(HeaderName::from_static("x-client-id"));

        if cfg.enabled {
            info!(
                "[auth] X-Client-Id auth enabled ({} clients, header={})",
                clients_map.len(),
                header_name.as_str()
            );
        } else if !clients_map.is_empty() {
            warn!("[auth] X-Client-Id clients configured but auth.client_id.enabled=false");
        } else {
            info!("[auth] X-Client-Id auth disabled");
        }

        Self {
            enabled: cfg.enabled,
            header_name,
            clients: clients_map,
        }
    }

    fn get_client<'a>(&'a self, headers: &'a HeaderMap) -> Option<&'a ClientIdClient> {
        if !self.enabled {
            return None;
        }
        let value = headers.get(&self.header_name)?;
        let id = value.to_str().ok()?;
        self.clients.get(id)
    }
}

#[derive(Debug, Deserialize)]
struct OidcDiscoveryDocument {
    jwks_uri: String,
}

async fn load_jwks(url: &str) -> Result<HashMap<String, DecodingKey>, ServerError> {
    let body = reqwest::get(url)
        .await
        .map_err(|err| ServerError::Other(format!("failed to load JWKS {url}: {err}")))?
        .text()
        .await
        .map_err(|err| ServerError::Other(format!("failed to read JWKS {url}: {err}")))?;
    let set: JwkSet = serde_json::from_str(&body)?;
    let mut map = HashMap::new();
    for jwk in set.keys {
        if let Some(kid) = jwk.common.key_id.clone() {
            let key = DecodingKey::from_jwk(&jwk)
                .map_err(|err| ServerError::Other(format!("invalid JWKS key {kid}: {err}")))?;
            map.insert(kid, key);
        }
    }
    Ok(map)
}

async fn discover_jwks_uri(url: &str) -> Result<String, ServerError> {
    let body = reqwest::get(url)
        .await
        .map_err(|err| ServerError::Other(format!("failed to load OIDC discovery {url}: {err}")))?
        .text()
        .await
        .map_err(|err| ServerError::Other(format!("failed to read OIDC discovery {url}: {err}")))?;
    let doc: OidcDiscoveryDocument = serde_json::from_str(&body)?;
    let jwks_uri = doc.jwks_uri.trim();
    if jwks_uri.is_empty() {
        return Err(ServerError::Other(format!(
            "OIDC discovery document {url} has empty jwks_uri"
        )));
    }
    Ok(jwks_uri.to_string())
}

impl BearerAuth {
    async fn from_config(cfg: &BearerAuthConfig) -> Result<Self, ServerError> {
        if !cfg.enabled {
            info!("[auth] Bearer JWT auth disabled");
            return Ok(Self {
                enabled: false,
                issuers: Arc::new(RwLock::new(Vec::new())),
            });
        }

        let mut issuers = Vec::new();
        for issuer_cfg in &cfg.issuers {
            let kind = BearerIssuerKind::parse(&issuer_cfg.kind).ok_or_else(|| {
                ServerError::BadRequest(format!(
                    "unsupported bearer issuer kind `{}`",
                    issuer_cfg.kind
                ))
            })?;
            let jwks_url = if let Some(url) = issuer_cfg
                .jwks_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                url.to_string()
            } else {
                let discovery_url = issuer_cfg
                    .discovery_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        format!(
                            "{}/.well-known/openid-configuration",
                            issuer_cfg.issuer.trim_end_matches('/')
                        )
                    });
                discover_jwks_uri(&discovery_url).await?
            };
            info!(
                "[auth] loading Bearer issuer {} kind={} issuer={} jwks={}",
                issuer_cfg.name, issuer_cfg.kind, issuer_cfg.issuer, jwks_url
            );
            issuers.push(BearerIssuer {
                name: issuer_cfg.name.clone(),
                kind,
                issuer: issuer_cfg.issuer.clone(),
                audience: issuer_cfg.audience.clone(),
                jwks_url: jwks_url.clone(),
                jwks: load_jwks(&jwks_url).await?,
            });
        }

        if issuers.is_empty() {
            warn!("[auth] Bearer JWT auth enabled but no issuers are configured");
        } else {
            info!("[auth] Bearer JWT auth enabled ({} issuers)", issuers.len());
        }

        Ok(Self {
            enabled: true,
            issuers: Arc::new(RwLock::new(issuers)),
        })
    }
}

#[derive(Clone)]
struct AuthConfig {
    /// Whether basic auth is required (AUTH_USERNAME/PASSWORD set)
    required: bool,
    username: String,
    password: String,
    /// Optional X-Client-Id based auth
    client_id: ClientIdAuth,
    /// Optional trusted proxy X-Auth-* based auth
    trusted_proxy_enabled: bool,
    /// Optional Authorization: Bearer JWT auth
    bearer: BearerAuth,
}

impl AuthConfig {
    async fn from_env_and_config(auth_cfg: &RootAuthConfig) -> Result<Self, ServerError> {
        let user = std::env::var("AUTH_USERNAME").ok();
        let pass = std::env::var("AUTH_PASSWORD").ok();

        let (required, username, password) = match (user, pass) {
            (Some(u), Some(p)) => {
                info!("[auth] Basic auth enabled");
                (true, u, p)
            }
            _ => {
                warn!("[auth] Basic auth disabled (env AUTH_USERNAME / AUTH_PASSWORD not set)");
                (false, String::new(), String::new())
            }
        };

        let client_id = ClientIdAuth::from_config(&auth_cfg.client_id);
        if auth_cfg.trusted_proxy.enabled {
            info!("[auth] trusted proxy X-Auth-* headers enabled");
        } else {
            info!("[auth] trusted proxy X-Auth-* headers disabled");
        }
        let bearer = BearerAuth::from_config(&auth_cfg.bearer).await?;

        Ok(Self {
            required,
            username,
            password,
            client_id,
            trusted_proxy_enabled: auth_cfg.trusted_proxy.enabled,
            bearer,
        })
    }
}

struct AppState {
    server: ServerConfig,
    envs: HashMap<String, EnvState>,
    auth: AuthConfig,
    startup_time: chrono::DateTime<Utc>,
    tenancy: TenancyConfig,
}

/// ---------- Errors ----------
/// ---------- Errors ----------

#[derive(Error, Debug)]
enum ServerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("UTF-8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("Git error: {0}")]
    Git(String),
    #[error("Not found")]
    NotFound,
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Other error: {0}")]
    #[allow(dead_code)]
    Other(String),
}

/// ---------- Global template regex & UI template ----------
static TEMPLATE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"\{\{\s*([A-Za-z_][A-Za-z0-9_]*)\s*\}\}"#).unwrap());

static UI_TEMPLATE: &str = include_str!("../templates/ui.html");

/// ---------- Main ----------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let cli = Cli::parse();
    info!("[main] Loading config from {}", cli.config.display());

    let root_cfg = load_root_config(&cli.config)?;
    let server_cfg = resolve_server_config(&root_cfg)?;

    // Build global env map
    let mut global_env: HashMap<String, String> = HashMap::new();

    if root_cfg.env_from_process {
        for (k, v) in std::env::vars() {
            global_env.insert(k, v);
        }
    }

    if let Some(ref env_file) = root_cfg.env_file {
        merge_env_file_into(env_file, &mut global_env);
    }

    // Build environments map (tenant/env)
    let mut envs: HashMap<String, EnvState> = HashMap::new();

    if !root_cfg.tenants.is_empty() {
        for (tenant, tenant_def) in &root_cfg.tenants {
            for (name, env_def) in &tenant_def.environments {
                let mut env_map = global_env.clone();
                if let Some(ref path) = env_def.env_file {
                    merge_env_file_into(path, &mut env_map);
                }

                let mut git_cfg = env_def.git.clone();
                git_cfg.normalize_branches();

                envs.insert(
                    env_key(tenant, name),
                    EnvState {
                        tenant: tenant.clone(),
                        name: name.clone(),
                        git: git_cfg,
                        env_map: Arc::new(env_map),
                    },
                );
            }
        }
    } else if !root_cfg.environments.is_empty() {
        // Legacy multi-tenant: environments only (default tenant)
        for (name, env_def) in &root_cfg.environments {
            let mut env_map = global_env.clone();
            if let Some(ref path) = env_def.env_file {
                merge_env_file_into(path, &mut env_map);
            }

            let mut git_cfg = env_def.git.clone();
            git_cfg.normalize_branches();

            envs.insert(
                env_key(&root_cfg.tenancy.default_tenant, name),
                EnvState {
                    tenant: root_cfg.tenancy.default_tenant.clone(),
                    name: name.clone(),
                    git: git_cfg,
                    env_map: Arc::new(env_map),
                },
            );
        }
    } else if let Some(ref git) = root_cfg.git {
        // Single-instance, exposed as logical env "default" under default tenant
        let mut git_cfg = git.clone();
        git_cfg.normalize_branches();

        envs.insert(
            env_key(&root_cfg.tenancy.default_tenant, "default"),
            EnvState {
                tenant: root_cfg.tenancy.default_tenant.clone(),
                name: "default".to_string(),
                git: git_cfg,
                env_map: Arc::new(global_env.clone()),
            },
        );
    } else {
        return Err("config.yaml must contain either `git`, `environments`, or `tenants`".into());
    }

    let auth = AuthConfig::from_env_and_config(&root_cfg.auth).await?;

    // Initial sync for all envs
    for env in envs.values() {
        sync_git_repo(&env.git).await?;
    }

    // Background refresh loops
    for env in envs.values() {
        let git = env.git.clone();
        tokio::spawn(async move {
            git_sync_loop(git).await;
        });
    }

    let state = Arc::new(AppState {
        server: server_cfg.clone(),
        envs,
        auth,
        startup_time: Utc::now(),
        tenancy: root_cfg.tenancy.clone(),
    });

    let app = build_router(state.clone());

    let addr: SocketAddr = state.server.bind.parse()?;
    info!("[main] Listening on http://{}", addr);

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let _ = fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_level(true)
        .try_init();
}

/// ---------- Config helpers ----------
fn load_root_config(path: &Path) -> Result<RootConfig, ServerError> {
    let contents = std::fs::read_to_string(path)?;
    let cfg: RootConfig = serde_yaml_ng::from_str(&contents)?;
    Ok(cfg)
}

fn env_key(tenant: &str, env: &str) -> String {
    format!("{tenant}/{env}")
}

fn resolve_server_config(cfg: &RootConfig) -> Result<ServerConfig, ServerError> {
    if let Some(s) = &cfg.server {
        return Ok(s.clone());
    }
    if let Some(h) = &cfg.http {
        return Ok(ServerConfig {
            bind: h.bind_addr.clone(),
            base_path: h.base_path.clone(),
        });
    }
    Err(ServerError::BadRequest(
        "config.yaml must contain `server` or legacy `http`".to_string(),
    ))
}

fn merge_env_file_into(path: &str, target: &mut HashMap<String, String>) {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    target.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
        Err(e) => {
            warn!("[env] Failed to read env_file {}: {}", path, e);
        }
    }
}

fn normalize_base_path(base: &str) -> String {
    if base.is_empty() || base == "/" {
        "/".to_string()
    } else {
        let trimmed = base.trim().trim_matches('/');
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", trimmed)
        }
    }
}

/// ---------- Git helpers ----------
async fn sync_git_repo(git: &GitConfig) -> Result<(), ServerError> {
    std::fs::create_dir_all(&git.workdir)?;
    let git_dir = git.workdir.join(".git");

    if !git_dir.exists() {
        info!(
            "[git] Cloning {} into {} (branch {})",
            git.repo_url,
            git.workdir.display(),
            git.branch
        );
        let output = Command::new("git")
            .arg("clone")
            .arg("--branch")
            .arg(&git.branch)
            .arg(&git.repo_url)
            .arg(&git.workdir)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ServerError::Git(format!(
                "git clone failed: {}",
                stderr.trim()
            )));
        }
    } else {
        info!(
            "[git] Fetching & resetting repo in {} (branch {})",
            git.workdir.display(),
            git.branch
        );

        let fetch_out = Command::new("git")
            .arg("-C")
            .arg(&git.workdir)
            .arg("fetch")
            .arg("origin")
            .arg("--prune")
            .arg("+refs/heads/*:refs/remotes/origin/*")
            .output()
            .await?;

        if !fetch_out.status.success() {
            let stderr = String::from_utf8_lossy(&fetch_out.stderr);
            return Err(ServerError::Git(format!(
                "git fetch failed: {}",
                stderr.trim()
            )));
        }

        let reset_target = format!("origin/{}", git.branch);
        let reset_out = Command::new("git")
            .arg("-C")
            .arg(&git.workdir)
            .arg("reset")
            .arg("--hard")
            .arg(&reset_target)
            .output()
            .await?;

        if !reset_out.status.success() {
            let stderr = String::from_utf8_lossy(&reset_out.stderr);
            return Err(ServerError::Git(format!(
                "git reset --hard {} failed: {}",
                reset_target,
                stderr.trim()
            )));
        }
    }

    Ok(())
}

async fn git_sync_loop(git: GitConfig) {
    let interval = if git.refresh_interval_secs == 0 {
        30
    } else {
        git.refresh_interval_secs
    };

    loop {
        sleep(Duration::from_secs(interval)).await;
        if let Err(e) = sync_git_repo(&git).await {
            warn!(
                "[git] Periodic refresh failed for {}: {:?}",
                git.workdir.display(),
                e
            );
        }
    }
}

fn build_git_rev(git: &GitConfig, label: Option<&str>) -> String {
    let name = match label {
        Some(l) => l,
        None => &git.branch,
    };

    if name.contains('/') {
        name.to_string()
    } else {
        format!("origin/{}", name)
    }
}
async fn git_version_for_label(
    git: &GitConfig,
    label: Option<&str>,
) -> Result<String, ServerError> {
    let rev = build_git_rev(git, label);
    let output = Command::new("git")
        .arg("-C")
        .arg(&git.workdir)
        .arg("rev-parse")
        .arg(&rev)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ServerError::Git(format!(
            "git rev-parse {} failed: {}",
            rev,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout.trim().to_string())
}

async fn git_commit_date_for_label(
    git: &GitConfig,
    label: Option<&str>,
) -> Result<String, ServerError> {
    let rev = build_git_rev(git, label);
    let output = Command::new("git")
        .arg("-C")
        .arg(&git.workdir)
        .arg("show")
        .arg("-s")
        .arg("--format=%cI")
        .arg(&rev)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ServerError::Git(format!(
            "git show {} failed: {}",
            rev,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout.trim().to_string())
}

async fn read_file_from_git(
    git: &GitConfig,
    label_opt: Option<&str>,
    rel_path: &Path,
) -> Result<Option<Vec<u8>>, ServerError> {
    let mut full_rel = PathBuf::new();
    if let Some(sub) = &git.subpath {
        full_rel.push(sub);
    }
    full_rel.push(rel_path);

    let rel_str = full_rel
        .to_str()
        .ok_or_else(|| ServerError::BadRequest("Non-UTF8 path".to_string()))?
        .replace('\\', "/");

    let rev = build_git_rev(git, label_opt);
    let spec = format!("{}:{}", rev, rel_str);

    let output = Command::new("git")
        .arg("-C")
        .arg(&git.workdir)
        .arg("show")
        .arg(&spec)
        .output()
        .await?;

    if output.status.success() {
        Ok(Some(output.stdout))
    } else {
        Ok(None)
    }
}

async fn list_files_in_git(git: &GitConfig) -> Result<Vec<String>, ServerError> {
    let rev = build_git_rev(git, None);
    let output = Command::new("git")
        .arg("-C")
        .arg(&git.workdir)
        .arg("ls-tree")
        .arg("-r")
        .arg("--name-only")
        .arg(&rev)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ServerError::Git(format!(
            "git ls-tree failed: {}",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8(output.stdout)?;
    let mut files = Vec::new();

    let sub = git
        .subpath
        .as_ref()
        .map(|p| p.to_string_lossy().replace('\\', "/"));

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut rel = line.to_string();
        if let Some(ref subpath) = sub {
            if let Some(stripped) = rel.strip_prefix(&(subpath.clone() + "/")) {
                rel = stripped.to_string();
            } else {
                continue;
            }
        }
        files.push(rel);
    }

    Ok(files)
}

/// ---------- Template & YAML helpers ----------
fn apply_template(input: &str, env: &HashMap<String, String>) -> String {
    TEMPLATE_RE
        .replace_all(input, |caps: &regex::Captures| {
            let key = &caps[1];
            env.get(key).cloned().unwrap_or_else(|| caps[0].to_string())
        })
        .into_owned()
}

fn flatten_yaml_value(
    prefix: Option<&str>,
    value: &YamlValue,
    out: &mut IndexMap<String, JsonValue>,
) {
    match value {
        YamlValue::Null => {
            if let Some(key) = prefix {
                out.insert(key.to_string(), JsonValue::Null);
            }
        }
        YamlValue::Bool(b) => {
            if let Some(key) = prefix {
                out.insert(key.to_string(), JsonValue::Bool(*b));
            }
        }
        YamlValue::Number(n) => {
            if let Some(key) = prefix {
                let json_num = if let Some(i) = n.as_i64() {
                    JsonNumber::from(i)
                } else if let Some(u) = n.as_u64() {
                    JsonNumber::from(u)
                } else if let Some(f) = n.as_f64() {
                    JsonNumber::from_f64(f).unwrap_or_else(|| JsonNumber::from(0))
                } else {
                    JsonNumber::from(0)
                };
                out.insert(key.to_string(), JsonValue::Number(json_num));
            }
        }
        YamlValue::String(s) => {
            if let Some(key) = prefix {
                out.insert(key.to_string(), JsonValue::String(s.clone()));
            }
        }
        YamlValue::Sequence(seq) => {
            for (idx, v) in seq.iter().enumerate() {
                let new_prefix = match prefix {
                    Some(p) => format!("{}[{}]", p, idx),
                    None => format!("[{}]", idx),
                };
                flatten_yaml_value(Some(&new_prefix), v, out);
            }
        }
        YamlValue::Mapping(map) => {
            for (k, v) in map {
                let key_str = match k {
                    YamlValue::String(s) => s.clone(),
                    YamlValue::Number(n) => n.to_string(),
                    YamlValue::Bool(b) => b.to_string(),
                    other => format!("{:?}", other),
                };
                let new_prefix = match prefix {
                    Some(p) => format!("{}.{}", p, key_str),
                    None => key_str,
                };
                flatten_yaml_value(Some(&new_prefix), v, out);
            }
        }
        YamlValue::Tagged(inner) => {
            flatten_yaml_value(prefix, &inner.value, out);
        }
    }
}

/// Načte YAML soubory podle spring-like konvence a vrátí je jako seznam
/// SpringPropertySource (jeden soubor = jeden propertySource).
/// Pořadí v seznamu odpovídá Springu: vyšší precedence je dříve v seznamu.
async fn read_and_merge_yaml_files(
    git: &GitConfig,
    application: &str,
    profiles: &[String],
    label_opt: Option<&str>,
    env_map: &HashMap<String, String>,
) -> Result<(Vec<SpringPropertySource>, bool), ServerError> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // Spring-like precedence (nejvyšší první):
    //  1) {application}-{profile}.yml / .yaml
    //  2) application-{profile}.yml / .yaml
    //  3) {application}.yml / .yaml
    //  4) application.yml / application.yaml

    // 1) {application}-{profile}.yml / .yaml
    for p in profiles {
        candidates.push(PathBuf::from(format!("{application}-{p}.yml")));
        candidates.push(PathBuf::from(format!("{application}-{p}.yaml")));
    }

    // 2) application-{profile}.yml / .yaml
    for p in profiles {
        candidates.push(PathBuf::from(format!("application-{p}.yml")));
        candidates.push(PathBuf::from(format!("application-{p}.yaml")));
    }

    // 3) {application}.yml / .yaml
    candidates.push(PathBuf::from(format!("{application}.yml")));
    candidates.push(PathBuf::from(format!("{application}.yaml")));

    // 4) application.yml / application.yaml
    candidates.push(PathBuf::from("application.yml"));
    candidates.push(PathBuf::from("application.yaml"));

    let search_roots = vec![PathBuf::new()];

    let mut property_sources: Vec<SpringPropertySource> = Vec::new();
    let mut found_any = false;

    for root in &search_roots {
        for candidate in &candidates {
            let rel = if root.as_os_str().is_empty() {
                candidate.clone()
            } else {
                let mut p = root.clone();
                p.push(candidate);
                p
            };

            if let Some(bytes) = read_file_from_git(git, label_opt, &rel).await? {
                found_any = true;

                let content = String::from_utf8(bytes)?;
                let templated = apply_template(&content, env_map);
                let yaml: YamlValue = serde_yaml_ng::from_str(&templated)?;

                // Zploštíme YAML do mapy key -> JsonValue pro *tento* soubor
                let mut flat: IndexMap<String, JsonValue> = IndexMap::new();
                flatten_yaml_value(None, &yaml, &mut flat);

                // Jméno property source ve stylu Springu:
                // <repo_url>/<subpath>/<relativní_cesta_souboru>
                let mut rel_with_subpath = PathBuf::new();
                if let Some(sub) = &git.subpath {
                    rel_with_subpath.push(sub);
                }
                rel_with_subpath.push(&rel);

                let rel_str = rel_with_subpath
                    .components()
                    .fold(String::new(), |mut acc, c| {
                        if !acc.is_empty() {
                            acc.push('/');
                        }
                        acc.push_str(&c.as_os_str().to_string_lossy());
                        acc
                    });

                let base = git.repo_url.trim_end_matches('/');
                let name = format!("{}/{}", base, rel_str);

                property_sources.push(SpringPropertySource { name, source: flat });
            }
        }
    }

    Ok((property_sources, found_any))
}

fn parse_profiles(profile_str: &str) -> Vec<String> {
    profile_str
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn validate_rel_path(raw: &str) -> Result<PathBuf, ServerError> {
    let path = Path::new(raw);
    let mut clean = PathBuf::new();

    for comp in path.components() {
        match comp {
            Component::Normal(seg) => clean.push(seg),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(ServerError::BadRequest(
                    "Parent '..' segments are not allowed".to_string(),
                ));
            }
            _ => {
                return Err(ServerError::BadRequest(
                    "Absolute or root-relative paths are not allowed".to_string(),
                ));
            }
        }
    }

    Ok(clean)
}

/// ---------- Spring-compatible response types ----------

#[derive(Serialize)]
struct SpringPropertySource {
    name: String,
    source: IndexMap<String, JsonValue>,
}

#[derive(Serialize)]
struct SpringEnvResponse {
    name: String,
    profiles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    version: String,
    state: String,
    #[serde(rename = "propertySources")]
    property_sources: Vec<SpringPropertySource>,
}

async fn handle_spring_request(
    env_state: &EnvState,
    application: &str,
    profile_str: &str,
    label_opt: Option<&str>,
) -> Result<SpringEnvResponse, ServerError> {
    let profiles = parse_profiles(profile_str);

    // Teď dostaneme rovnou seznam SpringPropertySource po jednotlivých souborech
    let (property_sources, _found_any) = read_and_merge_yaml_files(
        &env_state.git,
        application,
        &profiles,
        label_opt,
        &env_state.env_map,
    )
    .await?;

    // Git commit hash (version) - pro daný label / branch
    let version = match git_version_for_label(&env_state.git, label_opt).await {
        Ok(v) => v,
        Err(e) => {
            warn!("[spring] git version lookup failed: {:?}", e);
            String::new()
        }
    };

    Ok(SpringEnvResponse {
        name: application.to_string(),
        profiles,
        label: label_opt.map(|s| s.to_string()),
        version,
        state: "".to_string(),
        property_sources,
    })
}

/// ---------- HTTP helpers ----------

#[derive(Clone, Copy)]
enum AuthScope {
    Config,
    Files,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct JwtClaims {
    sub: Option<String>,
    iss: Option<String>,
    aud: Option<serde_json::Value>,
    exp: usize,
    scope: Option<String>,
    client_id: Option<String>,
    email: Option<String>,
    preferred_username: Option<String>,
    groups: Option<JwtGroups>,
    #[serde(rename = "kubernetes.io")]
    kubernetes: Option<KubernetesClaims>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct KubernetesClaims {
    namespace: Option<String>,
    serviceaccount: Option<KubernetesServiceAccountClaims>,
    pod: Option<KubernetesPodClaims>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct KubernetesServiceAccountClaims {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct KubernetesPodClaims {
    name: Option<String>,
    uid: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum JwtGroups {
    One(String),
    Many(Vec<String>),
}

/// Basic-auth check only (no fallback semantics)
fn check_basic_auth_only(state: &AppState, headers: &HeaderMap) -> bool {
    let value = match headers.get(AUTHORIZATION) {
        Some(v) => v,
        None => return false,
    };

    let value_str = match value.to_str() {
        Ok(s) => s,
        Err(_) => return false,
    };

    if !value_str.starts_with("Basic ") {
        return false;
    }

    let b64 = &value_str[6..];
    let decoded = match BASE64_STANDARD.decode(b64) {
        Ok(d) => d,
        Err(_) => return false,
    };

    let creds = String::from_utf8_lossy(&decoded);
    let mut parts = creds.splitn(2, ':');
    let user = parts.next().unwrap_or("");
    let pass = parts.next().unwrap_or("");

    user == state.auth.username && pass == state.auth.password
}

fn client_has_env(client: &ClientIdClient, env: Option<&str>) -> bool {
    match env {
        None => true,
        Some(e) => {
            if client.environments.iter().any(|v| v == "*") {
                true
            } else {
                client.environments.iter().any(|v| v == e)
            }
        }
    }
}

fn client_has_tenant(client: &ClientIdClient, tenant: Option<&str>) -> bool {
    match tenant {
        None => true,
        Some(t) => {
            if client.tenants.iter().any(|v| v == "*") {
                true
            } else {
                client.tenants.iter().any(|v| v == t)
            }
        }
    }
}

fn client_has_scope(client: &ClientIdClient, scope: AuthScope) -> bool {
    let needed = match scope {
        AuthScope::Config => "config:read",
        AuthScope::Files => "files:read",
    };
    client.scopes.iter().any(|s| s == needed)
}

fn jwt_groups_to_vec(groups: JwtGroups) -> Vec<String> {
    match groups {
        JwtGroups::One(value) => vec![value],
        JwtGroups::Many(values) => values,
    }
}

fn jwt_scopes_to_vec(scope: Option<&str>) -> Vec<String> {
    scope
        .unwrap_or("")
        .split_whitespace()
        .filter(|scope| !scope.is_empty())
        .map(str::to_string)
        .collect()
}

fn validate_kube_service_account_claims(claims: &JwtClaims) -> bool {
    let Some(subject) = claims.sub.as_deref() else {
        return false;
    };
    let Some(rest) = subject.strip_prefix("system:serviceaccount:") else {
        return false;
    };
    let mut parts = rest.split(':');
    let Some(subject_namespace) = parts.next().filter(|value| !value.is_empty()) else {
        return false;
    };
    let Some(subject_service_account) = parts.next().filter(|value| !value.is_empty()) else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }

    let namespace = claims
        .kubernetes
        .as_ref()
        .and_then(|k| k.namespace.as_deref());
    let service_account = claims
        .kubernetes
        .as_ref()
        .and_then(|k| k.serviceaccount.as_ref())
        .and_then(|sa| sa.name.as_deref());

    namespace == Some(subject_namespace) && service_account == Some(subject_service_account)
}

fn claims_authorized(
    issuer: &BearerIssuer,
    claims: &JwtClaims,
    tenant: Option<&str>,
    env: Option<&str>,
    scope: Option<AuthScope>,
) -> bool {
    if issuer.kind == BearerIssuerKind::KubeSaJwt && !validate_kube_service_account_claims(claims) {
        return false;
    }

    let groups = claims
        .groups
        .clone()
        .map(jwt_groups_to_vec)
        .unwrap_or_default();
    if groups
        .iter()
        .any(|group| group == "simple-config:role:admin")
    {
        return true;
    }

    let tenant_allowed =
        tenant.is_none() || proxy_group_matches(&groups, "simple-config:tenant", tenant);
    let env_allowed = env.is_none() || proxy_group_matches(&groups, "simple-config:env", env);
    if !tenant_allowed || !env_allowed {
        return false;
    }

    match scope {
        None => groups.iter().any(|group| group == "simple-config:ui"),
        Some(scope) => {
            let needed = match scope {
                AuthScope::Config => "config:read",
                AuthScope::Files => "files:read",
            };
            let token_scopes = jwt_scopes_to_vec(claims.scope.as_deref());
            groups
                .iter()
                .any(|group| group == &format!("simple-config:scope:{needed}") || group == needed)
                || token_scopes.iter().any(|token_scope| token_scope == needed)
        }
    }
}

enum BearerAuthAttempt {
    Allowed,
    UnknownKid,
    Invalid,
}

async fn try_bearer_auth(
    auth: &BearerAuth,
    token: &str,
    header: &jsonwebtoken::Header,
    kid: &str,
    tenant: Option<&str>,
    env: Option<&str>,
    scope: Option<AuthScope>,
) -> BearerAuthAttempt {
    let issuers = auth.issuers.read().await;
    let mut saw_kid = false;
    for issuer in issuers.iter() {
        let Some(key) = issuer.jwks.get(kid) else {
            continue;
        };
        saw_kid = true;
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[issuer.issuer.as_str()]);
        if let Some(audience) = issuer.audience.as_ref() {
            validation.set_audience(&[audience.as_str()]);
        } else {
            validation.validate_aud = false;
        }

        let decoded = match decode::<JwtClaims>(token, key, &validation) {
            Ok(decoded) => decoded,
            Err(_) => continue,
        };
        if claims_authorized(issuer, &decoded.claims, tenant, env, scope) {
            return BearerAuthAttempt::Allowed;
        }
        return BearerAuthAttempt::Invalid;
    }

    if saw_kid {
        BearerAuthAttempt::Invalid
    } else {
        BearerAuthAttempt::UnknownKid
    }
}

async fn refresh_bearer_jwks(auth: &BearerAuth) -> bool {
    let targets = {
        let issuers = auth.issuers.read().await;
        issuers
            .iter()
            .map(|issuer| (issuer.name.clone(), issuer.jwks_url.clone()))
            .collect::<Vec<_>>()
    };

    let mut refreshed = Vec::new();
    for (name, jwks_url) in targets {
        if let Ok(jwks) = load_jwks(&jwks_url).await {
            refreshed.push((name, jwks));
        }
    }
    if refreshed.is_empty() {
        return false;
    }

    let mut issuers = auth.issuers.write().await;
    for (name, jwks) in refreshed {
        if let Some(issuer) = issuers.iter_mut().find(|issuer| issuer.name == name) {
            issuer.jwks = jwks;
        }
    }
    true
}

async fn bearer_authorized(
    auth: &BearerAuth,
    headers: &HeaderMap,
    tenant: Option<&str>,
    env: Option<&str>,
    scope: Option<AuthScope>,
) -> bool {
    if !auth.enabled {
        return false;
    }
    let Some(value) = headers.get(AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    let header = match decode_header(token) {
        Ok(header) => header,
        Err(_) => return false,
    };
    let Some(kid) = header.kid.as_deref() else {
        return false;
    };

    match try_bearer_auth(auth, token, &header, kid, tenant, env, scope).await {
        BearerAuthAttempt::Allowed => true,
        BearerAuthAttempt::Invalid => false,
        BearerAuthAttempt::UnknownKid => {
            refresh_bearer_jwks(auth).await
                && matches!(
                    try_bearer_auth(auth, token, &header, kid, tenant, env, scope).await,
                    BearerAuthAttempt::Allowed
                )
        }
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn split_csv_header(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn proxy_group_matches(groups: &[String], prefix: &str, value: Option<&str>) -> bool {
    groups.iter().any(|group| group == &format!("{prefix}:*"))
        || value
            .map(|value| {
                groups
                    .iter()
                    .any(|group| group == &format!("{prefix}:{value}"))
            })
            .unwrap_or(false)
}

fn trusted_proxy_authorized(
    headers: &HeaderMap,
    tenant: Option<&str>,
    env: Option<&str>,
    scope: Option<AuthScope>,
) -> bool {
    let subject =
        header_str(headers, "x-auth-subject").or_else(|| header_str(headers, "x-auth-user"));
    if subject.is_none() {
        return false;
    }

    let groups = header_str(headers, "x-auth-groups")
        .map(split_csv_header)
        .unwrap_or_default();
    if groups
        .iter()
        .any(|group| group == "simple-config:role:admin")
    {
        return true;
    }

    let tenant_allowed =
        tenant.is_none() || proxy_group_matches(&groups, "simple-config:tenant", tenant);
    let env_allowed = env.is_none() || proxy_group_matches(&groups, "simple-config:env", env);
    if !tenant_allowed || !env_allowed {
        return false;
    }

    match scope {
        None => groups.iter().any(|group| group == "simple-config:ui"),
        Some(scope) => {
            let needed = match scope {
                AuthScope::Config => "config:read",
                AuthScope::Files => "files:read",
            };
            groups
                .iter()
                .any(|group| group == &format!("simple-config:scope:{needed}") || group == needed)
        }
    }
}

/// Combined authorization for basic + X-Client-Id + trusted proxy headers
async fn is_authorized_for(
    state: &AppState,
    headers: &HeaderMap,
    tenant: Option<&str>,
    env: Option<&str>,
    scope: Option<AuthScope>,
) -> bool {
    let basic_enabled = state.auth.required;
    let client_auth = &state.auth.client_id;
    let client_enabled = client_auth.enabled;
    let proxy_enabled = state.auth.trusted_proxy_enabled;
    let bearer_enabled = state.auth.bearer.enabled;

    // No auth configured at all -> open access (backwards compatible)
    if !basic_enabled && !client_enabled && !proxy_enabled && !bearer_enabled {
        return true;
    }

    // 1) Basic auth
    if basic_enabled && check_basic_auth_only(state, headers) {
        return true;
    }

    // 2) Bearer JWT
    if bearer_authorized(&state.auth.bearer, headers, tenant, env, scope).await {
        return true;
    }

    // 3) X-Client-Id
    if client_enabled && let Some(client) = client_auth.get_client(headers) {
        if !client_has_tenant(client, tenant) || !client_has_env(client, env) {
            return false;
        }

        match scope {
            // UI access
            None => {
                if client.ui_access {
                    return true;
                }
            }
            Some(s) => {
                if client_has_scope(client, s) {
                    return true;
                }
            }
        }
    }

    // 4) Trusted proxy X-Auth-* headers
    if proxy_enabled && trusted_proxy_authorized(headers, tenant, env, scope) {
        return true;
    }

    false
}

fn unauthorized_response() -> Response {
    let mut resp = Response::new("Unauthorized".into());
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    resp.headers_mut().insert(
        WWW_AUTHENTICATE,
        r#"Basic realm="SecureConfigServer""#.parse().unwrap(),
    );
    resp
}

fn spring_not_found_json(path: &str) -> Response {
    let body = serde_json::json!({
        "timestamp": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "status": 404,
        "error": "Not Found",
        "path": path,
    });
    (StatusCode::NOT_FOUND, Json(body)).into_response()
}

async fn spring_like_404(OriginalUri(uri): OriginalUri) -> Response {
    spring_not_found_json(uri.path())
}

fn env_state_or_404<'a>(
    state: &'a AppState,
    tenant: &str,
    env: &str,
    path_for_error: &str,
) -> Result<&'a EnvState, Box<Response>> {
    let key = env_key(tenant, env);
    match state.envs.get(&key) {
        Some(e) => Ok(e),
        None => Err(Box::new(spring_not_found_json(path_for_error))),
    }
}

/// ---------- HTTP handlers ----------
async fn spring_handler_tenant(
    State(state): State<Arc<AppState>>,
    AxumPath((tenant, env, application, profile, label)): AxumPath<(
        String,
        String,
        String,
        String,
        String,
    )>,
    headers: HeaderMap,
) -> Response {
    if !is_authorized_for(
        &state,
        &headers,
        Some(&tenant),
        Some(&env),
        Some(AuthScope::Config),
    )
    .await
    {
        return unauthorized_response();
    }

    let path = format!("/api/v1/tenants/{tenant}/envs/{env}/{application}/{profile}/{label}");
    let env_state = match env_state_or_404(&state, &tenant, &env, &path) {
        Ok(e) => e,
        Err(r) => return *r,
    };

    match handle_spring_request(env_state, &application, &profile, Some(&label)).await {
        Ok(body) => Json(body).into_response(),
        Err(e) => {
            error!("[spring] error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

// Legacy Spring-compatible route (default tenant)
async fn spring_handler_legacy(
    State(state): State<Arc<AppState>>,
    AxumPath((env, application, profile, label)): AxumPath<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let tenant = state.tenancy.default_tenant.clone();
    spring_handler_tenant(
        State(state),
        AxumPath((tenant, env, application, profile, label)),
        headers,
    )
    .await
}

async fn spring_handler_no_label_tenant(
    State(state): State<Arc<AppState>>,
    AxumPath((tenant, env, application, profile)): AxumPath<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if !is_authorized_for(
        &state,
        &headers,
        Some(&tenant),
        Some(&env),
        Some(AuthScope::Config),
    )
    .await
    {
        return unauthorized_response();
    }

    let path = format!("/api/v1/tenants/{tenant}/envs/{env}/{application}/{profile}");
    let env_state = match env_state_or_404(&state, &tenant, &env, &path) {
        Ok(e) => e,
        Err(r) => return *r,
    };

    match handle_spring_request(env_state, &application, &profile, None).await {
        Ok(body) => Json(body).into_response(),
        Err(e) => {
            error!("[spring] error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

// Legacy Spring-compatible route (default tenant, no label)
async fn spring_handler_no_label_legacy(
    State(state): State<Arc<AppState>>,
    AxumPath((env, application, profile)): AxumPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let tenant = state.tenancy.default_tenant.clone();
    spring_handler_no_label_tenant(
        State(state),
        AxumPath((tenant, env, application, profile)),
        headers,
    )
    .await
}

async fn env_files_handler_tenant(
    State(state): State<Arc<AppState>>,
    AxumPath((tenant, env)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !is_authorized_for(
        &state,
        &headers,
        Some(&tenant),
        Some(&env),
        Some(AuthScope::Files),
    )
    .await
    {
        return unauthorized_response();
    }

    let path = format!("/api/v1/tenants/{tenant}/envs/{env}/assets");
    let env_state = match env_state_or_404(&state, &tenant, &env, &path) {
        Ok(e) => e,
        Err(r) => return *r,
    };

    match list_files_in_git(&env_state.git).await {
        Ok(files) => {
            let items: Vec<AssetListItem> = files.iter().cloned().map(asset_list_item).collect();
            Json(serde_json::json!({ "files": files, "items": items })).into_response()
        }
        Err(e) => {
            error!("[assets] list_files_in_git failed: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

// Legacy assets list route (default tenant)
async fn env_files_handler_legacy(
    State(state): State<Arc<AppState>>,
    AxumPath(env): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let tenant = state.tenancy.default_tenant.clone();
    env_files_handler_tenant(State(state), AxumPath((tenant, env)), headers).await
}

async fn env_file_handler_tenant(
    State(state): State<Arc<AppState>>,
    AxumPath((tenant, env, rel_path)): AxumPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if !is_authorized_for(
        &state,
        &headers,
        Some(&tenant),
        Some(&env),
        Some(AuthScope::Files),
    )
    .await
    {
        return unauthorized_response();
    }

    let env_state = match env_state_or_404(
        &state,
        &tenant,
        &env,
        &format!("/api/v1/tenants/{tenant}/envs/{env}/assets/{rel_path}"),
    ) {
        Ok(e) => e,
        Err(r) => return *r,
    };

    // Normalize (just in case)
    let rel_path = rel_path.trim_start_matches('/').to_string();
    if rel_path.is_empty() {
        return (StatusCode::NOT_FOUND, "File not found").into_response();
    }

    let res = if let Some((first, rest)) = rel_path.split_once('/') {
        match handle_file_request(env_state, Some(first), rest).await {
            Ok(resp) => Ok(resp),
            Err(ServerError::NotFound) => handle_file_request(env_state, None, &rel_path).await,
            Err(e) => Err(e),
        }
    } else {
        handle_file_request(env_state, None, &rel_path).await
    };

    match res {
        Ok(resp) => resp,
        Err(ServerError::NotFound) => (StatusCode::NOT_FOUND, "File not found").into_response(),
        Err(e) => {
            error!("[assets] error: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

// Legacy asset file route (default tenant)
async fn env_file_handler_legacy(
    State(state): State<Arc<AppState>>,
    AxumPath((env, rel_path)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let tenant = state.tenancy.default_tenant.clone();
    env_file_handler_tenant(State(state), AxumPath((tenant, env, rel_path)), headers).await
}
async fn handle_file_request(
    env_state: &EnvState,
    label: Option<&str>,
    rel_path: &str,
) -> Result<Response, ServerError> {
    let safe_rel = validate_rel_path(rel_path)?;
    let bytes_opt = read_file_from_git(&env_state.git, label, &safe_rel).await?;
    let bytes = match bytes_opt {
        Some(b) => b,
        None => return Err(ServerError::NotFound),
    };

    let is_binary = bytes.contains(&0) || std::str::from_utf8(&bytes).is_err();
    let is_opaque_asset_bundle = safe_rel
        .file_name()
        .and_then(|n| n.to_str())
        .map(|name| name == "assets.secured.json" || name == "assets.unsecured.json")
        .unwrap_or(false);

    if is_binary {
        let mime = MimeGuess::from_path(&safe_rel)
            .first_or_octet_stream()
            .to_string();
        let mut resp = Response::new(bytes.into());
        resp.headers_mut().insert(
            CONTENT_TYPE,
            mime.parse()
                .unwrap_or_else(|_| "application/octet-stream".parse().unwrap()),
        );
        Ok(resp)
    } else {
        let text = String::from_utf8(bytes)?;
        let body = if is_opaque_asset_bundle {
            text
        } else {
            apply_template(&text, &env_state.env_map)
        };
        let mime = if is_opaque_asset_bundle {
            "application/json".to_string()
        } else {
            MimeGuess::from_path(&safe_rel)
                .first_or_octet_stream()
                .to_string()
        };
        let mut resp = Response::new(body.into());
        resp.headers_mut().insert(
            CONTENT_TYPE,
            mime.parse()
                .unwrap_or_else(|_| "text/plain; charset=utf-8".parse().unwrap()),
        );
        Ok(resp)
    }
}

/// ---------- UI handler & router ----------
/// ---------- Health endpoints ----------

#[derive(Serialize)]
struct HealthStatus {
    status: &'static str,
    startup_time: String,
}

#[derive(Serialize)]
struct EnvHealthSummary {
    env: String,
    env_var_count: usize,
    file_count: usize,
}

#[derive(Serialize)]
struct EnvHealthDetail {
    status: &'static str,
    startup_time: String,
    env: String,
    env_var_count: usize,
    file_count: usize,
}

#[derive(Serialize)]
struct EnvHealthList {
    status: &'static str,
    startup_time: String,
    environments: Vec<EnvHealthSummary>,
}

#[derive(Debug, Clone, Serialize)]
struct AssetListItem {
    path: String,
    kind: &'static str,
    opaque: bool,
    templated: bool,
}

fn asset_list_item(path: String) -> AssetListItem {
    match path.as_str() {
        "assets.secured.json" => AssetListItem {
            path,
            kind: "assets-secured-bundle",
            opaque: true,
            templated: false,
        },
        "assets.unsecured.json" => AssetListItem {
            path,
            kind: "assets-unsecured-bundle",
            opaque: true,
            templated: false,
        },
        _ => AssetListItem {
            path,
            kind: "file",
            opaque: false,
            templated: true,
        },
    }
}

/// Count regular files in the working tree for the given environment (excluding .git).
fn count_files_for_env(env_state: &EnvState) -> usize {
    let root = if let Some(sub) = &env_state.git.subpath {
        env_state.git.workdir.join(sub)
    } else {
        env_state.git.workdir.clone()
    };

    let mut count = 0usize;
    let mut stack = vec![root];

    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str())
                        && name == ".git"
                    {
                        continue;
                    }
                    stack.push(path);
                } else if path.is_file() {
                    count += 1;
                }
            }
        }
    }

    count
}

async fn healthz_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let ts = state
        .startup_time
        .to_rfc3339_opts(SecondsFormat::Secs, true);

    let body = HealthStatus {
        status: "UP",
        startup_time: ts,
    };

    (StatusCode::OK, Json(body))
}

async fn healthz_env_all_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let ts = state
        .startup_time
        .to_rfc3339_opts(SecondsFormat::Secs, true);

    let mut envs_vec = Vec::new();
    for env_state in state.envs.values() {
        envs_vec.push(EnvHealthSummary {
            env: format!("{}/{}", env_state.tenant, env_state.name),
            env_var_count: env_state.env_map.len(),
            file_count: count_files_for_env(env_state),
        });
    }

    let body = EnvHealthList {
        status: "UP",
        startup_time: ts,
        environments: envs_vec,
    };

    (StatusCode::OK, Json(body))
}

async fn healthz_env_single_handler_tenant(
    State(state): State<Arc<AppState>>,
    AxumPath((tenant, env)): AxumPath<(String, String)>,
) -> impl IntoResponse {
    let env_state = match env_state_or_404(
        &state,
        &tenant,
        &env,
        &format!("/api/v1/tenants/{tenant}/envs/{env}/healthz"),
    ) {
        Ok(e) => e,
        Err(r) => return *r,
    };

    let ts = state
        .startup_time
        .to_rfc3339_opts(SecondsFormat::Secs, true);

    let body = EnvHealthDetail {
        status: "UP",
        startup_time: ts,
        env: env_state.name.clone(),
        env_var_count: env_state.env_map.len(),
        file_count: count_files_for_env(env_state),
    };

    (StatusCode::OK, Json(body)).into_response()
}

async fn ui_handler(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !is_authorized_for(&state, &headers, None, None, None).await {
        return unauthorized_response();
    }

    #[derive(Serialize)]
    struct EnvMeta {
        tenant: String,
        name: String,
        api_base: String,
        repo_url: String,
        branch: String,
        workdir: String,
        subpath: String,
        last_commit: String,
        last_commit_date: String,
    }

    #[derive(Serialize)]
    struct UiMeta {
        base_path: String,
        environments: Vec<EnvMeta>,
        auth_enabled: bool,
    }

    let base_path = normalize_base_path(&state.server.base_path);
    let mut envs_meta = Vec::new();
    for env_state in state.envs.values() {
        let last_commit = match git_version_for_label(&env_state.git, None).await {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "[ui] failed to get git version for {}: {:?}",
                    env_state.name, e
                );
                String::new()
            }
        };
        let last_commit_date = match git_commit_date_for_label(&env_state.git, None).await {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "[ui] failed to get git date for {}: {:?}",
                    env_state.name, e
                );
                String::new()
            }
        };

        let api_base = if base_path == "/" {
            format!(
                "/api/v1/tenants/{}/envs/{}",
                env_state.tenant, env_state.name
            )
        } else {
            format!(
                "{}/api/v1/tenants/{}/envs/{}",
                base_path, env_state.tenant, env_state.name
            )
        };

        envs_meta.push(EnvMeta {
            tenant: env_state.tenant.clone(),
            name: env_state.name.clone(),
            api_base,
            repo_url: env_state.git.repo_url.clone(),
            branch: env_state.git.branch.clone(),
            workdir: env_state.git.workdir.display().to_string(),
            subpath: env_state
                .git
                .subpath
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            last_commit,
            last_commit_date,
        });
    }

    let meta = UiMeta {
        base_path,
        environments: envs_meta,
        auth_enabled: state.auth.required || state.auth.client_id.enabled,
    };

    let meta_json = match serde_json::to_string(&meta) {
        Ok(s) => s,
        Err(e) => {
            error!("[ui] failed to serialize meta: {:?}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
        }
    };

    let html = UI_TEMPLATE.replace("__META_JSON__", &meta_json);
    Html(html).into_response()
}

fn build_router(state: Arc<AppState>) -> Router {
    let base_path = normalize_base_path(&state.server.base_path);

    let inner = Router::new()
        // Health endpoints (no auth, good for k8s probes)
        .route("/healthz", get(healthz_handler))
        .route("/helthz", get(healthz_handler)) // alias for typo-friendly access
        .route("/healthz/env", get(healthz_env_all_handler))
        .route(
            "/api/v1/tenants/{tenant}/envs/{env}/healthz",
            get(healthz_env_single_handler_tenant),
        )
        // Asset listing & raw asset access with templating for non-Spring clients
        .route(
            "/api/v1/tenants/{tenant}/envs/{env}/assets",
            get(env_files_handler_tenant),
        )
        .route(
            "/api/v1/tenants/{tenant}/envs/{env}/assets/{*path}",
            get(env_file_handler_tenant),
        )
        // Spring-compatible (tenant-aware)
        .route(
            "/api/v1/tenants/{tenant}/envs/{env}/{application}/{profile}/{label}",
            get(spring_handler_tenant),
        )
        .route(
            "/api/v1/tenants/{tenant}/envs/{env}/{application}/{profile}",
            get(spring_handler_no_label_tenant),
        )
        // UI
        .route("/ui", get(ui_handler));

    let inner = if state.tenancy.mode == "simple" {
        inner
            // Legacy Spring-compatible routes (default tenant)
            .route(
                "/{env}/{application}/{profile}/{label}",
                get(spring_handler_legacy),
            )
            .route(
                "/{env}/{application}/{profile}",
                get(spring_handler_no_label_legacy),
            )
            // Short API (default tenant)
            .route("/api/v1/envs/{env}/assets", get(env_files_handler_legacy))
            .route(
                "/api/v1/envs/{env}/assets/{*path}",
                get(env_file_handler_legacy),
            )
    } else {
        inner
    };

    let app = if base_path == "/" {
        inner
    } else {
        Router::new().nest(&base_path, inner)
    };

    app.with_state(state).fallback(spring_like_404)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_issuer() -> BearerIssuer {
        BearerIssuer {
            name: "simple-idm-jwt".to_string(),
            kind: BearerIssuerKind::SimpleIdmJwt,
            issuer: "https://sso.example.com".to_string(),
            audience: Some("simple-config-server".to_string()),
            jwks_url: "https://sso.example.com/.well-known/jwks.json".to_string(),
            jwks: HashMap::new(),
        }
    }

    fn kube_issuer() -> BearerIssuer {
        BearerIssuer {
            name: "kube-sa-jwt".to_string(),
            kind: BearerIssuerKind::KubeSaJwt,
            issuer: "https://openshift.example.com".to_string(),
            audience: Some("simple-config-server".to_string()),
            jwks_url: "https://openshift.example.com/openid/v1/jwks".to_string(),
            jwks: HashMap::new(),
        }
    }

    fn claims(groups: Vec<&str>, scope: Option<&str>) -> JwtClaims {
        JwtClaims {
            sub: Some("user-1".to_string()),
            iss: Some("https://sso.example.com".to_string()),
            aud: Some(serde_json::json!("simple-config-server")),
            exp: 123,
            scope: scope.map(str::to_string),
            client_id: None,
            email: Some("mares@example.com".to_string()),
            preferred_username: Some("mares".to_string()),
            groups: Some(JwtGroups::Many(
                groups.into_iter().map(str::to_string).collect(),
            )),
            kubernetes: None,
        }
    }

    fn kube_claims(namespace: &str, service_account: &str, subject: &str) -> JwtClaims {
        JwtClaims {
            sub: Some(subject.to_string()),
            iss: Some("https://openshift.example.com".to_string()),
            aud: Some(serde_json::json!("simple-config-server")),
            exp: 123,
            scope: Some("config:read files:read".to_string()),
            client_id: None,
            email: None,
            preferred_username: None,
            groups: Some(JwtGroups::Many(vec![
                "simple-config:tenant:o2".to_string(),
                "simple-config:env:test".to_string(),
            ])),
            kubernetes: Some(KubernetesClaims {
                namespace: Some(namespace.to_string()),
                serviceaccount: Some(KubernetesServiceAccountClaims {
                    name: Some(service_account.to_string()),
                }),
                pod: Some(KubernetesPodClaims {
                    name: Some("order-api-abc".to_string()),
                    uid: Some("pod-uid".to_string()),
                }),
            }),
        }
    }

    #[test]
    fn trusted_proxy_requires_identity() {
        let mut headers = HeaderMap::new();
        headers.insert("x-auth-groups", "simple-config:role:admin".parse().unwrap());

        assert!(!trusted_proxy_authorized(
            &headers,
            Some("o2"),
            Some("test"),
            Some(AuthScope::Config),
        ));
    }

    #[test]
    fn trusted_proxy_allows_tenant_env_scope_match() {
        let mut headers = HeaderMap::new();
        headers.insert("x-auth-user", "mares".parse().unwrap());
        headers.insert(
            "x-auth-groups",
            "simple-config:tenant:o2,simple-config:env:test,simple-config:scope:config:read"
                .parse()
                .unwrap(),
        );

        assert!(trusted_proxy_authorized(
            &headers,
            Some("o2"),
            Some("test"),
            Some(AuthScope::Config),
        ));
        assert!(!trusted_proxy_authorized(
            &headers,
            Some("o2"),
            Some("prod"),
            Some(AuthScope::Config),
        ));
    }

    #[test]
    fn bearer_claims_allow_tenant_env_and_oauth_scope() {
        let claims = claims(
            vec!["simple-config:tenant:o2", "simple-config:env:test"],
            Some("config:read"),
        );

        assert!(claims_authorized(
            &simple_issuer(),
            &claims,
            Some("o2"),
            Some("test"),
            Some(AuthScope::Config),
        ));
        assert!(!claims_authorized(
            &simple_issuer(),
            &claims,
            Some("o2"),
            Some("test"),
            Some(AuthScope::Files),
        ));
    }

    #[test]
    fn bearer_claims_admin_bypasses_tenant_env_scope() {
        let claims = claims(vec!["simple-config:role:admin"], None);

        assert!(claims_authorized(
            &simple_issuer(),
            &claims,
            Some("other"),
            Some("prod"),
            Some(AuthScope::Files),
        ));
    }

    #[test]
    fn kube_service_account_claims_must_match_subject() {
        let ok_claims = kube_claims(
            "zis-test",
            "order-api",
            "system:serviceaccount:zis-test:order-api",
        );
        let bad_claims = kube_claims(
            "zis-prod",
            "order-api",
            "system:serviceaccount:zis-test:order-api",
        );

        assert!(claims_authorized(
            &kube_issuer(),
            &ok_claims,
            Some("o2"),
            Some("test"),
            Some(AuthScope::Config),
        ));
        assert!(!claims_authorized(
            &kube_issuer(),
            &bad_claims,
            Some("o2"),
            Some("test"),
            Some(AuthScope::Config),
        ));
    }

    #[test]
    fn client_id_auth_checks_tenant_env_and_scope() {
        let client = ClientIdClient {
            id: "ci".to_string(),
            tenants: vec!["o2".to_string()],
            environments: vec!["test".to_string()],
            scopes: vec!["config:read".to_string()],
            ui_access: false,
        };

        assert!(client_has_tenant(&client, Some("o2")));
        assert!(!client_has_tenant(&client, Some("cetin")));
        assert!(client_has_env(&client, Some("test")));
        assert!(!client_has_env(&client, Some("prod")));
        assert!(client_has_scope(&client, AuthScope::Config));
        assert!(!client_has_scope(&client, AuthScope::Files));
    }
}
