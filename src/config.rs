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
    /// on a missing required variable, or on one present but unparseable —
    /// failing loudly beats failing confusingly on first use.
    pub fn from_env() -> Self {
        dotenvy::dotenv().ok();

        let control_database_url =
            env::var("CONTROL_DATABASE_URL").expect("CONTROL_DATABASE_URL must be set");
        let database_max_connections = match env::var("DATABASE_MAX_CONNECTIONS") {
            Ok(v) => v.parse::<u32>().unwrap_or_else(|_| {
                panic!("DATABASE_MAX_CONNECTIONS must be a valid u32, got {v:?}")
            }),
            Err(_) => 20,
        };
        let profile = Profile::from_env();

        Self {
            control_database_url,
            database_max_connections,
            profile,
        }
    }
}
