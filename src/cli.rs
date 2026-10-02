//! Command-line interface (standalone CLI tool).
//!
//! Kinetix ships as a single binary that is both the server and its own
//! administration tool. `kinetix serve` runs the proxy; every other subcommand
//! reads or writes the SQLite control plane directly (using the master key from
//! the config directory), so an operator never needs a `.env` file, an admin
//! password, or the dashboard to configure the system.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{CliOverrides, Config};
use crate::crypto::Crypto;
use crate::db::{self, Pool};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "kinetix",
    version,
    about = "Self-hosted, multi-protocol LLM gateway",
    long_about = None,
    after_help = "First run (initialization is explicit):\n  kinetix init\n\nCommon setup:\n  kinetix provider add --name \"My Provider\" --base-url https://api.example.com/v1 \\\n    --wire-format openai --auth-scheme bearer --api-key \"$UPSTREAM_API_KEY\" --account-label primary\n  kinetix model add --provider \"My Provider\" --upstream-id <model-id> --display-name \"<Model>\"\n  kinetix key create --name local-client --owner me\n  kinetix serve\n\nRun `kinetix <COMMAND> --help` for command options. Most setup commands work without the server; `kinetix api` calls a running Admin API.",
)]
pub struct Cli {
    /// Root directory holding config/, data/, and state/ (overrides XDG).
    #[arg(long, global = true, env = "KINETIX_HOME")]
    pub home: Option<std::path::PathBuf>,

    /// Explicit config file path.
    #[arg(long, global = true)]
    pub config: Option<std::path::PathBuf>,

    /// Override the bind address (e.g. 127.0.0.1:8080).
    #[arg(long, global = true)]
    pub bind: Option<String>,

    /// Override the database URL (sqlite://...).
    #[arg(long, global = true)]
    pub database_url: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Run the proxy server.
    Serve(ServeArgs),
    /// Print the generated Admin API reference (no server or database required).
    ApiReference,
    /// Call the supported Admin API; prints its JSON contract unchanged.
    Api(AdminApiArgs),
    /// First-run setup: create directories and print generated secrets.
    Init,
    /// Print configuration, paths, and control-plane counts.
    Status,
    /// Run diagnostics against the local installation.
    Doctor,
    /// Check for or install a released Kinetix binary.
    Update(UpdateArgs),
    /// Show or change the dashboard admin password.
    Password(PasswordArgs),
    /// Manage virtual keys.
    Key(KeyArgs),
    /// Manage upstream providers.
    Provider(ProviderArgs),
    /// Manage models.
    Model(ModelArgs),
    /// Manage provider accounts (credential pools).
    Account(AccountArgs),
    /// Manage routes.
    Route(RouteArgs),
    /// Manage model aliases.
    Alias(AliasArgs),
    /// Manage plugins.
    Plugin(PluginArgs),
    /// Export usage/logs to disk (JSONL + CSV).
    Export(ExportArgs),
    /// Run or list database backups.
    Backup(BackupArgs),
    /// Remove Kinetix state (config, database, exports, backups, logs).
    Uninstall(UninstallArgs),
}

#[derive(Args, Debug, Clone)]
pub struct AdminApiArgs {
    /// Admin API base URL, e.g. http://127.0.0.1:8080.
    #[arg(long, env = "KINETIX_ADMIN_URL")]
    pub url: String,
    /// Admin password or session token (prefer the environment variable).
    #[arg(long, env = "KINETIX_ADMIN_TOKEN", hide_env_values = true)]
    pub token: String,
    /// Path relative to /admin/api, including optional query parameters.
    pub path: String,
    #[arg(long, default_value = "GET", value_parser = ["GET", "POST", "PUT", "DELETE"])]
    pub method: String,
    /// JSON request body file.
    #[arg(long)]
    pub body: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct ServeArgs {
    #[arg(long)]
    pub log_json: bool,
    #[arg(long)]
    pub allow_private_upstreams: bool,
    #[arg(long)]
    pub allow_insecure_tls: bool,
}

#[derive(Args, Debug, Clone)]
pub struct UpdateArgs {
    /// Check for an update without downloading or replacing the binary.
    #[arg(short = 'c', long)]
    pub check: bool,
    /// Install/check a specific release tag, e.g. v0.4.0.
    #[arg(long)]
    pub tag: Option<String>,
    /// Skip the interactive replacement confirmation.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Reinstall even when the target version is already installed.
    #[arg(short = 'f', long)]
    pub force: bool,
    /// Download and verify the release without replacing the binary.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone)]
pub struct PasswordArgs {
    #[command(subcommand)]
    pub action: PasswordAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum PasswordAction {
    /// Set the admin password (min 8 chars).
    Set { password: String },
    /// Show whether a password is configured.
    Show,
}

#[derive(Args, Debug, Clone)]
pub struct KeyArgs {
    #[command(subcommand)]
    pub action: KeyAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum KeyAction {
    /// List virtual keys (never shows the secret).
    List,
    /// Create a key and print it once.
    Create {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "admin")]
        owner: String,
        #[arg(long, default_value = "")]
        tag: String,
        /// Allowed models/aliases/routes (comma-separated, default '*').
        #[arg(long, default_value = "*")]
        allowed_models: String,
        #[arg(long)]
        rpm: Option<i64>,
        #[arg(long)]
        tpm: Option<i64>,
        #[arg(long)]
        daily_budget: Option<f64>,
        #[arg(long)]
        monthly_budget: Option<f64>,
    },
    /// Revoke a key: hard-delete it and its usage logs.
    Revoke { id: String },
    /// Disable (keep) a key.
    Disable { id: String },
    /// Re-enable a key.
    Enable { id: String },
}

#[derive(Args, Debug, Clone)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub action: ProviderAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ProviderAction {
    List,
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        base_url: String,
        /// openai | anthropic | gemini
        #[arg(long, default_value = "openai")]
        wire_format: String,
        /// none | bearer | custom_header | query_param
        #[arg(long, default_value = "bearer")]
        auth_scheme: String,
        #[arg(long)]
        custom_header_name: Option<String>,
        #[arg(long)]
        custom_param_name: Option<String>,
        #[arg(long)]
        models_path: Option<String>,
        #[arg(long, default_value_t = 120000)]
        timeout_ms: i64,
        /// Create the first account with this credential.
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        account_label: Option<String>,
        /// JSON file containing declarations, values and network_hosts (non-secret).
        #[arg(long)]
        connection_parameters: Option<PathBuf>,
    },
    Remove {
        id: String,
    },
}

#[derive(Args, Debug, Clone)]
pub struct ModelArgs {
    #[command(subcommand)]
    pub action: ModelAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ModelAction {
    List,
    Add {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        upstream_id: String,
        #[arg(long)]
        display_name: Option<String>,
        #[arg(long)]
        context_window: Option<i64>,
        #[arg(long)]
        max_output_tokens: Option<i64>,
        /// Comma-separated capabilities (text,vision,reasoning,tool_calling,audio).
        #[arg(long)]
        capabilities: Option<String>,
        #[arg(long)]
        input_price: Option<f64>,
        #[arg(long)]
        output_price: Option<f64>,
    },
    Remove {
        id: String,
    },
}

#[derive(Args, Debug, Clone)]
pub struct AccountArgs {
    #[command(subcommand)]
    pub action: AccountAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum AccountAction {
    List {
        /// Filter by provider name or ID.
        #[arg(long)]
        provider: Option<String>,
    },
    Add {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        label: String,
        #[arg(long)]
        api_key: String,
        #[arg(long, default_value_t = 1)]
        priority: i64,
        #[arg(long, default_value_t = 1)]
        weight: i64,
        #[arg(long)]
        soft_quota_usd: Option<f64>,
        /// none | daily | monthly | rolling
        #[arg(long, default_value = "none")]
        quota_type: String,
    },
    Remove {
        id: String,
    },
    /// Clear cooldown/exhaustion/circuit state.
    Reset {
        id: String,
    },
}

#[derive(Args, Debug, Clone)]
pub struct RouteArgs {
    #[command(subcommand)]
    pub action: RouteAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum RouteAction {
    List,
    Add {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "")]
        description: String,
        /// priority | round-robin | weighted | least-used | adaptive
        #[arg(long, default_value = "priority")]
        strategy: String,
        /// reject | strip_with_warning
        #[arg(long, default_value = "strip_with_warning")]
        portability_policy: String,
        #[arg(long, default_value_t = false)]
        cache_affinity: bool,
        #[arg(long, default_value_t = 5)]
        max_attempts: i64,
        /// Targets as `provider/upstream_id` (repeatable, ordered).
        #[arg(long = "target", required = true)]
        targets: Vec<String>,
    },
    /// Validate a saved Route by name or id.
    Validate {
        selector: String,
    },
    /// Simulate routing for a model/Route using an optional JSON descriptor file.
    DryRun {
        #[arg(long)]
        model: String,
        /// JSON object with the same fields accepted by the Admin API.
        #[arg(long)]
        descriptor: Option<PathBuf>,
    },
    Remove {
        id: String,
    },
}

#[derive(Args, Debug, Clone)]
pub struct AliasArgs {
    #[command(subcommand)]
    pub action: AliasAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum AliasAction {
    List,
    Add {
        #[arg(long)]
        alias: String,
        /// model | route
        #[arg(long, default_value = "model")]
        target_type: String,
        /// A model label (`provider/upstream_id`) or a route name.
        #[arg(long)]
        target: String,
        #[arg(long, default_value = "")]
        description: String,
    },
    Remove {
        alias: String,
    },
}

#[derive(Args, Debug, Clone)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub action: PluginAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum PluginAction {
    /// Install a `.kxp` package from a file or from the official marketplace catalog.
    Install {
        /// Path to a local `.kxp` file.
        #[arg(conflicts_with = "catalog")]
        path: Option<String>,
        /// Install directly by official catalog/marketplace plugin ID.
        #[arg(long, conflicts_with = "path")]
        catalog: Option<String>,
        /// Automatically approve declared permissions after installation.
        #[arg(long)]
        approve: bool,
        /// Enable the plugin immediately (requires approved permissions).
        #[arg(long)]
        enable: bool,
        /// Expected SHA-256 (optional; recorded regardless).
        #[arg(long)]
        sha256: Option<String>,
        /// Trusted Ed25519 publisher public key (base64 or hex); repeatable.
        #[arg(long = "trusted-key")]
        trusted_keys: Vec<String>,
        /// Allow a present-but-untrusted signature.
        #[arg(long)]
        allow_untrusted_signature: bool,
    },
    /// Search available marketplace plugins.
    Search {
        /// Search query (matches ID, name, description, publisher).
        query: String,
        /// Filter by capability.
        #[arg(long)]
        capability: Option<String>,
    },
    /// Browse the official marketplace catalog.
    Catalog {
        #[command(subcommand)]
        action: CatalogAction,
    },
    /// Check for updates or update an installed catalog plugin.
    Update {
        /// Plugin ID to update (or all installed plugins if omitted).
        id: Option<String>,
        /// Automatically approve new permissions and enable after update.
        #[arg(long)]
        approve: bool,
    },
    /// List installed plugins.
    List,
    /// Show one plugin's manifest, permissions, and runtime state.
    Show { id: String },
    /// Enable an installed plugin.
    Enable { id: String },
    /// Disable a plugin (new requests stop referencing it).
    Disable { id: String },
    /// Validate a plugin by instantiating it (self-check).
    Validate { id: String },
    /// Remove a plugin and its stored state.
    Remove { id: String },
    /// List approved permission grants.
    Permissions { id: String },
    /// Approve the plugin's currently declared permission set (all-or-nothing).
    Approve { id: String },
    /// Revoke one permission grant (KV state is retained).
    Revoke { id: String, permission: String },
}

#[derive(Subcommand, Debug, Clone)]
pub enum CatalogAction {
    /// List plugins in the official marketplace.
    List {
        /// Refresh catalog from remote before listing.
        #[arg(long)]
        refresh: bool,
        /// Filter by capability.
        #[arg(long)]
        capability: Option<String>,
    },
    /// View detailed marketplace metadata and release information.
    Show {
        /// Catalog plugin ID.
        id: String,
        /// Refresh catalog from remote before showing.
        #[arg(long)]
        refresh: bool,
    },
}

#[derive(Args, Debug, Clone)]
pub struct ExportArgs {
    #[command(subcommand)]
    pub action: ExportAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ExportAction {
    /// List exported files on disk.
    List,
    /// Export one day (YYYY-MM-DD, default yesterday) to JSONL + CSV.
    Run {
        #[arg(long)]
        day: Option<String>,
    },
    /// Prune files older than the retention window.
    Prune,
}

#[derive(Args, Debug, Clone)]
pub struct BackupArgs {
    #[command(subcommand)]
    pub action: BackupAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum BackupAction {
    /// Run a backup now.
    Run,
    /// List backup files on disk.
    List,
}

#[derive(Args, Debug, Clone)]
pub struct UninstallArgs {
    /// Delete everything without asking (otherwise you must confirm).
    #[arg(long)]
    pub yes: bool,
    /// Also remove the `kinetix` binary from ~/.local/bin.
    #[arg(long)]
    pub remove_binary: bool,
    /// Keep the SQLite database (removes config/state/exports/backups only).
    #[arg(long)]
    pub keep_data: bool,
    /// Print what would be removed without deleting anything.
    #[arg(long)]
    pub dry_run: bool,
}

fn overrides(cli: &Cli) -> CliOverrides {
    CliOverrides {
        bind: cli.bind.clone(),
        database_url: cli.database_url.clone(),
        home: cli.home.clone(),
        config_file: cli.config.clone(),
        ..Default::default()
    }
}

fn config_from_cli(cli: &Cli) -> Result<Config> {
    Config::build(overrides(cli))
}

/// Open the control-plane database and derive the master key, without starting
/// the server. Used by every non-`serve` subcommand.
async fn open(cli: &Cli) -> Result<(Config, Pool, Crypto)> {
    let config = config_from_cli(cli)?;
    let pool = db::open_and_migrate(&config.database_url, &config.data_dir).await?;
    let crypto = Crypto::new(&config.master_key);
    Ok((config, pool, crypto))
}

pub async fn run(cli: Cli) -> Result<()> {
    let home = cli.home.clone();
    let config_file = cli.config.clone();
    let bind = cli.bind.clone();
    let database_url = cli.database_url.clone();
    match cli.command.clone() {
        Command::ApiReference => {
            println!(
                "{}",
                serde_json::to_string_pretty(&crate::router::admin_routes().reference())?
            );
            Ok(())
        }
        Command::Api(args) => cmd_admin_api(args).await,
        Command::Serve(args) => {
            serve(ServeOpts {
                home,
                config_file,
                bind,
                database_url,
                log_json: args.log_json,
                allow_private_upstreams: args.allow_private_upstreams,
                allow_insecure_tls: args.allow_insecure_tls,
            })
            .await
        }
        Command::Init => cmd_init(&cli).await,
        Command::Status => cmd_status(&cli).await,
        Command::Doctor => cmd_doctor(&cli).await,
        Command::Update(a) => crate::update::run(a).await,
        Command::Password(a) => cmd_password(&cli, a).await,
        Command::Key(a) => cmd_key(&cli, a).await,
        Command::Provider(a) => cmd_provider(&cli, a).await,
        Command::Model(a) => cmd_model(&cli, a).await,
        Command::Account(a) => cmd_account(&cli, a).await,
        Command::Route(a) => cmd_route(&cli, a).await,
        Command::Alias(a) => cmd_alias(&cli, a).await,
        Command::Plugin(a) => cmd_plugin(&cli, a).await,
        Command::Export(a) => cmd_export(&cli, a).await,
        Command::Backup(a) => cmd_backup(&cli, a).await,
        Command::Uninstall(a) => cmd_uninstall(&cli, a).await,
    }
}

async fn cmd_admin_api(args: AdminApiArgs) -> Result<()> {
    let mut url = url::Url::parse(&args.url).context("invalid Admin API base URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("Admin API URL must use http/https without credentials, query, or fragment");
    }
    let path = args.path.trim_start_matches('/');
    if path.contains('#') || path.starts_with("admin/api/") {
        bail!("path must be relative to /admin/api without a fragment");
    }
    let (path, query) = path
        .split_once('?')
        .map_or((path, None), |(p, q)| (p, Some(q)));
    url.set_path(&format!("/admin/api/{path}"));
    url.set_query(query);
    if !url.path().starts_with("/admin/api/") {
        bail!("path must remain within /admin/api");
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let method = args.method.parse::<reqwest::Method>()?;
    let mut request = client
        .request(method, url)
        .header("x-kinetix-admin-token", args.token);
    if let Some(path) = args.body {
        let bytes = tokio::fs::read(path)
            .await
            .context("reading JSON request body")?;
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).context("invalid JSON request body")?;
        request = request.json(&body);
    }
    let response = request.send().await.context("Admin API request failed")?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        let error: crate::admin_contract::ErrorBody = serde_json::from_str(&text)
            .context("server returned an unsupported Admin API error contract")?;
        println!("{}", serde_json::to_string_pretty(&error)?);
        bail!("Admin API request failed (HTTP {})", status.as_u16());
    }
    println!("{text}");
    Ok(())
}

// ---------------------------------------------------------------------------
// serve
// ---------------------------------------------------------------------------

pub struct ServeOpts {
    pub home: Option<std::path::PathBuf>,
    pub config_file: Option<std::path::PathBuf>,
    pub bind: Option<String>,
    pub database_url: Option<String>,
    pub log_json: bool,
    pub allow_private_upstreams: bool,
    pub allow_insecure_tls: bool,
}

/// Run the proxy server (the `serve` subcommand). This is the same startup
/// sequence as the library's server module, driven by CLI flags.
pub async fn serve(opts: ServeOpts) -> Result<()> {
    // A boolean CLI flag can only force the value ON; leaving it off must not
    // shadow an environment variable or config file that enables the same
    // option (precedence is CLI > env > file > default, but an absent flag is
    // not an override).
    let on = |v: bool| if v { Some(true) } else { None };
    let config = Arc::new(Config::build(CliOverrides {
        bind: opts.bind,
        database_url: opts.database_url,
        home: opts.home,
        config_file: opts.config_file,
        log_json: on(opts.log_json),
        allow_private_upstreams: on(opts.allow_private_upstreams),
        allow_insecure_tls: on(opts.allow_insecure_tls),
        ..Default::default()
    })?);

    crate::server::run(config).await
}

// ---------------------------------------------------------------------------
// simple commands
// ---------------------------------------------------------------------------

async fn cmd_init(cli: &Cli) -> Result<()> {
    let config = config_from_cli(cli)?;
    config.paths.ensure_dirs()?;
    println!("Kinetix initialized.");
    println!("  config dir : {}", config.paths.config_dir.display());
    println!("  data dir   : {}", config.paths.data_dir.display());
    println!("  state dir  : {}", config.paths.state_dir.display());
    println!("  database   : {}", config.database_url);
    if let Some(pw) = &config.generated_admin_password {
        println!("\nGenerated admin password (shown once — store it now):\n  {pw}");
    } else {
        println!("\nAdmin password already configured (change with `kinetix password set`).");
    }
    println!("\nStart the server with `kinetix serve`.");
    Ok(())
}

async fn cmd_status(cli: &Cli) -> Result<()> {
    let config = config_from_cli(cli)?;
    println!("Kinetix {}", env!("CARGO_PKG_VERSION"));
    println!("  bind          : {}", config.bind);
    println!("  public base   : {}", config.public_base_url);
    println!("  database      : {}", config.database_url);
    println!("  config dir    : {}", config.paths.config_dir.display());
    println!("  data dir      : {}", config.paths.data_dir.display());
    println!("  state dir     : {}", config.paths.state_dir.display());
    println!("  exports dir   : {}", config.paths.exports_dir().display());
    println!("  backups dir   : {}", config.paths.backups_dir().display());
    println!("  ip rate limit : {}/min", config.ip_rate_limit_per_min);
    println!("  session ttl   : {} min", config.session_ttl_minutes);
    println!("  admin pw set  : {}", admin_password_set(&config));
    if let Ok((_, pool, _)) = open(cli).await {
        let keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM virtual_keys")
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
        let providers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM providers")
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
        let routes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM routes")
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
        let usage: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_request_logs")
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
        println!(
            "  control plane : {providers} provider(s), {keys} key(s), {routes} route(s), {usage} usage row(s)"
        );
    }
    Ok(())
}

async fn cmd_doctor(cli: &Cli) -> Result<()> {
    let config = config_from_cli(cli)?;
    let mut problems = 0;
    println!("Kinetix doctor");
    for (label, dir) in [
        ("config", &config.paths.config_dir),
        ("data", &config.paths.data_dir),
        ("state", &config.paths.state_dir),
    ] {
        if dir.exists() {
            println!("  [ok]   {label} dir exists: {}", dir.display());
        } else {
            println!(
                "  [warn] {label} dir missing: {} (run `kinetix init`)",
                dir.display()
            );
            problems += 1;
        }
    }
    match db::open_and_migrate(&config.database_url, &config.data_dir).await {
        Ok(_) => println!("  [ok]   database reachable and migrated"),
        Err(db::OpenDatabaseError::Connection(error)) => {
            println!("  [fail] database connection failed: {error}");
            problems += 1;
        }
        Err(db::OpenDatabaseError::Startup(error)) => {
            println!("  [fail] database startup failed (backup, migration, or repair): {error}");
            problems += 1;
        }
    }
    let url = format!("{}/healthz", config.public_base_url.trim_end_matches('/'));
    match reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(r) => println!("  [ok]   server responding at {url} ({})", r.status()),
        Err(_) => println!("  [info] no server responding at {url} (not running?)"),
    }
    if problems == 0 {
        println!("\nNo blocking problems found.");
    } else {
        println!("\n{problems} item(s) need attention.");
    }
    Ok(())
}

fn admin_password_set(config: &Config) -> bool {
    config.paths.config_dir.join("admin_password.hash").exists()
}

async fn cmd_password(cli: &Cli, args: PasswordArgs) -> Result<()> {
    match args.action {
        PasswordAction::Show => {
            let config = config_from_cli(cli)?;
            println!(
                "admin password {}",
                if admin_password_set(&config) {
                    "is configured"
                } else {
                    "is NOT configured (a fresh `kinetix serve` will generate one)"
                }
            );
            Ok(())
        }
        PasswordAction::Set { password } => {
            if password.trim().len() < 8 {
                bail!("password must be at least 8 characters");
            }
            let config = config_from_cli(cli)?;
            config.paths.ensure_dirs()?;
            let hash = crate::crypto::hash_virtual_key(password.trim());
            std::fs::write(config.paths.config_dir.join("admin_password.hash"), &hash)?;
            // Also persist into the DB setting so a running server picks it up.
            if let Ok(pool) = db::open_and_migrate(&config.database_url, &config.data_dir).await {
                let _ = db::set_setting(&pool, crate::auth::ADMIN_PASSWORD_SETTING, &hash).await;
            }
            println!("Admin password updated. Existing dashboard sessions are invalidated.");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// keys
// ---------------------------------------------------------------------------

async fn cmd_key(cli: &Cli, args: KeyArgs) -> Result<()> {
    let (_, pool, _) = open(cli).await?;
    match args.action {
        KeyAction::List => {
            for k in db::list_virtual_keys(&pool).await? {
                println!(
                    "{}\t{}\tstatus={}\tmodels={}",
                    k.id, k.name, k.status, k.allowed_models
                );
            }
            Ok(())
        }
        KeyAction::Create {
            name,
            owner,
            tag,
            allowed_models,
            rpm,
            tpm,
            daily_budget,
            monthly_budget,
        } => {
            let (full, hash) = crypto_generate_key();
            let id = format!("key_{}", uuid::Uuid::new_v4().simple());
            let allowed: Vec<String> = allowed_models
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let allowed = if allowed.is_empty() {
                vec!["*".to_string()]
            } else {
                allowed
            };
            sqlx::query(
                "INSERT INTO virtual_keys
                 (id, key_hash, name, owner, tag, allowed_models, allowed_providers, rpm_limit, tpm_limit,
                  daily_budget, monthly_budget, expires_at, status, allowed_ips, body_logging, created_at)
                 VALUES (?,?,?,?,?,?,'[]',?,?,?,?,NULL,'active','[]',0,?)",
            )
            .bind(&id)
            .bind(&hash)
            .bind(&name)
            .bind(&owner)
            .bind(&tag)
            .bind(serde_json::to_string(&allowed)?)
            .bind(rpm)
            .bind(tpm)
            .bind(daily_budget)
            .bind(monthly_budget)
            .bind(db::now_iso())
            .execute(&pool)
            .await?;
            println!("Virtual key created (shown once — store it now):\n  {full}");
            println!("  id: {id}");
            Ok(())
        }
        KeyAction::Revoke { id } => {
            db::delete_virtual_key_cascade(&pool, &id).await?;
            println!("revoked and removed key {id}");
            Ok(())
        }
        KeyAction::Disable { id } => {
            db::set_virtual_key_status(&pool, &id, "disabled").await?;
            println!("disabled key {id}");
            Ok(())
        }
        KeyAction::Enable { id } => {
            db::set_virtual_key_status(&pool, &id, "active").await?;
            println!("enabled key {id}");
            Ok(())
        }
    }
}

/// Generate a virtual key (`sk-kinetix-...`) and its stored hash.
fn crypto_generate_key() -> (String, String) {
    let full = crate::crypto::generate_virtual_key();
    let hash = crate::crypto::hash_virtual_key(&full);
    (full, hash)
}

// ---------------------------------------------------------------------------
// providers / models / accounts / routes / aliases
// ---------------------------------------------------------------------------

async fn cmd_provider(cli: &Cli, args: ProviderArgs) -> Result<()> {
    let (_, pool, crypto) = open(cli).await?;
    match args.action {
        ProviderAction::List => {
            for p in db::list_providers(&pool).await? {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    p.id, p.name, p.wire_format, p.auth_scheme, p.base_url
                );
            }
            Ok(())
        }
        ProviderAction::Add {
            name,
            base_url,
            wire_format,
            auth_scheme,
            custom_header_name,
            custom_param_name,
            models_path,
            timeout_ms,
            api_key,
            account_label,
            connection_parameters,
        } => {
            let problems = crate::validate::validate_provider_schema(
                &name,
                &base_url,
                &wire_format,
                &auth_scheme,
                custom_header_name.as_deref(),
                custom_param_name.as_deref(),
            );
            anyhow::ensure!(problems.is_empty(), "{}", problems.join("; "));
            let anonymous = auth_scheme == "none";
            anyhow::ensure!(
                !anonymous
                    || (api_key.is_none()
                        && custom_header_name.is_none()
                        && custom_param_name.is_none()),
                "no-auth providers must not configure auth fields"
            );
            let parameters: Option<crate::provider_connection::ConnectionParameters> =
                match connection_parameters {
                    Some(path) => Some(serde_json::from_str(
                        &tokio::fs::read_to_string(path).await?,
                    )?),
                    None => None,
                };
            crate::provider_connection::resolve_endpoint(
                &base_url,
                models_path.as_deref(),
                parameters.as_ref(),
            )
            .map_err(anyhow::Error::msg)?;
            let id = format!("prov_{}", uuid::Uuid::new_v4().simple());
            sqlx::query(
                "INSERT INTO providers
                 (id, name, base_url, wire_format, auth_scheme, custom_header_name, custom_param_name,
                  extra_headers, timeout_ms, capability_mode, models_path, rate_limit_rules, enabled,
                  follow_redirects, credential_hosts, allow_insecure_tls, created_at, credential_mode, connection_parameters)
                 VALUES (?,?,?,?,?,?,?,'{}',?,'permissive',?,'{}',1,0,'',0,?,?,?)",
            )
            .bind(&id)
            .bind(&name)
            .bind(&base_url)
            .bind(&wire_format)
            .bind(&auth_scheme)
            .bind(&custom_header_name)
            .bind(&custom_param_name)
            .bind(timeout_ms)
            .bind(&models_path)
            .bind(db::now_iso())
            .bind(if anonymous { "none" } else { "manual" })
            .bind(parameters.map(|parameters| serde_json::to_string(&parameters)).transpose()?)
            .execute(&pool)
            .await?;
            println!("provider created: {id}");
            if anonymous {
                db::insert_account(&pool, &id, "__kinetix_noauth__", "", "", 1, 1, None, "none")
                    .await?;
            }
            if let Some(key) = api_key {
                let label = account_label.unwrap_or_else(|| "Primary key".to_string());
                let acc_id =
                    insert_account_row(&pool, &crypto, &id, &label, &key, 1, 1, None, "none")
                        .await?;
                println!("account created: {acc_id}");
            }
            Ok(())
        }
        ProviderAction::Remove { id } => {
            db::delete_provider(&pool, &id).await?;
            println!("removed provider {id}");
            Ok(())
        }
    }
}

async fn cmd_model(cli: &Cli, args: ModelArgs) -> Result<()> {
    let (_, pool, _) = open(cli).await?;
    match args.action {
        ModelAction::List => {
            for m in db::list_models(&pool).await? {
                println!(
                    "{}\t{}\t{}\tenabled={}",
                    m.id, m.upstream_id, m.display_name, m.enabled
                );
            }
            Ok(())
        }
        ModelAction::Add {
            provider,
            upstream_id,
            display_name,
            context_window,
            max_output_tokens,
            capabilities,
            input_price,
            output_price,
        } => {
            let provider_id = resolve_provider(&pool, &provider).await?;
            let id = format!("model_{}", uuid::Uuid::new_v4().simple());
            let caps = match capabilities {
                Some(c) => {
                    let mut obj = serde_json::Map::new();
                    for cap in c.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                        obj.insert(cap.to_string(), serde_json::json!(true));
                    }
                    serde_json::Value::Object(obj).to_string()
                }
                None => "{}".to_string(),
            };
            let prices = serde_json::json!({
                "input_per_1m": input_price,
                "output_per_1m": output_price,
            })
            .to_string();
            let display = display_name.unwrap_or_else(|| upstream_id.clone());
            sqlx::query(
                "INSERT INTO models
                 (id, provider_id, upstream_id, display_name, enabled, context_window, max_output_tokens,
                  capabilities, prices, parameters, thinking_map, extra_request, discovery, created_at)
                 VALUES (?,?,?,?,1,?,?,?,?,'{}','{}','{}','{}',?)",
            )
            .bind(&id)
            .bind(&provider_id)
            .bind(&upstream_id)
            .bind(&display)
            .bind(context_window)
            .bind(max_output_tokens)
            .bind(&caps)
            .bind(&prices)
            .bind(db::now_iso())
            .execute(&pool)
            .await?;
            println!("model created: {id}");
            Ok(())
        }
        ModelAction::Remove { id } => {
            db::delete_model(&pool, &id).await?;
            println!("removed model {id}");
            Ok(())
        }
    }
}

async fn cmd_account(cli: &Cli, args: AccountArgs) -> Result<()> {
    let (_, pool, crypto) = open(cli).await?;
    match args.action {
        AccountAction::List { provider } => {
            let mut accounts = db::list_accounts(&pool).await?;
            let providers = db::list_providers(&pool).await?;
            if let Some(provider) = provider {
                let provider_id = resolve_provider(&pool, &provider).await?;
                accounts.retain(|a| a.provider_id == provider_id);
            }

            for p in &providers {
                let group: Vec<_> = accounts.iter().filter(|a| a.provider_id == p.id).collect();
                if group.is_empty() {
                    continue;
                }
                println!("{} ({})", p.name, p.id);
                for a in group {
                    println!(
                        "  {}\t{}\tTier #{}\tstatus={}",
                        a.id, a.label, a.priority, a.status
                    );
                }
            }

            for a in accounts
                .iter()
                .filter(|a| !providers.iter().any(|p| p.id == a.provider_id))
            {
                println!("{} ({})", a.provider_id, a.provider_id);
                println!(
                    "  {}\t{}\tTier #{}\tstatus={}",
                    a.id, a.label, a.priority, a.status
                );
            }
            Ok(())
        }
        AccountAction::Add {
            provider,
            label,
            api_key,
            priority,
            weight,
            soft_quota_usd,
            quota_type,
        } => {
            let provider_id = resolve_provider(&pool, &provider).await?;
            let row = db::get_provider(&pool, &provider_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("provider not found"))?;
            anyhow::ensure!(
                row.credential_mode == "manual",
                "provider does not use manual credential enrollment"
            );
            let id = insert_account_row(
                &pool,
                &crypto,
                &provider_id,
                &label,
                &api_key,
                priority,
                weight,
                soft_quota_usd,
                &quota_type,
            )
            .await?;
            println!("account created: {id}");
            Ok(())
        }
        AccountAction::Remove { id } => {
            db::delete_account(&pool, &id).await?;
            println!("removed account {id}");
            Ok(())
        }
        AccountAction::Reset { id } => {
            db::set_account_status(&pool, &id, "healthy", "operator_reset", None, None, None)
                .await?;
            println!("reset account {id}");
            Ok(())
        }
    }
}

async fn cmd_route(cli: &Cli, args: RouteArgs) -> Result<()> {
    let (config, pool, crypto) = open(cli).await?;
    match args.action {
        RouteAction::Validate { selector } => {
            let state = route_tool_state(config, pool, crypto).await?;
            let validation = crate::route_validation::validate_saved(&state, &selector).await?;
            println!("{}", serde_json::to_string_pretty(&validation)?);
            if !validation.valid {
                bail!("Route validation failed");
            }
            Ok(())
        }
        RouteAction::DryRun { model, descriptor } => {
            let state = route_tool_state(config, pool, crypto).await?;
            let descriptor = match descriptor {
                Some(path) => {
                    let contents = tokio::fs::read_to_string(&path).await.with_context(|| {
                        format!("reading dry-run descriptor {}", path.display())
                    })?;
                    serde_json::from_str::<crate::pipeline::DryRunRequest>(&contents)
                        .with_context(|| format!("parsing dry-run descriptor {}", path.display()))?
                }
                None => crate::pipeline::DryRunRequest::default(),
            };
            let result = crate::pipeline::dry_run(&state, &model, &descriptor).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        RouteAction::List => {
            for r in db::list_routes(&pool).await? {
                let targets = db::route_targets(&pool, &r.id).await?;
                println!(
                    "{}\t{}\t{}\t{} target(s)",
                    r.id,
                    r.name,
                    r.strategy,
                    targets.len()
                );
            }
            Ok(())
        }
        RouteAction::Add {
            name,
            description,
            strategy,
            portability_policy,
            cache_affinity,
            max_attempts,
            targets,
        } => {
            let mut resolved_targets = Vec::with_capacity(targets.len());
            let mut validation_targets = Vec::with_capacity(targets.len());
            for (index, spec) in targets.iter().enumerate() {
                let (provider_name, upstream) = spec
                    .split_once('/')
                    .with_context(|| format!("target '{spec}' must be provider/upstream_id"))?;
                let provider_id = resolve_provider(&pool, provider_name).await?;
                let model = db::find_model_by_upstream(&pool, &provider_id, upstream)
                    .await?
                    .with_context(|| format!("no model {upstream} on provider {provider_name}"))?;
                validation_targets.push(crate::route_validation::RouteTargetConfig {
                    model_id: model.id.clone(),
                    account_id: None,
                    priority: index as i64 + 1,
                    weight: 1,
                    predicate: serde_json::json!({}),
                    param_overrides: serde_json::json!({}),
                });
                resolved_targets.push(model.id);
            }

            let state = route_tool_state(config, pool.clone(), crypto).await?;
            let validation = crate::route_validation::validate(
                &state,
                &crate::route_validation::RouteConfig {
                    id: None,
                    name: name.clone(),
                    strategy: strategy.clone(),
                    portability_policy: portability_policy.clone(),
                    fallback_triggers: serde_json::json!({
                        "on429": true,
                        "onQuota": true,
                        "on5xx": true,
                        "onTimeout": true,
                    }),
                    max_attempts: Some(max_attempts),
                    max_concurrent_requests: None,
                    enabled: true,
                    targets: validation_targets,
                },
            )
            .await?;
            if !validation.valid {
                let errors = validation
                    .issues
                    .iter()
                    .filter(|issue| issue.severity == "error")
                    .map(|issue| format!("{}: {}", issue.code, issue.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                bail!("Route validation failed: {errors}");
            }

            let id = db::insert_route(
                &pool,
                &db::NewRoute {
                    name: &name,
                    description: &description,
                    strategy: &strategy,
                    fallback_triggers: serde_json::json!({
                        "on429": true,
                        "onQuota": true,
                        "on5xx": true,
                        "onTimeout": true,
                    }),
                    portability_policy: &portability_policy,
                    sticky_routing: false,
                    cache_affinity,
                    max_attempts: Some(max_attempts),
                    max_concurrent_requests: None,
                },
            )
            .await?;
            for (index, model_id) in resolved_targets.iter().enumerate() {
                db::insert_route_target(
                    &pool,
                    &id,
                    None,
                    model_id,
                    index as i64 + 1,
                    1,
                    "{}",
                    "{}",
                )
                .await?;
            }
            println!("route created: {id}");
            Ok(())
        }
        RouteAction::Remove { id } => {
            db::delete_route(&pool, &id).await?;
            println!("removed route {id}");
            Ok(())
        }
    }
}

async fn route_tool_state(
    config: Config,
    pool: Pool,
    crypto: Crypto,
) -> Result<crate::app::AppState> {
    let registry = Arc::new(crate::registry::Registry::new());
    registry.reload(&pool).await?;
    let state =
        crate::server::build_app_state(Arc::new(config), pool, registry, Arc::new(crypto)).await?;

    // Register enabled plugin adapters in memory only. Unlike server startup,
    // offline inspection must not reconcile or mutate plugin state.
    if let Some(manager) = state.plugin_manager().cloned() {
        for plugin in manager.list().await? {
            if !plugin.status().is_enabled() {
                continue;
            }
            let Some(manifest) = plugin.manifest() else {
                continue;
            };
            if let Err(error) = crate::plugins::adapter::register_declared_adapters(
                &state.adapters,
                manager.as_ref().clone(),
                &plugin.id,
                &manifest.provides,
            )
            .await
            {
                tracing::warn!(plugin = %plugin.id, error = %error, "plugin adapter unavailable to route inspection");
            }
        }
    }
    Ok(state)
}

async fn cmd_alias(cli: &Cli, args: AliasArgs) -> Result<()> {
    let (_, pool, _) = open(cli).await?;
    match args.action {
        AliasAction::List => {
            for a in db::list_aliases(&pool).await? {
                println!("{}\t{} -> {}", a.alias, a.target_type, a.target_id);
            }
            Ok(())
        }
        AliasAction::Add {
            alias,
            target_type,
            target,
            description,
        } => {
            let target_id = match target_type.as_str() {
                "route" => db::get_route_by_name(&pool, &target)
                    .await?
                    .map(|r| r.id)
                    .with_context(|| format!("no route named {target}"))?,
                _ => {
                    let (prov, upstream) = target.split_once('/').with_context(|| {
                        format!("model target '{target}' must be provider/upstream_id")
                    })?;
                    let provider_id = resolve_provider(&pool, prov).await?;
                    db::find_model_by_upstream(&pool, &provider_id, upstream)
                        .await?
                        .with_context(|| format!("no model {upstream} on provider {prov}"))?
                        .id
                }
            };
            db::upsert_alias(&pool, &alias, &target_type, &target_id, &description).await?;
            println!("alias '{alias}' set");
            Ok(())
        }
        AliasAction::Remove { alias } => {
            if let Some(a) = db::get_alias(&pool, &alias).await? {
                db::delete_alias(&pool, &a.id).await?;
                println!("removed alias '{alias}'");
            } else {
                println!("no alias '{alias}'");
            }
            Ok(())
        }
    }
}

async fn download_catalog_bytes(
    client: &reqwest::Client,
    distribution: &crate::plugins::catalog::CatalogDistribution,
) -> Result<Vec<u8>> {
    let mut url = url::Url::parse(&distribution.url)
        .with_context(|| format!("invalid catalog artifact URL: {}", distribution.url))?;

    for redirect_count in 0..=5 {
        crate::plugins::catalog::validate_download_url(distribution, &url)?;

        let response = client
            .get(url.clone())
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .with_context(|| format!("catalog artifact download failed for {url}"))?;

        if response.status().is_redirection() {
            if redirect_count == 5 {
                bail!("catalog artifact exceeded redirect limit");
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| anyhow!("catalog artifact redirect has no valid Location"))?;
            url = url
                .join(location)
                .with_context(|| format!("invalid catalog artifact redirect: {location}"))?;
            continue;
        }

        if !response.status().is_success() {
            bail!("catalog artifact returned HTTP {}", response.status());
        }

        let bytes = response
            .bytes()
            .await
            .context("reading catalog artifact response body")?
            .to_vec();

        if bytes.len() as u64 > crate::plugins::package::MAX_PACKAGE_BYTES {
            bail!("catalog artifact exceeds package size limit");
        }

        return Ok(bytes);
    }

    bail!("catalog artifact download failed")
}

async fn cmd_plugin(cli: &Cli, args: PluginArgs) -> Result<()> {
    let (config, pool, crypto) = open(cli).await?;
    let crypto = std::sync::Arc::new(crypto);
    let manager = crate::plugins::PluginManager::new(
        pool.clone(),
        crypto,
        crate::plugins::HostPolicy::default(),
        config.paths.plugin_packages_dir(),
    )
    .context("building plugin host")?;

    let http_client = reqwest::Client::builder()
        .user_agent(concat!("kinetix-cli/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let cache_file = config.paths.plugin_catalog_cache_file();

    match args.action {
        PluginAction::Install {
            path,
            catalog: catalog_id,
            approve,
            enable,
            sha256,
            trusted_keys,
            allow_untrusted_signature,
        } => {
            if let Some(cat_id) = catalog_id {
                let catalog = crate::plugins::catalog::load_catalog(
                    Some(&http_client),
                    Some(&cache_file),
                    false,
                )
                .await?;

                let plugin = crate::plugins::catalog::find_plugin_in_catalog(&catalog, &cat_id)
                    .ok_or_else(|| anyhow!("catalog plugin '{cat_id}' not found in marketplace"))?;

                let distribution = plugin.distribution.as_ref().ok_or_else(|| {
                    anyhow!("catalog plugin '{cat_id}' has no installable distribution")
                })?;

                let trust = crate::plugins::catalog::embedded_trust_store()?;
                if !crate::plugins::catalog::install_ready(plugin, &trust)? {
                    bail!("catalog plugin '{cat_id}' is not install-ready: signed artifact metadata or publisher trust is unavailable");
                }

                let keys = crate::plugins::catalog::trusted_keys(&trust, plugin)?;
                if keys.is_empty() {
                    bail!("catalog publisher key is not trusted");
                }

                println!("Downloading {} v{}...", plugin.id, plugin.latest_version);
                let bytes = download_catalog_bytes(&http_client, distribution).await?;

                let pkg = crate::plugins::package::read_package(&bytes)?;
                if !distribution
                    .sha256
                    .eq_ignore_ascii_case(&pkg.package_sha256)
                {
                    bail!(
                        "catalog package hash mismatch: expected {}, computed {}",
                        distribution.sha256,
                        pkg.package_sha256
                    );
                }

                let validated = crate::plugins::package::validate_manifest(&pkg, manager.policy())?;
                if validated.manifest.id != plugin.id {
                    bail!(
                        "catalog artifact id mismatch: expected '{}', package declares '{}'",
                        plugin.id,
                        validated.manifest.id
                    );
                }
                if validated.manifest.version != plugin.latest_version {
                    bail!(
                        "catalog artifact version mismatch: expected '{}', package declares '{}'",
                        plugin.latest_version,
                        validated.manifest.version
                    );
                }

                let sig_status = crate::plugins::package::verify_signature(&pkg, &keys)?;
                if sig_status != crate::plugins::package::SignatureStatus::Verified {
                    bail!("catalog package is not signed by trusted publisher key");
                }

                let source = format!("catalog:{}@{}", plugin.id, plugin.latest_version);
                let outcome = manager
                    .install_from_source(&bytes, Some(&distribution.sha256), &keys, false, &source)
                    .await?;

                println!(
                    "installed {} v{} (signature: {}); {} capabilities",
                    outcome.id,
                    outcome.version,
                    outcome.signature.as_str(),
                    outcome.provides.len()
                );
                for p in &outcome.provides {
                    println!("  provides {:?} = {}", p.capability, p.name);
                }

                let perms = &validated.manifest.permissions;
                println!("\nRequested permissions:");
                if perms.network_hosts.is_empty()
                    && perms.credential_scopes.is_empty()
                    && !perms.credential_read
                {
                    println!("  (none)");
                } else {
                    if !perms.network_hosts.is_empty() {
                        println!("  network_hosts: {}", perms.network_hosts.join(", "));
                    }
                    if !perms.credential_scopes.is_empty() {
                        println!(
                            "  credential_scopes: {}",
                            perms.credential_scopes.join(", ")
                        );
                    }
                    if perms.credential_read {
                        println!("  credential_read: true");
                    }
                }

                if approve || enable {
                    let grants = manager.approve_permissions(&outcome.id).await?;
                    println!("approved {} grant(s) for '{}'", grants.len(), outcome.id);
                } else {
                    println!("\nInstalled disabled pending permission review. Run:");
                    println!("  kinetix plugin approve {}", outcome.id);
                    println!("  kinetix plugin enable {}", outcome.id);
                }

                if enable {
                    manager.enable(&outcome.id).await?;
                    println!("enabled '{}'", outcome.id);
                }

                Ok(())
            } else if let Some(target) = path {
                let (bytes, source) = if target.starts_with("http://")
                    || target.starts_with("https://")
                {
                    println!("Downloading package from {}...", target);
                    let b =
                        crate::plugins::catalog::download_package_from_url(&http_client, &target)
                            .await?;
                    (b, format!("url:{}", target))
                } else {
                    let b = std::fs::read(&target).with_context(|| format!("reading {target}"))?;
                    (b, format!("file:{}", target))
                };

                let mut trusted: Vec<[u8; 32]> =
                    trusted_keys.iter().filter_map(|k| decode_key(k)).collect();

                if trusted.is_empty() {
                    if let Ok(store) = crate::plugins::catalog::embedded_trust_store() {
                        for publ in store
                            .publishers
                            .iter()
                            .filter(|p| p.enabled && p.algorithm == "ed25519")
                        {
                            if let Some(k) = decode_key(&publ.public_key_base64) {
                                trusted.push(k);
                            }
                        }
                    }
                }

                let outcome = manager
                    .install_from_source(
                        &bytes,
                        sha256.as_deref(),
                        &trusted,
                        allow_untrusted_signature,
                        &source,
                    )
                    .await?;
                println!(
                    "installed {} v{} (signature: {}); {} capabilities",
                    outcome.id,
                    outcome.version,
                    outcome.signature.as_str(),
                    outcome.provides.len()
                );
                for p in &outcome.provides {
                    println!("  provides {:?} = {}", p.capability, p.name);
                }

                if approve || enable {
                    let grants = manager.approve_permissions(&outcome.id).await?;
                    println!("approved {} grant(s) for '{}'", grants.len(), outcome.id);
                }

                if enable {
                    manager.enable(&outcome.id).await?;
                    println!("enabled '{}'", outcome.id);
                }

                Ok(())
            } else {
                bail!("specify either a local file path or --catalog <id>");
            }
        }
        PluginAction::Search { query, capability } => {
            let catalog =
                crate::plugins::catalog::load_catalog(Some(&http_client), Some(&cache_file), false)
                    .await?;

            let filtered = crate::plugins::catalog::filter_catalog(
                &catalog.plugins,
                Some(&query),
                capability.as_deref(),
            );

            if filtered.is_empty() {
                println!("No marketplace plugins matched '{query}'.");
                return Ok(());
            }

            let installed = manager.list().await.unwrap_or_default();

            println!(
                "{:<32} {:<10} {:<12} NAME / DESCRIPTION",
                "ID", "VERSION", "STATUS"
            );
            println!("{}", "-".repeat(80));
            for p in filtered {
                let inst = installed.iter().find(|i| i.id == p.id);
                let status = match inst {
                    Some(i) if i.version == p.latest_version => "installed",
                    Some(_) => "update avail",
                    None => "available",
                };
                println!(
                    "{:<32} {:<10} {:<12} {}",
                    p.id, p.latest_version, status, p.name
                );
                println!("  {}", p.description);
                println!("  Capabilities: {}", p.capabilities.join(", "));
                println!();
            }
            Ok(())
        }
        PluginAction::Catalog { action } => match action {
            CatalogAction::List {
                refresh,
                capability,
            } => {
                let catalog = crate::plugins::catalog::load_catalog(
                    Some(&http_client),
                    Some(&cache_file),
                    refresh,
                )
                .await?;

                let filtered = crate::plugins::catalog::filter_catalog(
                    &catalog.plugins,
                    None,
                    capability.as_deref(),
                );

                let installed = manager.list().await.unwrap_or_default();

                println!("{:<32} {:<10} {:<12} NAME", "ID", "VERSION", "STATUS");
                println!("{}", "-".repeat(80));
                for p in filtered {
                    let inst = installed.iter().find(|i| i.id == p.id);
                    let status = match inst {
                        Some(i) if i.version == p.latest_version => "installed",
                        Some(_) => "update avail",
                        None => "available",
                    };
                    println!(
                        "{:<32} {:<10} {:<12} {}",
                        p.id, p.latest_version, status, p.name
                    );
                }
                Ok(())
            }
            CatalogAction::Show { id, refresh } => {
                let catalog = crate::plugins::catalog::load_catalog(
                    Some(&http_client),
                    Some(&cache_file),
                    refresh,
                )
                .await?;

                let p = crate::plugins::catalog::find_plugin_in_catalog(&catalog, &id)
                    .ok_or_else(|| anyhow!("plugin '{id}' not found in marketplace catalog"))?;

                let trust = crate::plugins::catalog::embedded_trust_store()?;
                let ready = crate::plugins::catalog::install_ready(p, &trust).unwrap_or(false);

                println!("ID:           {}", p.id);
                println!("Name:         {}", p.name);
                println!("Publisher:    {}", p.publisher);
                println!("Latest:       v{}", p.latest_version);
                println!("Homepage:     {}", p.homepage);
                println!("Description:  {}", p.description);
                println!("Capabilities: {}", p.capabilities.join(", "));
                println!("Installable:  {}", if p.installable { "yes" } else { "no" });
                println!(
                    "Trust Status: {}",
                    if ready {
                        "trusted & ready"
                    } else {
                        "verification pending"
                    }
                );
                if let Some(dist) = &p.distribution {
                    println!("Artifact URL: {}", dist.url);
                    println!("SHA-256:      {}", dist.sha256);
                    println!("Publisher Key:{}", dist.publisher_key_id);
                    println!("Allowed Hosts:{}", dist.allowed_hosts.join(", "));
                }
                if let Some(note) = &p.note {
                    println!("Note:         {}", note);
                }
                Ok(())
            }
        },
        PluginAction::Update { id, approve } => {
            let catalog =
                crate::plugins::catalog::load_catalog(Some(&http_client), Some(&cache_file), true)
                    .await?;

            let trust = crate::plugins::catalog::embedded_trust_store()?;
            let installed = manager.list().await?;

            let to_update: Vec<_> = match id {
                Some(ref target_id) => {
                    let inst = installed
                        .into_iter()
                        .find(|p| p.id == *target_id)
                        .ok_or_else(|| anyhow!("plugin '{target_id}' is not installed"))?;
                    vec![inst]
                }
                None => installed,
            };

            let mut updated_any = false;
            for inst in to_update {
                let Some(cat_entry) =
                    crate::plugins::catalog::find_plugin_in_catalog(&catalog, &inst.id)
                else {
                    continue;
                };

                if !crate::plugins::catalog::is_update_available(
                    &inst.version,
                    &cat_entry.latest_version,
                ) {
                    println!(
                        "{}: already up to date (installed: v{}, latest: v{})",
                        inst.id, inst.version, cat_entry.latest_version
                    );
                    continue;
                }

                println!(
                    "Updating {} from v{} to v{}...",
                    inst.id, inst.version, cat_entry.latest_version
                );

                let Some(distribution) = &cat_entry.distribution else {
                    println!("  Skipping {}: no distribution metadata", inst.id);
                    continue;
                };

                let keys = crate::plugins::catalog::trusted_keys(&trust, cat_entry)?;
                if keys.is_empty() {
                    println!("  Skipping {}: untrusted publisher key", inst.id);
                    continue;
                }

                let bytes = download_catalog_bytes(&http_client, distribution).await?;
                let pkg = crate::plugins::package::read_package(&bytes)?;
                if !distribution
                    .sha256
                    .eq_ignore_ascii_case(&pkg.package_sha256)
                {
                    bail!("package hash mismatch for {}", inst.id);
                }

                let sig_status = crate::plugins::package::verify_signature(&pkg, &keys)?;
                if sig_status != crate::plugins::package::SignatureStatus::Verified {
                    bail!("signature verification failed for {}", inst.id);
                }

                let source = format!("catalog:{}@{}", cat_entry.id, cat_entry.latest_version);
                let was_enabled = inst.status().is_enabled();

                let outcome = manager
                    .install_from_source(&bytes, Some(&distribution.sha256), &keys, false, &source)
                    .await?;

                println!(
                    "  Installed {} v{} (installed-disabled pending permission approval)",
                    outcome.id, outcome.version
                );

                if approve {
                    let grants = manager.approve_permissions(&outcome.id).await?;
                    println!("  Approved {} permission grant(s)", grants.len());
                    if was_enabled {
                        manager.enable(&outcome.id).await?;
                        println!("  Re-enabled {}", outcome.id);
                    }
                } else {
                    println!("  To review permissions and re-enable:");
                    println!("    kinetix plugin approve {}", outcome.id);
                    if was_enabled {
                        println!("    kinetix plugin enable {}", outcome.id);
                    }
                }

                updated_any = true;
            }

            if !updated_any {
                println!("All catalog plugins are up to date.");
            }

            Ok(())
        }
        PluginAction::List => {
            for p in manager.list().await? {
                let m = p.manifest();
                println!(
                    "{}\t{}\t{}\t{}",
                    p.id,
                    p.version,
                    p.status().as_str(),
                    m.map(|m| m.name).unwrap_or_default()
                );
            }
            Ok(())
        }
        PluginAction::Show { id } => {
            let row = manager
                .get(&id)
                .await?
                .with_context(|| format!("plugin '{id}' is not installed"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&crate::plugins::manager::manifest_summary(&row))?
            );
            Ok(())
        }
        PluginAction::Enable { id } => {
            manager.enable(&id).await?;
            println!("enabled '{id}'");
            Ok(())
        }
        PluginAction::Disable { id } => {
            manager.disable(&id).await?;
            println!("disabled '{id}'");
            Ok(())
        }
        PluginAction::Validate { id } => {
            let provides = manager.validate(&id).await?;
            println!("'{id}' validates; provides {} capabilities", provides.len());
            Ok(())
        }
        PluginAction::Remove { id } => {
            manager.remove(&id).await?;
            println!("removed '{id}'");
            Ok(())
        }
        PluginAction::Permissions { id } => {
            for g in crate::plugins::store::permissions(&pool, &id).await? {
                println!("{}\t{}", g.permission, g.value_json);
            }
            Ok(())
        }
        PluginAction::Approve { id } => {
            let grants = manager.approve_permissions(&id).await?;
            println!("approved {} grant(s) for '{id}'", grants.len());
            Ok(())
        }
        PluginAction::Revoke { id, permission } => {
            manager.revoke_permission(&id, &permission).await?;
            println!("revoked '{permission}' from '{id}'; plugin disabled (KV state retained)");
            Ok(())
        }
    }
}

/// Decode a base64 or hex Ed25519 public key.
fn decode_key(k: &str) -> Option<[u8; 32]> {
    use base64::Engine;
    if let Ok(b) = base64::engine::general_purpose::STANDARD.decode(k.trim()) {
        if let Ok(arr) = <[u8; 32]>::try_from(b.as_slice()) {
            return Some(arr);
        }
    }
    if let Ok(b) = hex::decode(k.trim()) {
        if let Ok(arr) = <[u8; 32]>::try_from(b.as_slice()) {
            return Some(arr);
        }
    }
    None
}

async fn cmd_export(cli: &Cli, args: ExportArgs) -> Result<()> {
    let (config, pool, _) = open(cli).await?;
    let dir = config.paths.exports_dir();
    match args.action {
        ExportAction::List => {
            for f in crate::export::list_files(&dir) {
                println!("{}\t{}\t{} bytes", f.day, f.name, f.bytes);
            }
            Ok(())
        }
        ExportAction::Run { day } => {
            let day = day.unwrap_or_else(|| {
                (chrono::Utc::now().date_naive() - chrono::Duration::days(1))
                    .format("%Y-%m-%d")
                    .to_string()
            });
            let (jsonl, csv) = crate::export::export_day(&pool, &dir, &day).await?;
            println!(
                "exported {day}:\n  {}\n  {}",
                jsonl.display(),
                csv.display()
            );
            Ok(())
        }
        ExportAction::Prune => {
            let n = crate::export::prune(&dir, config.export_retention_days as i64)?;
            println!(
                "pruned {n} file(s) older than {} days",
                config.export_retention_days
            );
            Ok(())
        }
    }
}

async fn cmd_backup(cli: &Cli, args: BackupArgs) -> Result<()> {
    let (config, pool, _) = open(cli).await?;
    match args.action {
        BackupAction::Run => {
            match db::scheduled_backup(&pool, &config.database_url, &config.data_dir, 14)
                .await
                .map_err(anyhow::Error::msg)?
            {
                Some(path) => println!("backup written: {}", path.display()),
                None => println!("nothing to back up (in-memory database)"),
            }
            Ok(())
        }
        BackupAction::List => {
            let dir = config.paths.backups_dir();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    println!("{}", e.file_name().to_string_lossy());
                }
            }
            Ok(())
        }
    }
}

async fn cmd_uninstall(cli: &Cli, args: UninstallArgs) -> Result<()> {
    // Resolve paths without requiring a working database (the point is to
    // remove it), so build the config directly.
    let config = config_from_cli(cli)?;
    let paths = &config.paths;

    let mut targets: Vec<PathBuf> = vec![paths.config_dir.clone(), paths.state_dir.clone()];
    if !args.keep_data {
        targets.push(paths.data_dir.clone());
    }

    println!("Kinetix uninstall");
    for t in &targets {
        let mark = if t.exists() { "remove" } else { "absent" };
        println!("  [{mark}] {}", t.display());
    }
    let bin = crate::paths::bin_dir().join("kinetix");
    if args.remove_binary {
        let mark = if bin.exists() { "remove" } else { "absent" };
        println!("  [{mark}] {}", bin.display());
    }

    if args.dry_run {
        println!("\nDry run — nothing was deleted.");
        return Ok(());
    }

    if !args.yes {
        print!("\nDelete the items above? This cannot be undone. [y/N] ");
        use std::io::Write as _;
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let answer = line.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("Aborted.");
            return Ok(());
        }
    }

    for t in &targets {
        match std::fs::remove_dir_all(t) {
            Ok(()) => println!("removed {}", t.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => println!("warning: could not remove {}: {e}", t.display()),
        }
    }
    if args.remove_binary {
        match std::fs::remove_file(&bin) {
            Ok(()) => println!("removed {}", bin.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => println!("warning: could not remove {}: {e}", bin.display()),
        }
    }

    println!("\nKinetix removed. Restart your shell if PATH still references the binary.");
    Ok(())
}

/// Insert an account row with an encrypted credential.
#[allow(clippy::too_many_arguments)]
async fn insert_account_row(
    pool: &Pool,
    crypto: &Crypto,
    provider_id: &str,
    label: &str,
    api_key: &str,
    priority: i64,
    weight: i64,
    soft_quota_usd: Option<f64>,
    quota_type: &str,
) -> Result<String> {
    let id = format!("acc_{}", uuid::Uuid::new_v4().simple());
    let secret_enc = crypto.encrypt(api_key)?;
    let mask = crate::crypto::mask_secret(api_key);
    sqlx::query(
        "INSERT INTO accounts
         (id, provider_id, label, secret_enc, key_mask, status, quota_type, soft_quota_usd, priority, weight, created_at)
         VALUES (?,?,?,?,?,'healthy',?,?,?,?,?)",
    )
    .bind(&id)
    .bind(provider_id)
    .bind(label)
    .bind(secret_enc)
    .bind(mask)
    .bind(quota_type)
    .bind(soft_quota_usd)
    .bind(priority)
    .bind(weight)
    .bind(db::now_iso())
    .execute(pool)
    .await?;
    Ok(id)
}

/// Resolve a provider by id or name.
async fn resolve_provider(pool: &Pool, id_or_name: &str) -> Result<String> {
    for p in db::list_providers(pool).await? {
        if p.id == id_or_name || p.name == id_or_name {
            return Ok(p.id);
        }
    }
    bail!("no provider matching '{id_or_name}'")
}
