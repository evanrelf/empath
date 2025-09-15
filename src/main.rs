use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser as _;
use etcetera::app_strategy::{AppStrategy as _, AppStrategyArgs, Xdg};
use jiff::Timestamp;
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use std::str::FromStr as _;
use tokio::{fs, process::Command};

#[derive(clap::Parser, Debug)]
struct Args {
    #[arg(value_name = "PATH")]
    paths: Vec<Utf8PathBuf>,
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

    let repo = repo().await?;

    for path in &args.paths {
        let path = path.canonicalize_utf8()?;

        if !path.starts_with(&repo) {
            continue;
        }

        log_path(&sqlite, &repo, &path).await?;
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
    let output = Command::new("git")
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
