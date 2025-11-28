
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::info;
use schemars::JsonSchema;
use immortal_worker_lib::models::activity::{ActContext, ActivityError};

use crate::workflows::test::WorkflowPayload;
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

pub async fn hs_tariff_sync(_ctx: ActContext, _payload: WorkflowPayload) -> Result<ActivityOutput, ActivityError> {
    

    for x in 0..50 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        info!("Hello from the test activity! ({x})");
    }
    // let activity_data: &ActivityData = ctx.app_data().unwrap();
    Ok(ActivityOutput {
        data: "Hello from the test activity!".to_string(),
    })
}

