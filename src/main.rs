use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser as _;
use etcetera::app_strategy::{AppStrategy as _, AppStrategyArgs, Xdg};
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
        // TODO: `continue` if `path` is not in `repo`

        // TODO: Make `path` absolute ("canonicalize")

        track_path(&sqlite, &repo, path).await?;
    }

    Ok(())
}

async fn sqlite_init(sqlite: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "
        create table if not exists paths (
            repo text not null,
            path text not null,
            count int not null,
            unique (repo, path)
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

    let repo = Utf8PathBuf::from(str::from_utf8(&output.stdout)?);

    Ok(repo)
}

async fn track_path(sqlite: &SqlitePool, repo: &Utf8Path, path: &Utf8Path) -> anyhow::Result<()> {
    let repo = repo.as_str();
    let path = path.as_str();

    let count = sqlx::query_scalar("select count from paths where repo = $1 and path = $2")
        .bind(repo)
        .bind(path)
        .fetch_optional(sqlite)
        .await?
        .unwrap_or(0);

    sqlx::query("insert into paths (repo, path, count) values ($1, $2, $3);")
        .bind(repo)
        .bind(path)
        .bind(count + 1)
        .execute(sqlite)
        .await?;

    Ok(())
}
