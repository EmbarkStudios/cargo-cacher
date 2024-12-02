use crate::{Path, PathBuf};
use anyhow::{bail, Context as _};
use tracing::debug;
use url::Url;

#[inline]
pub fn convert_request(req: http::Request<std::io::Empty>) -> reqwest::Request {
    let (parts, _) = req.into_parts();
    http::Request::from_parts(parts, Vec::new())
        .try_into()
        .unwrap()
}

pub async fn convert_response(
    res: reqwest::Response,
) -> anyhow::Result<http::Response<bytes::Bytes>> {
    let mut builder = http::Response::builder()
        .status(res.status())
        .version(res.version());

    let headers = builder
        .headers_mut()
        .context("failed to convert response headers")?;

    headers.extend(
        res.headers()
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );

    let body = res.bytes().await?;

    Ok(builder.body(body)?)
}

pub async fn send_request_with_retry(
    client: &crate::HttpClient,
    req: reqwest::Request,
) -> anyhow::Result<reqwest::Response> {
    loop {
        let reqc = req.try_clone().unwrap();

        match client.execute(reqc).await {
            Err(err) if err.is_connect() || err.is_timeout() || err.is_request() => continue,
            Err(err) => return Err(err.into()),
            Ok(res) => return Ok(res),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Encoding {
    Gzip,
    Zstd,
}

use bytes::Bytes;
use std::io;

#[tracing::instrument(level = "debug")]
pub(crate) fn unpack_tar(buffer: Bytes, encoding: Encoding, dir: &Path) -> anyhow::Result<u64> {
    struct DecoderWrapper<'z, R: io::Read + io::BufRead> {
        /// The total bytes read from the compressed stream
        total: u64,
        inner: Decoder<'z, R>,
    }

    #[allow(clippy::large_enum_variant)]
    enum Decoder<'z, R: io::Read + io::BufRead> {
        Gzip(flate2::read::GzDecoder<R>),
        Zstd(zstd::Decoder<'z, R>),
    }

    impl<R> io::Read for DecoderWrapper<'_, R>
    where
        R: io::Read + io::BufRead,
    {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let read = match &mut self.inner {
                Decoder::Gzip(gz) => gz.read(buf),
                Decoder::Zstd(zstd) => zstd.read(buf),
            };

            let read = read?;
            self.total += read as u64;
            Ok(read)
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

    let mut archive_reader = tar::Archive::new(DecoderWrapper {
        total: 0,
        inner: decoder,
    });

    #[cfg(unix)]
    #[allow(clippy::unnecessary_cast)]
    {
        use std::sync::OnceLock;
        static UMASK: OnceLock<libc::mode_t> = OnceLock::new();
        archive_reader.set_mask(
            *UMASK.get_or_init(|| {
                #[allow(unsafe_code)]
                // SAFETY: Syscalls are unsafe. Calling `umask` twice is even unsafer for
                // multithreading program, since it doesn't provide a way to retrive the
                // value without modifications. We use a static `OnceLock` here to ensure
                // it only gets call once during the entire program lifetime.
                unsafe {
                    let umask = libc::umask(0o022);
                    libc::umask(umask);
                    umask
                }
            }) as u32, // it is u16 on macos
        );
    }

    if let Err(e) = archive_reader.unpack(dir) {
        // Attempt to remove anything that may have been written so that we
        // _hopefully_ don't mess up cargo itself
        if dir.exists() {
            if let Err(e) = remove_dir_all::remove_dir_all(dir) {
                tracing::error!("error trying to remove contents of {dir}: {e}");
            }
        }

        return Err(e).context("failed to unpack");
    }

    Ok(archive_reader.into_inner().total)
}

#[tracing::instrument(level = "debug")]
pub(crate) fn pack_tar(path: &Path) -> anyhow::Result<Bytes> {
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

    impl<W> io::Write for Writer<'_, W>
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

/// Validates the specified buffer's SHA-256 checksum matches the specified value
pub fn validate_checksum(buffer: &[u8], expected: &str) -> anyhow::Result<()> {
    // All of cargo's checksums are currently SHA256
    anyhow::ensure!(
        expected.len() == 64,
        "hex checksum length is {} instead of expected 64",
        expected.len()
    );

    let content_digest = ring::digest::digest(&ring::digest::SHA256, buffer);
    let digest = content_digest.as_ref();

    for (ind, exp) in expected.as_bytes().chunks(2).enumerate() {
        #[inline]
        fn parse_hex(b: u8) -> Result<u8, anyhow::Error> {
            Ok(match b {
                b'A'..=b'F' => b - b'A' + 10,
                b'a'..=b'f' => b - b'a' + 10,
                b'0'..=b'9' => b - b'0',
                c => bail!("invalid byte in expected checksum string {c}"),
            })
        }

        let mut cur = parse_hex(exp[0])?;
        cur <<= 4;
        cur |= parse_hex(exp[1])?;

        anyhow::ensure!(digest[ind] == cur, "checksum mismatch, expected {expected}");
    }

    Ok(())
}

fn parse_s3_url(url: &Url) -> anyhow::Result<crate::S3Location<'_>> {
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
    pub fn from_url(url: Url) -> anyhow::Result<Self> {
        if url.scheme() == "file" {
            let path = url
                .to_file_path()
                .map_err(|_sigh| anyhow::anyhow!("failed to parse file path from url {url:?}"))
                .and_then(|path| match PathBuf::from_path_buf(path) {
                    Ok(p) => Ok(p),
                    Err(err) => Err(anyhow::anyhow!("url path '{}' is not utf-8", err.display())),
                })?;
            Ok(CloudLocationUrl {
                url,
                path: Some(path),
            })
        } else {
            Ok(CloudLocationUrl { url, path: None })
        }
    }
}

#[inline]
pub fn path(p: &std::path::Path) -> anyhow::Result<&Path> {
    p.try_into().context("path is not utf-8")
}

pub fn parse_cloud_location(
    cloud_url: &CloudLocationUrl,
) -> anyhow::Result<crate::CloudLocation<'_>> {
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
        "file" => {
            let path = _path.as_ref().unwrap();
            Ok(crate::CloudLocation::Fs(crate::FilesystemLocation { path }))
        }
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

pub(crate) fn write_ok(to: &Path) -> anyhow::Result<()> {
    let mut f = std::fs::File::create(to).with_context(|| format!("failed to create: {to}"))?;

    use std::io::Write;
    f.write_all(b"{\"v\":1}")?;
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use tame_index::utils::url_to_local_dir;

    #[test]
    fn idents_urls() {
        let url = Url::parse("git+https://github.com/gfx-rs/genmesh?rev=71abe4d").unwrap();

        assert_eq!(
            url_to_local_dir(url.as_str()).unwrap().dir_name,
            "genmesh-401fe503e87439cc"
        );

        let url = Url::parse("git+https://github.com/EmbarkStudios/cpal?rev=d59b4de#d59b4decf72a96932a1482cc27fe4c0b50c40d32").unwrap();

        assert_eq!(
            url_to_local_dir(url.as_str()).unwrap().dir_name,
            "cpal-a7ffd7cabefac714"
        );
    }

    #[test]
    fn gets_proper_registry_ident() {
        use crate::cargo::RegistryProtocol;
        let crates_io_registry = crate::Registry::crates_io(RegistryProtocol::Git);

        assert_eq!(
            "github.com-1ecc6299db9ec823",
            crates_io_registry.short_name()
        );

        let crates_io_sparse_registry = crate::Registry::crates_io(RegistryProtocol::Sparse);

        assert_eq!(
            "index.crates.io-6f17d22bba15001f",
            crates_io_sparse_registry.short_name()
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
