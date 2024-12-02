use crate::{util, Krate, Path, PathBuf, Registry, RegistryProtocol, Source};
use anyhow::Context as _;
use std::io::Write;
use tracing::{debug, error, info, warn};

pub const INDEX_DIR: &str = "registry/index";
pub const CACHE_DIR: &str = "registry/cache";
pub const SRC_DIR: &str = "registry/src";
pub const GIT_DB_DIR: &str = "git/db";
pub const GIT_CO_DIR: &str = "git/checkouts";

pub async fn registry_indices(
    root_dir: PathBuf,
    backend: crate::Storage,
    registries: Vec<std::sync::Arc<Registry>>,
) {
    #[allow(unsafe_code)]
    // SAFETY: we don't forget the future :p
    unsafe {
        async_scoped::TokioScope::scope_and_collect(|s| {
            for registry in registries {
                s.spawn(async {
                    if let Err(err) = registry_index(&root_dir, backend.clone(), registry).await {
                        error!("{err:#}");
                    }
                });
            }
        })
        .await;
    }
}

/// Just skip the index if the git directory already exists, as a patch on
/// top of an existing repo via git fetch is presumably faster
async fn maybe_fetch_index(index_path: &Path, registry: &Registry) -> anyhow::Result<()> {
    anyhow::ensure!(gix::open(index_path).is_ok(), "failed to open index repo");
    info!("registry index already exists, fetching  instead");

    let index_path = index_path.to_owned();
    let index_url = registry.index.to_string();
    tokio::task::spawn_blocking(move || {
        let last_updated = index_path.join(".last-updated");

        let gi = tame_index::GitIndex::new(tame_index::IndexLocation {
            url: tame_index::IndexUrl::NonCratesIo(index_url.as_str().into()),
            root: tame_index::IndexPath::Exact(index_path),
        })?;

        {
            let span = tracing::debug_span!("fetch", index = index_url.clone());
            let _sf = span.enter();
            let unlocked = &tame_index::index::FileLock::unlocked();
            let mut rgi = tame_index::index::RemoteGitIndex::new(gi, unlocked)?;
            rgi.fetch(unlocked)?;
        }

        std::fs::File::create(last_updated).context("failed to crate .last-updated")?;
        Ok(())
    })
    .await
    .unwrap()
}

#[tracing::instrument(skip(backend))]
pub async fn registry_index(
    root_dir: &Path,
    backend: crate::Storage,
    registry: std::sync::Arc<Registry>,
) -> anyhow::Result<()> {
    let ident = registry.short_name().to_owned();

    let index_path = {
        let mut ip = root_dir.join(INDEX_DIR);
        ip.push(&ident);
        ip
    };
    std::fs::create_dir_all(&index_path).context("failed to create index dir")?;

    if registry.protocol == RegistryProtocol::Git {
        match maybe_fetch_index(&index_path, &registry).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                debug!(error = %err, "unable to fetch index");
                // Attempt to nuke the directory in case there are actually files
                // there, to give the best chance for the tarball unpack to work
                let _ = remove_dir_all::remove_dir_all(&index_path);
            }
        }
    }

    let krate = Krate {
        name: ident.clone(),
        version: "2.0.0".to_owned(),
        source: Source::Git(crate::cargo::GitSource {
            url: registry.index.clone(),
            ident,
            rev: crate::cargo::GitRev::parse("feedc0de00000000000000000000000000000000").unwrap(),
            follow: None,
        }),
    };

    let index_data = backend.fetch(krate.cloud_id(false)).await?;

    if let Err(e) = util::unpack_tar(index_data, util::Encoding::Zstd, &index_path) {
        error!(err = ?e, "failed to unpack crates.io-index");
    }

    Ok(())
}

#[tracing::instrument(level = "debug", skip_all, fields(name = krate.name, version = krate.version, rev = %rev.id))]
fn sync_git(
    db_dir: &Path,
    co_dir: &Path,
    krate: &Krate,
    pkg: crate::git::GitPackage,
    rev: &crate::cargo::GitRev,
) -> anyhow::Result<()> {
    let db_path = db_dir.join(krate.local_id().to_string());

    // Always just blow away and do a sync from the remote tar
    if db_path.exists() {
        remove_dir_all::remove_dir_all(&db_path).context("failed to remove existing DB path")?;
    }

    let crate::git::GitPackage { db, checkout } = pkg;

    let unpack_path = db_path.clone();
    let compressed = db.len();
    let uncompressed = util::unpack_tar(db, util::Encoding::Zstd, &unpack_path)?;
    debug!(
        compressed = compressed,
        uncompressed = uncompressed,
        "unpacked db dir"
    );

    let co_path = co_dir.join(format!("{}/{}", krate.local_id(), rev.short()));

    // If we get here, it means there wasn't a .cargo-ok in the dir, even if the
    // rest of it is checked out and ready, so blow it away just in case as we are
    // doing a clone/checkout from a local bare repository rather than a remote one
    if co_path.exists() {
        debug!("removing checkout dir {co_path} for {krate}");
        remove_dir_all::remove_dir_all(&co_path)
            .with_context(|| format!("unable to remove {co_path}"))?;
    }

    // If we have a checkout tarball, use that, as it will include submodules,
    // otherwise do a checkout
    match checkout {
        Some(checkout) => {
            let compressed = checkout.len();
            let uncompressed = util::unpack_tar(checkout, util::Encoding::Zstd, &co_path)?;
            debug!(
                compressed = compressed,
                uncompressed = uncompressed,
                "unpacked checkout dir"
            );
        }
        None => {
            // Do a checkout of the bare clone if we didn't/couldn't unpack the
            // checkout tarball
            crate::git::checkout(db_path, co_path.clone(), rev.id)?;
        }
    }

    let ok = co_path.join(".cargo-ok");
    std::fs::File::create(&ok).with_context(|| ok.to_string())?;

    Ok(())
}

#[tracing::instrument(level = "debug", skip_all, fields(name = krate.name, version = krate.version))]
fn sync_package(
    cache_dir: &Path,
    src_dir: &Path,
    krate: &Krate,
    data: bytes::Bytes,
    chksum: &str,
) -> anyhow::Result<()> {
    util::validate_checksum(&data, chksum)?;

    let packed_krate_path = cache_dir.join(format!("{}", krate.local_id()));

    let pack_data = data.clone();
    let packed_path = packed_krate_path;

    let (pack_write, unpack) = rayon::join(
        // Spawn a worker thread to write the original pack file to disk as we don't
        // particularly care when it is done
        || -> anyhow::Result<()> {
            let s = tracing::debug_span!("pack_write");
            let _ = s.enter();
            let mut f = std::fs::File::create(&packed_path)?;

            let _ = f.set_len(pack_data.len() as u64);
            f.write_all(&pack_data)?;
            f.sync_all()?;

            debug!(bytes = pack_data.len(), "wrote pack file to disk");
            Ok(())
        },
        || -> anyhow::Result<()> {
            let mut src_path = src_dir.join(format!("{}", krate.local_id()));

            // Remove the .crate extension
            src_path.set_extension("");
            let ok = src_path.join(".cargo-ok");

            if !ok.exists() {
                if src_path.exists() {
                    debug!("cleaning src/");
                    if let Err(e) = remove_dir_all::remove_dir_all(&src_path) {
                        error!(err = ?e, "failed to remove src/");
                        return Err(e.into());
                    }
                }

                // Crate tarballs already include the top level directory internally,
                // so unpack in the top-level source directory
                if let Err(e) =
                    util::unpack_tar(data, util::Encoding::Gzip, src_path.parent().unwrap())
                {
                    error!(err = ?e, "failed to unpack to src/");
                    return Err(e);
                }

                // Create the .cargo-ok file so that cargo doesn't suspect a thing
                if let Err(e) = util::write_ok(&ok) {
                    // If this happens, cargo will just resync and recheckout the repo most likely
                    warn!(err = ?e, "failed to write .cargo-ok");
                }
            }

            Ok(())
        },
    );

    if let Err(err) = pack_write {
        error!(?err, path = ?packed_path, "failed to write tarball to disk");
    }

    if let Err(err) = unpack {
        error!(?err, "failed to unpack tarball to disk");
    }

    Ok(())
}

fn get_missing_git_sources<'krate>(
    ctx: &'krate crate::Ctx,
    git_co_dir: &Path,
    to_sync: &mut Vec<&'krate Krate>,
) {
    for (rev, ident, krate) in ctx.krates.iter().filter_map(|k| match &k.source {
        Source::Git(gs) => Some((gs.rev.short(), &gs.ident, k)),
        Source::Registry { .. } => None,
    }) {
        let path = git_co_dir.join(format!("{ident}/{rev}/.cargo-ok"));

        if !path.exists() {
            to_sync.push(krate);
        }
    }
}

fn get_missing_registry_sources<'krate>(
    ctx: &'krate crate::Ctx,
    registry: &Registry,
    cache_dir: &Path,
    to_sync: &mut Vec<&'krate Krate>,
) -> anyhow::Result<()> {
    let cache_iter = std::fs::read_dir(cache_dir)?;

    let mut cached_crates: Vec<String> = cache_iter
        .filter_map(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_name().to_str().map(|s| s.to_owned()))
        })
        .collect();

    cached_crates.sort();

    let mut krate_name = String::with_capacity(128);

    for krate in ctx.krates.iter().filter(|k| *k == registry) {
        use std::fmt::Write;
        write!(&mut krate_name, "{}", krate.local_id()).unwrap();

        if cached_crates.binary_search(&krate_name).is_err() {
            to_sync.push(krate);
        }

        krate_name.clear();
    }

    Ok(())
}

#[derive(Debug)]
pub struct Summary {
    pub total_bytes: usize,
    pub bad: u32,
    pub good: u32,
}

pub async fn crates(ctx: &crate::Ctx) -> anyhow::Result<Summary> {
    info!("synchronizing {} crates...", ctx.krates.len());

    let root_dir = &ctx.root_dir;
    let git_db_dir = root_dir.join(GIT_DB_DIR);
    let git_co_dir = root_dir.join(GIT_CO_DIR);

    std::fs::create_dir_all(&git_db_dir).context("failed to create git/db/")?;
    std::fs::create_dir_all(&git_co_dir).context("failed to create git/checkouts/")?;

    info!("checking local cache for missing crates...");
    let mut git_sync = Vec::new();
    get_missing_git_sources(ctx, &git_co_dir, &mut git_sync);

    let mut registry_sync = Vec::new();
    for registry in &ctx.registries {
        let (cache_dir, src_dir) = registry.sync_dirs(root_dir);
        std::fs::create_dir_all(&cache_dir).context("failed to create registry/cache")?;
        std::fs::create_dir_all(src_dir).context("failed to create registry/src")?;

        get_missing_registry_sources(ctx, registry, &cache_dir, &mut registry_sync)?;
    }

    // Remove duplicates, eg. when 2 crates are sourced from the same git repository
    git_sync.sort();
    git_sync.dedup();

    // probably shouldn't be needed, but why not
    registry_sync.sort();
    registry_sync.dedup();

    if git_sync.is_empty() && registry_sync.is_empty() {
        info!("all crates already available on local disk");
        return Ok(Summary {
            total_bytes: 0,
            good: 0,
            bad: 0,
        });
    }

    info!(
        "synchronizing {} missing crates...",
        git_sync.len() + registry_sync.len()
    );

    enum Pkg {
        Registry(bytes::Bytes),
        Git(crate::git::GitPackage),
    }

    // Kick off all the remote I/O first
    let mut tasks = tokio::task::JoinSet::new();
    for krate in git_sync
        .into_iter()
        .chain(registry_sync.into_iter())
        .cloned()
    {
        let backend = ctx.backend.clone();

        tasks.spawn(async move {
            let span = tracing::info_span!("sync", %krate);
            let _ss = span.enter();

            match &krate.source {
                Source::Registry(_rs) => {
                    let fetch_result = {
                        let span = tracing::debug_span!("download");
                        let _ds = span.enter();
                        backend.fetch(krate.cloud_id(false)).await
                    };

                    match fetch_result {
                        Ok(krate_data) => {
                            Some((krate, Pkg::Registry(krate_data)))
                        }
                        Err(err) => {
                            error!(err = ?err, krate = %krate, cloud = %krate.cloud_id(false), "failed to download");
                            None
                        }
                    }
                }
                Source::Git(_gs) => {
                    let kd = krate.clone();
                    let kdb = backend.clone();
                    let co = krate.clone();
                    let (krate_data, checkout) = tokio::join!(
                        tokio::task::spawn(async move {
                            let span = tracing::debug_span!("download");
                            let _ds = span.enter();
                            kdb.fetch(kd.cloud_id(false)).await
                        }),
                        tokio::task::spawn(async move {
                            let span = tracing::debug_span!("download_checkout");
                            let _ds = span.enter();
                            backend.fetch(co.cloud_id(true)).await.ok()
                        }),
                    );

                    let krate_data = match krate_data.unwrap() {
                        Ok(krate_data) => {
                            krate_data
                        }
                        Err(err) => {
                            error!(err = ?err, krate = %krate, cloud = %krate.cloud_id(false), "failed to download");
                            return None;
                        }
                    };

                    let git_pkg = crate::git::GitPackage {
                        db: krate_data,
                        checkout: checkout.unwrap(),
                    };

                    Some((krate, Pkg::Git(git_pkg)))
                }
            }
        });
    }

    let summary = std::sync::Arc::new(std::sync::Mutex::new(Summary {
        total_bytes: 0,
        bad: 0,
        good: 0,
    }));

    let (tx, rx) = crossbeam_channel::unbounded::<(Krate, Pkg)>();
    let fs_thread = {
        let summary = summary.clone();
        let root_dir = root_dir.clone();

        std::thread::spawn(move || {
            let db_dir = &git_db_dir;
            let co_dir = &git_co_dir;
            let root_dir = &root_dir;
            let summary = &summary;
            rayon::scope(|s| {
                while let Ok((krate, pkg)) = rx.recv() {
                    s.spawn(move |_s| {
                        let synced = match (&krate.source, pkg) {
                            (Source::Registry(rs), Pkg::Registry(krate_data)) => {
                                let len = krate_data.len();
                                let (cache_dir, src_dir) = rs.registry.sync_dirs(root_dir);
                                if let Err(err) = sync_package(
                                    &cache_dir, &src_dir, &krate, krate_data, &rs.chksum,
                                ) {
                                    error!(krate = %krate, "failed to splat package: {err:#}");
                                    None
                                } else {
                                    Some(len)
                                }
                            }
                            (Source::Git(gs), Pkg::Git(pkg)) => {
                                let mut len = pkg.db.len();

                                if let Some(co) = &pkg.checkout {
                                    len += co.len();
                                }

                                match sync_git(db_dir, co_dir, &krate, pkg, &gs.rev) {
                                    Ok(_) => Some(len),
                                    Err(err) => {
                                        error!(krate = %krate, "failed to splat git repo: {err:#}");
                                        None
                                    }
                                }
                            }
                            _ => unreachable!(),
                        };

                        let mut sum = summary.lock().unwrap();
                        if let Some(synced) = synced {
                            sum.good += 1;
                            sum.total_bytes += synced;
                        } else {
                            sum.bad += 1;
                        }
                    });
                }
            });
        })
    };

    // As each remote I/O op completes, pass it off to the thread pool to do
    // the more CPU intensive work of decompression, etc
    while let Some(res) = tasks.join_next().await {
        let Ok(res) = res else {
            continue;
        };

        if let Some(pkg) = res {
            let _ = tx.send(pkg);
        } else {
            summary.lock().unwrap().bad += 1;
        }
    }

    // Drop the sender otherwise we'll deadlock
    drop(tx);

    fs_thread.join().expect("failed to join thread");

    Ok(std::sync::Arc::into_inner(summary)
        .unwrap()
        .into_inner()
        .unwrap())
}
