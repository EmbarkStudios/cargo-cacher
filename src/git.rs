use crate::{util, PathBuf};
use anyhow::{Context as _, Result};

pub struct GitPackage {
    /// The tarball of the bare repository
    pub db: bytes::Bytes,
    /// The tarball of the checked out repository, including all submodules
    pub checkout: Option<bytes::Bytes>,
}

const DIR: gix::remote::Direction = gix::remote::Direction::Fetch;
use gix::progress::Discard;

/// Clones the git source and all of its submodules
///
/// The bare git clone acts as the source for `$CARGO_HOME/git/db/*`
/// The checkout and submodules clones act as the source for `$CARGO_HOME/git/checkouts/*`
#[tracing::instrument(level = "debug")]
pub fn clone(src: &crate::cargo::GitSource) -> Result<GitPackage> {
    // Create a temporary directory to fetch the repo into
    let temp_dir = tempfile::tempdir()?;
    // Create another temporary directory where we *may* checkout submodules into
    let submodule_dir = tempfile::tempdir()?;

    let (repo, _out) = {
        let span = tracing::debug_span!("fetch");
        let _fs = span.enter();
        gix::prepare_clone_bare(src.url.as_str(), temp_dir.path())
            .context("failed to prepare clone")?
            .with_remote_name("origin")?
            .configure_remote(|remote| {
                Ok(remote
                    .with_fetch_tags(gix::remote::fetch::Tags::All)
                    .with_refspecs(["+HEAD:refs/remotes/origin/HEAD"], DIR)?)
            })
            .fetch_only(&mut Discard, &Default::default())
            .context("failed to fetch")?
    };

    // Ensure that the repo actually contains the revision we need
    repo.find_object(src.rev.id).with_context(|| {
        format!(
            "'{}' doesn't contain rev '{}'",
            src.url,
            src.rev.id.to_hex()
        )
    })?;

    let fetch_rev = src.rev.id;
    let temp_db_path = util::path(temp_dir.path())?;
    let sub_dir_path = util::path(submodule_dir.path())?;

    let (checkout, db) = rayon::join(
        || -> anyhow::Result<_> {
            let span = tracing::info_span!("cloning submodules", %src.url);
            let _ms = span.enter();

            crate::git::prepare_submodules(
                temp_db_path.to_owned(),
                sub_dir_path.to_owned(),
                fetch_rev,
            )?;

            util::pack_tar(sub_dir_path)
        },
        || -> anyhow::Result<_> { util::pack_tar(temp_db_path) },
    );

    Ok(crate::git::GitPackage {
        db: db?,
        checkout: match checkout {
            Ok(co) => Some(co),
            Err(err) => {
                tracing::error!("failed to checkout: {err:#}");
                None
            }
        },
    })
}

#[tracing::instrument(level = "debug")]
pub(crate) fn checkout(
    src: PathBuf,
    target: PathBuf,
    rev: gix::ObjectId,
) -> Result<gix::Repository> {
    // We require the target directory to be clean
    std::fs::create_dir_all(target.parent().unwrap()).context("failed to create checkout dir")?;
    if target.exists() {
        remove_dir_all::remove_dir_all(&target).context("failed to clean checkout dir")?;
    }

    // NOTE: gix does not support local hardlink clones like git/libgit2 does,
    // and is essentially doing `git clone file://<src> <target>`, which, by
    // default, only gets the history for the default branch, meaning if the revision
    // comes from a non-default branch it won't be available on the checkout
    // clone. So...we cheat and shell out to git, at least for now
    {
        let start = std::time::Instant::now();
        let mut cmd = std::process::Command::new("git");
        cmd.args(["clone", "--local", "--no-checkout"])
            .args([&src, &target])
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());

        let output = cmd.output().context("failed to spawn git")?;
        if !output.status.success() {
            let error = String::from_utf8(output.stderr)
                .unwrap_or_else(|_err| "git error output is non-utf8".to_owned());

            anyhow::bail!("failed to perform local clone:\n{error}");
        }

        tracing::debug!("local clone performed in {}ms", start.elapsed().as_millis());
    }

    let mut repo = gix::open(target).context("failed to open local clone")?;

    modify_config(&mut repo, |config| {
        let mut core = config
            .section_mut("core", None)
            .context("unable to find core section")?;
        core.set(
            "autocrlf"
                .try_into()
                .context("autocrlf is not a valid key")?,
            "false".into(),
        );
        Ok(())
    })
    .context("failed to set autocrlf")?;

    reset(&mut repo, rev)?;

    Ok(repo)
}

use gix::bstr::BString;
use gix::bstr::ByteSlice;

struct Submodule {
    name: BString,
    path: BString,
    url: BString,
    branch: Option<BString>,
    head_id: Option<gix::ObjectId>,
}

impl Submodule {
    #[inline]
    fn path(&self) -> &crate::Path {
        crate::Path::new(self.path.to_str().unwrap())
    }
}

fn read_submodule_config(config: &gix::config::File<'_>) -> Vec<Submodule> {
    let Some(iter) = config.sections_by_name("submodule") else {
        return Vec::new();
    };

    iter.filter_map(|sec| {
        // Each submodule _should_ be a subsection with a name, that
        // (usually, always?) matches the path the submodule will be
        // checked out to
        let name = sec.header().subsection_name()?;

        // Every submodule must have a url
        let url = sec.value("url")?;

        // Every submodule must have a path
        let path = sec.value("path")?;

        // Validate the path is utf-8
        path.to_str().ok()?;

        // Branch is optional
        let branch = sec.value("branch");

        Some(Submodule {
            name: name.into(),
            url: url.into_owned(),
            path: path.into_owned(),
            branch: branch.map(|b| b.into_owned()),
            head_id: None,
        })
    })
    .collect()
}

fn modify_config(
    repo: &mut gix::Repository,
    mutate: impl FnOnce(&mut gix::config::SnapshotMut<'_>) -> Result<()>,
) -> Result<()> {
    let mut config = repo.config_snapshot_mut();

    mutate(&mut config)?;

    {
        use std::io::Write;
        let mut local_config = std::fs::OpenOptions::new()
            .create(false)
            .write(true)
            .append(false)
            .open(
                config
                    .meta()
                    .path
                    .as_deref()
                    .context("local config with path set")?,
            )
            .context("failed to open local config")?;
        local_config.write_all(config.detect_newline_style())?;
        config
            .write_to_filter(&mut local_config, &mut |s: &gix::config::file::Section<
                '_,
            >| {
                s.meta().source == gix::config::Source::Local
            })
            .context("failed to write submodules to config")?;
    }

    config
        .commit()
        .context("failed to commit submodule(s) to config")?;
    Ok(())
}

fn reset(repo: &mut gix::Repository, rev: gix::ObjectId) -> Result<()> {
    let workdir = repo
        .work_dir()
        .context("unable to checkout, repository is bare")?;
    let root_tree = repo
        .find_object(rev)
        .context("failed to find revision")?
        .peel_to_tree()
        .context("unable to peel to tree")?
        .id;

    let index = gix::index::State::from_tree(&root_tree, &repo.objects, Default::default())
        .with_context(|| format!("failed to create index from tree '{root_tree}'"))?;
    let mut index = gix::index::File::from_state(index, repo.index_path());

    let opts = gix::worktree::state::checkout::Options {
        destination_is_initially_empty: false,
        overwrite_existing: true,
        ..Default::default()
    };

    gix::worktree::state::checkout(
        &mut index,
        workdir,
        repo.objects.clone().into_arc()?,
        &Discard,
        &Discard,
        &Default::default(),
        opts,
    )
    .context("failed to checkout")?;

    index
        .write(Default::default())
        .context("failed to write index")?;

    // cargo uses the head oid to check if it needs to update the submodule
    // so force set HEAD to the appropriate commit, since we don't really
    // care about updates for the head we just set it directly rather than
    // via reference
    let head_path = repo.path().join("HEAD");
    std::fs::write(head_path, format!("{}\n", rev.to_hex()))?;

    Ok(())
}

#[tracing::instrument(level = "debug")]
pub(crate) fn prepare_submodules(src: PathBuf, target: PathBuf, rev: gix::ObjectId) -> Result<()> {
    fn update_submodules(repo: &mut gix::Repository, rev: gix::ObjectId) -> Result<()> {
        // We only get here if checkout succeeds, so we're guaranteed to have a working dir
        let work_dir = repo.work_dir().unwrap().to_owned();

        let submodules_config = work_dir.join(".gitmodules");
        if !submodules_config.exists() {
            return Ok(());
        }

        // Open the .gitmodules file, which has the same format as regular git config
        // Note we don't use the more convenient gix::config::File::from_path_no_includes
        // here since it forces a 'static lifetime :(
        let subm_config_buf =
            std::fs::read(&submodules_config).context("failed to read .gitmodules")?;
        let submodules_file = {
            let meta = gix::config::file::Metadata {
                path: Some(submodules_config.clone()),
                source: gix::config::Source::Local,
                level: 0,
                trust: gix::sec::Trust::Full,
            };

            gix::config::File::from_bytes_no_includes(&subm_config_buf, meta, Default::default())
                .context("failed to deserialize .gitmodules")?
        };

        let submodules = {
            let mut submodules = read_submodule_config(&submodules_file);
            if submodules.is_empty() {
                tracing::info!("repo contained a .gitmodules file, but it had no valid submodules");
                return Ok(());
            }

            // This is really all that git2::Submodule::init(false) does, write
            // each submodule url to the git config. Note that we follow cargo here
            // by not updating the submodule if it exists already, but I'm not actually
            // sure if that is correct...
            modify_config(repo, |config| {
                for subm in &submodules {
                    if config
                        .section("submodule", Some(subm.name.as_bstr()))
                        .is_ok()
                    {
                        tracing::debug!("submodule {} already exists in config", subm.name);
                        continue;
                    }

                    let mut sec = config
                        .new_section("submodule", Some(subm.name.clone().into()))
                        .context("failed to add submodule section")?;
                    sec.push("path".try_into()?, Some(subm.path.as_bstr()));
                    sec.push("url".try_into()?, Some(subm.url.as_bstr()));

                    if let Some(branch) = &subm.branch {
                        sec.push("branch".try_into()?, Some(branch.as_bstr()));
                    }
                }

                Ok(())
            })
            .context("failed to add submodules")?;

            // Now, find the actual head id of the module so that we can determine
            // what tree to set the submodule checkout to
            let tree = repo
                .find_object(rev)
                .context("failed to find rev")?
                .peel_to_tree()
                .context("failed to peel rev to tree")?;
            for subm in &mut submodules {
                let span = tracing::info_span!("locating submodule head", name = %subm.name, path = %subm.path);
                let _ms = span.enter();

                let path = subm.path();

                let entry = match tree.lookup_entry(path) {
                    Ok(Some(e)) => e,
                    Ok(None) => {
                        tracing::warn!("unable to locate submodule path in tree");
                        continue;
                    }
                    Err(err) => {
                        tracing::warn!(err = %err, "failed to lookup entry for submodule");
                        continue;
                    }
                };

                if !entry.mode().is_commit() {
                    tracing::warn!(kind = ?entry.mode(), "path is not a submodule");
                    continue;
                }

                subm.head_id = Some(entry.id().detach());
            }

            submodules
        };

        // The initial config editing is the only thing we really need to do
        // serially, the rest of work of actually cloning/updating submodules
        // can be done in parallel since they are each distinct entities
        use rayon::prelude::*;
        let mut res = Vec::new();
        submodules
            .into_par_iter()
            .map(|subm| update_submodule(&work_dir, subm).context("failed to update submodule"))
            .collect_into_vec(&mut res);

        res.into_iter().collect::<Result<()>>()?;
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    fn update_submodule(parent: &std::path::Path, subm: Submodule) -> Result<()> {
        // A submodule which is listed in .gitmodules but not actually
        // checked out will not have a head id, so we should ignore it.
        let Some(head) = subm.head_id else {
            tracing::debug!("skipping submodule '{}' without HEAD", subm.name);
            return Ok(());
        };

        let submodule_path = parent.join(subm.path());

        let open_or_init_repo = || -> Result<_> {
            let open_with_complete_config =
                gix::open::Options::default().permissions(gix::open::Permissions {
                    config: gix::open::permissions::Config {
                        // Be sure to get all configuration, some of which is only known by the git binary.
                        // That way we are sure to see all the systems credential helpers
                        git_binary: true,
                        ..Default::default()
                    },
                    ..Default::default()
                });

            let repo = if let Ok(repo) = gix::open_opts(&submodule_path, open_with_complete_config)
            {
                repo
            } else {
                // Blow away the submodules directory in case it exists but is
                // corrupted somehow which cause gix to fail to open it, if there
                // is an error the init or subsequent clone _might_ fail but also
                // might not!
                let _ = remove_dir_all::remove_dir_all(&submodule_path);
                gix::init(&submodule_path).context("failed to init submodule")?
            };

            Ok(repo)
        };

        // If the submodule hasn't been checked out yet, we need to clone it. If
        // it has been checked out and the head is the same as the submodule's
        // head, then we can skip an update and keep recursing.
        let mut repo = open_or_init_repo()?;
        if repo
            .head_commit()
            .ok()
            .map_or(false, |commit| commit.id == head)
        {
            return update_submodules(&mut repo, head);
        }

        // We perform fetches and update the reflog, and gix forces us to set a
        // committer for these, this is particularly true in CI environments
        // that likely don't have a global committer set
        modify_config(&mut repo, |config| {
            let mut core = config
                .section_mut("core", None)
                .context("unable to find core section")?;
            core.set(
                "autocrlf"
                    .try_into()
                    .context("autocrlf is not a valid key")?,
                "false".into(),
            );

            config
                .set_raw_value(&"committer.name", "cargo-fetcher")
                .context("failed to set committer.name")?;
            // Note we _have_ to set the email as well, but luckily gix does not actually
            // validate if it's a proper email or not :)
            config
                .set_raw_value(&"committer.email", "")
                .context("failed to set committer.email")?;
            Ok(())
        })?;

        let mut remote = repo
            .remote_at(subm.url.as_bstr())
            .context("invalid submodule url")?;

        remote
            .replace_refspecs(
                [
                    "+refs/heads/*:refs/remotes/origin/*",
                    "+HEAD:refs/remotes/origin/HEAD",
                ],
                DIR,
            )
            .expect("valid statically known refspec");
        remote = remote.with_fetch_tags(gix::remote::fetch::Tags::All);

        // Perform the actual fetch
        let outcome = remote
            .connect(DIR)
            .context("failed to connect to remote")?
            .prepare_fetch(&mut Discard, Default::default())
            .context("failed to prepare fetch")?
            .receive(&mut Discard, &Default::default())
            .context("failed to fetch submodule")?;

        tame_index::utils::git::write_fetch_head(&repo, &outcome, &remote)
            .context("failed to write FETCH_HEAD")?;

        reset(&mut repo, head)?;
        update_submodules(&mut repo, head)
    }

    let mut repo = checkout(src, target, rev)?;
    update_submodules(&mut repo, rev)
}
