use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::info;
use schemars::JsonSchema;
use uuid::Uuid;
use crate::models::activity::{ActContext, ActivityError};
pub struct ActivityData {
    pub data: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct TempPayload {
    pub data: String
}


#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ActivityOutput {
    pub data: String,
}

pub async fn hs_tariff_sync(_ctx: ActContext, _payload: Uuid) -> Result<ActivityOutput, ActivityError> {
    

    for _x in 0..10 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        info!("Hello from the test activity!");
    }
    // let activity_data: &ActivityData = ctx.app_data().unwrap();
    Ok(ActivityOutput {
        data: "Hello from the test activity!".to_string(),
    })
}
