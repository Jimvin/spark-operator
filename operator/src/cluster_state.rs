use crate::config::SparkNodeUrl;
use reqwest::Error;
use serde::Deserialize;
use tracing::error;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SparkMasterState {
    pub url: String,
    pub workers: Vec<SparkWorkerState>,
    #[serde(rename(deserialize = "aliveworkers"))]
    pub alive_workers: usize,
    #[serde(rename(deserialize = "activeapps"))]
    pub active_apps: Vec<SparkApplication>,
    #[serde(rename(deserialize = "completedapps"))]
    pub completed_apps: Vec<SparkApplication>,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SparkWorkerState {
    pub id: String,
    pub host: String,
    pub port: usize,
    #[serde(rename(deserialize = "webuiaddress"))]
    pub web_ui_address: String,
    pub cores: usize,
    pub memory: usize,
    #[serde(rename(deserialize = "memoryused"))]
    pub memory_used: usize,
    #[serde(rename(deserialize = "memoryfree"))]
    pub memory_free: usize,
    pub state: String,
    #[serde(rename(deserialize = "lastheartbeat"))]
    pub last_heartbeat: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SparkApplication {
    pub id: String,
    #[serde(rename(deserialize = "starttime"))]
    pub start_time: usize,
    pub name: String,
    pub cores: usize,
    #[serde(rename(deserialize = "memoryperslave"))]
    pub memory_per_slave: usize,
    #[serde(rename(deserialize = "submitdate"))]
    pub submit_date: String,
    pub state: SparkApplicationState,
    pub duration: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub enum SparkApplicationState {
    FAILED,
    FINISHED,
    RUNNING,
    WAITING,
}

/// Request all master urls in the form of http(s)://master_url:master_port/json
///
/// # Arguments
///
/// * `master_urls` - List of all available master_urls or just the leader
///
async fn request_states(master_urls: Vec<SparkNodeUrl>) -> Result<Vec<SparkMasterState>, Error> {
    let mut master_states = vec![];
    for url in master_urls {
        let response = match reqwest::get(&url.to_string()).await {
            Ok(response) => response,
            Err(err) => {
                error!("Connection problem with spark url: {}", err);
                continue;
            }
        };

        let body = match response.text().await {
            Ok(body) => body,
            Err(err) => {
                error!("Could not read response from [{}]: {}", url, err);
                continue;
            }
        };

        let master_state = match serde_json::from_slice(body.as_ref()) {
            Ok(state) => state,
            Err(err) => {
                error!("Could not parse to MasterState: {}", err);
                continue;
            }
        };

        master_states.push(master_state);
    }
    Ok(master_states)
}

/// Checks if all active applications in the spark cluster are RUNNING and
/// collects these applications. Required for graceful restart / update.
///
/// # Arguments
///
/// * `master_urls` - List of all available master_urls or just the leader
///
pub async fn get_running_applications(
    master_urls: Vec<SparkNodeUrl>,
) -> Result<Vec<SparkApplication>, Error> {
    let mut running_applications = vec![];
    let master_states = request_states(master_urls).await?;

    for state in master_states {
        if !state.active_apps.is_empty() {
            for app in state.active_apps {
                if app.state == SparkApplicationState::RUNNING {
                    running_applications.push(app);
                }
            }
        }
    }

    Ok(running_applications)
}
