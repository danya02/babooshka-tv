use serde_json::json;

pub struct SmartPlug {
    pub base_url: String,
    pub entity_id: String,
    pub token: String,
}

impl SmartPlug {
    pub async fn set_state(&self, on: bool) -> Result<(), reqwest::Error> {
        let action = if on { "turn_on" } else { "turn_off" };
        let url = format!("{}/api/services/switch/{action}", self.base_url);
        tracing::info!("Sending smart plug request: {url}");
        reqwest::Client::new()
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&json!({"entity_id": self.entity_id}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}
