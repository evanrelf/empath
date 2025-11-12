use camino::{Utf8Path, Utf8PathBuf, absolute_utf8};
use clap::Parser as _;
use etcetera::app_strategy::{AppStrategy as _, AppStrategyArgs, Xdg};
use jiff::Timestamp;
use pathdiff::diff_utf8_paths;
use sqlx::{
    Row as _, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
};
use std::{cmp::Ordering, collections::HashMap, env, str::FromStr as _};
use tokio::{fs, process};

#[derive(clap::Parser, Debug)]
#[command(disable_help_subcommand = true)]
struct Args {
    /// Run as if started in another Git repo instead of working directory
    #[arg(long)]
    repo: Option<Utf8PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Record path access
    Record {
        /// Record as if accessed at a different time
        #[arg(long)]
        time: Option<Timestamp>,

        #[arg(value_name = "PATH", required = true)]
        paths: Vec<Utf8PathBuf>,
    },

    /// Forget paths
    Forget {
        #[arg(value_name = "PATH", required = true)]
        paths: Vec<Utf8PathBuf>,
    },

    /// Print most frequent+recently accessed paths
    Frecent {
        /// Print absolute paths
        #[arg(long)]
        absolute: bool,
    },

    /// Print most recently accessed paths
    Recent {
        /// Print absolute paths
        #[arg(long)]
        absolute: bool,
    },

    /// Print most frequently accessed paths
    Frequent {
        /// Print absolute paths
        #[arg(long)]
        absolute: bool,
    },
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

    let sqlite = SqlitePool::connect_with(
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
        Command::Record { time, paths } => {
            for path in &paths {
                let path = absolute_utf8(path)?;
                // TODO: Allow recording files outside of repo? Need to exclude temporary files like
                // `*.jjdescription` and such.
                if path.starts_with(&repo) {
                    record(&sqlite, &repo, &path, time.as_ref()).await?;
                }
            }
        }
        Command::Forget { paths } => {
            for path in &paths {
                // Try to forget even if it doesn't exist anymore.
                let path = absolute_utf8(path).unwrap_or_else(|_| path.clone());
                forget(&sqlite, &repo, &path).await?;
            }
        }
        Command::Frecent { absolute } => {
            for path in frecent(&sqlite, &repo).await? {
                if !path.try_exists().unwrap_or(false) {
                    continue;
                }
                let path = if absolute {
                    path
                } else {
                    diff_utf8_paths(&path, &current_dir).unwrap()
                };
                println!("{path}");
            }
        }
        Command::Recent { absolute } => {
            for path in recent(&sqlite, &repo).await? {
                if !path.try_exists().unwrap_or(false) {
                    continue;
                }
                let path = if absolute {
                    path
                } else {
                    diff_utf8_paths(&path, &current_dir).unwrap()
                };
                println!("{path}");
            }
        }
        Command::Frequent { absolute } => {
            for path in frequent(&sqlite, &repo).await? {
                if !path.try_exists().unwrap_or(false) {
                    continue;
                }
                let path = if absolute {
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
        create table if not exists empath (
            repo text not null,
            path text not null,
            time text not null,
            unique (repo, path, time)
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

    if !output.status.success() {
        anyhow::bail!("Failed to get Git repo");
    }

    let repo = Utf8PathBuf::from(str::from_utf8(&output.stdout)?.trim());

    Ok(repo)
}

async fn record(
    sqlite: &SqlitePool,
    repo: &Utf8Path,
    path: &Utf8Path,
    time: Option<&Timestamp>,
) -> anyhow::Result<()> {
    let repo = repo.as_str();
    let path = path.as_str();

    let time = match time {
        Some(time) => time.to_string(),
        None => Timestamp::now().to_string(),
    };

    sqlx::query("insert into empath (repo, path, time) values ($1, $2, $3)")
        .bind(repo)
        .bind(path)
        .bind(time)
        .execute(sqlite)
        .await?;

    Ok(())
}

async fn forget(sqlite: &SqlitePool, repo: &Utf8Path, path: &Utf8Path) -> anyhow::Result<()> {
    let repo = repo.as_str();
    let path = path.as_str();

    sqlx::query("delete from empath where repo = $1 and path = $2")
        .bind(repo)
        .bind(path)
        .execute(sqlite)
        .await?;

    Ok(())
}

// https://wiki.mozilla.org/User:Jesse/NewFrecency
async fn frecent(sqlite: &SqlitePool, repo: &Utf8Path) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let repo = repo.as_str();

    let rows = sqlx::query(
        "
        select
            path,
            julianday('now') - julianday(time) as age_days
        from empath
        where repo = $1
        ",
    )
    .bind(repo)
    .fetch_all(sqlite)
    .await?;

    let half_life_days = 30.0;

    let mut scores = HashMap::new();

    for row in rows {
        let path: String = row.get("path");
        let age_days: f64 = row.get("age_days");
        let weight = 2f64.powf(-age_days / half_life_days);
        *scores.entry(path).or_insert(0.0) += weight;
    }

    let mut items = scores.into_iter().collect::<Vec<_>>();

    items.sort_by(|(_, a), (_, b)| b.partial_cmp(a).unwrap_or(Ordering::Equal));

    let paths = items
        .into_iter()
        .map(|(path, _)| Utf8PathBuf::from(path))
        .collect();

    Ok(paths)
}

async fn recent(sqlite: &SqlitePool, repo: &Utf8Path) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let repo = repo.as_str();

    let rows: Vec<String> = sqlx::query_scalar(
        "
        select path
        from empath
        where repo = $1
        group by path
        order by max(time) desc
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

async fn frequent(sqlite: &SqlitePool, repo: &Utf8Path) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let repo = repo.as_str();

    let rows: Vec<String> = sqlx::query_scalar(
        "
        select path
        from empath
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
