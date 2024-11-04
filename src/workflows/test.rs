use std::ops::Deref;
use immortal::common::Payload;
use immortal_macros::wf;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

use crate::models::{
    activity::ActivityOptions,
    workflow::{WfContext, WfExitValue, WorkflowResult},
};

#[derive(Debug, Deserialize, Serialize)]
pub struct ActivityOutput {
    pub data: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct WfResult {
    result: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct WorkflowPayload {
    pub data: String,
}

#[wf]
pub async fn main(mut ctx: WfContext, arg1: Uuid) -> WorkflowResult<WfResult> {
    // tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    info!("Hello from the test workflow! {:#?}", arg1);

    match ctx
        .activity::<ActivityOutput>(ActivityOptions {
            activity_type: "get_avs_request_id".to_string(),
            input: Payload::new(&arg1),
            ..Default::default()
        })
        .await
    {
        Ok(activity_output) => {
            info!("Activity output: {:?}", activity_output);
        }
        Err(e) => {
            info!("Activity error: {:?}", e);
        }
    }
    // println!("Activity result3: {:?}", activity_result);

    Ok(WfExitValue::Normal(WfResult { result: 0 }))
}
