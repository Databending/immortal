use std::ops::Deref;
use itertools::Itertools;
use immortal_macros::wf;
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

#[derive(Debug, Deserialize, Serialize)]
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
    info!("Hello from the test workflow! {:#?}", ctx.args);

    let payloads = ctx.args.deref().clone();

    // let (id, data): (Uuid, WorkflowPayload) = payloads.payloads.iter().map(|f| f.to()).collect_tuple().unwrap();
    // tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    // let activity_result: ActivityOutput = ctx
    //     .activity(ActivityOptions {
    //         activity_type: "get_avs_request_id".to_string(),
    //         input: payloads.payloads.get(0).unwrap().clone(),
    //         ..Default::default()
    //     })
    //     .await?;
    match ctx
        .activity::<ActivityOutput>(ActivityOptions {
            activity_type: "get_avs_request_id".to_string(),
            input: payloads.payloads.get(0).unwrap().clone(),
            ..Default::default()
        })
        .await
    {
        Ok(activity_output) => {
            println!("Activity output: {:?}", activity_output);
        }
        Err(e) => {
            println!("Activity error: {:?}", e);
        }
    }
    // println!("Activity result3: {:?}", activity_result);

    Ok(WfExitValue::Normal(WfResult { result: 0 }))
}
