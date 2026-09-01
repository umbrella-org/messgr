use chrono::NaiveDate;

/// One monthly partition of `comms_request` or `comms_event`, as found by
/// `repo::list_partitions`. `month_start` is parsed from the partition's own
/// `<table>_<YYYY>_<MM>` name (T-009 decision 3), not from its `pg_class`
/// constraint bounds — the naming convention exists for exactly this.
#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub name: String,
    pub month_start: NaiveDate,
    pub tablespace: Option<String>,
}

/// What one `lifecycle::run` call actually did, returned to the CLI for
/// printing and to tests for assertions.
#[derive(Debug, Clone, Default)]
pub struct LifecycleReport {
    pub created: Vec<String>,
    pub moved: Vec<String>,
    pub dropped: Vec<String>,
    /// `true` when the tenant has no `tenant_config` row — the drop phase
    /// was skipped entirely rather than assuming a default retention
    /// (decision 7).
    pub retention_skipped: bool,
}
