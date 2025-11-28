//! Integration tests that deploy pvc-reaper via Helm into a k3s cluster.
//!
//! These tests:
//! 1. Spin up a k3s cluster using testcontainers
//! 2. Build and load the pvc-reaper Docker image
//! 3. Install pvc-reaper via Helm
//! 4. Create test PVCs and verify pvc-reaper deletes them

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{
    Namespace, Node, PersistentVolumeClaim, PersistentVolumeClaimSpec, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::{
    api::{Api, DeleteParams, PostParams},
    config::{KubeConfigOptions, Kubeconfig},
    Client, Config,
};
use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;
use testcontainers::{core::ExecCommand, runners::AsyncRunner, ContainerAsync, ImageExt};
use testcontainers_modules::k3s::{K3s, KUBE_SECURE_PORT};
use tokio::io::AsyncReadExt;

const STORAGE_CLASS: &str = "openebs-lvm";
const PROVISIONER: &str = "local.csi.openebs.io";
const IMAGE_NAME: &str = "pvc-reaper";
const IMAGE_TAG: &str = "test";

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

// ============================================================================
// Cluster Setup
// ============================================================================

struct TestCluster {
    container: ContainerAsync<K3s>,
    container_id: String,
    client: Client,
}

impl TestCluster {
    async fn new() -> TestResult<Self> {
        // Create unique temp directory for kubeconfig (using timestamp + random)
        let unique_id = format!(
            "{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::process::id()
        );
        let conf_dir = std::env::temp_dir().join(format!("k3s-test-{}", unique_id));
        std::fs::create_dir_all(&conf_dir)?;

        println!("Starting k3s cluster (conf_dir: {:?})...", conf_dir);

        // Configure k3s with proper settings for cross-platform support
        let k3s = K3s::default()
            .with_conf_mount(&conf_dir)
            .with_privileged(true)
            .with_userns_mode("host");

        let container = tokio::time::timeout(Duration::from_secs(180), k3s.start())
            .await
            .map_err(|_| "Timeout starting k3s (180s)")?
            .map_err(|e| format!("Failed to start k3s: {e}"))?;

        println!("✓ K3s container started");

        // Get container ID for docker cp commands
        let container_id = container.id().to_string();

        // Wait a moment for kubeconfig to be written
        tokio::time::sleep(Duration::from_secs(2)).await;

        let client = Self::create_client(&container, &conf_dir).await?;
        Self::wait_for_ready(&client).await?;

        Ok(Self {
            container,
            container_id,
            client,
        })
    }

    async fn create_client(
        container: &ContainerAsync<K3s>,
        conf_dir: &std::path::Path,
    ) -> TestResult<Client> {
        // Read kubeconfig from mounted directory
        let kubeconfig_path = conf_dir.join("k3s.yaml");

        // Wait for kubeconfig file to exist
        for i in 0..30 {
            if kubeconfig_path.exists() {
                break;
            }
            if i == 29 {
                return Err("Kubeconfig file not created".into());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        let yaml = std::fs::read_to_string(&kubeconfig_path)?;

        if yaml.is_empty() {
            return Err("Empty kubeconfig".into());
        }

        // Parse and update kubeconfig with correct port
        let mut kubeconfig: Kubeconfig = Kubeconfig::from_yaml(&yaml)?;
        let port = container.get_host_port_ipv4(KUBE_SECURE_PORT).await?;

        // Update server URL to use mapped port
        for cluster in &mut kubeconfig.clusters {
            if let Some(ref mut c) = cluster.cluster {
                if let Some(ref mut server) = c.server {
                    *server = format!("https://127.0.0.1:{}", port);
                }
            }
        }

        let config =
            Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default()).await?;

        Ok(Client::try_from(config)?)
    }

    async fn wait_for_ready(client: &Client) -> TestResult<()> {
        let nodes: Api<Node> = Api::all(client.clone());

        for i in 0..60 {
            if i > 0 && i % 10 == 0 {
                println!("  Waiting for k3s... (attempt {i}/60)");
            }
            if let Ok(list) = nodes.list(&Default::default()).await {
                if !list.items.is_empty() {
                    println!("✓ K3s cluster ready with {} node(s)", list.items.len());
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        Err("K3s cluster did not become ready".into())
    }

    async fn get_node_name(&self) -> Option<String> {
        Api::<Node>::all(self.client.clone())
            .list(&Default::default())
            .await
            .ok()?
            .items
            .first()?
            .metadata
            .name
            .clone()
    }

    /// Copy a file into the k3s container using docker cp
    fn docker_cp(&self, src: &str, dest: &str) -> TestResult<()> {
        let output = Command::new("docker")
            .args(["cp", src, &format!("{}:{}", self.container_id, dest)])
            .output()?;

        if !output.status.success() {
            return Err(format!(
                "docker cp failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(())
    }

    async fn install_pvc_reaper(&self, reap_interval: u64) -> TestResult<()> {
        // Build the Docker image
        println!("Building Docker image...");
        let output = Command::new("docker")
            .args(["build", "-t", &format!("{IMAGE_NAME}:{IMAGE_TAG}"), "."])
            .output()?;

        if !output.status.success() {
            return Err(format!(
                "Failed to build Docker image: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        println!("✓ Docker image built");

        // Save image to tar
        println!("Saving Docker image...");
        let host_tar_path = "/tmp/pvc-reaper-test.tar";
        let output = Command::new("docker")
            .args([
                "save",
                "-o",
                host_tar_path,
                &format!("{IMAGE_NAME}:{IMAGE_TAG}"),
            ])
            .output()?;

        if !output.status.success() {
            return Err(format!(
                "Failed to save Docker image: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }

        // Copy tar file into k3s container using docker cp
        println!("Loading image into k3s...");
        self.docker_cp(host_tar_path, "/tmp/pvc-reaper.tar")?;

        // Import into k3s using ctr
        let mut import_result = self
            .container
            .exec(ExecCommand::new(vec![
                "ctr",
                "images",
                "import",
                "/tmp/pvc-reaper.tar",
            ]))
            .await?;

        let mut import_output = String::new();
        import_result
            .stdout()
            .read_to_string(&mut import_output)
            .await?;

        println!("✓ Image loaded into k3s");

        // Create pvc-reaper namespace
        let ns = Namespace {
            metadata: ObjectMeta {
                name: Some("pvc-reaper".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let _ = Api::<Namespace>::all(self.client.clone())
            .create(&PostParams::default(), &ns)
            .await;

        // Render Helm template and apply via kubectl in container
        println!("Rendering Helm template...");
        let output = Command::new("helm")
            .args([
                "template",
                "pvc-reaper",
                "./helm/pvc-reaper",
                "--namespace",
                "pvc-reaper",
                "--set",
                &format!("image.repository=docker.io/library/{IMAGE_NAME}"),
                "--set",
                &format!("image.tag={IMAGE_TAG}"),
                "--set",
                "image.pullPolicy=Never",
                "--set",
                &format!("config.reapIntervalSecs={reap_interval}"),
                "--set",
                &format!("config.storageClassNames={STORAGE_CLASS}"),
                "--set",
                &format!("config.storageProvisioner={PROVISIONER}"),
                "--set",
                "config.dryRun=false",
                "--set",
                "logLevel=debug",
                "--set",
                "podSecurityContext.runAsNonRoot=false",
                "--set",
                "podSecurityContext.runAsUser=0",
                "--set",
                "securityContext.readOnlyRootFilesystem=false",
            ])
            .output()?;

        if !output.status.success() {
            return Err(format!(
                "Failed to render Helm template: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }

        let manifests = String::from_utf8_lossy(&output.stdout);

        // Write manifests to temp file and copy to container
        let manifest_path = "/tmp/pvc-reaper-manifests.yaml";
        std::fs::write(manifest_path, manifests.as_bytes())?;
        self.docker_cp(manifest_path, "/tmp/manifests.yaml")?;

        println!("Applying manifests...");
        let mut apply_result = self
            .container
            .exec(ExecCommand::new(vec![
                "kubectl",
                "apply",
                "-f",
                "/tmp/manifests.yaml",
            ]))
            .await?;

        let mut apply_output = String::new();
        apply_result
            .stdout()
            .read_to_string(&mut apply_output)
            .await?;
        println!("Applied: {apply_output}");

        println!("✓ Helm chart installed");

        // Wait for deployment to be ready
        self.wait_for_deployment("pvc-reaper", "pvc-reaper").await?;

        Ok(())
    }

    async fn wait_for_deployment(&self, namespace: &str, name: &str) -> TestResult<()> {
        let deployments: Api<Deployment> = Api::namespaced(self.client.clone(), namespace);

        for i in 0..60 {
            if i > 0 && i % 10 == 0 {
                println!("  Waiting for deployment {name}... (attempt {i}/60)");
            }

            if let Ok(deploy) = deployments.get(name).await {
                if let Some(status) = deploy.status {
                    let ready = status.ready_replicas.unwrap_or(0);
                    let desired = status.replicas.unwrap_or(1);
                    if ready >= desired && ready > 0 {
                        println!("✓ Deployment {name} ready");
                        return Ok(());
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        // Get pod logs for debugging
        let mut logs_result = self
            .container
            .exec(ExecCommand::new(vec![
                "kubectl",
                "logs",
                "-n",
                namespace,
                "-l",
                &format!("app.kubernetes.io/name={name}"),
                "--tail=50",
            ]))
            .await?;

        let mut logs = String::new();
        logs_result.stdout().read_to_string(&mut logs).await?;
        eprintln!("Pod logs:\n{logs}");

        // Also get pod status
        let mut status_result = self
            .container
            .exec(ExecCommand::new(vec![
                "kubectl", "get", "pods", "-n", namespace, "-o", "wide",
            ]))
            .await?;

        let mut status = String::new();
        status_result.stdout().read_to_string(&mut status).await?;
        eprintln!("Pod status:\n{status}");

        Err(format!("Deployment {name} did not become ready").into())
    }
}

// ============================================================================
// Test Namespace Helper
// ============================================================================

struct TestNamespace<'a> {
    client: &'a Client,
    name: String,
}

impl<'a> TestNamespace<'a> {
    async fn create(client: &'a Client, name: &str) -> TestResult<Self> {
        let ns = Namespace {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let _ = Api::<Namespace>::all(client.clone())
            .create(&PostParams::default(), &ns)
            .await;

        Ok(Self {
            client,
            name: name.to_string(),
        })
    }

    async fn create_pvc(
        &self,
        name: &str,
        storage_class: &str,
        node: Option<&str>,
    ) -> TestResult<()> {
        let mut annotations = BTreeMap::new();
        annotations.insert(
            "volume.beta.kubernetes.io/storage-provisioner".to_string(),
            PROVISIONER.to_string(),
        );
        if let Some(n) = node {
            annotations.insert(
                "volume.kubernetes.io/selected-node".to_string(),
                n.to_string(),
            );
        }

        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(self.name.clone()),
                annotations: Some(annotations),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                storage_class_name: Some(storage_class.to_string()),
                access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                resources: Some(VolumeResourceRequirements {
                    requests: Some([("storage".to_string(), Quantity("1Gi".to_string()))].into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        Api::<PersistentVolumeClaim>::namespaced(self.client.clone(), &self.name)
            .create(&PostParams::default(), &pvc)
            .await?;
        Ok(())
    }

    async fn pvc_exists(&self, name: &str) -> bool {
        Api::<PersistentVolumeClaim>::namespaced(self.client.clone(), &self.name)
            .get(name)
            .await
            .is_ok()
    }

    async fn wait_for_pvc_deletion(&self, name: &str, timeout_secs: u64) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < timeout_secs {
            if !self.pvc_exists(name).await {
                return true;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        false
    }

    async fn cleanup(self) {
        let _ = Api::<Namespace>::all(self.client.clone())
            .delete(&self.name, &DeleteParams::default())
            .await;
    }
}

// ============================================================================
// Integration Tests
// ============================================================================

/// Test that pvc-reaper deletes PVCs referencing non-existent nodes
#[tokio::test]
async fn test_helm_deployment_deletes_orphaned_pvcs() {
    let cluster = TestCluster::new().await.expect("Failed to create cluster");

    // Install pvc-reaper with 5-second reap interval for faster testing
    cluster
        .install_pvc_reaper(5)
        .await
        .expect("Failed to install pvc-reaper");

    let ns = TestNamespace::create(&cluster.client, "test-orphaned")
        .await
        .unwrap();

    // Create a PVC pointing to a non-existent node
    ns.create_pvc("orphaned-pvc", STORAGE_CLASS, Some("fake-missing-node"))
        .await
        .unwrap();

    assert!(
        ns.pvc_exists("orphaned-pvc").await,
        "PVC should exist initially"
    );

    // Wait for pvc-reaper to delete it (give it 30 seconds with 5s reap interval)
    let deleted = ns.wait_for_pvc_deletion("orphaned-pvc", 30).await;
    assert!(deleted, "PVC should have been deleted by pvc-reaper");

    println!("✓ Test passed: pvc-reaper deleted orphaned PVC");

    ns.cleanup().await;
}

/// Test that pvc-reaper keeps PVCs referencing existing nodes
#[tokio::test]
async fn test_helm_deployment_keeps_valid_pvcs() {
    let cluster = TestCluster::new().await.expect("Failed to create cluster");

    cluster
        .install_pvc_reaper(5)
        .await
        .expect("Failed to install pvc-reaper");

    let ns = TestNamespace::create(&cluster.client, "test-valid")
        .await
        .unwrap();

    // Get the actual node name
    let node_name = cluster.get_node_name().await.expect("No nodes in cluster");
    println!("Using existing node: {node_name}");

    // Create a PVC pointing to an existing node
    ns.create_pvc("valid-pvc", STORAGE_CLASS, Some(&node_name))
        .await
        .unwrap();

    assert!(ns.pvc_exists("valid-pvc").await);

    // Wait a couple reap cycles and verify PVC still exists
    tokio::time::sleep(Duration::from_secs(15)).await;
    assert!(
        ns.pvc_exists("valid-pvc").await,
        "PVC should NOT have been deleted"
    );

    println!("✓ Test passed: pvc-reaper kept valid PVC");

    ns.cleanup().await;
}

/// Test that pvc-reaper ignores PVCs with wrong storage class
#[tokio::test]
async fn test_helm_deployment_ignores_wrong_storage_class() {
    let cluster = TestCluster::new().await.expect("Failed to create cluster");

    cluster
        .install_pvc_reaper(5)
        .await
        .expect("Failed to install pvc-reaper");

    let ns = TestNamespace::create(&cluster.client, "test-wrong-class")
        .await
        .unwrap();

    // Create a PVC with wrong storage class but pointing to non-existent node
    ns.create_pvc("wrong-class-pvc", "other-storage-class", Some("fake-node"))
        .await
        .unwrap();

    assert!(ns.pvc_exists("wrong-class-pvc").await);

    // Wait and verify PVC still exists (should be ignored due to wrong storage class)
    tokio::time::sleep(Duration::from_secs(15)).await;
    assert!(
        ns.pvc_exists("wrong-class-pvc").await,
        "PVC should NOT have been deleted (wrong storage class)"
    );

    println!("✓ Test passed: pvc-reaper ignored PVC with wrong storage class");

    ns.cleanup().await;
}

/// Test multiple PVCs with mixed scenarios
#[tokio::test]
async fn test_helm_deployment_mixed_scenarios() {
    let cluster = TestCluster::new().await.expect("Failed to create cluster");

    cluster
        .install_pvc_reaper(5)
        .await
        .expect("Failed to install pvc-reaper");

    let ns = TestNamespace::create(&cluster.client, "test-mixed")
        .await
        .unwrap();

    let node_name = cluster.get_node_name().await.unwrap();

    // Create various PVCs
    ns.create_pvc("pvc-delete-1", STORAGE_CLASS, Some("missing-node-1"))
        .await
        .unwrap();
    ns.create_pvc("pvc-keep", STORAGE_CLASS, Some(&node_name))
        .await
        .unwrap();
    ns.create_pvc("pvc-ignore", "wrong-class", Some("missing-node-2"))
        .await
        .unwrap();
    ns.create_pvc("pvc-delete-2", STORAGE_CLASS, Some("missing-node-3"))
        .await
        .unwrap();

    // Wait for reaping
    tokio::time::sleep(Duration::from_secs(20)).await;

    // Verify final state
    assert!(
        !ns.pvc_exists("pvc-delete-1").await,
        "pvc-delete-1 should be deleted"
    );
    assert!(ns.pvc_exists("pvc-keep").await, "pvc-keep should exist");
    assert!(ns.pvc_exists("pvc-ignore").await, "pvc-ignore should exist");
    assert!(
        !ns.pvc_exists("pvc-delete-2").await,
        "pvc-delete-2 should be deleted"
    );

    println!("✓ Test passed: mixed scenarios handled correctly");

    ns.cleanup().await;
}
