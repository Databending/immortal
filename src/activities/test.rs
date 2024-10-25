use serde::{Deserialize, Serialize};
use immortal_macros::function_schema;
use crate::models::activity::{ActContext, ActivityError};
pub struct ActivityData {
    pub data: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TempPayload {
    pub data: String
}


#[derive(Debug, Deserialize, Serialize)]
pub struct ActivityOutput {
    pub data: String,
}

#[function_schema]
pub async fn hs_tariff_sync(ctx: ActContext, _payload: TempPayload) -> Result<ActivityOutput, ActivityError> {
    println!("Hello from the test activity!");
    // let activity_data: &ActivityData = ctx.app_data().unwrap();
    Ok(ActivityOutput {
        data: "Hello from the test activity!".to_string(),
    })
}
