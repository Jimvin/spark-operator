use snafu::Snafu;
use stackable_operator::kube::runtime::reflector::ObjectRef;
use stackable_operator::role_utils::RoleGroupRef;
use stackable_spark_crd::SparkCluster;
use std::str::FromStr;

#[derive(Snafu, Debug)]
#[allow(clippy::enum_variant_names)]
pub enum Error {
    #[snafu(display("object {} is missing metadata to build owner reference", obj_ref))]
    ObjectMissingMetadataForOwnerRef {
        source: stackable_operator::error::Error,
        obj_ref: ObjectRef<SparkCluster>,
    },
    #[snafu(display("object {} defines no version", obj_ref))]
    ObjectHasNoVersion { obj_ref: ObjectRef<SparkCluster> },
    #[snafu(display("{} has no server role", obj_ref))]
    MissingRoleGroup { obj_ref: RoleGroupRef<SparkCluster> },
    #[snafu(display("failed to calculate global service name for {}", obj_ref))]
    GlobalServiceNameNotFound { obj_ref: ObjectRef<SparkCluster> },
    #[snafu(display("failed to apply global Service for {}", sc))]
    ApplyRoleService {
        source: stackable_operator::error::Error,
        sc: ObjectRef<SparkCluster>,
    },
    #[snafu(display("failed to apply Service for {}", rolegroup))]
    ApplyRoleGroupService {
        source: stackable_operator::error::Error,
        rolegroup: RoleGroupRef<SparkCluster>,
    },
    #[snafu(display("failed to build ConfigMap for {}", rolegroup))]
    BuildRoleGroupConfig {
        source: stackable_operator::error::Error,
        rolegroup: RoleGroupRef<SparkCluster>,
    },
    #[snafu(display("failed to apply ConfigMap for {}", rolegroup))]
    ApplyRoleGroupConfig {
        source: stackable_operator::error::Error,
        rolegroup: RoleGroupRef<SparkCluster>,
    },
    #[snafu(display("failed to apply StatefulSet for {}", rolegroup))]
    ApplyRoleGroupStatefulSet {
        source: stackable_operator::error::Error,
        rolegroup: RoleGroupRef<SparkCluster>,
    },
    #[snafu(display("invalid product config for {}", sc))]
    InvalidProductConfig {
        source: stackable_operator::error::Error,
        sc: ObjectRef<SparkCluster>,
    },
    #[snafu(display("failed to serialize spark-defaults.conf for {}", rolegroup))]
    SerializeSparkDefaults {
        rolegroup: RoleGroupRef<SparkCluster>,
    },
    #[snafu(display("failed to serialize spark-env.sh for {}", rolegroup))]
    SerializeSparkEnv {
        rolegroup: RoleGroupRef<SparkCluster>,
    },
    #[snafu(display("a master role group named 'default' is expected in the cluster defintion"))]
    MasterRoleGroupDefaultExpected,
    #[snafu(display("Invalid port configuration for rolegroup {}", rolegroup_ref))]
    InvalidPort {
        source: <i32 as FromStr>::Err,
        rolegroup_ref: RoleGroupRef<SparkCluster>,
    },
}
