use std::env;

use crate::profile::Profile;

#[derive(Debug, Clone)]
pub struct Config {
    pub control_database_url: String,
    pub database_max_connections: u32,
    pub profile: Profile,
}

impl Config {
    /// Loads from the process environment (via `.env`, same as the removed
    /// prototype's `main.rs` did with `dotenvy`). Panics with a clear message
    /// on a missing required variable — failing loudly at startup beats
    /// failing confusingly on first use.
    pub fn from_env() -> Self {
        dotenvy::dotenv().ok();

        let control_database_url =
            env::var("CONTROL_DATABASE_URL").expect("CONTROL_DATABASE_URL must be set");
        let database_max_connections = env::var("DATABASE_MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20);
        let profile = Profile::from_env();

        Self {
            control_database_url,
            database_max_connections,
            profile,
        }
    }
}
