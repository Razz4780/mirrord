use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{BranchBaseConfig, ConnectionParamsConfig};
use crate::config::ConfigError;

/// The only [`ConnectionParamsVars::extra`] param an S3 branch takes: the name of the source
/// bucket, or rather where to read that name from on the target.
///
/// The bucket lives in `extra` instead of a fixed slot because none of the fixed slots (host,
/// port, user, password, database) mean anything for object storage. The operator's
/// `S3Param` mirrors this key on the CRD side and rejects any other.
///
/// [`ConnectionParamsVars::extra`]: super::ConnectionParamsVars::extra
pub const BUCKET_PARAM: &str = "bucket";

/// When configuring a branch for an object storage bucket, set `type` to `s3`.
///
/// The branch bucket is created and seeded by the provider's own API, so - unlike the engines
/// mirrord runs as a database server - an S3 branch has no pod in the cluster and takes no
/// `image`/`version`. It is otherwise a normal branch: the standard `id`, `ttl_secs`/`ttl_mins`,
/// `creation_timeout_secs` and `profile` options all apply.
///
/// The source bucket is located the same way every other branch locates its source: a
/// [`bucket`](#feature-db_branches-s3-source) param naming the env var on the target that holds
/// the bucket name. Once the branch bucket exists, the operator points that same variable at it,
/// so the app reads and writes the branch instead of the real bucket with no code change.
///
/// Example:
/// ```json
/// {
///   "type": "s3",
///   "provider": "AWS",
///   "source": {
///     "params": {
///       "bucket": "MY_BUCKET_ENV_VAR"
///     }
///   },
///   "copy": { "mode": "all", "objects": ["^fixtures/.*"] }
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3BranchConfig {
    #[serde(flatten)]
    pub base: BranchBaseConfig,

    /// #### feature.db_branches[].provider (type: s3) {#feature-db_branches-s3-provider}
    ///
    /// Cloud provider hosting the source bucket. Defaults to `AWS`.
    #[serde(default)]
    pub provider: S3Provider,

    /// #### feature.db_branches[].source (type: s3) {#feature-db_branches-s3-source}
    ///
    /// Where to read the source bucket's name from, in the same params shape the other engines
    /// use for their connection details - `type` picks how the variables are resolved on the
    /// target (`env` for the pod spec's own `env`, `env_from` for its `envFrom` sources), and is
    /// auto-detected when omitted.
    ///
    /// The single param an S3 branch takes is `bucket`:
    ///
    /// ```json
    /// { "source": { "params": { "bucket": "MY_BUCKET_ENV_VAR" } } }
    /// ```
    ///
    /// ```json
    /// { "source": { "type": "env_from", "params": { "bucket": "MY_BUCKET_ENV_VAR" } } }
    /// ```
    ///
    /// Its value is as flexible as any other engine's params: a Kubernetes Secret
    /// (`{ "secret": "my-secret", "key": "bucket" }`), a literal
    /// (`{ "variable": "MY_BUCKET_ENV_VAR", "value": "my-bucket" }`), or a regex extracting the
    /// name out of a larger variable (`{ "env_var_name": "S3_URI", "value_pattern": "..." }`).
    #[serde(alias = "connection")]
    pub source: ConnectionParamsConfig,

    /// #### feature.db_branches[].copy (type: s3) {#feature-db_branches-s3-copy}
    ///
    /// How the branch bucket is seeded from the source bucket.
    #[serde(default)]
    pub copy: S3BranchCopyConfig,
}

impl S3BranchConfig {
    pub fn verify(&self) -> Result<(), ConfigError> {
        self.base.verify()?;

        let params = &self.source.params;
        if !params.extra.contains_key(BUCKET_PARAM) {
            return Err(ConfigError::Conflict(format!(
                "`feature.db_branches[].source.params.{BUCKET_PARAM}` is required when \
                 `feature.db_branches[].type` is `s3`."
            )));
        }

        if let Some(unknown) = params.extra.keys().find(|key| *key != BUCKET_PARAM) {
            return Err(ConfigError::Conflict(format!(
                "`{unknown}` is not a valid `feature.db_branches[].source.params` entry for an \
                 s3 branch; the only accepted param is `{BUCKET_PARAM}`."
            )));
        }

        // The fixed slots describe a connection to a database server, which an S3 branch never
        // makes - catching them here beats letting the operator reject the branch later.
        let fixed_slot = [
            ("host", &params.host),
            ("port", &params.port),
            ("user", &params.user),
            ("password", &params.password),
            ("database", &params.database),
        ]
        .into_iter()
        .find_map(|(name, slot)| slot.is_some().then_some(name));
        if let Some(name) = fixed_slot {
            return Err(ConfigError::Conflict(format!(
                "`feature.db_branches[].source.params.{name}` is not a valid param for an s3 \
                 branch; the only accepted param is `{BUCKET_PARAM}`."
            )));
        }

        Ok(())
    }
}

/// Cloud provider hosting an S3 branch's source bucket.
///
/// Amazon S3 is the only provider mirrord branches today; the field exists so a branch config
/// written now keeps meaning the same thing once there are more.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum S3Provider {
    /// Amazon S3.
    #[default]
    #[serde(alias = "aws")]
    Aws,
}

/// Users can choose from the following copy modes to bootstrap their S3 branch bucket:
///
/// - Empty (default)
///
///   Creates the branch bucket with no objects in it. Useful for apps that write their own
///   fixtures, or when the source bucket is far too large to clone.
///
/// - All
///
///   Copies the objects of the source bucket into the branch bucket. Optional `objects` are
///   regular expressions matched against object keys, limiting the copy to the objects the app
///   actually reads; omitting them copies the whole bucket.
///
/// The copy runs in the provider's cloud - mirrord never streams the objects through the
/// cluster - so a wide `all` costs provider-side copy time rather than local bandwidth.
///
/// ```json
/// { "copy": { "mode": "all", "objects": ["^fixtures/.*", "\\.json$"] } }
/// ```
#[derive(Clone, Debug, Eq, PartialEq, JsonSchema, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum S3BranchCopyConfig {
    #[default]
    Empty,
    All {
        /// Regular expressions matched against the source objects' keys. An object is copied
        /// when it matches any of them. All objects are copied when this is not set.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        objects: Option<Vec<String>>,
    },
}
