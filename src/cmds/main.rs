// crate-specific exceptions:
#![allow(clippy::exit)]

extern crate cargo_fetcher as cf;

use anyhow::Context as _;
use cf::PathBuf;
use std::{sync::Arc, time::Duration};
use tracing_subscriber::filter::LevelFilter;
use url::Url;

mod mirror;
mod sync;

#[derive(Clone)]
struct Dur(Duration);

impl std::str::FromStr for Dur {
    type Err = clap::Error;

    fn from_str(src: &str) -> Result<Self, Self::Err> {
        let suffix_pos = src.find(char::is_alphabetic).unwrap_or(src.len());

        let num: u64 = src[..suffix_pos]
            .parse()
            .map_err(|err| clap::Error::raw(clap::error::ErrorKind::ValueValidation, err))?;
        let suffix = if suffix_pos == src.len() {
            "s"
        } else {
            &src[suffix_pos..]
        };

        let duration = match suffix {
            "s" | "S" => Duration::from_secs(num),
            "m" | "M" => Duration::from_secs(num * 60),
            "h" | "H" => Duration::from_secs(num * 60 * 60),
            "d" | "D" => Duration::from_secs(num * 60 * 60 * 24),
            s => {
                return Err(clap::Error::raw(
                    clap::error::ErrorKind::ValueValidation,
                    format!("unknown duration suffix '{s}'"),
                ))
            }
        };

        Ok(Dur(duration))
    }
}

#[derive(clap::Subcommand)]
enum Command {
    /// Uploads any crates in the lockfile that aren't already present
    /// in the cloud storage location
    #[clap(name = "mirror")]
    Mirror(mirror::Args),
    /// Downloads missing crates to the local cargo locations and unpacks
    /// them
    #[clap(name = "sync")]
    Sync(sync::Args),
}

#[derive(clap::Parser)]
#[clap(
    author,
    version,
    about,
    long_about = "cargo plugin to quickly fetch crate sources from cloud or local storage"
)]
struct Opts {
    /// Path to a service account credentials file used to obtain
    /// oauth2 tokens. By default uses `GOOGLE_APPLICATION_CREDENTIALS`
    /// environment variable.
    #[clap(short, long, env = "GOOGLE_APPLICATION_CREDENTIALS")]
    credentials: Option<PathBuf>,
    /// A url to a cloud storage bucket and prefix path at which to store
    /// or retrieve archives
    #[clap(short, long)]
    url: Url,
    /// Path to the lockfile used for determining what crates to operate on
    #[clap(short, long, default_value = "Cargo.lock")]
    lock_files: Vec<PathBuf>,
    #[clap(
        short = 'L',
        long,
        default_value = "info",
        long_help = "The log level for messages, only log messages at or above the level will be emitted.

Possible values:
* off
* error
* warn
* info (default)
* debug
* trace"
    )]
    log_level: LevelFilter,
    /// Output log messages as json
    #[clap(long)]
    json: bool,
    /// A snapshot of the registry index is also included when mirroring or syncing
    #[clap(short, long)]
    include_index: bool,
    #[clap(
        short,
        env = "CARGO_FETCHER_TIMEOUT",
        default_value = "30s",
        long_help = "The maximum duration of a single crate request

Times may be specified with no suffix (default seconds), or one of:
* (s)econds
* (m)inutes
* (h)ours
* (d)ays

"
    )]
    timeout: Dur,
    #[clap(subcommand)]
    cmd: Command,
}

async fn init_backend(
    loc: cf::CloudLocation<'_>,
    _credentials: Option<PathBuf>,
    _timeout: Duration,
) -> anyhow::Result<Arc<dyn cf::Backend + Sync + Send>> {
    match loc {
        #[cfg(feature = "gcs")]
        cf::CloudLocation::Gcs(gcs) => {
            let cred_path = _credentials.context("GCS credentials not specified")?;

            let gcs = cf::backends::gcs::GcsBackend::new(gcs, &cred_path, _timeout).await?;
            Ok(Arc::new(gcs))
        }
        #[cfg(not(feature = "gcs"))]
        cf::CloudLocation::Gcs(_) => anyhow::bail!("GCS backend not enabled"),
        #[cfg(feature = "s3")]
        cf::CloudLocation::S3(loc) => {
            // Special case local testing
            let make_bucket = loc.bucket == "testing" && loc.host.contains("localhost");

            let s3 = cf::backends::s3::S3Backend::new(loc, _timeout).await?;

            if make_bucket {
                s3.make_bucket()
                    .await
                    .context("failed to create test bucket")?;
            }

            Ok(Arc::new(s3))
        }
        #[cfg(not(feature = "s3"))]
        cf::CloudLocation::S3(_) => anyhow::bail!("S3 backend not enabled"),
        cf::CloudLocation::Fs(loc) => Ok(Arc::new(cf::backends::fs::FsBackend::new(loc)?)),
        #[cfg(feature = "blob")]
        cf::CloudLocation::Blob(loc) => Ok(Arc::new(cf::backends::blob::BlobBackend::new(
            loc, _timeout,
        )?)),
        #[cfg(not(feature = "blob"))]
        cf::CloudLocation::Blob(_) => anyhow::bail!("blob backend not enabled"),
    }
}

async fn real_main() -> anyhow::Result<()> {
    use clap::Parser;
    let args = Opts::parse_from({
        std::env::args().enumerate().filter_map(|(i, a)| {
            if i == 1 && a == "fetcher" {
                None
            } else {
                Some(a)
            }
        })
    });

    let mut env_filter = tracing_subscriber::EnvFilter::from_default_env();

    // If a user specifies a log level, we assume it only pertains to cargo_fetcher,
    // if they want to trace other crates they can use the RUST_LOG env approach
    env_filter = env_filter.add_directive(format!("cargo_fetcher={}", args.log_level).parse()?);

    let subscriber = tracing_subscriber::FmtSubscriber::builder().with_env_filter(env_filter);

    if args.json {
        tracing::subscriber::set_global_default(subscriber.json().finish())
            .context("failed to set default subscriber")?;
    } else {
        tracing::subscriber::set_global_default(subscriber.finish())
            .context("failed to set default subscriber")?;
    };

    let cloud_location = cf::util::CloudLocationUrl::from_url(args.url.clone())?;
    let location = cf::util::parse_cloud_location(&cloud_location)?;
    let backend = init_backend(location, args.credentials, args.timeout.0).await?;

    // Since we can take multiple lock files unlike...every? other cargo command,
    // we'll just decide that the first one is the most important and where config
    // data is pulled from
    let lock_files = args.lock_files;
    anyhow::ensure!(
        !lock_files.is_empty(),
        "must provide at least one Cargo.lock"
    );

    let lock_file = &lock_files[0];

    // Note that unlike cargo (since we require a Cargo.lock), we don't use the
    // current directory as the root when resolving cargo configurations, but
    // rather the directory in which the lockfile is located
    let root_dir = if lock_file.is_relative() {
        let root_dir = std::env::current_dir().context("unable to acquire current directory")?;
        let mut root_dir = cf::util::path(&root_dir)?.to_owned();
        root_dir.push(lock_file);
        root_dir.pop();
        root_dir
    } else {
        let mut root_dir = lock_file.clone();
        root_dir.pop();
        root_dir
    };

    let cargo_root = cf::cargo::determine_cargo_root(Some(&root_dir))
        .context("failed to determine $CARGO_HOME")?;

    let registries = cf::read_cargo_config(cargo_root.clone(), root_dir)?;

    let (krates, registries) = cf::cargo::read_lock_files(lock_files, registries)
        .context("failed to get crates from lock file")?;

    match args.cmd {
        Command::Mirror(margs) => {
            let ctx = cf::Ctx::new(None, backend, krates, registries)
                .context("failed to create context")?;
            mirror::cmd(ctx, args.include_index, margs).await
        }
        Command::Sync(sargs) => {
            let ctx = cf::Ctx::new(Some(cargo_root), backend, krates, registries)
                .context("failed to create context")?;
            sync::cmd(ctx, args.include_index, sargs).await
        }
    }
}

#[tokio::main]
async fn main() {
    match real_main().await {
        Ok(_) => {}
        Err(e) => {
            tracing::error!("{:#}", e);
            std::process::exit(1);
        }
    }
}
