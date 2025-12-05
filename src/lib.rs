use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use k8s_openapi::api::core::v1::{Node, PersistentVolumeClaim, Pod};
use kube::{
    Client, ResourceExt,
    api::{Api, DeleteParams, ListParams},
};
use std::collections::HashSet;
use std::time::Duration;
use tracing::{error, info};

const SELECTED_NODE_ANNOTATION: &str = "volume.kubernetes.io/selected-node";
const PROVISIONER_ANNOTATION: &str = "volume.beta.kubernetes.io/storage-provisioner";

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct ReaperConfig {
    /// Storage class names to filter PVCs (comma-separated for multiple)
    #[arg(
        long,
        env = "STORAGE_CLASS_NAMES",
        value_delimiter = ',',
        default_value = "openebs-lvm"
    )]
    pub storage_classes: Vec<String>,

    /// Storage provisioner annotation value to filter PVCs
    #[arg(
        long,
        env = "STORAGE_PROVISIONER",
        default_value = "local.csi.openebs.io"
    )]
    pub storage_provisioner: String,

    /// Interval between reaping loops in seconds
    #[arg(long, env = "REAP_INTERVAL_SECS", default_value_t = 60)]
    pub reap_interval_secs: u64,

    /// Dry run mode - don't actually delete PVCs
    #[arg(long, env = "DRY_RUN", default_value_t = false)]
    pub dry_run: bool,

    /// Check for unschedulable pods with unschedulable PVCs
    #[arg(long, env = "CHECK_UNSCHEDULABLE_PODS", default_value_t = true)]
    pub check_unschedulable_pods: bool,

    /// How long a pod must be unschedulable before considering its PVC for deletion (seconds)
    #[arg(long, env = "UNSCHEDULABLE_POD_THRESHOLD_SECS", default_value_t = 120)]
    pub unschedulable_pod_threshold_secs: u64,
}

#[derive(Debug, Default)]
pub struct ReapResult {
    pub deleted_count: usize,
    pub skipped_count: usize,
}

#[derive(Debug)]
struct State {
    nodes: Vec<Node>,
    node_names: HashSet<String>,
    pods: Vec<Pod>,
    pvcs: Vec<PersistentVolumeClaim>,
    now: DateTime<Utc>,
}

impl State {
    async fn new(client: &Client) -> Result<Self> {
        let nodes = Api::<Node>::all(client.clone())
            .list(&ListParams::default())
            .await
            .context("Failed to list nodes")?
            .items;

        let pods = Api::<Pod>::all(client.clone())
            .list(&ListParams::default())
            .await
            .context("Failed to list pods")?
            .items;

        let pvcs = Api::<PersistentVolumeClaim>::all(client.clone())
            .list(&ListParams::default())
            .await
            .context("Failed to list PVCs")?
            .items;

        let node_names = nodes.iter().map(ResourceExt::name_any).collect();

        Ok(Self {
            nodes,
            node_names,
            pods,
            pvcs,
            now: Utc::now(),
        })
    }

    async fn reap(&self, client: &Client, config: &ReaperConfig) -> Result<ReapResult> {
        let mut result = ReapResult::default();

        for pvc in &self.pvcs {
            if !matches_storage_criteria(pvc, config) {
                continue;
            }

            let namespace = pvc.namespace().unwrap_or_default();
            let pvc_name = pvc.name_any();

            match self.deletion_reason(pvc, config) {
                Some(reason) => {
                    let description = reason.describe();
                    info!(
                        "PVC {}/{} scheduled for deletion: {}",
                        namespace, pvc_name, description
                    );

                    if let Err(e) = self
                        .perform_delete(client, config, &namespace, &pvc_name, &description)
                        .await
                    {
                        error!("Failed to delete PVC {}/{}: {:#}", namespace, pvc_name, e);
                    } else {
                        result.deleted_count += 1;
                    }
                }
                None => {
                    result.skipped_count += 1;
                }
            }
        }

        info!(
            "Reaping complete: deleted={}, skipped={}",
            result.deleted_count, result.skipped_count
        );

        Ok(result)
    }

    fn deletion_reason(
        &self,
        pvc: &PersistentVolumeClaim,
        config: &ReaperConfig,
    ) -> Option<DeleteReason> {
        let unschedulable_pod = self.unschedulable_pod(pvc)?;
        let pod_name = unschedulable_pod.name_any();

        if let Some(node) = self.missing_node(pvc) {
            return Some(DeleteReason::MissingNode {
                node,
                pod: pod_name,
            });
        }

        if config.check_unschedulable_pods {
            let threshold = Duration::from_secs(config.unschedulable_pod_threshold_secs);
            return pod_exceeds_unschedulable_thresh(unschedulable_pod, threshold, self.now)
                .then_some(DeleteReason::UnschedulableTooLong { pod: pod_name });
        }

        None
    }

    fn unschedulable_pod<'a>(&'a self, pvc: &'a PersistentVolumeClaim) -> Option<&'a Pod> {
        let pvc_name = pvc.name_any();

        let pod = self.pods.iter().find(|p| pod_uses_pvc(p, &pvc_name))?;

        if !pod_is_pending(pod) {
            return None;
        }

        if !pod_is_unschedulable(pod) {
            info!("Pod {} is pending but not unschedulable", pod.name_any());
            return None;
        }

        info!("Pod {} is unschedulable", pod.name_any());

        Some(pod)
    }

    fn missing_node(&self, pvc: &PersistentVolumeClaim) -> Option<String> {
        let node = get_selected_node(pvc)?;
        if self.node_names.contains(node) {
            None
        } else {
            Some(node.to_string())
        }
    }

    async fn perform_delete(
        &self,
        client: &Client,
        config: &ReaperConfig,
        namespace: &str,
        name: &str,
        reason: &str,
    ) -> Result<()> {
        if config.dry_run {
            info!(
                "[DRY RUN] Would delete PVC {}/{} ({})",
                namespace, name, reason
            );
            return Ok(());
        }

        delete_pvc(client, namespace, name).await
    }
}

#[derive(Debug)]
enum DeleteReason {
    MissingNode { node: String, pod: String },
    UnschedulableTooLong { pod: String },
}

impl DeleteReason {
    fn describe(&self) -> String {
        match self {
            Self::MissingNode { node, pod } => {
                format!("pod '{}' references missing node '{}'", pod, node)
            }
            Self::UnschedulableTooLong { pod } => {
                format!(
                    "pod '{}' has been pending past the configured threshold",
                    pod
                )
            }
        }
    }
}

/// Get annotation value from PVC metadata
fn get_pvc_annotation<'a>(pvc: &'a PersistentVolumeClaim, key: &str) -> Option<&'a str> {
    pvc.metadata
        .annotations
        .as_ref()?
        .get(key)
        .map(String::as_str)
}

/// Get the selected node annotation from a PVC
fn get_selected_node(pvc: &PersistentVolumeClaim) -> Option<&str> {
    get_pvc_annotation(pvc, SELECTED_NODE_ANNOTATION)
}

pub async fn reap(client: &Client, config: &ReaperConfig) -> Result<ReapResult> {
    let state = State::new(client).await?;
    info!(
        "Loaded state: {} nodes, {} pods, {} PVCs",
        state.nodes.len(),
        state.pods.len(),
        state.pvcs.len()
    );

    state.reap(client, config).await
}

pub fn matches_storage_criteria(pvc: &PersistentVolumeClaim, config: &ReaperConfig) -> bool {
    let storage_class = pvc
        .spec
        .as_ref()
        .and_then(|s| s.storage_class_name.as_ref());

    let provisioner = get_pvc_annotation(pvc, PROVISIONER_ANNOTATION);

    matches!(
        (storage_class, provisioner),
        (Some(sc), Some(prov)) if config.storage_classes.contains(sc) && prov == config.storage_provisioner
    )
}

fn pod_uses_pvc(pod: &Pod, pvc_name: &str) -> bool {
    get_pod_pvc_names(pod)
        .iter()
        .any(|claim_name| claim_name == pvc_name)
}

fn pod_is_pending(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|status| status.phase.as_deref())
        .is_some_and(|phase| phase == "Pending")
}

fn pod_exceeds_unschedulable_thresh(pod: &Pod, threshold: Duration, now: DateTime<Utc>) -> bool {
    if !pod_is_pending(pod) {
        return false;
    }

    pod.metadata.creation_timestamp.as_ref().is_some_and(|ts| {
        now.signed_duration_since(ts.0).num_seconds() >= threshold.as_secs() as i64
    })
}

fn pod_is_unschedulable(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .and_then(|conds| {
            conds.iter().find(|cond| {
                cond.type_ == "PodScheduled"
                    && cond.status == "False"
                    && cond.reason.as_deref() == Some("Unschedulable")
            })
        })
        .is_some()
}

fn get_pod_pvc_names(pod: &Pod) -> Vec<String> {
    pod.spec
        .as_ref()
        .and_then(|s| s.volumes.as_ref())
        .map(|volumes| {
            volumes
                .iter()
                .filter_map(|v| v.persistent_volume_claim.as_ref())
                .map(|pvc| pvc.claim_name.clone())
                .collect()
        })
        .unwrap_or_default()
}

pub async fn delete_pvc(client: &Client, namespace: &str, name: &str) -> Result<()> {
    Api::<PersistentVolumeClaim>::namespaced(client.clone(), namespace)
        .delete(name, &DeleteParams::default())
        .await
        .context("Failed to delete PVC")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::{
        api::core::v1::{PersistentVolumeClaimVolumeSource, PodCondition, PodStatus, Volume},
        apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time},
    };

    fn test_pvc(
        name: &str,
        storage_class: &str,
        provisioner: &str,
        selected_node: Option<&str>,
    ) -> PersistentVolumeClaim {
        let mut annotations = std::collections::BTreeMap::new();
        annotations.insert(PROVISIONER_ANNOTATION.to_string(), provisioner.to_string());
        if let Some(node) = selected_node {
            annotations.insert(SELECTED_NODE_ANNOTATION.to_string(), node.to_string());
        }

        PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some("default".to_string()),
                annotations: Some(annotations),
                ..Default::default()
            },
            spec: Some(k8s_openapi::api::core::v1::PersistentVolumeClaimSpec {
                storage_class_name: Some(storage_class.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn test_config() -> ReaperConfig {
        ReaperConfig {
            storage_classes: vec!["openebs-lvm".to_string()],
            storage_provisioner: "local.csi.openebs.io".to_string(),
            reap_interval_secs: 60,
            dry_run: false,
            check_unschedulable_pods: true,
            unschedulable_pod_threshold_secs: 300,
        }
    }

    fn state_with(node_names: &[&str], pods: Vec<Pod>, pvcs: Vec<PersistentVolumeClaim>) -> State {
        let nodes = node_names
            .iter()
            .map(|name| Node {
                metadata: ObjectMeta {
                    name: Some((*name).to_string()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect::<Vec<_>>();

        State {
            node_names: node_names.iter().map(|s| s.to_string()).collect(),
            nodes,
            pods,
            pvcs,
            now: Utc::now(),
        }
    }

    fn pod_with_pvc(
        pod_name: &str,
        pvc_name: &str,
        phase: &str,
        condition_reason: Option<&str>,
        creation_offset_secs: i64,
    ) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(pod_name.to_string()),
                namespace: Some("default".to_string()),
                creation_timestamp: Some(Time(
                    chrono::Utc::now() - chrono::Duration::seconds(creation_offset_secs),
                )),
                ..Default::default()
            },
            spec: Some(k8s_openapi::api::core::v1::PodSpec {
                volumes: Some(vec![Volume {
                    name: "data".to_string(),
                    persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                        claim_name: pvc_name.to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            status: Some(PodStatus {
                phase: Some(phase.to_string()),
                conditions: condition_reason.map(|reason| {
                    vec![PodCondition {
                        type_: "PodScheduled".to_string(),
                        status: "False".to_string(),
                        reason: Some(reason.to_string()),
                        ..Default::default()
                    }]
                }),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn test_matches_storage_criteria() {
        let pvc = test_pvc(
            "test",
            "openebs-lvm",
            "local.csi.openebs.io",
            Some("node-1"),
        );
        assert!(matches_storage_criteria(&pvc, &test_config()));
    }

    #[test]
    fn test_matches_storage_criteria_multiple_classes() {
        let pvc = test_pvc(
            "test",
            "local-storage",
            "local.csi.openebs.io",
            Some("node-1"),
        );
        let mut config = test_config();
        config.storage_classes = vec!["openebs-lvm".to_string(), "local-storage".to_string()];
        assert!(matches_storage_criteria(&pvc, &config));
    }

    #[test]
    fn test_pod_unschedulable_long_enough_with_unschedulable_condition() {
        let pod = pod_with_pvc("pending-pod", "test", "Pending", Some("Unschedulable"), 600);
        assert!(pod_exceeds_unschedulable_thresh(
            &pod,
            Duration::from_secs(300),
            Utc::now()
        ));
    }

    #[test]
    fn test_pod_unschedulable_not_long_enough() {
        let pod = pod_with_pvc("pending-pod", "test", "Pending", Some("Unschedulable"), 60);
        assert!(!pod_exceeds_unschedulable_thresh(
            &pod,
            Duration::from_secs(300),
            Utc::now()
        ));
    }

    #[test]
    fn test_deletion_reason_when_node_missing() {
        let pvc = test_pvc(
            "test",
            "openebs-lvm",
            "local.csi.openebs.io",
            Some("missing-node"),
        );
        let pod = pod_with_pvc("pending-pod", "test", "Pending", Some("Unschedulable"), 10);

        let state = state_with(&[], vec![pod], vec![pvc.clone()]);

        let reason = state
            .deletion_reason(&pvc, &test_config())
            .expect("expected deletion reason");

        match reason {
            DeleteReason::MissingNode { node, pod } => {
                assert_eq!(node, "missing-node");
                assert_eq!(pod, "pending-pod");
            }
            _ => panic!("expected missing node reason"),
        }
    }

    #[test]
    fn test_deletion_reason_when_unschedulable_too_long() {
        let pvc = test_pvc(
            "test",
            "openebs-lvm",
            "local.csi.openebs.io",
            Some("node-1"),
        );
        let pod = pod_with_pvc("pending-pod", "test", "Pending", Some("Unschedulable"), 601);

        let state = state_with(&["node-1"], vec![pod], vec![pvc.clone()]);

        let reason = state
            .deletion_reason(&pvc, &test_config())
            .expect("expected deletion reason");

        match reason {
            DeleteReason::UnschedulableTooLong { pod } => assert_eq!(pod, "pending-pod"),
            _ => panic!("expected pending too long reason"),
        }
    }

    #[test]
    fn test_deletion_reason_skips_when_pod_not_unschedulable() {
        let pvc = test_pvc(
            "test",
            "openebs-lvm",
            "local.csi.openebs.io",
            Some("node-1"),
        );
        let pod = pod_with_pvc("pending-pod", "test", "Pending", Some("OtherReason"), 600);

        let state = state_with(&["node-1"], vec![pod], vec![pvc.clone()]);

        assert!(state.deletion_reason(&pvc, &test_config()).is_none());
    }
}
