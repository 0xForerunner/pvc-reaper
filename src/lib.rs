use anyhow::{Context, Result};
use clap::Parser;
use k8s_openapi::api::core::v1::{Node, PersistentVolumeClaim, Pod};
use kube::{
    api::{Api, DeleteParams, ListParams},
    Client, ResourceExt,
};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tracing::{error, info, warn};

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

    /// Check for pending pods with unschedulable PVCs
    #[arg(long, env = "CHECK_PENDING_PODS", default_value_t = true)]
    pub check_pending_pods: bool,

    /// How long a pod must be pending before considering its PVC for deletion (seconds)
    #[arg(long, env = "PENDING_POD_THRESHOLD_SECS", default_value_t = 300)]
    pub pending_pod_threshold_secs: u64,
}

#[derive(Debug, Default)]
pub struct ReapResult {
    pub deleted_count: usize,
    pub skipped_count: usize,
}

#[derive(Debug, Default)]
struct NodeInventory {
    available: HashSet<String>,
    schedulable: HashSet<String>,
    unschedulable_reasons: HashMap<String, String>,
}

impl NodeInventory {
    fn available_nodes(&self) -> &HashSet<String> {
        &self.available
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
    info!("Starting reaping cycle");

    let node_inventory = get_node_inventory(client).await?;
    info!(
        "Found {} nodes ({} schedulable)",
        node_inventory.available.len(),
        node_inventory.schedulable.len()
    );

    let pvcs = Api::<PersistentVolumeClaim>::all(client.clone())
        .list(&ListParams::default())
        .await
        .context("Failed to list PVCs")?;

    let mut result = ReapResult::default();

    for pvc in pvcs.items {
        if should_delete_pvc(&pvc, node_inventory.available_nodes(), config) {
            let ns = pvc.namespace().unwrap_or_default();
            let name = pvc.name_any();
            let node = get_selected_node(&pvc).unwrap_or("unknown");

            info!(
                "PVC {}/{} references missing node '{}' - marking for deletion",
                ns, name, node
            );

            if config.dry_run {
                info!("[DRY RUN] Would delete PVC {}/{}", ns, name);
                result.deleted_count += 1;
            } else if let Err(e) = delete_pvc(client, &ns, &name).await {
                error!("Failed to delete PVC {}/{}: {:#}", ns, name, e);
            } else {
                info!("Successfully deleted PVC {}/{}", ns, name);
                result.deleted_count += 1;
            }
        } else if matches_storage_criteria(&pvc, config) {
            result.skipped_count += 1;
        }
    }

    info!(
        "Reaping complete: deleted={}, skipped={}",
        result.deleted_count, result.skipped_count
    );

    if config.check_pending_pods {
        if let Err(e) = check_pending_pods(client, config, &node_inventory).await {
            error!("Error checking pending pods: {:#}", e);
        }
    }

    Ok(result)
}

async fn get_node_inventory(client: &Client) -> Result<NodeInventory> {
    let nodes = Api::<Node>::all(client.clone())
        .list(&ListParams::default())
        .await
        .context("Failed to list nodes")?;

    let mut inventory = NodeInventory::default();
    for node in nodes.items {
        let name = node.name_any();
        inventory.available.insert(name.clone());

        if is_node_schedulable(&node) {
            inventory.schedulable.insert(name);
        } else if let Some(reason) = node_unavailable_reason(&node) {
            inventory.unschedulable_reasons.insert(name, reason);
        } else {
            inventory
                .unschedulable_reasons
                .insert(name, "Node schedulability unknown".to_string());
        }
    }

    Ok(inventory)
}

fn is_node_schedulable(node: &Node) -> bool {
    let cordoned = node
        .spec
        .as_ref()
        .and_then(|spec| spec.unschedulable)
        .unwrap_or(false);
    if cordoned {
        return false;
    }

    node.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .and_then(|conds| conds.iter().find(|c| c.type_ == "Ready"))
        .map(|cond| cond.status == "True")
        .unwrap_or(false)
}

fn node_unavailable_reason(node: &Node) -> Option<String> {
    let mut reasons = Vec::new();
    if node
        .spec
        .as_ref()
        .and_then(|spec| spec.unschedulable)
        .unwrap_or(false)
    {
        reasons.push("cordoned".to_string());
    }

    match node
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .and_then(|conds| conds.iter().find(|c| c.type_ == "Ready"))
    {
        Some(cond) if cond.status != "True" => {
            let cause = cond.reason.as_deref().unwrap_or("NotReady");
            reasons.push(format!("Ready={}", cause));
        }
        None => reasons.push("Ready condition missing".to_string()),
        _ => {}
    }

    if reasons.is_empty() {
        None
    } else {
        Some(reasons.join(", "))
    }
}

pub fn should_delete_pvc(
    pvc: &PersistentVolumeClaim,
    available_nodes: &HashSet<String>,
    config: &ReaperConfig,
) -> bool {
    if !matches_storage_criteria(pvc, config) {
        return false;
    }

    get_selected_node(pvc)
        .map(|node| !available_nodes.contains(node))
        .unwrap_or(false)
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

async fn check_pending_pods(
    client: &Client,
    config: &ReaperConfig,
    nodes: &NodeInventory,
) -> Result<()> {
    info!("Checking for pending pods with unschedulable PVCs");

    let pods = Api::<Pod>::all(client.clone())
        .list(&ListParams::default())
        .await
        .context("Failed to list pods")?;

    let threshold = Duration::from_secs(config.pending_pod_threshold_secs);

    for pod in pods.items {
        if !is_pod_pending_long_enough(&pod, threshold) || !is_pod_unschedulable(&pod) {
            continue;
        }

        let ns = pod.namespace().unwrap_or_default();
        let pod_name = pod.name_any();
        let pvc_api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &ns);

        for pvc_name in get_pod_pvc_names(&pod) {
            let pvc = match pvc_api.get(&pvc_name).await {
                Ok(pvc) => pvc,
                Err(_) => continue,
            };

            let delete_reason = if should_delete_pvc(&pvc, nodes.available_nodes(), config) {
                Some(PendingDeleteReason::MissingNode {
                    node: get_selected_node(&pvc).unwrap_or("unknown").to_string(),
                })
            } else {
                pvc_on_unavailable_node(&pvc, nodes)
                    .map(|(node, reason)| PendingDeleteReason::UnavailableNode { node, reason })
            };

            if let Some(reason) = delete_reason {
                let description = reason.describe();
                warn!(
                    "Pod {}/{} is pending and references PVC {} whose {}",
                    ns, pod_name, pvc_name, description
                );

                if config.dry_run {
                    info!(
                        "[DRY RUN] Would delete PVC {}/{} ({})",
                        ns, pvc_name, description
                    );
                } else if let Err(e) = delete_pvc(client, &ns, &pvc_name).await {
                    error!("Failed to delete PVC {}/{}: {:#}", ns, pvc_name, e);
                } else {
                    info!(
                        "Deleted PVC {}/{} due to pending pod ({})",
                        ns, pvc_name, description
                    );
                }
            }
        }
    }

    Ok(())
}

fn is_pod_pending_long_enough(pod: &Pod, threshold: Duration) -> bool {
    if !pod
        .status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .is_some_and(|phase| phase == "Pending")
    {
        return false;
    }

    info!("Found pending pod: {}", pod.name_any());

    if pod.metadata.creation_timestamp.as_ref().is_some_and(|ts| {
        chrono::Utc::now().signed_duration_since(ts.0).num_seconds() >= threshold.as_secs() as i64
    }) {
        info!("Pod {} has exceeded pending threshold", pod.name_any());
        return true;
    } else {
        info!("Pod {} has not exceeded pending threshold", pod.name_any());
        return false;
    }
}

fn is_pod_unschedulable(pod: &Pod) -> bool {
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

fn pvc_on_unavailable_node(
    pvc: &PersistentVolumeClaim,
    nodes: &NodeInventory,
) -> Option<(String, Option<String>)> {
    let node = get_selected_node(pvc)?;
    if !nodes.available.contains(node) || nodes.schedulable.contains(node) {
        return None;
    }

    Some((
        node.to_string(),
        nodes.unschedulable_reasons.get(node).cloned(),
    ))
}

enum PendingDeleteReason {
    MissingNode {
        node: String,
    },
    UnavailableNode {
        node: String,
        reason: Option<String>,
    },
}

impl PendingDeleteReason {
    fn describe(&self) -> String {
        match self {
            Self::MissingNode { node } => format!("selected node '{}' no longer exists", node),
            Self::UnavailableNode { node, reason } => match reason {
                Some(r) => format!("selected node '{}' is unavailable ({})", node, r),
                None => format!("selected node '{}' is unavailable", node),
            },
        }
    }
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
        api::core::v1::{NodeSpec, PodCondition, PodStatus},
        apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time},
    };
    use std::collections::BTreeMap;

    fn test_pvc(
        name: &str,
        storage_class: &str,
        provisioner: &str,
        selected_node: Option<&str>,
    ) -> PersistentVolumeClaim {
        let mut annotations = BTreeMap::new();
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
            check_pending_pods: false,
            pending_pod_threshold_secs: 300,
        }
    }

    fn nodes(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn pending_pod(condition_reason: Option<&str>, creation_offset_secs: i64) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some("pending-pod".to_string()),
                namespace: Some("default".to_string()),
                creation_timestamp: Some(Time(
                    chrono::Utc::now() - chrono::Duration::seconds(creation_offset_secs),
                )),
                ..Default::default()
            },
            status: Some(PodStatus {
                phase: Some("Pending".to_string()),
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
            ..Default::default()
        }
    }

    #[test]
    fn test_should_delete_pvc_with_missing_node() {
        let pvc = test_pvc(
            "test",
            "openebs-lvm",
            "local.csi.openebs.io",
            Some("missing-node"),
        );
        assert!(should_delete_pvc(
            &pvc,
            &nodes(&["node-1", "node-2"]),
            &test_config()
        ));
    }

    #[test]
    fn test_should_not_delete_pvc_with_existing_node() {
        let pvc = test_pvc(
            "test",
            "openebs-lvm",
            "local.csi.openebs.io",
            Some("node-1"),
        );
        assert!(!should_delete_pvc(
            &pvc,
            &nodes(&["node-1", "node-2"]),
            &test_config()
        ));
    }

    #[test]
    fn test_should_not_delete_pvc_without_selected_node() {
        let pvc = test_pvc("test", "openebs-lvm", "local.csi.openebs.io", None);
        assert!(!should_delete_pvc(
            &pvc,
            &nodes(&["node-1"]),
            &test_config()
        ));
    }

    #[test]
    fn test_should_not_delete_pvc_with_wrong_storage_class() {
        let pvc = test_pvc(
            "test",
            "other-class",
            "local.csi.openebs.io",
            Some("missing-node"),
        );
        assert!(!should_delete_pvc(
            &pvc,
            &nodes(&["node-1"]),
            &test_config()
        ));
    }

    #[test]
    fn test_should_not_delete_pvc_with_wrong_provisioner() {
        let pvc = test_pvc(
            "test",
            "openebs-lvm",
            "other.provisioner",
            Some("missing-node"),
        );
        assert!(!should_delete_pvc(
            &pvc,
            &nodes(&["node-1"]),
            &test_config()
        ));
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
    fn test_pod_pending_long_enough_without_start_time() {
        let pod = pending_pod(Some("Unschedulable"), 600);
        assert!(is_pod_pending_long_enough(&pod, Duration::from_secs(300)));
    }

    #[test]
    fn test_pod_pending_not_long_enough() {
        let pod = pending_pod(Some("Unschedulable"), 60);
        assert!(!is_pod_pending_long_enough(&pod, Duration::from_secs(300)));
    }

    #[test]
    fn test_pod_unschedulable_condition_detected() {
        let pod = pending_pod(Some("Unschedulable"), 600);
        assert!(is_pod_unschedulable(&pod));
    }

    #[test]
    fn test_pod_unschedulable_condition_not_detected_when_reason_differs() {
        let pod = pending_pod(Some("OtherReason"), 600);
        assert!(!is_pod_unschedulable(&pod));
    }

    #[test]
    fn test_pvc_on_unavailable_node_detected() {
        let pvc = test_pvc(
            "test",
            "openebs-lvm",
            "local.csi.openebs.io",
            Some("node-1"),
        );
        let mut inventory = NodeInventory::default();
        inventory.available.insert("node-1".to_string());
        inventory
            .unschedulable_reasons
            .insert("node-1".to_string(), "cordoned".to_string());

        let result = pvc_on_unavailable_node(&pvc, &inventory)
            .expect("expected node to be flagged as unavailable");
        assert_eq!(result.0, "node-1".to_string());
        assert_eq!(result.1.as_deref(), Some("cordoned"));
    }

    #[test]
    fn test_pvc_on_unavailable_node_ignored_when_schedulable() {
        let pvc = test_pvc(
            "test",
            "openebs-lvm",
            "local.csi.openebs.io",
            Some("node-1"),
        );
        let mut inventory = NodeInventory::default();
        inventory.available.insert("node-1".to_string());
        inventory.schedulable.insert("node-1".to_string());
        assert!(pvc_on_unavailable_node(&pvc, &inventory).is_none());
    }

    fn node_with_status(
        name: &str,
        ready_status: Option<&str>,
        ready_reason: Option<&str>,
        unschedulable: bool,
    ) -> Node {
        Node {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            spec: Some(NodeSpec {
                unschedulable: Some(unschedulable),
                ..Default::default()
            }),
            status: Some(k8s_openapi::api::core::v1::NodeStatus {
                conditions: ready_status.map(|status| {
                    vec![k8s_openapi::api::core::v1::NodeCondition {
                        type_: "Ready".to_string(),
                        status: status.to_string(),
                        reason: ready_reason.map(|r| r.to_string()),
                        ..Default::default()
                    }]
                }),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn test_node_schedulable_when_ready_true_and_not_cordoned() {
        let node = node_with_status("node-a", Some("True"), None, false);
        assert!(is_node_schedulable(&node));
        assert!(node_unavailable_reason(&node).is_none());
    }

    #[test]
    fn test_node_not_schedulable_when_cordoned() {
        let node = node_with_status("node-b", Some("True"), None, true);
        assert!(!is_node_schedulable(&node));
        assert_eq!(node_unavailable_reason(&node), Some("cordoned".to_string()));
    }

    #[test]
    fn test_node_not_schedulable_when_not_ready() {
        let node = node_with_status("node-c", Some("False"), Some("KubeletNotReady"), false);
        assert!(!is_node_schedulable(&node));
        assert_eq!(
            node_unavailable_reason(&node),
            Some("Ready=KubeletNotReady".to_string())
        );
    }

    #[test]
    fn test_node_not_schedulable_when_condition_missing() {
        let node = node_with_status("node-d", None, None, false);
        assert!(!is_node_schedulable(&node));
        assert_eq!(
            node_unavailable_reason(&node),
            Some("Ready condition missing".to_string())
        );
    }
}
