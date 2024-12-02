use anyhow::Error;
pub use camino::{Utf8Path as Path, Utf8PathBuf as PathBuf};
use std::{fmt, sync::Arc};
pub use url::Url;

pub mod backends;
pub mod cargo;
mod fetch;
pub(crate) mod git;
pub mod mirror;
pub mod sync;
pub mod util;

pub type HttpClient = reqwest::Client;

pub use cargo::{read_cargo_config, GitSource, Registry, RegistryProtocol, RegistrySource, Source};

#[derive(Eq, Clone, Debug)]
pub struct Krate {
    pub name: String,
    pub version: String, // We just treat versions as opaque strings
    pub source: Source,
}

impl Ord for Krate {
    #[inline]
    fn cmp(&self, b: &Self) -> std::cmp::Ordering {
        self.source.cmp(&b.source)
    }
}

impl PartialOrd for Krate {
    #[inline]
    fn partial_cmp(&self, b: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(b))
    }
}

impl PartialEq for Krate {
    fn eq(&self, b: &Self) -> bool {
        self.source.eq(&b.source)
    }
}

impl PartialEq<Registry> for Krate {
    fn eq(&self, b: &Registry) -> bool {
        match &self.source {
            Source::Git(..) => false,
            Source::Registry(rs) => b.eq(&rs.registry),
        }
    }
}

impl Krate {
    #[inline]
    pub fn cloud_id(&self, is_checkout: bool) -> CloudId<'_> {
        CloudId {
            inner: self,
            is_checkout,
        }
    }

    #[inline]
    pub fn local_id(&self) -> LocalId<'_> {
        LocalId { inner: self }
    }
}

impl fmt::Display for Krate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let typ = match &self.source {
            Source::Git { .. } => "git",
            Source::Registry { .. } => "registry",
        };

        write!(f, "{}-{}({typ})", self.name, self.version)
    }
}

pub struct LocalId<'a> {
    inner: &'a Krate,
}

impl fmt::Display for LocalId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner.source {
            Source::Git(gs) => f.write_str(&gs.ident),
            Source::Registry(..) => {
                write!(f, "{}-{}.crate", self.inner.name, self.inner.version)
            }
        }
    }
}

pub struct CloudId<'a> {
    inner: &'a Krate,
    is_checkout: bool,
}

impl fmt::Display for CloudId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner.source {
            Source::Git(gs) => write!(
                f,
                "{}-{}{}",
                gs.ident,
                gs.rev.short(),
                if self.is_checkout { "-checkout" } else { "" }
            ),
            Source::Registry(rs) => f.write_str(&rs.chksum),
        }
    }
}

#[allow(dead_code)]
pub struct GcsLocation<'a> {
    pub bucket: &'a str,
    pub prefix: &'a str,
}

#[allow(dead_code)]
pub struct S3Location<'a> {
    pub bucket: &'a str,
    pub region: &'a str,
    pub host: &'a str,
    pub prefix: &'a str,
}

pub struct FilesystemLocation<'a> {
    pub path: &'a Path,
}

pub struct BlobLocation<'a> {
    pub prefix: &'a str,
    pub container: &'a str,
}

pub enum CloudLocation<'a> {
    Gcs(GcsLocation<'a>),
    S3(S3Location<'a>),
    Fs(FilesystemLocation<'a>),
    Blob(BlobLocation<'a>),
}

pub type Storage = Arc<dyn Backend + Sync + Send>;

pub struct Ctx {
    pub client: HttpClient,
    pub backend: Storage,
    pub krates: Vec<Krate>,
    pub registries: Vec<Arc<Registry>>,
    pub root_dir: PathBuf,
}

impl Ctx {
    pub fn new(
        root_dir: Option<PathBuf>,
        backend: Storage,
        krates: Vec<Krate>,
        registries: Vec<Arc<Registry>>,
    ) -> Result<Self, Error> {
        Ok(Self {
            client: HttpClient::builder().build()?,
            backend,
            krates,
            registries,
            root_dir: root_dir.unwrap_or_else(|| PathBuf::from(".")),
        })
    }

    /// Create the registry and git directories as they are the root of multiple other ones
    pub fn prep_sync_dirs(&self) -> Result<(), Error> {
        std::fs::create_dir_all(self.root_dir.join("registry"))?;
        std::fs::create_dir_all(self.root_dir.join("git"))?;

        Ok(())
    }

    pub fn registry_sets(&self) -> Vec<mirror::RegistrySet> {
        self.registries
            .iter()
            .map(|registry| {
                // Gather the names of all of the crates sourced in the registry so we
                // can add .cache entries
                let krates = self
                    .krates
                    .iter()
                    .filter_map(|krate| {
                        if krate == registry.as_ref() {
                            Some(krate.name.clone())
                        } else {
                            None
                        }
                    })
                    .collect();

                mirror::RegistrySet {
                    registry: registry.clone(),
                    krates,
                }
            })
            .collect()
    }
}

impl fmt::Debug for Ctx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "krates: {}", self.krates.len())
    }
}

pub type Timestamp = time::OffsetDateTime;

#[async_trait::async_trait]
pub trait Backend: fmt::Debug {
    async fn fetch(&self, id: CloudId<'_>) -> Result<bytes::Bytes, Error>;
    async fn upload(&self, source: bytes::Bytes, id: CloudId<'_>) -> Result<usize, Error>;
    async fn list(&self) -> Result<Vec<String>, Error>;
    async fn updated(&self, id: CloudId<'_>) -> Result<Option<Timestamp>, Error>;
}
