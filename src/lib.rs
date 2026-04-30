use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use k8s_openapi::api::core::v1::{Node, PersistentVolume, PersistentVolumeClaim, Pod};
use kube::{
    Client, ResourceExt,
    api::{Api, DeleteParams, ListParams, Patch, PatchParams},
    core::{ApiResource, DynamicObject, GroupVersionKind},
};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tracing::{error, info, warn};

const SELECTED_NODE_ANNOTATION: &str = "volume.kubernetes.io/selected-node";
const PROVISIONER_ANNOTATION: &str = "volume.beta.kubernetes.io/storage-provisioner";
const PV_PROVISIONER_ANNOTATION: &str = "pv.kubernetes.io/provisioned-by";
const EXTERNAL_PROVISIONER_FINALIZER: &str = "external-provisioner.volume.kubernetes.io/finalizer";
const OPENEBS_NODE_AFFINITY_KEY: &str = "openebs.io/nodename";
const HOSTNAME_NODE_AFFINITY_KEY: &str = "kubernetes.io/hostname";

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

    /// Dry run mode - don't actually modify Kubernetes resources
    #[arg(long, env = "DRY_RUN", default_value_t = false)]
    pub dry_run: bool,

    /// Delete PVCs selected for cleanup
    #[arg(
        long,
        env = "CLEANUP_PVCS",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub cleanup_pvcs: bool,

    /// Check for unschedulable pods with unschedulable PVCs
    #[arg(
        long,
        env = "CHECK_UNSCHEDULABLE_PODS",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub check_unschedulable_pods: bool,

    /// How long a pod must be unschedulable before considering its PVC for deletion (seconds)
    #[arg(long, env = "UNSCHEDULABLE_POD_THRESHOLD_SECS", default_value_t = 120)]
    pub unschedulable_pod_threshold_secs: u64,

    /// Remove finalizers from matching PVs whose local-storage node is gone
    #[arg(
        long,
        env = "CLEANUP_PVS",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub cleanup_pvs: bool,

    /// How long a PV must be released/deleting before finalizers are removed (seconds)
    #[arg(long, env = "PV_GRACE_PERIOD_SECS", default_value_t = 600)]
    pub pv_grace_period_secs: u64,

    /// Delete matching OpenEBS LVMVolume objects and remove finalizers for PV cleanup
    #[arg(
        long,
        env = "CLEANUP_OPENEBS_LVMVOLUMES",
        default_value_t = true,
        action = clap::ArgAction::Set,
    )]
    pub cleanup_openebs_lvmvolumes: bool,

    /// Namespace where OpenEBS LVMVolume objects are stored
    #[arg(long, env = "OPENEBS_NAMESPACE", default_value = "openebs")]
    pub openebs_namespace: String,
}

#[derive(Debug, Default)]
pub struct ReapResult {
    pub deleted_count: usize,
    pub finalized_pv_count: usize,
    pub finalized_lvmvolume_count: usize,
    pub stale_pv_candidate_count: usize,
    pub stale_lvmvolume_candidate_count: usize,
    pub skipped_count: usize,
}

#[derive(Debug)]
struct State {
    nodes: Vec<Node>,
    node_names: HashSet<String>,
    pods: Vec<Pod>,
    pvcs: Vec<PersistentVolumeClaim>,
    pvc_refs: HashMap<String, Option<String>>,
    pvs: Vec<PersistentVolume>,
    lvmvolumes: HashMap<String, DynamicObject>,
    now: DateTime<Utc>,
}

impl State {
    async fn new(client: &Client, config: &ReaperConfig) -> Result<Self> {
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

        let pvs = Api::<PersistentVolume>::all(client.clone())
            .list(&ListParams::default())
            .await
            .context("Failed to list PVs")?
            .items;

        let lvmvolumes = match list_lvmvolumes(client, &config.openebs_namespace).await {
            Ok(items) => items
                .into_iter()
                .map(|item| (item.name_any(), item))
                .collect::<HashMap<_, _>>(),
            Err(e) => {
                warn!("Failed to list OpenEBS LVMVolumes: {:#}", e);
                HashMap::new()
            }
        };

        let node_names = nodes.iter().map(ResourceExt::name_any).collect();
        let pvc_refs = pvcs
            .iter()
            .filter_map(|pvc| {
                pvc.namespace()
                    .map(|ns| (pvc_key(&ns, &pvc.name_any()), pvc.metadata.uid.clone()))
            })
            .collect();

        Ok(Self {
            nodes,
            node_names,
            pods,
            pvcs,
            pvc_refs,
            pvs,
            lvmvolumes,
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
                        "PVC {}/{} is a cleanup candidate: {}; cleanup_pvcs={}, dry_run={}",
                        namespace, pvc_name, description, config.cleanup_pvcs, config.dry_run
                    );

                    match self
                        .perform_delete(client, config, &namespace, &pvc_name, &description)
                        .await
                    {
                        Ok(true) => result.deleted_count += 1,
                        Ok(false) => {}
                        Err(e) => {
                            error!("Failed to delete PVC {}/{}: {:#}", namespace, pvc_name, e)
                        }
                    }
                }
                None => {
                    result.skipped_count += 1;
                }
            }
        }

        self.cleanup_pvs(client, config, &mut result).await;

        info!(
            "Reaping complete: deleted_pvcs={}, stale_pv_candidates={}, stale_lvmvolume_candidates={}, finalized_pvs={}, finalized_lvmvolumes={}, skipped={}",
            result.deleted_count,
            result.stale_pv_candidate_count,
            result.stale_lvmvolume_candidate_count,
            result.finalized_pv_count,
            result.finalized_lvmvolume_count,
            result.skipped_count
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
    ) -> Result<bool> {
        if !config.cleanup_pvcs {
            info!(
                "Would delete PVC {}/{} if CLEANUP_PVCS=true ({})",
                namespace, name, reason
            );
            return Ok(false);
        }

        if config.dry_run {
            info!(
                "[DRY RUN] Would delete PVC {}/{} ({})",
                namespace, name, reason
            );
            return Ok(false);
        }

        delete_pvc(client, namespace, name).await?;
        Ok(true)
    }

    async fn cleanup_pvs(&self, client: &Client, config: &ReaperConfig, result: &mut ReapResult) {
        for pv in &self.pvs {
            let Some(reason) = self.stale_pv_reason(pv, config) else {
                continue;
            };

            let pv_name = pv.name_any();
            let mut pv_actions = Vec::new();
            if has_external_provisioner_finalizer(pv) {
                pv_actions.push("clear PV finalizers");
            }
            if pv_actions.is_empty() {
                pv_actions.push("none");
            }

            result.stale_pv_candidate_count += 1;
            info!(
                "PV {} is a stale local-storage cleanup candidate: {}; actions={}; cleanup_pvs={}, dry_run={}",
                pv_name,
                reason.describe(),
                pv_actions.join(","),
                config.cleanup_pvs,
                config.dry_run
            );

            if let Some(lvmvolume) = self.lvmvolumes.get(&pv_name)
                && (has_finalizers(lvmvolume) || lvmvolume.metadata.deletion_timestamp.is_none()) {
                    let lvmvolume_state = lvmvolume
                        .data
                        .get("status")
                        .and_then(|status| status.get("state"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("<unknown>");
                    let finalizers = lvmvolume
                        .metadata
                        .finalizers
                        .as_ref()
                        .filter(|values| !values.is_empty())
                        .map(|values| values.join(","))
                        .unwrap_or_else(|| "<none>".to_string());
                    let mut lvmvolume_actions = Vec::new();
                    if has_finalizers(lvmvolume) {
                        lvmvolume_actions.push("clear LVMVolume finalizers");
                    }
                    if lvmvolume.metadata.deletion_timestamp.is_none() {
                        lvmvolume_actions.push("delete LVMVolume");
                    }
                    if lvmvolume_actions.is_empty() {
                        lvmvolume_actions.push("none");
                    }

                    result.stale_lvmvolume_candidate_count += 1;
                    info!(
                        "OpenEBS LVMVolume {}/{} matches stale PV {}: owner_node={}, state={}, deleting={}, finalizers={}; actions={}; cleanup_openebs_lvmvolumes={}",
                        config.openebs_namespace,
                        pv_name,
                        pv_name,
                        lvmvolume_owner_node(lvmvolume).unwrap_or("<unknown>"),
                        lvmvolume_state,
                        lvmvolume.metadata.deletion_timestamp.is_some(),
                        finalizers,
                        lvmvolume_actions.join(","),
                        config.cleanup_openebs_lvmvolumes
                    );

                    if config.cleanup_pvs && config.cleanup_openebs_lvmvolumes {
                        match cleanup_lvmvolume(client, config, &pv_name, lvmvolume).await {
                            Ok(true) => result.finalized_lvmvolume_count += 1,
                            Ok(false) => {}
                            Err(e) => {
                                error!(
                                    "Failed to clean up OpenEBS LVMVolume {}/{}: {:#}",
                                    config.openebs_namespace, pv_name, e
                                );
                                continue;
                            }
                        }
                    } else {
                        info!(
                            "Would clean up OpenEBS LVMVolume {}/{} if CLEANUP_PVS=true and CLEANUP_OPENEBS_LVMVOLUMES=true",
                            config.openebs_namespace, pv_name
                        );
                    }
                }

            if has_external_provisioner_finalizer(pv) {
                if config.cleanup_pvs {
                    match cleanup_pv(client, config, &pv_name, pv).await {
                        Ok(true) => result.finalized_pv_count += 1,
                        Ok(false) => {}
                        Err(e) => {
                            error!("Failed to clean up stale PV {}: {:#}", pv_name, e);
                            continue;
                        }
                    }
                } else {
                    info!(
                        "Would clear PV finalizers for {} if CLEANUP_PVS=true",
                        pv_name
                    );
                }
            }
        }
    }

    fn stale_pv_reason(
        &self,
        pv: &PersistentVolume,
        config: &ReaperConfig,
    ) -> Option<StalePvReason> {
        if !matches_pv_storage_criteria(pv, config) {
            return None;
        }

        if pv_phase(pv) != Some("Released") {
            return None;
        }

        if reclaim_policy(pv) != Some("Delete") {
            return None;
        }

        if self.claim_ref_points_to_live_pvc(pv) {
            return None;
        }

        if !pv_is_past_grace_period(
            pv,
            Duration::from_secs(config.pv_grace_period_secs),
            self.now,
        ) {
            return None;
        }

        let node = pv_node_name(pv)?.to_string();
        if self.node_names.contains(&node) {
            return None;
        }

        if let Some(lvmvolume) = self.lvmvolumes.get(&pv.name_any())
            && let Some(owner_node) = lvmvolume_owner_node(lvmvolume)
                && self.node_names.contains(owner_node) {
                    return None;
                }

        Some(StalePvReason { node })
    }

    fn claim_ref_points_to_live_pvc(&self, pv: &PersistentVolume) -> bool {
        let Some((key, claim_uid)) = claim_ref_key_and_uid(pv) else {
            return false;
        };

        let Some(live_uid) = self.pvc_refs.get(&key) else {
            return false;
        };

        match (claim_uid, live_uid.as_deref()) {
            (Some(claim_uid), Some(live_uid)) => claim_uid == live_uid,
            _ => true,
        }
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

#[derive(Debug)]
struct StalePvReason {
    node: String,
}

impl StalePvReason {
    fn describe(&self) -> String {
        format!("released Delete PV points at missing node '{}'", self.node)
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
    let state = State::new(client, config).await?;
    info!(
        "Loaded state: {} nodes, {} pods, {} PVCs, {} PVs, {} OpenEBS LVMVolumes",
        state.nodes.len(),
        state.pods.len(),
        state.pvcs.len(),
        state.pvs.len(),
        state.lvmvolumes.len()
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

pub fn matches_pv_storage_criteria(pv: &PersistentVolume, config: &ReaperConfig) -> bool {
    let storage_class = pv
        .spec
        .as_ref()
        .and_then(|spec| spec.storage_class_name.as_ref());

    let provisioner = pv
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(PV_PROVISIONER_ANNOTATION))
        .map(String::as_str)
        .or_else(|| {
            pv.spec
                .as_ref()
                .and_then(|spec| spec.csi.as_ref())
                .map(|csi| csi.driver.as_str())
        });

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

pub async fn clear_pv_finalizers(client: &Client, name: &str) -> Result<()> {
    let result = Api::<PersistentVolume>::all(client.clone())
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(&json!({ "metadata": { "finalizers": null } })),
        )
        .await;

    match result {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
        Err(error) => Err(error).context("Failed to clear PV finalizers"),
    }
}

pub async fn clear_lvmvolume_finalizers(
    client: &Client,
    namespace: &str,
    name: &str,
) -> Result<()> {
    let result = lvmvolume_api(client, namespace)
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(&json!({ "metadata": { "finalizers": null } })),
        )
        .await;

    match result {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
        Err(error) => Err(error).context("Failed to clear OpenEBS LVMVolume finalizers"),
    }
}

pub async fn delete_lvmvolume(client: &Client, namespace: &str, name: &str) -> Result<()> {
    let result = lvmvolume_api(client, namespace)
        .delete(name, &DeleteParams::default())
        .await;

    match result {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
        Err(error) => Err(error).context("Failed to delete OpenEBS LVMVolume"),
    }
}

async fn list_lvmvolumes(client: &Client, namespace: &str) -> Result<Vec<DynamicObject>> {
    Ok(lvmvolume_api(client, namespace)
        .list(&ListParams::default())
        .await
        .context("Failed to list OpenEBS LVMVolumes")?
        .items)
}

fn lvmvolume_api(client: &Client, namespace: &str) -> Api<DynamicObject> {
    let gvk = GroupVersionKind::gvk("local.openebs.io", "v1alpha1", "LVMVolume");
    let resource = ApiResource::from_gvk_with_plural(&gvk, "lvmvolumes");
    Api::namespaced_with(client.clone(), namespace, &resource)
}

fn pvc_key(namespace: &str, name: &str) -> String {
    format!("{namespace}/{name}")
}

fn claim_ref_key_and_uid(pv: &PersistentVolume) -> Option<(String, Option<&str>)> {
    let claim_ref = pv.spec.as_ref()?.claim_ref.as_ref()?;
    let key = pvc_key(claim_ref.namespace.as_deref()?, claim_ref.name.as_deref()?);
    Some((key, claim_ref.uid.as_deref()))
}

fn pv_phase(pv: &PersistentVolume) -> Option<&str> {
    pv.status.as_ref()?.phase.as_deref()
}

fn reclaim_policy(pv: &PersistentVolume) -> Option<&str> {
    pv.spec
        .as_ref()?
        .persistent_volume_reclaim_policy
        .as_deref()
}

fn pv_node_name(pv: &PersistentVolume) -> Option<&str> {
    pv.spec
        .as_ref()?
        .node_affinity
        .as_ref()?
        .required
        .as_ref()?
        .node_selector_terms
        .iter()
        .filter_map(|term| term.match_expressions.as_ref())
        .flat_map(|expressions| expressions.iter())
        .find(|expression| {
            expression.operator == "In"
                && (expression.key == OPENEBS_NODE_AFFINITY_KEY
                    || expression.key == HOSTNAME_NODE_AFFINITY_KEY)
        })?
        .values
        .as_ref()?
        .first()
        .map(String::as_str)
}

fn pv_is_past_grace_period(
    pv: &PersistentVolume,
    grace_period: Duration,
    now: DateTime<Utc>,
) -> bool {
    let timestamp = pv
        .metadata
        .deletion_timestamp
        .as_ref()
        .map(|ts| ts.0)
        .or_else(|| {
            pv.status
                .as_ref()?
                .last_phase_transition_time
                .as_ref()
                .map(|ts| ts.0)
        })
        .or_else(|| pv.metadata.creation_timestamp.as_ref().map(|ts| ts.0));

    timestamp.is_some_and(|ts| {
        now.signed_duration_since(ts).num_seconds() >= grace_period.as_secs() as i64
    })
}

fn has_external_provisioner_finalizer(pv: &PersistentVolume) -> bool {
    pv.metadata.finalizers.as_ref().is_some_and(|finalizers| {
        finalizers
            .iter()
            .any(|f| f == EXTERNAL_PROVISIONER_FINALIZER)
    })
}

fn has_finalizers(resource: &DynamicObject) -> bool {
    resource
        .metadata
        .finalizers
        .as_ref()
        .is_some_and(|finalizers| !finalizers.is_empty())
}

fn lvmvolume_owner_node(lvmvolume: &DynamicObject) -> Option<&str> {
    lvmvolume
        .data
        .get("spec")
        .and_then(|spec| spec.get("ownerNodeID"))
        .and_then(|value| value.as_str())
        .or_else(|| {
            lvmvolume
                .data
                .get("status")
                .and_then(|status| status.get("ownerNodeID"))
                .and_then(|value| value.as_str())
        })
}

async fn cleanup_lvmvolume(
    client: &Client,
    config: &ReaperConfig,
    name: &str,
    lvmvolume: &DynamicObject,
) -> Result<bool> {
    let mut cleaned = false;

    if has_finalizers(lvmvolume) {
        if config.dry_run {
            info!(
                "[DRY RUN] Would clear OpenEBS LVMVolume finalizers for {}/{}",
                config.openebs_namespace, name
            );
        } else {
            clear_lvmvolume_finalizers(client, &config.openebs_namespace, name).await?;
            cleaned = true;
        }
    }

    if lvmvolume.metadata.deletion_timestamp.is_none() {
        if config.dry_run {
            info!(
                "[DRY RUN] Would delete OpenEBS LVMVolume {}/{}",
                config.openebs_namespace, name
            );
        } else {
            delete_lvmvolume(client, &config.openebs_namespace, name).await?;
            cleaned = true;
        }
    }

    Ok(cleaned)
}

async fn cleanup_pv(
    client: &Client,
    config: &ReaperConfig,
    name: &str,
    pv: &PersistentVolume,
) -> Result<bool> {
    if has_external_provisioner_finalizer(pv) {
        if config.dry_run {
            info!("[DRY RUN] Would clear PV finalizers for {}", name);
            return Ok(false);
        } else {
            clear_pv_finalizers(client, name).await?;
            return Ok(true);
        }
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::{
        api::core::v1::{
            CSIPersistentVolumeSource, NodeSelector, NodeSelectorRequirement, NodeSelectorTerm,
            PersistentVolumeClaimVolumeSource, PersistentVolumeSpec, PersistentVolumeStatus,
            PodCondition, PodStatus, Volume, VolumeNodeAffinity,
        },
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
            cleanup_pvcs: true,
            check_unschedulable_pods: true,
            unschedulable_pod_threshold_secs: 120,
            cleanup_pvs: true,
            pv_grace_period_secs: 600,
            cleanup_openebs_lvmvolumes: true,
            openebs_namespace: "openebs".to_string(),
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
        let pvc_refs = pvcs
            .iter()
            .filter_map(|pvc| {
                pvc.namespace()
                    .map(|ns| (pvc_key(&ns, &pvc.name_any()), pvc.metadata.uid.clone()))
            })
            .collect();

        State {
            node_names: node_names.iter().map(|s| s.to_string()).collect(),
            nodes,
            pods,
            pvcs,
            pvc_refs,
            pvs: Vec::new(),
            lvmvolumes: HashMap::new(),
            now: Utc::now(),
        }
    }

    fn with_pvc_uid(mut pvc: PersistentVolumeClaim, uid: &str) -> PersistentVolumeClaim {
        pvc.metadata.uid = Some(uid.to_string());
        pvc
    }

    fn with_pv_claim_uid(mut pv: PersistentVolume, uid: &str) -> PersistentVolume {
        if let Some(claim_ref) = pv.spec.as_mut().and_then(|spec| spec.claim_ref.as_mut()) {
            claim_ref.uid = Some(uid.to_string());
        }
        pv
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

    fn test_pv(
        name: &str,
        storage_class: &str,
        provisioner: &str,
        node: &str,
        phase: &str,
        claim: Option<(&str, &str)>,
    ) -> PersistentVolume {
        let mut annotations = std::collections::BTreeMap::new();
        annotations.insert(
            PV_PROVISIONER_ANNOTATION.to_string(),
            provisioner.to_string(),
        );

        PersistentVolume {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                annotations: Some(annotations),
                deletion_timestamp: Some(Time(
                    chrono::Utc::now() - chrono::Duration::seconds(1_000),
                )),
                finalizers: Some(vec![EXTERNAL_PROVISIONER_FINALIZER.to_string()]),
                ..Default::default()
            },
            spec: Some(PersistentVolumeSpec {
                storage_class_name: Some(storage_class.to_string()),
                persistent_volume_reclaim_policy: Some("Delete".to_string()),
                claim_ref: claim.map(|(namespace, name)| {
                    k8s_openapi::api::core::v1::ObjectReference {
                        namespace: Some(namespace.to_string()),
                        name: Some(name.to_string()),
                        ..Default::default()
                    }
                }),
                csi: Some(CSIPersistentVolumeSource {
                    driver: provisioner.to_string(),
                    volume_handle: name.to_string(),
                    ..Default::default()
                }),
                node_affinity: Some(VolumeNodeAffinity {
                    required: Some(NodeSelector {
                        node_selector_terms: vec![NodeSelectorTerm {
                            match_expressions: Some(vec![NodeSelectorRequirement {
                                key: OPENEBS_NODE_AFFINITY_KEY.to_string(),
                                operator: "In".to_string(),
                                values: Some(vec![node.to_string()]),
                            }]),
                            ..Default::default()
                        }],
                    }),
                }),
                ..Default::default()
            }),
            status: Some(PersistentVolumeStatus {
                phase: Some(phase.to_string()),
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
    fn test_matches_pv_storage_criteria() {
        let pv = test_pv(
            "pv-test",
            "openebs-lvm",
            "local.csi.openebs.io",
            "node-1",
            "Released",
            Some(("default", "test")),
        );
        assert!(matches_pv_storage_criteria(&pv, &test_config()));
    }

    #[test]
    fn test_pod_unschedulable_long_enough_with_unschedulable_condition() {
        let pod = pod_with_pvc("pending-pod", "test", "Pending", Some("Unschedulable"), 600);
        assert!(pod_exceeds_unschedulable_thresh(
            &pod,
            Duration::from_secs(120),
            Utc::now()
        ));
    }

    #[test]
    fn test_pod_unschedulable_not_long_enough() {
        let pod = pod_with_pvc("pending-pod", "test", "Pending", Some("Unschedulable"), 60);
        assert!(!pod_exceeds_unschedulable_thresh(
            &pod,
            Duration::from_secs(120),
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

    #[test]
    fn test_stale_pv_reason_when_node_missing_and_claim_gone() {
        let pv = test_pv(
            "pv-test",
            "openebs-lvm",
            "local.csi.openebs.io",
            "missing-node",
            "Released",
            Some(("default", "gone")),
        );
        let mut state = state_with(&[], Vec::new(), Vec::new());
        state.now = Utc::now();

        let reason = state
            .stale_pv_reason(&pv, &test_config())
            .expect("expected stale PV reason");

        assert_eq!(reason.node, "missing-node");
    }

    #[test]
    fn test_stale_pv_reason_skips_when_claim_still_exists() {
        let pv = with_pv_claim_uid(
            test_pv(
                "pv-test",
                "openebs-lvm",
                "local.csi.openebs.io",
                "missing-node",
                "Released",
                Some(("default", "still-here")),
            ),
            "claim-uid",
        );
        let pvc = with_pvc_uid(
            test_pvc(
                "still-here",
                "openebs-lvm",
                "local.csi.openebs.io",
                Some("node-1"),
            ),
            "claim-uid",
        );
        let state = state_with(&[], Vec::new(), vec![pvc]);

        assert!(state.stale_pv_reason(&pv, &test_config()).is_none());
    }

    #[test]
    fn test_stale_pv_reason_allows_reused_claim_name_with_different_uid() {
        let pv = with_pv_claim_uid(
            test_pv(
                "pv-test",
                "openebs-lvm",
                "local.csi.openebs.io",
                "missing-node",
                "Released",
                Some(("default", "reused-name")),
            ),
            "old-claim-uid",
        );
        let pvc = with_pvc_uid(
            test_pvc(
                "reused-name",
                "openebs-lvm",
                "local.csi.openebs.io",
                Some("node-1"),
            ),
            "new-claim-uid",
        );
        let state = state_with(&[], Vec::new(), vec![pvc]);

        let reason = state
            .stale_pv_reason(&pv, &test_config())
            .expect("expected stale PV reason for old claim UID");

        assert_eq!(reason.node, "missing-node");
    }

    #[test]
    fn test_stale_pv_reason_skips_when_node_exists() {
        let pv = test_pv(
            "pv-test",
            "openebs-lvm",
            "local.csi.openebs.io",
            "node-1",
            "Released",
            Some(("default", "gone")),
        );
        let state = state_with(&["node-1"], Vec::new(), Vec::new());

        assert!(state.stale_pv_reason(&pv, &test_config()).is_none());
    }
}
