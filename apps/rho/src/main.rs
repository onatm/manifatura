//! rho: a coding agent in the terminal, on your ChatGPT plan. Built on manifatura: its commands run in a sandbox,
//! and it asks before a command leaves it.

mod agent;
mod mcp;
mod paths;
mod plan;
mod ui;

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use manifatura::Store;
use manifatura::store::JsonlStore;

use agent::{ChatOptions, recent_sessions};
use ui::logs::{LogSink, Logs};

/// A coding agent in the terminal, on your ChatGPT plan.
#[derive(Parser)]
#[command(name = "rho", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Continue the latest session in this directory.
    #[arg(short, long)]
    r#continue: bool,
    /// Resume a session by ID, or list this directory's sessions without one.
    #[arg(long, value_name = "ID", num_args = 0..=1)]
    resume: Option<Option<String>>,
    /// The model to use (`rho models` lists them); the plan's first otherwise.
    #[arg(short, long)]
    model: Option<String>,
    /// How much the model reasons: off, low, medium or high.
    #[arg(long)]
    effort: Option<String>,
    /// Keep a log, behind a button on the screen.
    #[arg(long)]
    logs: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in with ChatGPT, so rho runs on your plan.
    Login,
    /// Sign out.
    Logout,
    /// The models your plan offers.
    Models,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?;
    match cli.command {
        Some(Command::Login) => runtime.block_on(login()),
        Some(Command::Logout) => runtime.block_on(async {
            agent::sign_in().logout(false).await?;
            println!("Signed out.");
            Ok(())
        }),
        Some(Command::Models) => runtime.block_on(async {
            for m in agent::sign_in().models().await? {
                println!("{:<24}{}", m.slug, m.display_name);
            }
            Ok(())
        }),
        None => chat(cli, runtime),
    }
}

/// Signs in through the browser.
async fn login() -> Result<()> {
    let sign_in = agent::sign_in();
    let login = sign_in.login().await?;
    println!(
        "Sign in with ChatGPT in your browser:\n\n  {}\n",
        login.url()
    );
    open_browser(login.url());
    let account = login.finish().await?;
    println!(
        "Signed in as {}.",
        account.email.as_deref().unwrap_or(&account.subject)
    );
    if !account.plan_usage {
        println!("rho may not use your plan yet: sign in again and allow it.");
    }
    Ok(())
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn chat(cli: Cli, runtime: tokio::runtime::Runtime) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let store = Arc::new(JsonlStore::new(paths::sessions_dir(&cwd))?);
    if cli.resume == Some(None) {
        let sessions = runtime.block_on(recent_sessions(store.as_ref()))?;
        if sessions.is_empty() {
            println!("No sessions in this directory yet.");
        }
        for session in sessions {
            println!("{}  {}", session.id.as_str(), agent::ago(session.updated));
        }
        return Ok(());
    }
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "rho needs a terminal"
    );
    if agent::sign_in().account().is_none() {
        println!("rho runs on your ChatGPT plan; sign in first.");
        runtime.block_on(login())?;
    }
    let config = paths::config()?;
    let effort = cli
        .effort
        .or(config.effort)
        .map(|e| agent::parse_effort(&e))
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let options = ChatOptions {
        resume_latest: cli.r#continue,
        resume: cli.resume.flatten(),
        model: cli.model.or(config.model),
        effort,
        logs: cli.logs,
        mouse: config.mouse.unwrap_or(false),
        context_window: config.context_window,
    };
    let mcp = mcp::configured(&paths::config_dir().join("mcp.toml"))?;
    let (sink, mut logs) = ui::logs::channel();
    start_logging(cli.logs, sink);
    let start = ui::Start {
        cwd,
        store: store.clone(),
        options,
        mcp,
    };
    let (session, chatted) = runtime.block_on(ui::run(start, &mut logs));
    // Only a session that was used is kept, and worth coming back to.
    let session = session.filter(|id| {
        runtime
            .block_on(store.list())
            .is_ok_and(|sessions| sessions.iter().any(|s| s.id == *id))
    });
    drop(runtime);
    if let Some(id) = session {
        // The screen may leave the cursor mid-line.
        println!(
            "\r\x1b[2KSession {} · continue it with: rho --resume {}",
            id.as_str(),
            id.as_str()
        );
    }
    logs_rest(logs);
    chatted.context("rho stopped")
}

/// Sends the log to the screen: rho's own lines at info, the libraries' problems only.
fn start_logging(on: bool, sink: LogSink) {
    use tracing_subscriber::Layer;
    use tracing_subscriber::filter::{LevelFilter, Targets};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let level = if on {
        LevelFilter::INFO
    } else {
        LevelFilter::WARN
    };
    let ours = Targets::new()
        .with_default(LevelFilter::WARN)
        .with_target("rho", level);
    let log = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(sink)
        .with_ansi(false)
        .without_time()
        .with_level(false);
    tracing_subscriber::registry()
        .with(log.with_filter(ours))
        .init();
}

fn logs_rest(mut logs: Logs) {
    logs.print_rest();
}
