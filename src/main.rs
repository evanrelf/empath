use camino::{Utf8Path, Utf8PathBuf, absolute_utf8};
use clap::Parser as _;
use etcetera::app_strategy::{AppStrategy as _, AppStrategyArgs, Xdg};
use jiff::Timestamp;
use parse_datetime::parse_datetime;
use pathdiff::diff_utf8_paths;
use sqlx::{
    Row as _, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
};
use std::{
    cmp::Ordering,
    collections::HashMap,
    env,
    io::{self, Write},
    str::FromStr as _,
};
use tokio::{fs, process, task::JoinHandle};

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
        #[arg(long, value_parser = parse_timestamp)]
        time: Option<Timestamp>,

        #[arg(value_name = "PATH", required = true)]
        paths: Vec<Utf8PathBuf>,
    },

    /// Query recorded paths
    Query {
        /// Print absolute paths
        #[arg(long)]
        absolute: bool,

        /// Include ignored paths
        #[arg(long)]
        no_ignore: bool,

        /// Query at a different time
        #[arg(long, value_parser = parse_timestamp)]
        time: Option<Timestamp>,

        /// Print top n paths
        #[arg(long, default_value_t = 100)]
        limit: u32,

        #[command(subcommand)]
        command: QueryCommand,
    },

    /// Forget paths
    Forget {
        #[arg(value_name = "PATH", required = true)]
        paths: Vec<Utf8PathBuf>,
    },
}

#[derive(clap::Subcommand, Debug)]
enum QueryCommand {
    /// Most frequent+recently accessed
    Frecent,

    /// Most recently accessed
    Recent,

    /// Most frequently accessed
    Frequent,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

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

    sqlite_migrate(&sqlite).await?;

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
        Command::Query {
            absolute,
            no_ignore,
            time,
            limit,
            command,
        } => {
            let paths = match command {
                QueryCommand::Frecent => frecent(&sqlite, &repo, time.as_ref(), limit).await?,
                QueryCommand::Recent => recent(&sqlite, &repo, time.as_ref(), limit).await?,
                QueryCommand::Frequent => frequent(&sqlite, &repo, time.as_ref(), limit).await?,
            };

            let mut handles: Vec<JoinHandle<anyhow::Result<Option<Utf8PathBuf>>>> =
                Vec::with_capacity(paths.len());

            for path in paths {
                let current_dir = current_dir.clone();
                let handle = tokio::spawn(async move {
                    let exists_fut = async { Ok(fs::try_exists(&path).await.unwrap_or(false)) };
                    let ignored_fut = async {
                        if no_ignore {
                            Ok(false)
                        } else {
                            is_ignored(&path).await
                        }
                    };
                    let (exists, ignored) = tokio::try_join!(exists_fut, ignored_fut)?;
                    if !exists || ignored {
                        return Ok(None);
                    }
                    let path = if absolute {
                        path
                    } else {
                        diff_utf8_paths(path, &current_dir).unwrap()
                    };
                    Ok(Some(path))
                });
                handles.push(handle);
            }

            #[expect(clippy::collapsible_if)]
            for handle in handles {
                if let Some(path) = handle.await?? {
                    if writeln!(io::stdout(), "{path}").is_err() {
                        break;
                    }
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
    }

    sqlite_finish(&sqlite).await?;

    Ok(())
}

fn parse_timestamp(input: &str) -> anyhow::Result<Timestamp> {
    let zoned = parse_datetime(input)?;
    Ok(zoned.timestamp())
}

async fn sqlite_migrate(sqlite: &SqlitePool) -> anyhow::Result<()> {
    // SQLite's 12-step generalized `alter table` procedure:
    // https://www.sqlite.org/lang_altertable.html#otheralter

    const LATEST_VERSION: u16 = 2;

    loop {
        let mut tx = sqlite.begin().await?;

        let current_version: u16 = sqlx::query_scalar("pragma user_version")
            .fetch_one(&mut *tx)
            .await?;

        match current_version {
            0 => {
                // Initialize database
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
                .execute(&mut *tx)
                .await?;
            }
            1 => {
                // Put `time` before `path` in the unique index
                sqlx::query("alter table empath rename to empath_old;")
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "
                    create table empath (
                        repo text not null,
                        path text not null,
                        time text not null,
                        unique (repo, time, path)
                    ) strict;
                    ",
                )
                .execute(&mut *tx)
                .await?;
                sqlx::query("insert into empath select * from empath_old;")
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("drop table empath_old;")
                    .execute(&mut *tx)
                    .await?;
            }
            LATEST_VERSION => {
                tx.rollback().await?;
                break;
            }
            _ => {
                tx.rollback().await?;
                anyhow::bail!(
                    "Database version {current_version} is newer than supported (max: {LATEST_VERSION})"
                );
            }
        }

        sqlx::query(&format!("pragma user_version = {}", current_version + 1))
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
    }

    Ok(())
}

async fn sqlite_finish(sqlite: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "
        pragma optimize;
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

async fn is_ignored(path: &Utf8Path) -> anyhow::Result<bool> {
    let exit_status = process::Command::new("git")
        .arg("check-ignore")
        .arg("--quiet")
        .arg(path)
        .status()
        .await?;

    match exit_status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        Some(128) => anyhow::bail!("`git check-ignore` encountered a fatal error"),
        code => anyhow::bail!("`git check-ignore` returned unexpected exit code: {code:?}"),
    }
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
async fn frecent(
    sqlite: &SqlitePool,
    repo: &Utf8Path,
    time: Option<&Timestamp>,
    limit: u32,
) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let repo = repo.as_str();
    let time = match time {
        Some(time) => time.to_string(),
        None => Timestamp::now().to_string(),
    };

    let rows = sqlx::query(
        "
        select
            path,
            julianday($2) - julianday(time) as age_days
        from empath
        where repo = $1
          and time <= $2
        ",
    )
    .bind(repo)
    .bind(time)
    .bind(limit)
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
        .take(usize::try_from(limit).expect("Machine has 64-bit pointers"))
        .map(|(path, _)| Utf8PathBuf::from(path))
        .collect();

    Ok(paths)
}

async fn recent(
    sqlite: &SqlitePool,
    repo: &Utf8Path,
    time: Option<&Timestamp>,
    limit: u32,
) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let repo = repo.as_str();
    let time = match time {
        Some(time) => time.to_string(),
        None => Timestamp::now().to_string(),
    };

    let rows: Vec<String> = sqlx::query_scalar(
        "
        select path
        from empath
        where repo = $1
          and time <= $2
        group by path
        order by max(time) desc
        limit $3
        ",
    )
    .bind(repo)
    .bind(time)
    .bind(limit)
    .fetch_all(sqlite)
    .await?;

    let paths = rows
        .into_iter()
        .map(|string| Utf8PathBuf::from(string))
        .collect();

    Ok(paths)
}

async fn frequent(
    sqlite: &SqlitePool,
    repo: &Utf8Path,
    time: Option<&Timestamp>,
    limit: u32,
) -> anyhow::Result<Vec<Utf8PathBuf>> {
    let repo = repo.as_str();
    let time = match time {
        Some(time) => time.to_string(),
        None => Timestamp::now().to_string(),
    };

    let rows: Vec<String> = sqlx::query_scalar(
        "
        select path
        from empath
        where repo = $1
          and time <= $2
        group by path
        order by count(*) desc
        limit $3
        ",
    )
    .bind(repo)
    .bind(time)
    .bind(limit)
    .fetch_all(sqlite)
    .await?;

    let paths = rows
        .into_iter()
        .map(|string| Utf8PathBuf::from(string))
        .collect();

    Ok(paths)
}
