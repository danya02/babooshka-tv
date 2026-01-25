#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LidState {
    pub lid_open: bool,
    pub changed_at: chrono::DateTime<chrono::Utc>,
}

/// Convenience export to get current datetime
pub fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}
