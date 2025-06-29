use ::immortal::common::Payloads;
use ::immortal::models::workflow::WfExitValue;
use immortal::immortal_client::ImmortalClient;
use immortal::workflow_result_version;
use immortal::{
    client_start_workflow_options_version, ClientStartWorkflowOptionsV1,
    ClientStartWorkflowOptionsVersion,
};
use serde::Deserialize;
use serde::Serialize;
use std::str::FromStr;

pub mod immortal {
    tonic::include_proto!("immortal");
}
use ::immortal::common;
use ::immortal::failure;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratePreview {
    pub file_storage_id: Uuid,
    pub file_mimetype: String,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = ImmortalClient::connect("http://localhost:10000").await?;
    client
        .start_workflow(ClientStartWorkflowOptionsVersion {
            version: Some(client_start_workflow_options_version::Version::V1(
                ClientStartWorkflowOptionsV1 {
                    workflow_type: "generate_preview_wf".to_string(),
                    task_queue: "attachamizer".to_string(),
                    input: Some(Payloads::new(vec![&GeneratePreview {
                        file_storage_id: Uuid::from_str("a402841b-dbf9-4a47-a237-560c5f072fa7")?,
                        file_mimetype: "video/mp4".to_string(),
                    }])),
                    ..Default::default()
                },
            )),
        })
        .await
        .unwrap();

    let result = client
        .execute_workflow(ClientStartWorkflowOptionsVersion {
            version: Some(client_start_workflow_options_version::Version::V1(
                ClientStartWorkflowOptionsV1 {
                    workflow_type: "update_schema".to_string(),
                    input: Some(Payloads::new(vec![&"".to_string()])),
                    task_queue: "builder.postgres".to_string(),
                    ..Default::default()
                },
            )),
        })
        .await
        .unwrap();
    if let Some(version) = &result.get_ref().version {
        match version {
            workflow_result_version::Version::V1(v1) => {
                if let Some(status) = &v1.status {
                    match status {
                        immortal::workflow_result_v1::Status::Completed(x) => {
                            if let Some(result) = &x.result {
                                let x = result.to::<WfExitValue<GeneratePreview>>()?;
                                match x {
                                    WfExitValue::Normal(value) => if value.file_mimetype.len() == 0 {},
                                    _ => {}
                                }
                            }
                        }
                        immortal::workflow_result_v1::Status::Failed(_failure) => {
                            //return Err(anyhow::anyhow!("Failed to update schema: {:?}", failure))
                        }
                        immortal::workflow_result_v1::Status::Cancelled(_cancelled) => {}
                    }
                }
            }
        }
    }
    // client
    //     .start_workflow(ClientStartWorkflowOptionsVersion {
    //         version: Some(client_start_workflow_options_version::Version::V1(
    //             ClientStartWorkflowOptionsV1 {
    //                 task_queue: "test".to_string(),
    //                 workflow_type: "new_validate_repair_wf".to_string(),
    //                 input: Some(Payloads::new(vec![&Uuid::new_v4()])),
    //                 workflow_version: "v1".to_string(),
    //                 ..Default::default()
    //             },
    //         )),
    //     })
    //     .await?;

    Ok(())
}
