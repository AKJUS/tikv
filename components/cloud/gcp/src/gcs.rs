// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.
use std::{fmt::Display, io};

use async_trait::async_trait;
use cloud::{
    blob::{
        BlobConfig, BlobObject, BlobStorage, BucketConf, DeletableStorage, IterableStorage,
        PutResource, StringNonEmpty, none_to_empty, read_to_end,
    },
    metrics,
};
use futures_util::{
    future::{FutureExt, LocalBoxFuture, TryFutureExt},
    io::Cursor,
    stream::{self, Stream, StreamExt, TryStreamExt},
};
use http::HeaderValue;
use hyper::{Body, Request, Response};
pub use kvproto::brpb::Gcs as InputConfig;
use tame_gcs::{
    common::{PredefinedAcl, StorageClass},
    objects::{InsertObjectOptional, ListOptional, ListResponse, Metadata, Object},
    types::{BucketName, ObjectId},
};
use tame_oauth::gcp::ServiceAccountInfo;
use tikv_util::{
    stream::{AsyncReadAsSyncStreamOfBytes, error_stream},
    time::Instant,
};

use crate::{
    client::{GcpClient, RequestError, status_code_error},
    utils::{self, retry},
};

const DEFAULT_SEP: char = '/';
const GOOGLE_APIS: &str = "https://www.googleapis.com";
const HARDCODED_ENDPOINTS_SUFFIX: &[&str] = &["upload/storage/v1/", "storage/v1/"];

#[derive(Clone, Debug)]
pub struct Config {
    bucket: BucketConf,
    predefined_acl: Option<PredefinedAcl>,
    storage_class: Option<StorageClass>,
    svc_info: Option<ServiceAccountInfo>,
}

impl Config {
    #[cfg(test)]
    pub fn default(bucket: BucketConf) -> Self {
        Self {
            bucket,
            predefined_acl: None,
            storage_class: None,
            svc_info: None,
        }
    }

    pub fn missing_credentials() -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, "missing credentials")
    }

    pub fn from_input(input: InputConfig) -> io::Result<Config> {
        let endpoint = StringNonEmpty::opt(input.endpoint);
        let bucket = BucketConf {
            endpoint,
            bucket: StringNonEmpty::required_field(input.bucket, "bucket")?,
            prefix: StringNonEmpty::opt(input.prefix),
            storage_class: StringNonEmpty::opt(input.storage_class),
            region: None,
        };
        let predefined_acl = parse_predefined_acl(&input.predefined_acl)
            .or_invalid_input("invalid predefined_acl")?;
        let storage_class = parse_storage_class(&none_to_empty(bucket.storage_class.clone()))
            .or_invalid_input("invalid storage_class")?;
        let svc_info = if let Some(cred) = StringNonEmpty::opt(input.credentials_blob) {
            Some(deserialize_service_account_info(cred)?)
        } else {
            None
        };
        Ok(Config {
            bucket,
            predefined_acl,
            svc_info,
            storage_class,
        })
    }
}

fn deserialize_service_account_info(
    cred: StringNonEmpty,
) -> std::result::Result<ServiceAccountInfo, RequestError> {
    ServiceAccountInfo::deserialize(cred.to_string())
        .map_err(|e| RequestError::OAuth(e, "deserialize ServiceAccountInfo".to_string()))
}

impl BlobConfig for Config {
    fn name(&self) -> &'static str {
        STORAGE_NAME
    }

    fn url(&self) -> io::Result<url::Url> {
        self.bucket
            .url("gcs")
            .map_err(|s| io::Error::other(format!("error creating bucket url: {}", s)))
    }
}

// GCS compatible storage
#[derive(Clone)]
pub struct GcsStorage {
    config: Config,
    client: GcpClient,
}

pub trait ResultExt {
    type Ok;

    // Maps the error of this result as an `std::io::Error` with `Other` error
    // kind.
    fn or_io_error<D: Display>(self, msg: D) -> io::Result<Self::Ok>;

    // Maps the error of this result as an `std::io::Error` with `InvalidInput`
    // error kind.
    fn or_invalid_input<D: Display>(self, msg: D) -> io::Result<Self::Ok>;
}

impl<T, E: Display> ResultExt for Result<T, E> {
    type Ok = T;
    fn or_io_error<D: Display>(self, msg: D) -> io::Result<T> {
        self.map_err(|e| io::Error::other(format!("{}: {}", msg, e)))
    }
    fn or_invalid_input<D: Display>(self, msg: D) -> io::Result<T> {
        self.map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{}: {}", msg, e)))
    }
}

impl DeletableStorage for GcsStorage {
    fn delete(&self, name: &str) -> LocalBoxFuture<'_, io::Result<()>> {
        let name = name.to_owned();
        async move {
            let key = self.maybe_prefix_key(&name);
            let oid = ObjectId::new(self.config.bucket.bucket.to_string(), key)
                .or_invalid_input(format_args!("invalid object id"))?;
            let now = Instant::now();
            retry(
                || async {
                    let req = Object::delete(&oid, None).map_err(RequestError::Gcs)?;
                    self.make_request(
                        req.map(|_: io::Empty| Body::empty()),
                        tame_gcs::Scopes::ReadWrite,
                    )
                    .await
                },
                "delete",
            )
            .await?;
            metrics::CLOUD_REQUEST_HISTOGRAM_VEC
                .with_label_values(&["gcp", "delete"])
                .observe(now.saturating_elapsed_secs());

            Ok(())
        }
        .boxed_local()
    }
}

impl GcsStorage {
    pub fn from_input(input: InputConfig) -> io::Result<Self> {
        Self::new(Config::from_input(input)?)
    }

    /// Create a new GCS storage for the given config.
    pub fn new(config: Config) -> io::Result<GcsStorage> {
        let client = GcpClient::with_svc_info(config.svc_info.clone())?;
        Ok(GcsStorage { config, client })
    }

    fn maybe_prefix_key(&self, key: &str) -> String {
        if let Some(prefix) = &self.config.bucket.prefix {
            return format!("{}{}{}", prefix, DEFAULT_SEP, key);
        }
        key.to_owned()
    }

    async fn make_request(
        &self,
        mut req: Request<Body>,
        scope: tame_gcs::Scopes,
    ) -> Result<Response<Body>, RequestError> {
        // replace the hard-coded GCS endpoint by the custom one.

        if let Some(endpoint) = &self.config.bucket.endpoint {
            let uri = req.uri().to_string();
            let new_url_opt = change_host(endpoint, &uri);
            if let Some(new_url) = new_url_opt {
                *req.uri_mut() = new_url.parse()?;
            }
        }

        self.client.make_request(req, scope).await
    }

    fn strip_prefix_if_needed(&self, key: String) -> String {
        if let Some(prefix) = &self.config.bucket.prefix {
            if key.starts_with(prefix.as_str()) {
                return key[prefix.len()..]
                    .trim_start_matches(DEFAULT_SEP)
                    .to_owned();
            }
        }
        key
    }

    fn error_to_async_read<E>(kind: io::ErrorKind, e: E) -> cloud::blob::BlobStream<'static>
    where
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        Box::new(error_stream(io::Error::new(kind, e)).into_async_read())
    }

    fn get_range(&self, name: &str, range: Option<String>) -> cloud::blob::BlobStream<'_> {
        let bucket = self.config.bucket.bucket.to_string();
        let name = self.maybe_prefix_key(name);
        debug!("read file from GCS storage"; "key" => %name);
        let oid = match ObjectId::new(bucket, name) {
            Ok(oid) => oid,
            Err(e) => return GcsStorage::error_to_async_read(io::ErrorKind::InvalidInput, e),
        };
        let mut request = match Object::download(&oid, None /* optional */) {
            Ok(request) => request.map(|_: io::Empty| Body::empty()),
            Err(e) => return GcsStorage::error_to_async_read(io::ErrorKind::Other, e),
        };
        if let Some(r) = range {
            let header_value = match HeaderValue::from_str(&r) {
                Ok(v) => v,
                Err(e) => return GcsStorage::error_to_async_read(io::ErrorKind::Other, e),
            };
            request.headers_mut().insert("Range", header_value);
        }
        Box::new(
            self.make_request(request, tame_gcs::Scopes::ReadOnly)
                .and_then(|response| async {
                    if response.status().is_success() {
                        Ok(response.into_body().map_err(|e| {
                            io::Error::new(
                                // Given the status is success, if the content stream has been cut down, 
                                // there must be some network unavailable, which should generally be retryable.
                                io::ErrorKind::Interrupted,
                                format!("download from GCS error: {}", e),
                            )
                        }))
                    } else {
                        Err(status_code_error(
                            response.status(),
                            "bucket read".to_string(),
                        ))
                    }
                })
                .err_into::<io::Error>()
                .try_flatten_stream()
                .boxed() // this `.boxed()` pin the stream.
                .into_async_read(),
        )
    }
}

fn change_host(host: &StringNonEmpty, url: &str) -> Option<String> {
    let new_host = (|| {
        for hardcoded in HARDCODED_ENDPOINTS_SUFFIX {
            if let Some(res) = host.strip_suffix(hardcoded) {
                return StringNonEmpty::opt(res.to_owned()).unwrap();
            }
        }
        host.to_owned()
    })();
    if let Some(res) = url.strip_prefix(GOOGLE_APIS) {
        return Some([new_host.trim_end_matches('/'), res].concat());
    }
    None
}

// Convert manually since they don't implement FromStr.
fn parse_storage_class(sc: &str) -> Result<Option<StorageClass>, &str> {
    Ok(Some(match sc {
        "" => return Ok(None),
        "STANDARD" => StorageClass::Standard,
        "NEARLINE" => StorageClass::Nearline,
        "COLDLINE" => StorageClass::Coldline,
        "DURABLE_REDUCED_AVAILABILITY" => StorageClass::DurableReducedAvailability,
        "REGIONAL" => StorageClass::Regional,
        "MULTI_REGIONAL" => StorageClass::MultiRegional,
        _ => return Err(sc),
    }))
}

fn parse_predefined_acl(acl: &str) -> Result<Option<PredefinedAcl>, &str> {
    Ok(Some(match acl {
        "" => return Ok(None),
        "authenticatedRead" => PredefinedAcl::AuthenticatedRead,
        "bucketOwnerFullControl" => PredefinedAcl::BucketOwnerFullControl,
        "bucketOwnerRead" => PredefinedAcl::BucketOwnerRead,
        "private" => PredefinedAcl::Private,
        "projectPrivate" => PredefinedAcl::ProjectPrivate,
        "publicRead" => PredefinedAcl::PublicRead,
        _ => return Err(acl),
    }))
}

const STORAGE_NAME: &str = "gcs";

#[async_trait]
impl BlobStorage for GcsStorage {
    fn config(&self) -> Box<dyn BlobConfig> {
        Box::new(self.config.clone()) as Box<dyn BlobConfig>
    }

    async fn put(
        &self,
        name: &str,
        reader: PutResource<'_>,
        content_length: u64,
    ) -> io::Result<()> {
        let key = self.maybe_prefix_key(name);
        debug!("save file to GCS storage"; "key" => %key);

        // Common setup
        let oid = ObjectId::new(self.config.bucket.bucket.to_string(), key.clone())
            .or_invalid_input(format_args!("invalid object id"))?;

        match content_length {
            // Empty file case
            0 => {
                let begin = Instant::now_coarse();
                retry(
                    || async {
                        let optional = InsertObjectOptional {
                            predefined_acl: self.config.predefined_acl,
                            ..Default::default()
                        };
                        let req = Object::insert_simple(&oid, "", 0, Some(optional))
                            .map_err(RequestError::Gcs)?
                            .map(|_| Body::empty());
                        self.make_request(req, tame_gcs::Scopes::ReadWrite).await
                    },
                    "insert_simple",
                )
                .await?;
                metrics::CLOUD_REQUEST_HISTOGRAM_VEC
                    .with_label_values(&["gcp", "insert_simple"])
                    .observe(begin.saturating_elapsed_secs());
                Ok(())
            }
            // Non-empty file case
            _ => {
                let begin = Instant::now_coarse();
                let mut data = Vec::with_capacity(content_length as usize);
                read_to_end(reader, &mut data).await?;
                metrics::CLOUD_REQUEST_HISTOGRAM_VEC
                    .with_label_values(&["gcp", "read_local"])
                    .observe(begin.saturating_elapsed_secs());

                let metadata = Metadata {
                    name: Some(key),
                    storage_class: self.config.storage_class,
                    ..Default::default()
                };
                let begin = Instant::now_coarse();
                retry(
                    || async {
                        let optional = InsertObjectOptional {
                            predefined_acl: self.config.predefined_acl,
                            ..Default::default()
                        };
                        let data = Cursor::new(data.clone());
                        let req = Object::insert_multipart(
                            &oid.bucket,
                            data,
                            content_length,
                            &metadata,
                            Some(optional),
                        )
                        .map_err(RequestError::Gcs)?
                        .map(|reader| Body::wrap_stream(AsyncReadAsSyncStreamOfBytes::new(reader)));
                        self.make_request(req, tame_gcs::Scopes::ReadWrite).await
                    },
                    "insert_multipart",
                )
                .await?;
                metrics::CLOUD_REQUEST_HISTOGRAM_VEC
                    .with_label_values(&["gcp", "insert_multipart"])
                    .observe(begin.saturating_elapsed_secs());
                Ok(())
            }
        }
    }

    fn get(&self, name: &str) -> cloud::blob::BlobStream<'_> {
        self.get_range(name, None)
    }

    fn get_part(&self, name: &str, off: u64, len: u64) -> cloud::blob::BlobStream<'_> {
        // inclusive, bytes=0-499 -> [0, 499]
        self.get_range(name, Some(format!("bytes={}-{}", off, off + len - 1)))
    }
}

struct GcsPrefixIter<'cli> {
    cli: &'cli GcsStorage,
    page_token: Option<String>,
    prefix: String,
    finished: bool,
}

impl GcsPrefixIter<'_> {
    async fn one_page(&mut self) -> io::Result<Option<Vec<BlobObject>>> {
        if self.finished {
            return Ok(None);
        }

        let mut opt = ListOptional::default();
        let bucket =
            BucketName::try_from(self.cli.config.bucket.bucket.to_string()).or_invalid_input(
                format_args!("invalid bucket {}", self.cli.config.bucket.bucket),
            )?;
        let prefix = self.cli.maybe_prefix_key(&self.prefix);
        opt.prefix = Some(&prefix);
        opt.page_token = self.page_token.as_deref();
        let now = Instant::now();
        let req = Object::list(&bucket, Some(opt)).or_io_error(format_args!(
            "failed to list with prefix {} page_token {:?}",
            self.prefix, self.page_token
        ))?;
        let res = self
            .cli
            .make_request(req.map(|_e| Body::empty()), tame_gcs::Scopes::ReadOnly)
            .await
            .map_err(|err| io::Error::other(err))?;
        let resp = utils::read_from_http_body::<ListResponse>(res).await?;
        metrics::CLOUD_REQUEST_HISTOGRAM_VEC
            .with_label_values(&["gcp", "list"])
            .observe(now.saturating_elapsed_secs());

        debug!("requesting paging GCP"; "prefix" => %self.prefix, "page_token" => self.page_token.as_deref(), 
            "response_size" => resp.objects.len(), "new_page_token" => resp.page_token.as_deref());
        // GCP returns an empty page token when returning the last page...
        // We need to break there or we will enter an infinity loop...
        if resp.page_token.is_none() {
            self.finished = true;
        }
        self.page_token = resp.page_token;
        let items = resp
            .objects
            .into_iter()
            .map(|v| BlobObject {
                key: self.cli.strip_prefix_if_needed(v.name.unwrap_or_default()),
            })
            .collect::<Vec<_>>();
        Ok(Some(items))
    }
}

impl IterableStorage for GcsStorage {
    fn iter_prefix(
        &self,
        prefix: &str,
    ) -> std::pin::Pin<Box<dyn Stream<Item = io::Result<cloud::blob::BlobObject>> + '_>> {
        let walker = GcsPrefixIter {
            cli: self,
            page_token: None,
            prefix: prefix.to_owned(),
            finished: false,
        };
        let s = stream::try_unfold(walker, |mut w| async move {
            let res = w.one_page().await?;
            io::Result::Ok(res.map(|v| (v, w)))
        })
        .map_ok(|data| stream::iter(data.into_iter().map(Ok)))
        .try_flatten();
        Box::pin(s)
    }
}

#[cfg(test)]
mod tests {
    use matches::assert_matches;

    use super::*;
    const HARDCODED_ENDPOINTS: &[&str] = &[
        "https://www.googleapis.com/upload/storage/v1",
        "https://www.googleapis.com/storage/v1",
    ];

    #[test]
    fn test_change_host() {
        let host = StringNonEmpty::static_str("http://localhost:4443");
        assert_eq!(
            &change_host(&host, &format!("{}/storage/v1/foo", GOOGLE_APIS)).unwrap(),
            "http://localhost:4443/storage/v1/foo"
        );

        let h1 = url::Url::parse(HARDCODED_ENDPOINTS[0]).unwrap();
        let h2 = url::Url::parse(HARDCODED_ENDPOINTS[1]).unwrap();

        let endpoint = StringNonEmpty::static_str("http://example.com");
        assert_eq!(
            &change_host(&endpoint, h1.as_str()).unwrap(),
            "http://example.com/upload/storage/v1"
        );
        assert_eq!(
            &change_host(&endpoint, h2.as_str()).unwrap(),
            "http://example.com/storage/v1"
        );
        assert_eq!(
            &change_host(&endpoint, &format!("{}/foo", h2)).unwrap(),
            "http://example.com/storage/v1/foo"
        );
        assert_matches!(&change_host(&endpoint, "foo"), None);

        // if we get the endpoint with suffix "/storage/v1/"
        let endpoint = StringNonEmpty::static_str("http://example.com/storage/v1/");
        assert_eq!(
            &change_host(&endpoint, &format!("{}/foo", h2)).unwrap(),
            "http://example.com/storage/v1/foo"
        );

        let endpoint = StringNonEmpty::static_str("http://example.com/upload/storage/v1/");
        assert_eq!(
            &change_host(&endpoint, &format!("{}/foo", h2)).unwrap(),
            "http://example.com/storage/v1/foo"
        );
    }

    #[test]
    fn test_parse_storage_class() {
        assert_matches!(
            parse_storage_class("STANDARD"),
            Ok(Some(StorageClass::Standard))
        );
        assert_matches!(parse_storage_class(""), Ok(None));
        assert_matches!(
            parse_storage_class("NOT_A_STORAGE_CLASS"),
            Err("NOT_A_STORAGE_CLASS")
        );
    }

    #[test]
    fn test_parse_acl() {
        // can't use assert_matches!(), PredefinedAcl doesn't even implement Debug.
        assert!(matches!(
            parse_predefined_acl("private"),
            Ok(Some(PredefinedAcl::Private))
        ));
        assert!(matches!(parse_predefined_acl(""), Ok(None)));
        assert!(matches!(parse_predefined_acl("notAnACL"), Err("notAnACL")));
    }

    #[test]
    fn test_url_of_backend() {
        let bucket_name = StringNonEmpty::static_str("bucket");
        let mut bucket = BucketConf::default(bucket_name);
        bucket.prefix = Some(StringNonEmpty::static_str("/backup 02/prefix/"));
        let gcs = Config::default(bucket.clone());
        // only 'bucket' and 'prefix' should be visible in url_of_backend()
        assert_eq!(
            gcs.url().unwrap().to_string(),
            "gcs://bucket/backup%2002/prefix/"
        );
        bucket.endpoint = Some(StringNonEmpty::static_str("http://endpoint.com"));
        assert_eq!(
            &Config::default(bucket).url().unwrap().to_string(),
            "http://endpoint.com/bucket/backup%2002/prefix/"
        );
    }
}
