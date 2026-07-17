// The parent Lore checkout bans raw Tokio spawning in favor of Lore context
// propagation macros. This is an independent service and intentionally has no
// dependency on lore-base, so ordinary Tokio tasks are the correct primitive.
#![allow(clippy::disallowed_methods)]

mod db;
mod keys;
mod lore_admin;
mod proto;
mod service;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use db::Database;
use keys::TokenIssuer;
use proto::epic_urc::urc_auth_api_server::UrcAuthApiServer;
use proto::rebac::rebac_api_server::RebacApiServer;
use service::AuthService;

#[derive(Debug, Parser)]
#[command(
    name = "lore-auth",
    version,
    about = "Standalone authentication service for EpicGames Lore"
)]
struct Cli {
    #[arg(long, env = "LORE_AUTH_DATA_DIR", default_value = "/data")]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Serve(ServeArgs),
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    Grant {
        #[command(subcommand)]
        command: GrantCommand,
    },
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[arg(long, env = "LORE_AUTH_HTTP_ADDR", default_value = "0.0.0.0:18080")]
    http_addr: SocketAddr,
    #[arg(long, env = "LORE_AUTH_GRPC_ADDR", default_value = "0.0.0.0:15051")]
    grpc_addr: SocketAddr,
    #[arg(long, env = "LORE_AUTH_PUBLIC_BASE_URL")]
    public_base_url: String,
    #[arg(long, env = "LORE_AUTH_ISSUER")]
    issuer: String,
    #[arg(long, env = "LORE_AUTH_AUDIENCE")]
    audience: Option<String>,
    #[arg(long, env = "LORE_AUTH_ENVIRONMENT", default_value = "local")]
    environment: String,
    #[arg(long, env = "LORE_AUTH_TOKEN_TTL_SECONDS", default_value_t = 3600)]
    token_ttl_seconds: u64,
    #[arg(long, env = "LORE_AUTH_LOGIN_TTL_SECONDS", default_value_t = 300)]
    login_ttl_seconds: u64,
    #[arg(long, env = "LORE_AUTH_LORE_GRPC_URL")]
    lore_grpc_url: Option<String>,
    #[arg(long, env = "LORE_AUTH_BOOTSTRAP_USERNAME")]
    bootstrap_username: Option<String>,
    #[arg(long, env = "LORE_AUTH_BOOTSTRAP_PASSWORD", hide_env_values = true)]
    bootstrap_password: Option<String>,
}

#[derive(Debug, Subcommand)]
enum UserCommand {
    Add {
        #[arg(long)]
        username: String,
        #[arg(long)]
        display_name: Option<String>,
        #[arg(long, env = "LORE_AUTH_PASSWORD", hide_env_values = true)]
        password: String,
        #[arg(long)]
        admin: bool,
    },
    List,
    Disable {
        username: String,
    },
    Enable {
        username: String,
    },
    Password {
        username: String,
        #[arg(long, env = "LORE_AUTH_PASSWORD", hide_env_values = true)]
        password: String,
    },
}

#[derive(Debug, Subcommand)]
enum GrantCommand {
    Set {
        username: String,
        resource: String,
        #[arg(long, value_delimiter = ',')]
        permissions: Vec<String>,
    },
    List {
        username: String,
    },
    Revoke {
        username: String,
        resource: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lore_auth=info".into()),
        )
        .json()
        .init();

    let mut cli = Cli::parse();
    if let Command::Serve(args) = &mut cli.command {
        // Image-level ENV declarations intentionally expose optional settings
        // as empty values in NAS container editors. Treat those empty strings
        // as unset instead of rejecting them as malformed configuration.
        args.lore_grpc_url = non_empty(args.lore_grpc_url.take());
        args.audience = non_empty(args.audience.take());
        args.bootstrap_username = non_empty(args.bootstrap_username.take());
        args.bootstrap_password = non_empty(args.bootstrap_password.take());
    }
    let db = Database::open(cli.data_dir.join("lore-auth.db"))?;
    match cli.command {
        Command::Serve(args) => serve(cli.data_dir, db, args).await,
        Command::User { command } => user_command(db, command),
        Command::Grant { command } => grant_command(db, command),
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn url_host(value: &str) -> Result<String> {
    let (_, remainder) = value
        .split_once("://")
        .context("URL must contain a scheme")?;
    let authority = remainder.split('/').next().unwrap_or_default();
    let host = if let Some(ipv6) = authority.strip_prefix('[') {
        ipv6.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    if host.is_empty() {
        bail!("URL must contain a host");
    }
    Ok(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::{non_empty, url_host};

    #[test]
    fn empty_image_environment_values_are_treated_as_unset() {
        assert_eq!(non_empty(None), None);
        assert_eq!(non_empty(Some(String::new())), None);
        assert_eq!(non_empty(Some("   ".to_string())), None);
        assert_eq!(
            non_empty(Some("http://lore-server:41337".to_string())),
            Some("http://lore-server:41337".to_string())
        );
    }

    #[test]
    fn audience_host_is_derived_from_the_authentication_url() {
        assert_eq!(
            url_host("https://lore.yxbro.com:12235/login").unwrap(),
            "lore.yxbro.com"
        );
        assert_eq!(url_host("https://[::1]:10443").unwrap(), "::1");
    }
}

async fn serve(data_dir: PathBuf, db: Database, args: ServeArgs) -> Result<()> {
    validate_serve_args(&args)?;
    let audience = url_host(&args.public_base_url)?;
    let bootstrap_username = args
        .bootstrap_username
        .clone()
        .context("LORE_AUTH_BOOTSTRAP_USERNAME must be set")?;
    db.ensure_bootstrap_admin(
        args.bootstrap_username.as_deref(),
        args.bootstrap_password.as_deref(),
    )?;
    let tokens = TokenIssuer::load_or_create(
        data_dir.join("jwt-private.pem"),
        args.issuer,
        audience,
        args.environment,
        args.token_ttl_seconds,
    )?;
    let state = AuthService::new(
        db,
        tokens,
        args.public_base_url,
        args.login_ttl_seconds,
        bootstrap_username,
        args.lore_grpc_url,
    );
    let http_listener = tokio::net::TcpListener::bind(args.http_addr)
        .await
        .with_context(|| format!("binding HTTP listener {}", args.http_addr))?;

    tracing::info!(address = %args.http_addr, "starting HTTP login and JWKS server");
    tracing::info!(address = %args.grpc_addr, "starting Lore authentication gRPC server (plaintext; terminate TLS at the reverse proxy)");

    let http = tokio::spawn(axum::serve(http_listener, web::router(state.clone())).into_future());
    let grpc = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(UrcAuthApiServer::new(state.clone()))
            .add_service(RebacApiServer::new(state))
            .serve(args.grpc_addr),
    );

    tokio::select! {
        result = http => result.context("HTTP server task failed")??,
        result = grpc => result.context("gRPC server task failed")??,
        _ = tokio::signal::ctrl_c() => tracing::info!("shutdown requested"),
    }
    Ok(())
}

fn validate_serve_args(args: &ServeArgs) -> Result<()> {
    if !args.public_base_url.starts_with("https://") && !args.public_base_url.starts_with("http://")
    {
        bail!("public base URL must start with http:// or https://");
    }
    if args.issuer.trim().is_empty() {
        bail!("issuer must not be empty");
    }
    let expected_audience = url_host(&args.public_base_url)?;
    if let Some(audience) = args.audience.as_deref()
        && audience != expected_audience
    {
        bail!(
            "audience must match the authentication domain: expected {expected_audience}, got {audience}"
        );
    }
    if let Some(url) = args.lore_grpc_url.as_deref()
        && !url.starts_with("http://")
        && !url.starts_with("https://")
    {
        bail!("Lore gRPC URL must start with http:// or https://");
    }
    if args.token_ttl_seconds < 60 || args.login_ttl_seconds < 60 {
        bail!("token and login TTLs must be at least 60 seconds");
    }
    Ok(())
}

fn user_command(db: Database, command: UserCommand) -> Result<()> {
    match command {
        UserCommand::Add {
            username,
            display_name,
            password,
            admin,
        } => {
            let display_name = display_name.unwrap_or_else(|| username.clone());
            let user = db.create_user(&username, &display_name, &password, admin)?;
            if admin {
                db.set_grant(&username, "urc-*", &["*".into()])?;
            }
            println!("created {} ({})", user.username, user.id);
        }
        UserCommand::List => {
            for user in db.list_users()? {
                println!(
                    "{}\t{}\tadmin={}\tdisabled={}",
                    user.username, user.id, user.is_admin, user.disabled
                );
            }
        }
        UserCommand::Disable { username } => db.set_disabled(&username, true)?,
        UserCommand::Enable { username } => db.set_disabled(&username, false)?,
        UserCommand::Password { username, password } => db.set_password(&username, &password)?,
    }
    Ok(())
}

fn grant_command(db: Database, command: GrantCommand) -> Result<()> {
    match command {
        GrantCommand::Set {
            username,
            resource,
            permissions,
        } => {
            db.set_grant(&username, &resource, &permissions)?;
            println!("grant updated");
        }
        GrantCommand::List { username } => {
            let user = db
                .find_user_by_username(&username)?
                .with_context(|| format!("user not found: {username}"))?;
            for grant in db.list_grants(&user.id, "")? {
                println!("{}\t{}", grant.resource_id, grant.permissions.join(","));
            }
            let wildcard = db.permissions_for(&user.id, "urc-*")?;
            if !wildcard.is_empty() {
                println!("urc-*\t{}", wildcard.join(","));
            }
        }
        GrantCommand::Revoke { username, resource } => db.revoke_grant(&username, &resource)?,
    }
    Ok(())
}
