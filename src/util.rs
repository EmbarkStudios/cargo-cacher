use anyhow::{anyhow, bail, Context, Error};
#[allow(deprecated)]
use std::{
    fmt,
    hash::{Hash, Hasher, SipHasher},
    path::{Path, PathBuf},
};
use tracing::debug;
use url::Url;

fn to_hex(num: u64) -> String {
    const CHARS: &[u8] = b"0123456789abcdef";

    let bytes = &[
        num as u8,
        (num >> 8) as u8,
        (num >> 16) as u8,
        (num >> 24) as u8,
        (num >> 32) as u8,
        (num >> 40) as u8,
        (num >> 48) as u8,
        (num >> 56) as u8,
    ];

    let mut output = vec![0u8; 16];

    let mut ind = 0;

    for &byte in bytes {
        output[ind] = CHARS[(byte >> 4) as usize];
        output[ind + 1] = CHARS[(byte & 0xf) as usize];

        ind += 2;
    }

    String::from_utf8(output).expect("valid utf-8 hex string")
}

fn hash_u64<H: Hash>(hashable: H) -> u64 {
    #[allow(deprecated)]
    let mut hasher = SipHasher::new_with_keys(0, 0);
    hashable.hash(&mut hasher);
    hasher.finish()
}

pub fn short_hash<H: Hash>(hashable: &H) -> String {
    to_hex(hash_u64(hashable))
}

#[derive(Clone)]
pub struct Canonicalized(Url);

impl Canonicalized {
    pub(crate) fn ident(&self) -> String {
        // This is the same identity function used by cargo
        let ident = self
            .0
            .path_segments()
            .and_then(|mut s| s.next_back())
            .unwrap_or("");

        let ident = if ident.is_empty() { "_empty" } else { ident };

        format!("{}-{}", ident, short_hash(&self.0))
    }
}

impl fmt::Display for Canonicalized {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_ref())
    }
}

impl AsRef<Url> for Canonicalized {
    fn as_ref(&self) -> &Url {
        &self.0
    }
}

#[allow(clippy::from_over_into)]
impl Into<Url> for Canonicalized {
    fn into(self) -> Url {
        self.0
    }
}

impl std::convert::TryFrom<&Url> for Canonicalized {
    type Error = Error;

    fn try_from(url: &Url) -> Result<Self, Self::Error> {
        // This is the same canonicalization that cargo does, except the URLs
        // they use don't have any query params or fragments, even though
        // they do occur in Cargo.lock files

        // cannot-be-a-base-urls (e.g., `github.com:rust-lang-nursery/rustfmt.git`)
        // are not supported.
        if url.cannot_be_a_base() {
            bail!(
                "invalid url `{}`: cannot-be-a-base-URLs are not supported",
                url
            )
        }

        let mut url_str = String::new();

        let is_github = url.host_str() == Some("github.com");

        // HACK: for GitHub URLs specifically, just lower-case
        // everything. GitHub treats both the same, but they hash
        // differently, and we're gonna be hashing them. This wants a more
        // general solution, and also we're almost certainly not using the
        // same case conversion rules that GitHub does. (See issue #84.)
        if is_github {
            url_str.push_str("https://");
        } else {
            url_str.push_str(url.scheme());
            url_str.push_str("://");
        }

        // Not handling username/password

        if let Some(host) = url.host_str() {
            url_str.push_str(host);
        }

        if let Some(port) = url.port() {
            use std::fmt::Write;
            url_str.push(':');
            write!(&mut url_str, "{}", port)?;
        }

        if is_github {
            url_str.push_str(&url.path().to_lowercase());
        } else {
            url_str.push_str(url.path());
        }

        // Strip a trailing slash.
        if url_str.ends_with('/') {
            url_str.pop();
        }

        // Repos can generally be accessed with or without `.git` extension.
        if url_str.ends_with(".git") {
            url_str.truncate(url_str.len() - 4);
        }

        let url = Url::parse(&url_str)?;

        Ok(Self(url))
    }
}

pub fn convert_response(
    res: reqwest::blocking::Response,
) -> Result<http::Response<bytes::Bytes>, Error> {
    let mut builder = http::Response::builder()
        .status(res.status())
        .version(res.version());

    let headers = builder
        .headers_mut()
        .ok_or_else(|| anyhow!("failed to convert response headers"))?;

    headers.extend(
        res.headers()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );

    let body = res.bytes()?;

    Ok(builder.body(body)?)
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Encoding {
    Gzip,
    Zstd,
}

use bytes::Bytes;
use std::io;

#[tracing::instrument(level = "debug")]
pub(crate) fn unpack_tar(buffer: Bytes, encoding: Encoding, dir: &Path) -> Result<(), Error> {
    #[allow(clippy::large_enum_variant)]
    enum Decoder<'z, R: io::Read + io::BufRead> {
        Gzip(flate2::read::GzDecoder<R>),
        Zstd(zstd::Decoder<'z, R>),
    }

    impl<'z, R> io::Read for Decoder<'z, R>
    where
        R: io::Read + io::BufRead,
    {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self {
                Self::Gzip(gz) => gz.read(buf),
                Self::Zstd(zstd) => zstd.read(buf),
            }
        }
    }

    use bytes::Buf;
    let buf_reader = buffer.reader();

    let decoder = match encoding {
        Encoding::Gzip => {
            // zstd::Decoder automatically wraps the Read(er) in a BufReader, so do
            // that explicitly for gzip so the types match
            let buf_reader = std::io::BufReader::new(buf_reader);
            Decoder::Gzip(flate2::read::GzDecoder::new(buf_reader))
        }
        Encoding::Zstd => Decoder::Zstd(zstd::Decoder::new(buf_reader)?),
    };

    let mut archive_reader = tar::Archive::new(decoder);

    if let Err(e) = archive_reader.unpack(dir) {
        // Attempt to remove anything that may have been written so that we
        // _hopefully_ don't mess up cargo itself
        if dir.exists() {
            if let Err(e) = remove_dir_all::remove_dir_all(dir) {
                tracing::error!(
                    "error trying to remove contents of {}: {}",
                    dir.display(),
                    e
                );
            }
        }

        return Err(e).context("failed to unpack");
    }

    Ok(())
}

#[tracing::instrument(level = "debug")]
pub(crate) fn pack_tar(path: &std::path::Path) -> Result<Bytes, Error> {
    // If we don't allocate adequate space in our output buffer, things
    // go very poorly for everyone involved
    let mut estimated_size = 0;
    const TAR_HEADER_SIZE: u64 = 512;
    for entry in walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        estimated_size += TAR_HEADER_SIZE;
        if let Ok(md) = entry.metadata() {
            estimated_size += md.len();

            // Add write permissions to all files, this is to
            // get around an issue where unpacking tar files on
            // Windows will result in errors if there are read-only
            // directories
            #[cfg(windows)]
            {
                let mut perms = md.permissions();
                perms.set_readonly(false);
                std::fs::set_permissions(entry.path(), perms)?;
            }
        }
    }

    struct Writer<'z, W: io::Write> {
        encoder: zstd::Encoder<'z, W>,
        original: usize,
    }

    // zstd has a pointer in it, which means it isn't Sync, but
    // this _should_ be fine as writing of the tar is never going to
    // do a write until a previous one has succeeded, as otherwise
    // the stream could be corrupted regardless of the actual write
    // implementation, so this should be fine. :tm:
    // #[allow(unsafe_code)]
    // unsafe impl<'z, W: io::Write + Sync> Sync for Writer<'z, W> {}

    impl<'z, W> io::Write for Writer<'z, W>
    where
        W: io::Write,
    {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.original += buf.len();
            self.encoder.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.encoder.flush()
        }
    }

    use bytes::BufMut;
    let out_buffer = bytes::BytesMut::with_capacity(estimated_size as usize);
    let buf_writer = out_buffer.writer();

    let zstd_encoder = zstd::Encoder::new(buf_writer, 9)?;

    let mut archiver = tar::Builder::new(Writer {
        encoder: zstd_encoder,
        original: 0,
    });
    archiver.append_dir_all(".", path)?;
    archiver.finish()?;

    let writer = archiver.into_inner()?;
    let buf_writer = writer.encoder.finish()?;
    let out_buffer = buf_writer.into_inner();

    debug!(
        input = writer.original,
        output = out_buffer.len(),
        ratio = (out_buffer.len() as f64 / writer.original as f64 * 100.0) as u32,
        "compressed"
    );

    Ok(out_buffer.freeze())
}

// All of cargo's checksums are currently SHA256
pub fn validate_checksum(buffer: &[u8], expected: &str) -> Result<(), Error> {
    if expected.len() != 64 {
        bail!(
            "hex checksum length is {} instead of expected 64",
            expected.len()
        );
    }

    let content_digest = ring::digest::digest(&ring::digest::SHA256, buffer);
    let digest = content_digest.as_ref();

    for (ind, exp) in expected.as_bytes().chunks(2).enumerate() {
        #[inline]
        fn parse_hex(b: u8) -> Result<u8, anyhow::Error> {
            Ok(match b {
                b'A'..=b'F' => b - b'A' + 10,
                b'a'..=b'f' => b - b'a' + 10,
                b'0'..=b'9' => b - b'0',
                c => bail!("invalid byte in expected checksum string {}", c),
            })
        }

        let mut cur = parse_hex(exp[0])?;
        cur <<= 4;
        cur |= parse_hex(exp[1])?;

        if digest[ind] != cur {
            bail!("checksum mismatch, expected {}", expected);
        }
    }

    Ok(())
}

fn parse_s3_url(url: &Url) -> Result<crate::S3Location<'_>, Error> {
    let host = url.host().context("url has no host")?;

    let host_dns = match host {
        url::Host::Domain(h) => h,
        _ => anyhow::bail!("host name is an IP"),
    };

    // We only support virtual-hosted-style references as path style is being deprecated
    // mybucket.s3-us-west-2.amazonaws.com
    // https://aws.amazon.com/blogs/aws/amazon-s3-path-deprecation-plan-the-rest-of-the-story/
    if host_dns.contains("s3") {
        let mut bucket = None;
        let mut region = None;
        let mut host = None;

        for part in host_dns.split('.') {
            if part.is_empty() {
                anyhow::bail!("malformed host name detected");
            }

            if bucket.is_none() {
                bucket = Some(part);
                continue;
            }

            if part.starts_with("s3") && region.is_none() {
                let rgn = &part[2..];

                if let Some(r) = rgn.strip_prefix('-') {
                    region = Some((r, part.len()));
                } else {
                    region = Some(("us-east-1", part.len()));
                }
            } else if region.is_none() {
                bucket = Some(&host_dns[..bucket.as_ref().unwrap().len() + 1 + part.len()]);
            } else if host.is_none() {
                host = Some(
                    &host_dns[2 // for the 2 dots
                        + bucket.as_ref().unwrap().len()
                        + region.as_ref().unwrap().1..],
                );
                break;
            }
        }

        let bucket = bucket.context("bucket not specified")?;
        let region = region.context("region not specified")?.0;
        let host = host.context("host not specified")?;

        Ok(crate::S3Location {
            bucket,
            region,
            host,
            prefix: if !url.path().is_empty() {
                &url.path()[1..]
            } else {
                url.path()
            },
        })
    } else if host_dns == "localhost" {
        let root = url.as_str();
        Ok(crate::S3Location {
            bucket: "testing",
            region: "",
            host: &root[..root.len() - 1],
            prefix: "",
        })
    } else {
        anyhow::bail!("not an s3 url");
    }
}

pub struct CloudLocationUrl {
    pub url: Url,
    pub path: Option<PathBuf>,
}

impl CloudLocationUrl {
    pub fn from_url(url: Url) -> Result<Self, Error> {
        match url.scheme() {
            "file" => {
                let path = url.to_file_path().map_err(|()| {
                    Error::msg(format!("failed to parse file path from url {:?}", url))
                })?;
                Ok(CloudLocationUrl {
                    url,
                    path: Some(path),
                })
            }
            _ => Ok(CloudLocationUrl { url, path: None }),
        }
    }
}

pub fn parse_cloud_location(
    cloud_url: &CloudLocationUrl,
) -> Result<crate::CloudLocation<'_>, Error> {
    let CloudLocationUrl { url, path: _path } = cloud_url;
    match url.scheme() {
        #[cfg(feature = "gcs")]
        "gs" => {
            let bucket = url.domain().context("url doesn't contain a bucket")?;
            // Remove the leading slash that url gives us
            let path = if !url.path().is_empty() {
                &url.path()[1..]
            } else {
                url.path()
            };

            let loc = crate::GcsLocation {
                bucket,
                prefix: path,
            };

            Ok(crate::CloudLocation::Gcs(loc))
        }
        #[cfg(not(feature = "gcs"))]
        "gs" => {
            anyhow::bail!("GCS support was not enabled, you must compile with the 'gcs' feature")
        }
        #[cfg(feature = "fs")]
        "file" => {
            let path = _path.as_ref().unwrap();
            Ok(crate::CloudLocation::Fs(crate::FilesystemLocation { path }))
        }
        #[cfg(not(feature = "fs"))]
        "file" => anyhow::bail!(
            "filesystem support was not enabled, you must compile with the 'fs' feature"
        ),
        "http" | "https" => {
            let s3 = parse_s3_url(url).context("failed to parse s3 url")?;

            if cfg!(feature = "s3") {
                Ok(crate::CloudLocation::S3(s3))
            } else {
                anyhow::bail!("S3 support was not enabled, you must compile with the 's3' feature")
            }
        }
        #[cfg(feature = "blob")]
        "blob" => {
            let container = url.domain().context("url doesn't contain a container")?;
            let prefix = if !url.path().is_empty() {
                &url.path()[1..]
            } else {
                url.path()
            };
            Ok(crate::CloudLocation::Blob(crate::BlobLocation {
                prefix,
                container,
            }))
        }
        #[cfg(not(feature = "blob"))]
        "blob" => {
            anyhow::bail!("Blob support was not enabled, you must compile with the 'blob' feature")
        }
        scheme => anyhow::bail!("the scheme '{}' is not supported", scheme),
    }
}

pub(crate) fn write_ok(to: &Path) -> Result<(), Error> {
    let mut f =
        std::fs::File::create(to).with_context(|| format!("failed to create: {}", to.display()))?;

    use std::io::Write;
    f.write_all(b"ok")?;
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use std::convert::TryFrom;

    #[test]
    fn canonicalizes_urls() {
        let url = Url::parse("git+https://github.com/EmbarkStudios/cpal.git?rev=d59b4de#d59b4decf72a96932a1482cc27fe4c0b50c40d32").unwrap();
        let canonicalized = Canonicalized::try_from(&url).unwrap();

        assert_eq!(
            "https://github.com/embarkstudios/cpal",
            canonicalized.as_ref().as_str()
        );
    }

    #[test]
    fn idents_urls() {
        let url = Url::parse("git+https://github.com/gfx-rs/genmesh?rev=71abe4d").unwrap();
        let canonicalized = Canonicalized::try_from(&url).unwrap();
        let ident = canonicalized.ident();

        assert_eq!(ident, "genmesh-401fe503e87439cc");

        let url = Url::parse("git+https://github.com/EmbarkStudios/cpal?rev=d59b4de#d59b4decf72a96932a1482cc27fe4c0b50c40d32").unwrap();
        let canonicalized = Canonicalized::try_from(&url).unwrap();
        let ident = canonicalized.ident();

        assert_eq!(ident, "cpal-a7ffd7cabefac714");
    }

    #[test]
    fn gets_proper_registry_ident() {
        let crates_io_registry = crate::Registry::default();

        assert_eq!(
            "github.com-1ecc6299db9ec823",
            crates_io_registry.short_name()
        );
    }

    #[test]
    fn validates_checksums() {
        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

        validate_checksum(b"hello world", expected).unwrap();
    }

    #[test]
    fn parses_s3_virtual_hosted_style() {
        let url = Url::parse("http://johnsmith.net.s3.amazonaws.com/homepage.html").unwrap();
        let loc = parse_s3_url(&url).unwrap();

        assert_eq!(loc.bucket, "johnsmith.net");
        assert_eq!(loc.region, "us-east-1");
        assert_eq!(loc.host, "amazonaws.com");
        assert_eq!(loc.prefix, "homepage.html");

        let url =
            Url::parse("http://johnsmith.eu.s3-eu-west-1.amazonaws.com/homepage.html").unwrap();
        let loc = parse_s3_url(&url).unwrap();

        assert_eq!(loc.bucket, "johnsmith.eu");
        assert_eq!(loc.region, "eu-west-1");
        assert_eq!(loc.host, "amazonaws.com");
        assert_eq!(loc.prefix, "homepage.html");

        let url = Url::parse("http://mybucket.s3-us-west-2.amazonaws.com/some_prefix/").unwrap();
        let loc = parse_s3_url(&url).unwrap();

        assert_eq!(loc.bucket, "mybucket");
        assert_eq!(loc.region, "us-west-2");
        assert_eq!(loc.host, "amazonaws.com");
        assert_eq!(loc.prefix, "some_prefix/");

        let url = Url::parse("http://mybucket.with.many.dots.in.it.s3.amazonaws.com/some_prefix/")
            .unwrap();
        let loc = parse_s3_url(&url).unwrap();

        assert_eq!(loc.bucket, "mybucket.with.many.dots.in.it");
        assert_eq!(loc.region, "us-east-1");
        assert_eq!(loc.host, "amazonaws.com");
        assert_eq!(loc.prefix, "some_prefix/");
    }
}
