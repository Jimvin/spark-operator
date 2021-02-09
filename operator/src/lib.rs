#![feature(backtrace)]
mod error;

use crate::error::Error;

use kube::Api;
use tracing::{debug, error, info, trace};

use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, Pod, PodSpec, Volume, VolumeMount,
};
use kube::api::{ListParams, Meta, ObjectMeta};
use serde_json::json;

use handlebars::Handlebars;
use stackable_operator::client::Client;
use stackable_operator::controller::{Controller, ControllerStrategy, ReconciliationState};
use stackable_operator::reconcile::{
    create_requeuing_reconcile_function_action, ReconcileFunctionAction, ReconcileResult,
    ReconciliationContext,
};
use stackable_operator::{create_config_map, create_tolerations, podutils};
use stackable_operator::{finalizer, metadata};
use stackable_spark_crd::{
    CrdError, SparkCluster, SparkClusterSpec, SparkClusterStatus, SparkNode, SparkNodeSelector,
    SparkNodeType,
};
use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::macros::support::Future;
use uuid::Uuid;

const FINALIZER_NAME: &str = "spark.stackable.de/cleanup";

const HASH: &str = "spark.stackable.de/hash";
const TYPE: &str = "spark.stackable.de/type";

const REQUEUE_SECONDS: u64 = 5;

type SparkReconcileResult = ReconcileResult<error::Error>;

struct SparkState {
    context: ReconciliationContext<SparkCluster>,
    spec: SparkClusterSpec,
    status: Option<SparkClusterStatus>,
    node_information: Option<NodeInformation>,
}

struct NodeInformation {
    // hash for selector -> corresponding pods
    pub master: HashMap<String, Vec<Pod>>,
    pub worker: HashMap<String, Vec<Pod>>,
    pub history_server: Option<HashMap<String, Vec<Pod>>>,
}

impl NodeInformation {
    pub fn get_pod_count(&self, node_type: SparkNodeType) -> usize {
        let mut num_pods = 0;
        match node_type {
            SparkNodeType::Master => self.count_pods(&self.master),
            SparkNodeType::Worker => self.count_pods(&self.worker),
            SparkNodeType::HistoryServer => {
                if self.history_server.is_some() {
                    self.count_pods(self.history_server.as_ref().unwrap())
                } else {
                    0
                }
            }
        }
    }

    fn count_pods(&self, hashed: &HashMap<String, Vec<Pod>>) -> usize {
        let mut num_pods: usize = 0;
        for (key, value) in hashed {
            num_pods += value.len();
        }
        num_pods
    }
}

impl SparkState {
    pub async fn read_existing_pod_information(&mut self) -> SparkReconcileResult {
        trace!(
            "Reading existing pod information for {}",
            self.context.log_name()
        );

        let mut existing_pods = self.context.list_pods().await?;
        trace!(
            "{}: Found [{}] pods",
            self.context.log_name(),
            existing_pods.len()
        );

        let hashed_selectors = self.spec.get_hashed_selectors();
        // for node information
        let mut master: HashMap<String, Vec<Pod>> = HashMap::new();
        let mut worker: HashMap<String, Vec<Pod>> = HashMap::new();
        let mut history_server: HashMap<String, Vec<Pod>> = HashMap::new();

        while let Some(pod) = existing_pods.pop() {
            // check if required labels exist and are correct
            if let Some(labels) = pod.metadata.labels.clone() {
                if let (Some(hash), Some(node_type)) = (labels.get(HASH), labels.get(TYPE)) {
                    // labels exist -> check if labels are correct
                    // node type
                    let spark_node_type = match SparkNodeType::from_str(node_type) {
                        Ok(nt) => nt,
                        Err(_) => {
                            error!("SparkCluster {}: Pod [{}] has an invalid type '{}' [{}], deleting it.", self.context.log_name(), Meta::name(&pod), TYPE, node_type);
                            self.context.client.delete(&pod).await?;
                            break;
                        }
                    };
                    // node type found
                    if let Some(hashed) = hashed_selectors.get(&spark_node_type) {
                        // hash not found
                        if !hashed.contains_key(hash) {
                            error!(
                                "SparkCluster {}: Pod [{}] has an outdated '{}' [{}], deleting it.",
                                self.context.log_name(),
                                Meta::name(&pod),
                                HASH,
                                hash
                            );
                            self.context.client.delete(&pod).await?;
                            break;
                        }
                    }

                    // sort into node information
                    match spark_node_type {
                        SparkNodeType::Master => {
                            self.sort_pod_info(pod, &mut master, hash.to_string())
                        }
                        SparkNodeType::Worker => {
                            self.sort_pod_info(pod, &mut worker, hash.to_string())
                        }
                        SparkNodeType::HistoryServer => {
                            self.sort_pod_info(pod, &mut history_server, hash.to_string())
                        }
                    };
                } else {
                    // all labels missing
                    error!("SparkCluster {}: Pod [{}] is missing one or more required '{:?}' labels, this is illegal, deleting it.",
                         self.context.log_name(), Meta::name(&pod), vec![HASH, TYPE]);
                    self.context.client.delete(&pod).await?;
                    break;
                }
            } else {
                // TODO: delete pod as well if no labels at all
            }
        }

        // set node information
        self.node_information = Some(NodeInformation {
            master,
            worker,
            history_server: Some(history_server),
        });

        info!(
            "SparkCluster {}: Status[current/spec] - {} [{}/{}] | {} [{}/{}] | {} [{}/{}]",
            self.context.log_name(),
            SparkNodeType::Master.as_str(),
            self.node_information
                .as_ref()
                .unwrap()
                .get_pod_count(SparkNodeType::Master),
            self.spec.master.get_instances(),
            SparkNodeType::Worker.as_str(),
            self.node_information
                .as_ref()
                .unwrap()
                .get_pod_count(SparkNodeType::Worker),
            self.spec.worker.get_instances(),
            SparkNodeType::HistoryServer.as_str(),
            self.node_information
                .as_ref()
                .unwrap()
                .get_pod_count(SparkNodeType::HistoryServer),
            self.spec.history_server.as_ref().unwrap().get_instances(),
        );

        Ok(ReconcileFunctionAction::Continue)
    }

    pub async fn reconcile_cluster(&self) -> SparkReconcileResult {
        trace!(
            "SparkCluster {}: Starting {} reconciliation",
            self.context.log_name(),
            SparkNodeType::Master.as_str()
        );

        if let Some(node_info) = &self.node_information {
            self.reconcile_node(&SparkNodeType::Master, &node_info.master)
                .await?;
            self.reconcile_node(&SparkNodeType::Worker, &node_info.worker)
                .await?;

            if let Some(history_server) = &node_info.history_server {
                self.reconcile_node(&SparkNodeType::HistoryServer, history_server)
                    .await?;
            } else {
                return Ok(ReconcileFunctionAction::Continue);
            }
        } else {
            return Ok(ReconcileFunctionAction::Done);
        }

        Ok(ReconcileFunctionAction::Continue)
    }

    async fn reconcile_node(
        &self,
        node_type: &SparkNodeType,
        nodes: &HashMap<String, Vec<Pod>>,
    ) -> SparkReconcileResult {
        // TODO: check if nodes terminated / check if nodes up and running
        //if !self.check_pods_ready(nodes) {
        //    return Ok(ReconcileFunctionAction::Requeue(Duration::from_secs(
        //        REQUEUE_SECONDS,
        //    )));
        //}
        // TODO: if no master available, reboot workers

        if let Some(hashed_pods) = self.spec.get_hashed_selectors().get(node_type) {
            for (hash, selector) in hashed_pods {
                let spec_pod_count = selector.instances;
                let mut current_count: usize = 0;
                // delete pod when 1) hash found but current pod count > spec pod count
                // create pod when 1) no hash found or 2) hash found but current pod count < spec pod count
                if let Some(pods) = nodes.get(hash) {
                    current_count = pods.len();
                    if current_count > spec_pod_count {
                        let pod = pods.get(0).unwrap();
                        self.context.client.delete(pod).await?;
                        info!(
                            "SparkCluster {}: deleting {} pod '{}'",
                            self.context.log_name(),
                            node_type.as_str(),
                            Meta::name(pod)
                        );
                    }
                }

                if current_count < spec_pod_count {
                    let pod = self.create_pod(hash, node_type).await?;
                    //let cm = self.create_config_maps(selector).await?;
                    info!(
                        "SparkCluster {}: creating {} pod '{}'",
                        self.context.log_name(),
                        node_type.as_str(),
                        Meta::name(&pod)
                    );
                }
            }
        }

        Ok(ReconcileFunctionAction::Continue)
    }

    fn check_pods_ready(&self, hashed_pods: &HashMap<String, Vec<Pod>>) -> bool {
        for pods in hashed_pods.values() {
            for pod in pods {
                // terminating
                if self.has_deletion_stamp(pod) {
                    return false;
                }
                if self.is_pod_running_and_ready(pod) {
                    return false;
                }
            }
        }
        return true;
    }

    fn has_deletion_stamp(&self, pod: &Pod) -> bool {
        if finalizer::has_deletion_stamp(pod) {
            info!(
                "SparkCluster {} is waiting for Pod [{}] to terminate",
                self.context.log_name(),
                Meta::name(pod)
            );
            return true;
        }
        return false;
    }

    fn is_pod_running_and_ready(&self, pod: &Pod) -> bool {
        if !podutils::is_pod_running_and_ready(pod) {
            info!(
                "SparkCluster {} is waiting for Pod [{}] to be running and ready",
                self.context.log_name(),
                Meta::name(pod)
            );
            return false;
        }
        return true;
    }

    async fn create_pod(&self, hash: &String, node_type: &SparkNodeType) -> Result<Pod, Error> {
        let pod = self.build_pod(hash, node_type)?;
        Ok(self.context.client.create(&pod).await?)
    }

    fn build_pod(&self, hash: &String, node_type: &SparkNodeType) -> Result<Pod, Error> {
        Ok(Pod {
            metadata: self.build_pod_metadata(node_type, hash)?,
            spec: Some(self.build_pod_spec(node_type, hash)),
            ..Pod::default()
        })
    }

    fn build_pod_metadata(
        &self,
        node_type: &SparkNodeType,
        hash: &String,
    ) -> Result<ObjectMeta, Error> {
        Ok(ObjectMeta {
            labels: Some(self.build_labels(node_type, hash).unwrap()),
            name: Some(self.create_pod_name(node_type, hash)),
            namespace: Some(self.context.namespace()),
            owner_references: Some(vec![metadata::object_to_owner_reference::<SparkCluster>(
                self.context.metadata(),
            )?]),
            ..ObjectMeta::default()
        })
    }

    fn build_pod_spec(&self, node_type: &SparkNodeType, hash: &String) -> PodSpec {
        let (containers, volumes) = self.build_containers(node_type, hash);

        PodSpec {
            //node_name: Some(self.get_pod_name(selector, false)),
            tolerations: Some(create_tolerations()),
            containers,
            volumes: Some(volumes),
            ..PodSpec::default()
        }
    }

    fn build_containers(
        &self,
        node_type: &SparkNodeType,
        hash: &String,
    ) -> (Vec<Container>, Vec<Volume>) {
        // TODO: get version from controller
        let version = "3.0.1".to_string();
        let image_name = format!(
            "stackable/spark:{}",
            serde_json::json!(version).as_str().unwrap()
        );

        let containers = vec![Container {
            image: Some(image_name),
            name: "spark".to_string(),
            // TODO: worker -> add master port
            command: Some(vec![node_type.get_command()]),
            volume_mounts: Some(vec![
                // One mount for the config directory, this will be relative to the extracted package
                VolumeMount {
                    mount_path: "conf".to_string(),
                    name: "config-volume".to_string(),
                    ..VolumeMount::default()
                },
                // We need a second mount for the data directory
                // because we need to write the myid file into the data directory
                VolumeMount {
                    mount_path: "/tmp/spark-events".to_string(), // TODO: get log dir from crd
                    name: "data-volume".to_string(),
                    ..VolumeMount::default()
                },
            ]),
            ..Container::default()
        }];

        let cm_name_prefix = format!("{}", self.create_config_map_name(node_type, hash));
        let volumes = vec![
            Volume {
                name: "config-volume".to_string(),
                config_map: Some(ConfigMapVolumeSource {
                    name: Some(format!("{}-config", cm_name_prefix)),
                    ..ConfigMapVolumeSource::default()
                }),
                ..Volume::default()
            },
            Volume {
                name: "data-volume".to_string(),
                config_map: Some(ConfigMapVolumeSource {
                    name: Some(format!("{}-data", cm_name_prefix)),
                    ..ConfigMapVolumeSource::default()
                }),
                ..Volume::default()
            },
        ];

        (containers, volumes)
    }
    //
    // async fn create_config_maps(&self, selector: &NodeSelector) -> Result<(), Error> {
    //     let mut options = HashMap::new();
    //     // TODO: use product-conf for validation
    //     options.insert("SPARK_NO_DAEMONIZE".to_string(), "true".to_string());
    //     options.insert(
    //         "SPARK_CONF_DIR".to_string(),
    //         "{{configroot}}/conf".to_string(),
    //     );
    //
    //     let mut handlebars = Handlebars::new();
    //     handlebars
    //         .register_template_string("conf", "{{#each options}}{{@key}}={{this}}\n{{/each}}")
    //         .expect("template should work");
    //
    //     let config = handlebars
    //         .render("conf", &json!({ "options": options }))
    //         .unwrap();
    //
    //     //let config = spark_env
    //     //    .iter()
    //     //    .map(|(key, value)| format!("{}={}\n", key.to_string(), value))
    //     //    .collect();
    //
    //     // Now we need to create two configmaps per server.
    //     // The names are "zk-<cluster name>-<node name>-config" and "zk-<cluster name>-<node name>-data"
    //     // One for the configuration directory...
    //     let mut data = BTreeMap::new();
    //     data.insert("spark-env.sh".to_string(), config);
    //
    //     let cm_name = format!("{}-cm", self.get_pod_name(selector, true));
    //     let cm = create_config_map(&self.context.resource, &cm_name, data)?;
    //     info!("{:?}", cm);
    //     self.context
    //         .client
    //         .apply_patch(&cm, serde_json::to_vec(&cm)?)
    //         .await?;
    //
    //     Ok(())
    // }

    fn build_labels(
        &self,
        node_type: &SparkNodeType,
        hash: &String,
    ) -> Result<BTreeMap<String, String>, error::Error> {
        let mut labels = BTreeMap::new();
        labels.insert(TYPE.to_string(), node_type.to_string());
        labels.insert(HASH.to_string(), hash.to_string());
        Ok(labels)
    }

    /// All pod names follow a simple pattern: <name of SparkCluster object>-<NodeType name>-<SelectorHash / UUID>
    fn create_pod_name(&self, node_type: &SparkNodeType, hash: &String) -> String {
        format!(
            "{}-{}-{}-{}",
            self.context.name(),
            node_type.as_str(),
            hash,
            Uuid::new_v4().as_fields().0.to_string(),
        )
    }

    /// All config map names follow a simple pattern: <name of SparkCluster object>-<NodeType name>-<SelectorHash>
    /// That means multiple pods of one selector share one and the same config map
    fn create_config_map_name(&self, node_type: &SparkNodeType, hash: &String) -> String {
        format!("{}-{}-{}", self.context.name(), node_type.as_str(), hash,)
    }

    /// sort valid pods into their corresponding hashed vector maps
    fn sort_pod_info(&self, pod: Pod, hashed_pods: &mut HashMap<String, Vec<Pod>>, hash: String) {
        if hashed_pods.contains_key(&hash) {
            hashed_pods.get_mut(hash.as_str()).unwrap().push(pod);
        } else {
            hashed_pods.insert(hash, vec![pod]);
        }
    }
}

impl ReconciliationState for SparkState {
    type Error = error::Error;

    fn reconcile(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<ReconcileFunctionAction, Self::Error>> + Send + '_>>
    {
        Box::pin(async move {
            self.read_existing_pod_information()
                .await?
                .then(self.reconcile_cluster())
                .await
        })
    }
}

#[derive(Debug)]
struct SparkStrategy {}

impl SparkStrategy {
    pub fn new() -> SparkStrategy {
        SparkStrategy {}
    }
}

impl ControllerStrategy for SparkStrategy {
    type Item = SparkCluster;
    type State = SparkState;

    fn finalizer_name(&self) -> String {
        return FINALIZER_NAME.to_string();
    }

    fn init_reconcile_state(&self, context: ReconciliationContext<Self::Item>) -> Self::State {
        SparkState {
            spec: context.resource.spec.clone(),
            status: context.resource.status.clone(),
            context,
            node_information: None,
        }
    }
}

/// This creates an instance of a [`Controller`] which waits for incoming events and reconciles them.
///
/// This is an async method and the returned future needs to be consumed to make progress.
pub async fn create_controller(client: Client) {
    let spark_api: Api<SparkCluster> = client.get_all_api();
    let pods_api: Api<Pod> = client.get_all_api();
    let config_maps_api: Api<ConfigMap> = client.get_all_api();

    let controller = Controller::new(spark_api)
        .owns(pods_api, ListParams::default())
        .owns(config_maps_api, ListParams::default());

    let strategy = SparkStrategy::new();

    controller.run(client, strategy).await;
}
