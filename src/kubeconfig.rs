//! Reading and (carefully) rewriting `kubeconfig` files.
//!
//! Two rules govern this module:
//!
//! 1. **Never lose data.** A kubeconfig is a user's most annoying file to
//!    rebuild. We parse it into a typed view for reading, but every write goes
//!    through the untyped YAML tree so that fields we do not model — exec
//!    plugins, extensions, proxy settings — survive untouched.
//! 2. **Never leave a half-written file.** Writes land in a sibling temp file
//!    and are renamed into place, so an interrupted write cannot truncate a
//!    working config.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Failures that can occur while locating, parsing, or rewriting a kubeconfig.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no kubeconfig found (looked at {})", format_paths(.searched))]
    NotFound { searched: Vec<PathBuf> },

    #[error("could not determine your home directory; set KUBECONFIG explicitly")]
    NoHomeDirectory,

    #[error("failed to read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is not valid kubeconfig YAML")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_yaml_ng::Error,
    },

    #[error("no context named {name:?}")]
    UnknownContext { name: String },
}

fn format_paths(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "<nothing>".to_owned();
    }
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A `contexts[].context` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ContextSpec {
    /// Name of the entry in `clusters[]` this context points at.
    #[serde(default)]
    pub cluster: String,
    #[serde(default)]
    pub user: String,
    /// The namespace a bare `kubectl` call would act on.
    #[serde(default)]
    pub namespace: Option<String>,
}

/// A named entry in `contexts[]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NamedContext {
    pub name: String,
    pub context: ContextSpec,
}

/// A `clusters[].cluster` entry. Only the fields we actually display are typed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct ClusterSpec {
    #[serde(default)]
    pub server: Option<String>,
}

/// A named entry in `clusters[]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NamedCluster {
    pub name: String,
    #[serde(default)]
    pub cluster: ClusterSpec,
}

/// The subset of a kubeconfig this tool reads.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct KubeConfig {
    #[serde(default)]
    pub current_context: Option<String>,
    #[serde(default)]
    pub contexts: Vec<NamedContext>,
    #[serde(default)]
    pub clusters: Vec<NamedCluster>,

    /// The files this view was assembled from, in precedence order. Empty when
    /// the config was parsed from a string rather than loaded from disk.
    #[serde(skip)]
    pub sources: Vec<PathBuf>,
}

/// A context joined with the cluster it references — what the UI actually wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedContext {
    pub name: String,
    pub cluster_name: String,
    pub user: String,
    pub namespace: Option<String>,
    pub server: Option<String>,
    pub is_current: bool,
}

impl ResolvedContext {
    /// The namespace a command should default to when the user names none.
    #[must_use]
    pub fn effective_namespace(&self) -> &str {
        self.namespace.as_deref().unwrap_or("default")
    }
}

impl KubeConfig {
    /// Parse a single kubeconfig document from YAML.
    pub fn parse(yaml: &str) -> Result<Self, serde_yaml_ng::Error> {
        // An empty file is a legitimate (if useless) kubeconfig; serde would
        // otherwise reject the resulting null document.
        if yaml.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_yaml_ng::from_str(yaml)
    }

    /// Load and merge every file on the kubeconfig search path.
    ///
    /// Mirrors `kubectl`'s precedence: earlier files win, so a name defined in
    /// two files resolves to the first one, and `current-context` comes from the
    /// first file that sets it.
    pub fn load() -> Result<Self, Error> {
        Self::load_from(&search_paths()?)
    }

    /// Load and merge an explicit list of files. Missing files are skipped so a
    /// stale entry on `KUBECONFIG` does not break the tool.
    pub fn load_from(paths: &[PathBuf]) -> Result<Self, Error> {
        let mut merged = Self::default();
        let mut seen_contexts = HashSet::new();
        let mut seen_clusters = HashSet::new();
        let mut found_any = false;

        for path in paths {
            if !path.exists() {
                continue;
            }
            found_any = true;

            let raw = fs::read_to_string(path).map_err(|source| Error::Read {
                path: path.clone(),
                source,
            })?;
            let parsed = Self::parse(&raw).map_err(|source| Error::Parse {
                path: path.clone(),
                source,
            })?;

            if merged.current_context.is_none() {
                merged.current_context = parsed.current_context;
            }
            for context in parsed.contexts {
                if seen_contexts.insert(context.name.clone()) {
                    merged.contexts.push(context);
                }
            }
            for cluster in parsed.clusters {
                if seen_clusters.insert(cluster.name.clone()) {
                    merged.clusters.push(cluster);
                }
            }
            merged.sources.push(path.clone());
        }

        if !found_any {
            return Err(Error::NotFound {
                searched: paths.to_vec(),
            });
        }
        Ok(merged)
    }

    /// The file a write should target: the first file that actually exists.
    #[must_use]
    pub fn primary_source(&self) -> Option<&Path> {
        self.sources.first().map(PathBuf::as_path)
    }

    /// Every context, joined against `clusters[]`, in file order.
    #[must_use]
    pub fn resolved_contexts(&self) -> Vec<ResolvedContext> {
        self.contexts
            .iter()
            .map(|entry| ResolvedContext {
                name: entry.name.clone(),
                cluster_name: entry.context.cluster.clone(),
                user: entry.context.user.clone(),
                namespace: entry.context.namespace.clone(),
                server: self
                    .clusters
                    .iter()
                    .find(|c| c.name == entry.context.cluster)
                    .and_then(|c| c.cluster.server.clone()),
                is_current: self.current_context.as_deref() == Some(entry.name.as_str()),
            })
            .collect()
    }

    /// Look up a single context by name.
    #[must_use]
    pub fn resolved_context(&self, name: &str) -> Option<ResolvedContext> {
        self.resolved_contexts()
            .into_iter()
            .find(|c| c.name == name)
    }

    /// The currently selected context, if `current-context` names a real one.
    #[must_use]
    pub fn current(&self) -> Option<ResolvedContext> {
        let name = self.current_context.as_deref()?;
        self.resolved_context(name)
    }

    /// Whether a context with this exact name exists.
    #[must_use]
    pub fn has_context(&self, name: &str) -> bool {
        self.contexts.iter().any(|c| c.name == name)
    }
}

/// The kubeconfig files to consult, in precedence order.
///
/// Honours `KUBECONFIG` (OS-specific path separator, empty entries ignored) and
/// falls back to `~/.kube/config`.
pub fn search_paths() -> Result<Vec<PathBuf>, Error> {
    if let Some(value) = std::env::var_os("KUBECONFIG")
        && !value.is_empty()
    {
        let paths = split_path_list(&value);
        if !paths.is_empty() {
            return Ok(paths);
        }
    }

    let home = directories::UserDirs::new().ok_or(Error::NoHomeDirectory)?;
    Ok(vec![home.home_dir().join(".kube").join("config")])
}

fn split_path_list(value: &OsString) -> Vec<PathBuf> {
    std::env::split_paths(value)
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// Point `current-context` at `name`, preserving every other byte of meaning in
/// the file.
///
/// Returns the name of the context that was previously selected, if any.
pub fn set_current_context(path: &Path, name: &str) -> Result<Option<String>, Error> {
    let raw = fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })?;

    let mut doc: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&raw).map_err(|source| Error::Parse {
            path: path.to_path_buf(),
            source,
        })?;

    // A brand new or empty file deserialises to null; make it a mapping so the
    // insert below has somewhere to go.
    if doc.is_null() {
        doc = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
    }

    let key = serde_yaml_ng::Value::String("current-context".to_owned());
    let Some(mapping) = doc.as_mapping_mut() else {
        return Err(Error::Parse {
            path: path.to_path_buf(),
            source: serde_yaml_ng::Error::custom("kubeconfig root is not a mapping"),
        });
    };

    let previous = mapping
        .get(&key)
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    mapping.insert(key, serde_yaml_ng::Value::String(name.to_owned()));

    let rendered = serde_yaml_ng::to_string(&doc).map_err(|source| Error::Parse {
        path: path.to_path_buf(),
        source,
    })?;

    write_atomically(path, &rendered)?;
    Ok(previous)
}

/// Write `contents` to `path` via a sibling temp file and a rename, so readers
/// never observe a partially written config.
fn write_atomically(path: &Path, contents: &str) -> Result<(), Error> {
    let temp = temp_sibling(path);

    fs::write(&temp, contents).map_err(|source| Error::Write {
        path: temp.clone(),
        source,
    })?;

    // Carry the original file's permissions across; a fresh temp file would
    // otherwise widen a deliberately restrictive mode.
    if let Ok(metadata) = fs::metadata(path) {
        let _ = fs::set_permissions(&temp, metadata.permissions());
    }

    fs::rename(&temp, path).map_err(|source| {
        let _ = fs::remove_file(&temp);
        Error::Write {
            path: path.to_path_buf(),
            source,
        }
    })
}

fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".eks-tmp.{}", std::process::id()));
    path.with_file_name(name)
}

// `serde_yaml_ng::Error` has no public constructor, so borrow serde's.
use serde::de::Error as _;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const SAMPLE: &str = r"
apiVersion: v1
kind: Config
current-context: arn:aws:eks:us-east-1:111122223333:cluster/prod
clusters:
  - name: arn:aws:eks:us-east-1:111122223333:cluster/prod
    cluster:
      server: https://AAAA.gr7.us-east-1.eks.amazonaws.com
      certificate-authority-data: Zm9v
  - name: arn:aws:eks:us-west-2:111122223333:cluster/staging
    cluster:
      server: https://BBBB.gr7.us-west-2.eks.amazonaws.com
contexts:
  - name: arn:aws:eks:us-east-1:111122223333:cluster/prod
    context:
      cluster: arn:aws:eks:us-east-1:111122223333:cluster/prod
      user: arn:aws:eks:us-east-1:111122223333:cluster/prod
  - name: staging
    context:
      cluster: arn:aws:eks:us-west-2:111122223333:cluster/staging
      user: arn:aws:eks:us-west-2:111122223333:cluster/staging
      namespace: payments
users:
  - name: arn:aws:eks:us-east-1:111122223333:cluster/prod
    user:
      exec:
        apiVersion: client.authentication.k8s.io/v1beta1
        command: aws
";

    fn write_config(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn parses_contexts_and_clusters() {
        let config = KubeConfig::parse(SAMPLE).unwrap();
        assert_eq!(config.contexts.len(), 2);
        assert_eq!(config.clusters.len(), 2);
        assert_eq!(
            config.current_context.as_deref(),
            Some("arn:aws:eks:us-east-1:111122223333:cluster/prod")
        );
    }

    #[test]
    fn parses_empty_document_as_empty_config() {
        let config = KubeConfig::parse("   \n").unwrap();
        assert!(config.contexts.is_empty());
        assert!(config.current_context.is_none());
    }

    #[test]
    fn resolves_context_against_cluster_server() {
        let config = KubeConfig::parse(SAMPLE).unwrap();
        let staging = config.resolved_context("staging").unwrap();

        assert_eq!(
            staging.server.as_deref(),
            Some("https://BBBB.gr7.us-west-2.eks.amazonaws.com")
        );
        assert_eq!(staging.namespace.as_deref(), Some("payments"));
        assert!(!staging.is_current);
    }

    #[test]
    fn namespace_defaults_to_default_when_unset() {
        let config = KubeConfig::parse(SAMPLE).unwrap();
        let prod = config.current().unwrap();

        assert!(prod.namespace.is_none());
        assert_eq!(prod.effective_namespace(), "default");
    }

    #[test]
    fn current_is_none_when_current_context_dangles() {
        let config = KubeConfig::parse("current-context: ghost\ncontexts: []\n").unwrap();
        assert!(config.current().is_none());
    }

    #[test]
    fn merge_prefers_the_earlier_file_on_name_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let first = write_config(
            dir.path(),
            "first.yaml",
            "current-context: alpha\ncontexts:\n  - name: alpha\n    context:\n      cluster: c1\n      user: u1\n",
        );
        let second = write_config(
            dir.path(),
            "second.yaml",
            "current-context: beta\ncontexts:\n  - name: alpha\n    context:\n      cluster: OVERWRITTEN\n      user: u9\n  - name: beta\n    context:\n      cluster: c2\n      user: u2\n",
        );

        let merged = KubeConfig::load_from(&[first.clone(), second]).unwrap();

        assert_eq!(merged.current_context.as_deref(), Some("alpha"));
        assert_eq!(merged.contexts.len(), 2);
        assert_eq!(merged.resolved_context("alpha").unwrap().cluster_name, "c1");
        assert_eq!(merged.primary_source(), Some(first.as_path()));
    }

    #[test]
    fn merge_skips_missing_files_but_errors_when_none_exist() {
        let dir = tempfile::tempdir().unwrap();
        let real = write_config(dir.path(), "real.yaml", SAMPLE);
        let ghost = dir.path().join("ghost.yaml");

        let merged = KubeConfig::load_from(&[ghost.clone(), real]).unwrap();
        assert_eq!(merged.contexts.len(), 2);

        let err = KubeConfig::load_from(&[ghost]).unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }));
    }

    #[test]
    fn set_current_context_preserves_unmodelled_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "config", SAMPLE);

        let previous = set_current_context(&path, "staging").unwrap();
        assert_eq!(
            previous.as_deref(),
            Some("arn:aws:eks:us-east-1:111122223333:cluster/prod")
        );

        let rewritten = fs::read_to_string(&path).unwrap();
        // The exec credential plugin is not part of our typed model; losing it
        // would break authentication entirely.
        assert!(rewritten.contains("client.authentication.k8s.io/v1beta1"));
        assert!(rewritten.contains("certificate-authority-data"));

        let reloaded = KubeConfig::load_from(&[path]).unwrap();
        assert_eq!(reloaded.current_context.as_deref(), Some("staging"));
        assert_eq!(reloaded.contexts.len(), 2);
    }

    #[test]
    fn set_current_context_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "config", SAMPLE);

        set_current_context(&path, "staging").unwrap();

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("eks-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[test]
    fn split_path_list_ignores_empty_entries() {
        let joined = std::env::join_paths(["/a", "/b"]).unwrap();
        assert_eq!(
            split_path_list(&joined),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
        assert!(split_path_list(&OsString::from("")).is_empty());
    }
}
