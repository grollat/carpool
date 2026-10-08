//! Every user-visible name lives here so the project can be renamed in one place.

pub const NAME: &str = "carpool";
pub const DISPLAY: &str = "Carpool";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// File a server ships (repo root, npm package root, or next to the entry script).
pub const MANIFEST_FILE: &str = "carpool.json";
/// Key a server may use inside its package.json instead of a separate file.
pub const PACKAGE_JSON_KEY: &str = "carpool";
/// Key inside `InitializeResult.capabilities.experimental` for in-band declaration.
pub const INBAND_KEY: &str = "carpool/manifest";

pub const HOME_ENV: &str = "CARPOOL_HOME";
pub const SOCKET_ENV: &str = "CARPOOL_SOCKET";

pub const SCHEMA_URL: &str = "https://raw.githubusercontent.com/grollat/carpool/main/schema/carpool.schema.json";
