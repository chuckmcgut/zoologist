/// Errors produced while loading or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("failed to read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML or does not match the schema.
    #[error("failed to parse configuration: {0}")]
    Parse(#[from] toml::de::Error),
    /// The configuration parsed but one or more values are invalid.
    #[error("invalid configuration:\n  - {}", .0.join("\n  - "))]
    Invalid(Vec<String>),
}
