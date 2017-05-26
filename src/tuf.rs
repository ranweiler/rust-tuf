use chrono::UTC;
use json;
use hyper::Url as HyperUrl;
use hyper::client::Client;
use ring::digest;
use ring::digest::{SHA256, SHA512};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, DirBuilder};
use std::io::{Read, Write, Seek, SeekFrom};
use std::path::PathBuf;
use url::Url;
use uuid::Uuid;

use cjson;
use error::Error;
use metadata::{Role, RoleType, Root, Targets, Timestamp, Snapshot, Metadata, SignedMetadata,
               RootMetadata, TargetsMetadata, TimestampMetadata, SnapshotMetadata, HashType,
               HashValue, KeyId, Key};
use util;


/// Interface for interacting with TUF repositories.
#[derive(Debug)]
pub struct Tuf {
    url: Url,
    local_path: PathBuf,
    http_client: Client,
    root: RootMetadata,
    targets: Option<TargetsMetadata>,
    timestamp: Option<TimestampMetadata>,
    snapshot: Option<SnapshotMetadata>,
}

impl Tuf {
    /// Create a `Tuf` struct from an existing repo with the initial root keys pinned. This also
    /// calls `initialize` to ensure the needed paths exist.
    pub fn from_root_keys(root_keys: Vec<Key>, config: Config) -> Result<Self, Error> {
        Self::initialize(&config.local_path)?;

        let root = {
            let fetch_type = &FetchType::Cache(config.local_path.clone());

            match Self::read_root_with_keys(fetch_type, &config.http_client, &root_keys) {
                Ok(modified_root) => {
                    Self::get_meta_num::<Root, RootMetadata, File>(fetch_type,
                                                                   &config.http_client,
                                                                   1,
                                                                   &modified_root,
                                                                   &mut None)?
                }
                Err(e) => {
                    debug!("Failed to read root locally: {:?}", e);
                    let fetch_type = &match config.url.scheme() {
                        "file" => FetchType::File(util::url_path_to_os_path(config.url.path())?),
                        "http" | "https" => FetchType::Http(util::url_to_hyper_url(&config.url)?),
                        x => return Err(Error::Generic(format!("Unsupported URL scheme: {}", x))),
                    };
                    let modified_root =
                        Self::read_root_with_keys(fetch_type, &config.http_client, &root_keys)?;
                    Self::get_meta_num::<Root, RootMetadata, File>(fetch_type,
                                                                   &config.http_client,
                                                                   1,
                                                                   &modified_root,
                                                                   &mut None)?
                }
            }
        };

        let mut tuf = Tuf {
            url: config.url,
            local_path: config.local_path,
            http_client: config.http_client,
            root: root,
            targets: None,
            timestamp: None,
            snapshot: None,
        };

        tuf.update()?;
        Ok(tuf)
    }

    /// Create a `Tuf` struct from a new repo. Must contain the `root.json`. The root is trusted
    /// with only verification on consistency, not authenticity. This call also calls `initialize`
    /// to ensure the needed paths exist.
    pub fn new(config: Config) -> Result<Self, Error> {
        Self::initialize(&config.local_path)?;

        let root = {
            let fetch_type = &FetchType::Cache(config.local_path.clone());
            let root = Self::unverified_read_root(fetch_type, &config.http_client)?;
            Self::get_metadata::<Root, RootMetadata, File>(fetch_type,
                                                           &config.http_client,
                                                           &root,
                                                           &mut None)?
        };

        let mut tuf = Tuf {
            url: config.url,
            local_path: config.local_path,
            http_client: config.http_client,
            root: root,
            targets: None,
            timestamp: None,
            snapshot: None,
        };
        tuf.update()?;

        Ok(tuf)
    }

    /// Create and verify the necessary directory structure for a TUF repo.
    pub fn initialize(local_path: &PathBuf) -> Result<(), Error> {
        info!("Initializing local storage: {}",
              local_path.to_string_lossy());

        for dir in vec![PathBuf::from("metadata").join("current"),
                        PathBuf::from("metadata").join("archive"),
                        PathBuf::from("targets"),
                        PathBuf::from("temp")]
            .iter() {
            let path = local_path.as_path().join(dir);
            debug!("Creating path: {}", path.to_string_lossy());
            DirBuilder::new().recursive(true).create(path)?
            // TODO error if path is not fully owned by the current user
        }

        Ok(())
    }

    // TODO clean function that cleans up local_path for old targets, old dirs, etc

    fn temp_file(&self) -> Result<(File, PathBuf), Error> {
        let uuid = Uuid::new_v4();
        let path = self.local_path.as_path().join("temp").join(uuid.hyphenated().to_string());

        debug!("Creating temp file: {:?}", path);
        Ok((File::create(path.clone())?, path.to_path_buf()))
    }

    /// Update the metadata from local and remote sources.
    pub fn update(&mut self) -> Result<(), Error> {
        info!("Updating metdata");
        match self.update_local() {
            Ok(()) => (),
            Err(e) => warn!("Error updating metadata from local sources: {:?}", e),
        };
        self.update_remote()?;
        info!("Successfully updated metadata");
        Ok(())
    }

    fn update_remote(&mut self) -> Result<(), Error> {
        debug!("Updating metadata from remote sources");
        let fetch_type = &match self.url.scheme() {
            "file" => FetchType::File(util::url_path_to_os_path(self.url.path())?),
            "http" | "https" => FetchType::Http(util::url_to_hyper_url(&self.url)?),
            x => {
                let msg = format!("Programming error: unsupported URL scheme {}. Please report \
                                   this as a bug.",
                                  x);
                error!("{}", msg);
                return Err(Error::Generic(msg));
            }
        };

        self.update_root(fetch_type)?;

        if self.update_timestamp(fetch_type)? && self.update_snapshot(fetch_type)? {
            self.update_targets(fetch_type)
        } else {
            Ok(())
        }
    }

    fn update_local(&mut self) -> Result<(), Error> {
        debug!("Updating metadata from local sources");
        let fetch_type = &FetchType::Cache(self.local_path.clone());

        self.update_root(fetch_type)?;

        if self.update_timestamp(fetch_type)? && self.update_snapshot(fetch_type)? {
            self.update_targets(fetch_type)
        } else {
            Ok(())
        }
    }

    fn update_root(&mut self, fetch_type: &FetchType) -> Result<(), Error> {
        debug!("Updating root metadata");

        let temp_root = Self::unverified_read_root(fetch_type, &self.http_client)?;

        // TODO reuse temp root as last one
        for i in (self.root.version + 1)..(temp_root.version + 1) {
            let (mut out, out_path) = if !fetch_type.is_cache() {
                let (file, path) = self.temp_file()?;
                (Some(file), Some(path))
            } else {
                (None, None)
            };

            let root = match Self::get_meta_num::<Root, RootMetadata, File>(fetch_type,
                                                                            &self.http_client,
                                                                            i,
                                                                            &self.root,
                                                                            &mut out) {
                Ok(root) => root,
                Err(e) => {
                    match out_path {
                        Some(out_path) => {
                            match fs::remove_file(out_path.clone()) {
                                Ok(_) => (),
                                Err(e) => warn!("Error removing temp file {:?}: {}", out_path, e),
                            }
                        }
                        None => (),
                    }
                    return Err(e);
                }
            };

            // verify root again against itself (for cross signing)
            // TODO this is not the most efficient way to do it, but it works
            match Self::get_meta_num::<Root, RootMetadata, File>(fetch_type,
                                                                 &self.http_client,
                                                                 i,
                                                                 &root,
                                                                 &mut None::<File>) {
                Ok(root_again) => {
                    if root != root_again {
                        // TODO better error message
                        return Err(Error::Generic(format!("Cross singning of root version {} \
                                                           failed",
                                                          i)));
                    }
                }
                Err(e) => {
                    match out_path {
                        Some(out_path) => {
                            match fs::remove_file(out_path.clone()) {
                                Ok(_) => (),
                                Err(e) => warn!("Error removing temp file {:?}: {}", out_path, e),
                            }
                        }
                        None => (),
                    }
                    return Err(e);
                }
            };

            info!("Rotated to root metadata version {}", i);
            self.root = root;

            match out_path {
                Some(out_path) => {
                    fs::rename(out_path,
                               self.local_path
                                   .join("metadata")
                                   .join("archive")
                                   .join(format!("{}.root.json", i)))?
                }
                None => (),
            };

            // set to None to untrust old metadata
            // TODO check that these resets are in line with the Mercury paper
            self.targets = None;
            self.timestamp = None;
            self.snapshot = None;
        }

        Ok(())
    }

    fn update_timestamp(&mut self, fetch_type: &FetchType) -> Result<bool, Error> {
        debug!("Updating timestamp metadata");

        let (mut out, out_path) = if !fetch_type.is_cache() {
            let (file, path) = self.temp_file()?;
            (Some(file), Some(path))
        } else {
            (None, None)
        };

        let timestamp =
            Self::get_metadata::<Timestamp, TimestampMetadata, File>(fetch_type,
                                                                     &self.http_client,
                                                                     &self.root,
                                                                     &mut out)?;

        match self.timestamp {
            Some(ref t) if t.version > timestamp.version => {
                match out_path {
                    Some(out_path) => fs::remove_file(out_path)?,
                    None => (),
                };

                return Err(Error::VersionDecrease(Role::Timestamp));
            }
            Some(ref t) if t.version == timestamp.version => return Ok(false),
            _ => self.timestamp = Some(timestamp),
        }

        if let Some(ref timestamp) = self.timestamp {
            if let Some(ref timestamp_meta) = timestamp.meta.get("snapshot.json") {
                if timestamp_meta.version > timestamp.version {
                    info!("Timestamp metadata is up to date");

                    match out_path {
                        Some(out_path) => {
                            match fs::remove_file(out_path.clone()) {
                                Ok(_) => (),
                                Err(e) => warn!("Error removing temp file {:?}: {}", out_path, e),
                            }
                        }
                        None => (),
                    };

                    return Ok(false);
                }
            }
        }

        match out_path {
            Some(out_path) => {
                let current_path = self.local_path
                    .join("metadata")
                    .join("current")
                    .join("timestamp.json");

                if current_path.exists() {
                    fs::rename(current_path.clone(),
                               self.local_path
                                   .join("metadata")
                                   .join("archive")
                                   .join("timestamp.json"))?;
                };

                fs::rename(out_path, current_path)?
            }
            None => (),
        };

        Ok(true)
    }

    fn update_snapshot(&mut self, fetch_type: &FetchType) -> Result<bool, Error> {
        debug!("Updating snapshot metadata");

        let meta = match self.timestamp {
            Some(ref timestamp) => {
                match timestamp.meta.get("snapshot.json") {
                    Some(meta) => meta,
                    None => {
                        return Err(Error::VerificationFailure("Missing snapshot.json in \
                                                               timestamp.json"
                            .to_string()))
                    }
                }
            }
            None => return Err(Error::MissingMetadata(Role::Timestamp)),
        };

        let (hash_alg, expected_hash): (&HashType, &HashValue) = HashType::preferences().iter()
            .fold(None, |res, pref| {
                res.or_else(|| if let Some(hash) = meta.hashes.get(&pref) {
                    Some((pref, hash))
                } else {
                    None
                })
            })
            .ok_or_else(|| Error::NoSupportedHashAlgorithms)?;

        let (mut out, out_path) = if !fetch_type.is_cache() {
            let (file, path) = self.temp_file()?;
            (Some(file), Some(path))
        } else {
            (None, None)
        };

        let snapshot = Self::get_meta_prefix::<Snapshot,
                                               SnapshotMetadata,
                                               File>(fetch_type,
                                                     &self.http_client,
                                                     "",
                                                     &self.root,
                                                     Some(meta.length),
                                                     Some((&hash_alg, &expected_hash.0)),
                                                     &mut out)?;

        // TODO ? check downloaded version matches what was in the timestamp.json

        match self.snapshot {
            Some(ref s) if s.version > snapshot.version => {
                match out_path {
                    Some(out_path) => fs::remove_file(out_path)?,
                    None => (),
                };
                return Err(Error::VersionDecrease(Role::Snapshot));
            }
            Some(ref s) if s.version == snapshot.version => return Ok(false),
            _ => self.snapshot = Some(snapshot),
        }

        // TODO this needs to be extended once we do delegations
        if let Some(ref snapshot) = self.snapshot {
            if let Some(ref snapshot_meta) = snapshot.meta.get("targets.json") {
                if let Some(ref targets) = self.targets {
                    if snapshot_meta.version > targets.version {
                        info!("Snapshot metadata is up to date");

                        match out_path {
                            Some(out_path) => fs::remove_file(out_path)?,
                            None => (),
                        };
                        return Ok(false);
                    }
                }
            }
        }

        match out_path {
            Some(out_path) => {
                let current_path = self.local_path
                    .join("metadata")
                    .join("current")
                    .join("snapshot.json");

                if current_path.exists() {
                    fs::rename(current_path.clone(),
                               self.local_path
                                   .join("metadata")
                                   .join("archive")
                                   .join("snapshot.json"))?;
                };

                fs::rename(out_path, current_path)?
            }
            None => (),
        };

        Ok(true)
    }

    fn update_targets(&mut self, fetch_type: &FetchType) -> Result<(), Error> {
        debug!("Updating targets metadata");

        let meta = match self.snapshot {
            Some(ref snapshot) => {
                match snapshot.meta.get("targets.json") {
                    Some(meta) => meta,
                    None => {
                        return Err(Error::VerificationFailure("Missing targets.json in \
                                                               snapshot.json"
                            .to_string()))
                    }
                }
            }
            None => return Err(Error::MissingMetadata(Role::Snapshot)),
        };

        let hash_data = match meta.hashes {
            Some(ref hashes) => {
                Some(HashType::preferences().iter()
                    .fold(None, |res, pref| {
                        res.or_else(|| if let Some(hash) = hashes.get(&pref) {
                            Some((pref, hash))
                        } else {
                            None
                        })
                    })
                    .ok_or_else(|| Error::NoSupportedHashAlgorithms)?)
            }
            None => None,
        };

        let hash_data = hash_data.map(|(t, v)| (t, v.0.as_slice()));

        let (mut out, out_path) = if !fetch_type.is_cache() {
            let (file, path) = self.temp_file()?;
            (Some(file), Some(path))
        } else {
            (None, None)
        };

        let targets = Self::get_meta_prefix::<Targets, TargetsMetadata, File>(fetch_type,
                                                                              &self.http_client,
                                                                              "",
                                                                              &self.root,
                                                                              meta.length,
                                                                              hash_data,
                                                                              &mut out)?;

        // TODO ? check downloaded version matches what was in the snapshot.json

        match self.targets {
            Some(ref t) if t.version > targets.version => {
                match out_path {
                    Some(out_path) => fs::remove_file(out_path)?,
                    None => (),
                };

                return Err(Error::VersionDecrease(Role::Targets));
            }
            Some(ref t) if t.version == targets.version => return Ok(()),
            _ => self.targets = Some(targets),
        }

        match out_path {
            Some(out_path) => {
                let current_path = self.local_path
                    .join("metadata")
                    .join("current")
                    .join("targets.json");

                if current_path.exists() {
                    fs::rename(current_path.clone(),
                               self.local_path
                                   .join("metadata")
                                   .join("archive")
                                   .join("targets.json"))?;
                };

                fs::rename(out_path, current_path)?
            }
            None => (),
        };

        Ok(())
    }

    fn get_metadata<R: RoleType, M: Metadata<R>, W: Write>(fetch_type: &FetchType,
                                                           http_client: &Client,
                                                           root: &RootMetadata,
                                                           mut out: &mut Option<W>)
                                                           -> Result<M, Error> {
        Self::get_meta_prefix(fetch_type, http_client, "", root, None, None, &mut out)
    }

    fn get_meta_num<R: RoleType, M: Metadata<R>, W: Write>(fetch_type: &FetchType,
                                                           http_client: &Client,
                                                           num: i32,
                                                           root: &RootMetadata,
                                                           mut out: &mut Option<W>)
                                                           -> Result<M, Error> {
        // TODO this should check that the metadata version == num
        Self::get_meta_prefix(fetch_type,
                              http_client,
                              &format!("{}.", num),
                              root,
                              None,
                              None,
                              &mut out)
    }

    fn get_meta_prefix<R: RoleType, M: Metadata<R>, W: Write>(fetch_type: &FetchType,
                                                              http_client: &Client,
                                                              prefix: &str,
                                                              root: &RootMetadata,
                                                              size: Option<i64>,
                                                              hash_data: Option<(&HashType,
                                                                                 &[u8])>,
                                                              mut out: &mut Option<W>)
                                                              -> Result<M, Error> {

        debug!("Loading metadata from {:?}", fetch_type);

        let buf: Vec<u8> = match fetch_type {
            &FetchType::Cache(ref local_path) => {
                let path = local_path.join("metadata")
                    .join("current")
                    .join(format!("{}{}.json", prefix, R::role()));
                info!("Reading metadata from local path: {:?}", path);

                let mut file = File::open(path.clone()).map_err(|e| Error::from_io(e, &path))?;
                let mut buf = Vec::new();

                match (size, hash_data) {
                    (None, None) => file.read_to_end(&mut buf).map(|_| ())?,
                    _ => Self::read_and_verify(&mut file, &mut Some(&mut buf), size, hash_data)?,
                };

                buf
            }
            &FetchType::File(ref path) => {
                let path = path.join(format!("{}{}.json", prefix, R::role()));
                info!("Reading metadata from path: {:?}", path);

                let mut file = File::open(path.clone()).map_err(|e| Error::from_io(e, &path))?;
                let mut buf = Vec::new();

                match (size, hash_data) {
                    (None, None) => file.read_to_end(&mut buf).map(|_| ())?,
                    _ => Self::read_and_verify(&mut file, &mut Some(&mut buf), size, hash_data)?,
                };

                buf
            }
            &FetchType::Http(ref url) => {
                let url = url.join(&format!("{}{}.json", prefix, R::role()))?;
                let mut resp = http_client.get(url).send()?;
                let mut buf = Vec::new();

                match (size, hash_data) {
                    (None, None) => resp.read_to_end(&mut buf).map(|_| ())?,
                    _ => Self::read_and_verify(&mut resp, &mut Some(&mut buf), size, hash_data)?,
                };

                buf
            }
        };

        let signed = json::from_slice(&buf)?;
        let safe_bytes = Self::verify_meta::<R>(signed, root)?;
        let meta: M = json::from_slice(&safe_bytes)?;

        // TODO this will be a problem with updating root metadata and this function probably
        // needs an arg like `allow_expired`.
        if meta.expires() <= &UTC::now() {
            return Err(Error::ExpiredMetadata(R::role()));
        }

        match out {
            &mut Some(ref mut out) => out.write_all(&buf)?,
            &mut None => (),
        };

        Ok(meta)
    }

    fn unverified_read_root(fetch_type: &FetchType,
                            http_client: &Client)
                            -> Result<RootMetadata, Error> {
        let buf: Vec<u8> = match fetch_type {
            &FetchType::Cache(ref local_path) => {
                let path = local_path.join("metadata")
                    .join("current")
                    .join("root.json");
                let mut file = File::open(path.clone()).map_err(|e| Error::from_io(e, &path))?;
                let mut buf = Vec::new();
                file.read_to_end(&mut buf).map(|_| ())?;
                buf
            }
            &FetchType::File(ref path) => {
                let path = path.join("root.json");
                let mut file = File::open(path.clone()).map_err(|e| Error::from_io(e, &path))?;
                let mut buf = Vec::new();
                file.read_to_end(&mut buf).map(|_| ())?;
                buf
            }
            &FetchType::Http(ref url) => {
                let url = url.join("root.json")?;
                let mut resp = http_client.get(url).send()?;
                let mut buf = Vec::new();
                resp.read_to_end(&mut buf).map(|_| ())?;
                buf
            }
        };

        let signed: SignedMetadata<Root> = json::from_slice(&buf)?;
        let root_str = signed.signed.to_string();
        Ok(json::from_str(&root_str)?)
    }

    /// Read the root.json metadata and replace keys for the root role with the keys that are given
    /// as arguments to this function. This initial read is unverified in any way.
    fn read_root_with_keys(fetch_type: &FetchType,
                           http_client: &Client,
                           root_keys: &[Key])
                           -> Result<RootMetadata, Error> {
        let buf: Vec<u8> = match fetch_type {
            &FetchType::Cache(ref local_path) => {
                let path = local_path.join("metadata")
                    .join("archive")
                    .join("1.root.json");

                debug!("Reading root.json from path: {:?}", path);

                let mut file = File::open(path.clone()).map_err(|e| Error::from_io(e, &path))?;
                let mut buf = Vec::new();
                file.read_to_end(&mut buf).map(|_| ())?;
                buf
            }
            &FetchType::File(ref path) => {
                let path = path.join("1.root.json");
                let mut file = File::open(path.clone()).map_err(|e| Error::from_io(e, &path))?;
                let mut buf = Vec::new();
                file.read_to_end(&mut buf).map(|_| ())?;
                buf
            }
            &FetchType::Http(ref url) => {
                let url = url.join("1.root.json")?;
                let mut resp = http_client.get(url).send()?;
                let mut buf = Vec::new();
                resp.read_to_end(&mut buf).map(|_| ())?;
                buf
            }
        };

        // TODO once serialize is implemented for all the types, don't use this
        // json manipulation mess here
        let mut signed = json::from_slice::<SignedMetadata<Root>>(&buf)?;
        if let json::Value::Object(ref mut object) = signed.signed {
            if let Some(&mut json::Value::Object(ref mut roles)) = object.get_mut("roles") {
                if let Some(&mut json::Value::Object(ref mut root)) = roles.get_mut("root") {
                    if let Some(&mut json::Value::Array(ref mut key_ids)) = root.get_mut("keyids") {
                        key_ids.clear();
                        key_ids.extend(root_keys.iter().map(|k| json!(k.value.key_id().0)));
                    }
                }
            }
        }

        Ok(json::from_value(signed.signed)?)
    }

    fn verify_meta<R: RoleType>(signed: SignedMetadata<R>,
                                root: &RootMetadata)
                                -> Result<Vec<u8>, Error> {
        let bytes =
            cjson::canonicalize(signed.signed).map_err(|err| Error::CanonicalJsonError(err))?;

        let role = root.role_definition::<R>();

        let unique_count = signed.signatures
            .iter()
            .map(|s| &s.key_id)
            .collect::<HashSet<&KeyId>>()
            .len();

        if signed.signatures.len() != unique_count {
            return Err(Error::NonUniqueSignatures);
        }

        let keys = role.key_ids
            .iter()
            .map(|id| (id, root.keys.get(id)))
            .fold(HashMap::new(), |mut m, (id, k)| {
                if let Some(key) = k {
                    m.insert(id, key);
                } else {
                    debug!("Unknown key ID: {:?}", id);
                }
                m
            });

        if role.threshold <= 0 {
            return Err(Error::VerificationFailure("Threshold not >= 1".into()));
        }

        let mut valid_sigs = 0;
        for sig in signed.signatures.iter() {
            if let Some(key) = keys.get(&sig.key_id) {
                debug!("Verifying role {:?} with key ID {:?}",
                       R::role(),
                       sig.key_id);

                match key.verify(&sig.method, &bytes, &sig.sig) {
                    Ok(()) => {
                        debug!("Good signature from key ID {:?}", sig.key_id);
                        valid_sigs += 1;
                    }
                    Err(e) => warn!("Failed to verify with key ID {:?}: {:?}", &sig.key_id, e),
                }
                if valid_sigs == role.threshold {
                    return Ok(bytes);
                }
            }
        }

        info!("Threshold not met: {}/{}", valid_sigs, role.threshold);
        return Err(Error::UnmetThreshold(R::role()));
    }

    /// Lists all targets that are currently available. If a target is missing, it means the
    /// metadata chain that leads to it cannot be verified, and the target is therefore untrusted.
    // TODO stronger return type
    pub fn list_targets(&self) -> Vec<String> {
        match self.targets {
            Some(ref targets) => {
                let mut res = targets.targets.keys().cloned().collect::<Vec<String>>();
                res.sort();
                res
            }
            None => Vec::new(),
        }
    }

    /// Reads a target from local storage or fetches it from a remote repository. Verifies the
    /// target. Fails if the target is missing, or if the metadata chain that leads to it cannot
    /// be verified.
    // TODO ? stronger input type
    pub fn fetch_target(&self, target: &str) -> Result<PathBuf, Error> {
        let target_meta = match self.targets {
            Some(ref targets) => {
                targets.targets
                    .get(target)
                    .ok_or_else(|| Error::UnknownTarget)?
            }
            None => return Err(Error::MissingMetadata(Role::Targets)),
        };

        let (hash_alg, expected_hash): (&HashType, HashValue) = HashType::preferences().iter()
            .fold(None, |res, pref| {
                res.or_else(|| if let Some(hash) = target_meta.hashes.get(&pref) {
                    Some((pref, hash.clone()))
                } else {
                    None
                })
            })
            .ok_or_else(|| Error::NoSupportedHashAlgorithms)?;

        // TODO correctly split path
        let path = self.local_path.join("targets").join(util::url_path_to_os_path(target)?);
        info!("reading target from local path: {:?}", path);

        if path.exists() {
            let mut file = File::open(path.clone()).map_err(|e| Error::from_io(e, &path))?;
            Self::read_and_verify(&mut file,
                                  &mut None::<&mut File>,
                                  Some(target_meta.length),
                                  Some((&hash_alg, &expected_hash.0)))?;
            let _ = file.seek(SeekFrom::Start(0))?;
            return Ok(path);
        } else {
            let (out, out_path) = self.temp_file()?;

            match self.url.scheme() {
                "file" => {
                    let mut url = self.url.clone();
                    {
                        url.path_segments_mut()
                            .map_err(|_| Error::Generic("Path could not be mutated".to_string()))?
                            .extend(util::url_path_to_os_path_components(target)?);
                    }

                    let path = util::url_path_to_os_path(url.path())?;
                    let mut file = File::open(path.clone()).map_err(|e| Error::from_io(e, &path))?;

                    match Self::read_and_verify(&mut file,
                                                &mut Some(out),
                                                Some(target_meta.length),
                                                Some((&hash_alg, &expected_hash.0))) {
                        Ok(()) => {
                            // TODO ensure intermediate directories exist
                            let mut storage_path = self.local_path.join("targets");
                            storage_path.extend(util::url_path_to_os_path_components(target)?);

                            {
                                let parent = storage_path.parent()
                                    .ok_or_else(|| Error::Generic("Path had no parent".to_string()))?;

                                DirBuilder::new()
                                    .recursive(true)
                                    .create(parent)?;
                            }

                            fs::rename(out_path, storage_path.clone())?;
                            Ok(storage_path)
                        }
                        Err(e) => {
                            match fs::remove_file(out_path.clone()) {
                                Ok(_) => Err(e),
                                Err(e) => {
                                    warn!("Error removing temp file {:?}: {}", out_path, e);
                                    Err(Error::from(e))
                                }
                            }
                        }
                    }
                }
                "http" | "https" => {
                    let url = util::url_to_hyper_url(&self.url.join(target)?)?;
                    let mut resp = self.http_client.get(url).send()?;

                    match Self::read_and_verify(&mut resp,
                                                &mut Some(out),
                                                Some(target_meta.length),
                                                Some((&hash_alg, &expected_hash.0))) {
                        Ok(()) => {
                            // TODO this isn't windows friendly
                            // TODO ensure intermediate directories exist
                            let mut storage_path = self.local_path.join("targets");
                            storage_path.extend(util::url_path_to_os_path_components(target)?);

                            {
                                let parent = storage_path.parent()
                                    .ok_or_else(|| Error::Generic("Path had no parent".to_string()))?;

                                DirBuilder::new()
                                    .recursive(true)
                                    .create(parent)?;
                            }

                            fs::rename(out_path, storage_path.clone())?;

                            Ok(storage_path)
                        }
                        Err(e) => {
                            match fs::remove_file(out_path.clone()) {
                                Ok(_) => Err(e),
                                Err(e) => {
                                    warn!("Error removing temp file {:?}: {}", out_path, e);
                                    Err(Error::from(e))
                                }
                            }
                        }
                    }
                }
                x => Err(Error::Generic(format!("Unsupported URL scheme: {}", x))),
            }
        }
    }

    fn read_and_verify<R: Read, W: Write>(input: &mut R,
                                          output: &mut Option<W>,
                                          size: Option<i64>,
                                          hash_data: Option<(&HashType, &[u8])>)
                                          -> Result<(), Error> {
        let mut context = match hash_data {
            Some((&HashType::Sha512, _)) => Some(digest::Context::new(&SHA512)),
            Some((&HashType::Sha256, _)) => Some(digest::Context::new(&SHA256)),
            Some((&HashType::Unsupported(_), _)) => return Err(Error::NoSupportedHashAlgorithms),
            _ => None,
        };

        let mut buf = [0; 1024];
        let mut bytes_left = size;

        loop {
            match input.read(&mut buf) {
                Ok(read_bytes) => {
                    if read_bytes == 0 {
                        break;
                    }

                    match output {
                        &mut Some(ref mut output) => output.write_all(&buf[0..read_bytes])?,
                        &mut None => (),
                    };

                    match context {
                        Some(ref mut c) => c.update(&buf[0..read_bytes]),
                        None => (),
                    };

                    match bytes_left {
                        Some(ref mut bytes_left) => {
                            *bytes_left -= read_bytes as i64;
                            if *bytes_left == 0 {
                                break;
                            } else if *bytes_left < 0 {
                                return Err(Error::OversizedTarget);
                            }
                        }
                        None => (),
                    };
                }
                e @ Err(_) => e.map(|_| ())?,
            }
        }

        let generated_hash = context.map(|c| c.finish());

        match (generated_hash, hash_data) {
            (Some(generated_hash), Some((_, expected_hash))) if generated_hash.as_ref() !=
                                                                expected_hash => {
                Err(Error::TargetHashMismatch)
            }
            // this should never happen, so err if it does for safety
            (Some(_), None) => {
                let msg = "Hash calculated when no expected hash supplied";
                error!("Programming error. Please report this as a bug: {}", msg);
                Err(Error::VerificationFailure(msg.to_string()))
            }
            // this should never happen, so err if it does for safety
            (None, Some(_)) => {
                let msg = "No hash calculated when expected hash supplied";
                error!("Programming error. Please report this as a bug: {}", msg);
                Err(Error::VerificationFailure(msg.to_string()))
            }
            (Some(_), Some(_)) |
            (None, None) => Ok(()),
        }
    }
}


/// The configuration used to initialize a `Tuf` struct.
pub struct Config {
    url: Url,
    local_path: PathBuf,
    http_client: Client,
    // TODO add `init: bool` to specify whether or not to create dir structure
}

impl Config {
    /// Create a new builder with the default configurations where applicable.
    pub fn build() -> ConfigBuilder {
        ConfigBuilder::new()
    }
}


/// Helper that constructs `Config`s and verifies the options.
pub struct ConfigBuilder {
    url: Option<Url>,
    local_path: Option<PathBuf>,
    http_client: Option<Client>,
}

impl ConfigBuilder {
    /// Create a new builder with the default configurations where applicable.
    pub fn new() -> Self {
        ConfigBuilder {
            url: None,
            local_path: None,
            http_client: None,
        }
    }

    /// The remote URL of the TUF repo.
    pub fn url(mut self, url: Url) -> Self {
        self.url = Some(url);
        self
    }

    /// The local path for metadata and target storage.
    pub fn local_path(mut self, local_path: PathBuf) -> Self {
        self.local_path = Some(local_path);
        self
    }

    /// The `hyper::client::Client` to use. Default: `Client::new()`.
    pub fn http_client(mut self, client: Client) -> Self {
        self.http_client = Some(client);
        self
    }

    /// Verify the configuration.
    pub fn finish(self) -> Result<Config, Error> {
        let url = self.url
            .ok_or_else(|| Error::InvalidConfig("Repository URL was not set".to_string()))?;

        match url.scheme() {
            "file" | "http" | "https" => (),
            x => return Err(Error::InvalidConfig(format!("Unsupported URL scheme: {}", x))),
        };

        let local_path = self.local_path
            .ok_or_else(|| Error::InvalidConfig("Local path was not set".to_string()))?;

        Ok(Config {
            url: url,
            local_path: local_path,
            http_client: self.http_client.unwrap_or_else(|| Client::new()),
        })
    }
}


#[derive(Debug)]
enum FetchType {
    Cache(PathBuf),
    File(PathBuf),
    Http(HyperUrl),
}

impl FetchType {
    fn is_cache(&self) -> bool {
        match self {
            &FetchType::Cache(_) => true,
            _ => false,
        }
    }
}
