use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser as _;
use etcetera::app_strategy::{AppStrategy as _, AppStrategyArgs, Xdg};
use jiff::Timestamp;
use pathdiff::diff_utf8_paths;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use std::{env, str::FromStr as _};
use tokio::{fs, process};

#[derive(clap::Parser, Debug)]
#[command(disable_help_subcommand = true)]
struct Args {
    /// Use specified Git repo instead of inferring from working directory
    #[arg(long)]
    repo: Option<Utf8PathBuf>,

    /// Print absolute paths
    #[arg(long)]
    absolute: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Record path access
    Log {
        #[arg(value_name = "PATH")]
        paths: Vec<Utf8PathBuf>,
    },

    /// Print most recently used paths
    Mru,

    /// Print most frequently used paths
    Mfu,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt::init();

    let xdg = Xdg::new(AppStrategyArgs {
        top_level_domain: String::from("com"),
        author: String::from("Evan Relf"),
        app_name: String::from("Empath"),
    })?;

    let state_dir = Utf8PathBuf::try_from(xdg.state_dir().unwrap())?;

    fs::create_dir_all(&state_dir).await?;

    let sqlite_path = state_dir.join("state.sqlite3");

    let sqlite = SqlitePoolOptions::new()
        .connect_with(
            SqliteConnectOptions::from_str(&format!("sqlite://{sqlite_path}"))?
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal),
        )
        .await?;

    sqlite_init(&sqlite).await?;

    let current_dir = Utf8PathBuf::try_from(env::current_dir()?)?;

    let repo = match args.repo {
        Some(repo) => repo,
        None => repo().await?,
    };

    match args.command {
        Command::Log { paths } => {
            for path in &paths {
                let path = path.canonicalize_utf8()?;
                log_path(&sqlite, &repo, &path).await?;
            }
        }
        Command::Mru => {
            for path in mru(&sqlite, &repo).await? {
                let path = if args.absolute {
                    path
                } else {
                    diff_utf8_paths(&path, &current_dir).unwrap()
                };
                println!("{path}");
            }
        }
        Command::Mfu => {
            for path in mfu(&sqlite, &repo).await? {
                let path = if args.absolute {
                    path
                } else {
                    diff_utf8_paths(&path, &current_dir).unwrap()
                };
                println!("{path}");
            }
        }
    }

    Ok(())
}

async fn sqlite_init(sqlite: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "
        create table if not exists log (
            repo text not null,
            path text not null,
            at text not null,
            unique (repo, path, at)
        ) strict;
        ",
    )
    .execute(sqlite)
    .await?;

    Ok(())
}

async fn repo() -> anyhow::Result<Utf8PathBuf> {
    let output = process::Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .await?;

    let repo = Utf8PathBuf::from(str::from_utf8(&output.stdout)?.trim());

    Ok(repo)
}

async fn log_path(sqlite: &SqlitePool, repo: &Utf8Path, path: &Utf8Path) -> anyhow::Result<()> {
    let repo = repo.as_str();
    let path = path.as_str();

    let now = Timestamp::now().to_string();

    sqlx::query("insert into log (repo, path, at) values ($1, $2, $3)")
        .bind(repo)
        .bind(path)
        .bind(now)
        .execute(sqlite)
        .await?;

    Ok(())
}

async fn mru(sqlite: &SqlitePool, repo: &Utf8Path) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let repo = repo.as_str();

    let rows: Vec<String> = sqlx::query_scalar(
        "
        select path
        from log
        where repo = $1
        group by path
        order by max(at) desc
        ",
    )
    .bind(repo)
    .fetch_all(sqlite)
    .await?;

    let paths = rows
        .into_iter()
        .map(|string| Utf8PathBuf::from(string))
        .collect();

    Ok(paths)
}

async fn mfu(sqlite: &SqlitePool, repo: &Utf8Path) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let repo = repo.as_str();

    let rows: Vec<String> = sqlx::query_scalar(
        "
        select path
        from log
        where repo = $1
        group by path
        order by count(*) desc
        ",
    )
    .bind(repo)
    .fetch_all(sqlite)
    .await?;

    let paths = rows
        .into_iter()
        .map(|string| Utf8PathBuf::from(string))
        .collect();

    Ok(paths)
}
