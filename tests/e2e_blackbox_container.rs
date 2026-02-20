#[path = "common/mod.rs"]
mod common;

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{start_relay, test_guard};

struct DockerContainerGuard {
    name: String,
}

impl Drop for DockerContainerGuard {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .output();
    }
}

fn unique_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos()
    )
}

fn ensure_image_exists() {
    let inspect = Command::new("docker")
        .args(["image", "inspect", "ldk-controller:e2e"])
        .output()
        .expect("failed to run docker image inspect");
    if inspect.status.success() {
        return;
    }

    let build = Command::new("docker")
        .args([
            "build",
            "-f",
            "tests/e2e/docker/ldk-controller/Dockerfile",
            "-t",
            "ldk-controller:e2e",
            ".",
        ])
        .output()
        .expect("failed to run docker build for ldk-controller:e2e");
    if !build.status.success() {
        let stderr = String::from_utf8_lossy(&build.stderr);
        panic!("docker build failed for ldk-controller:e2e: {stderr}");
    }
}

fn write_config(relay_url: &str, dir: &PathBuf) {
    let config = format!(
        r#"[node]
network = "regtest"
listening_port = 9735
data_dir = "/var/lib/ldk-controller/data"

[nostr]
relay = "{relay_url}"
private_key = "invalid-for-tests"

[wallet]
max_channel_size_sats = 1000000
min_channel_size_sats = 20000
auto_accept_channels = false
"#
    );

    fs::create_dir_all(dir).expect("failed to create test config directory");
    fs::write(dir.join("config.toml"), config).expect("failed to write config.toml");
}

fn start_controller_container(config_dir: &PathBuf) -> DockerContainerGuard {
    let name = unique_id("ldk-controller-e2e");
    let mount = format!("{}:/var/lib/ldk-controller", config_dir.display());

    let run = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "--add-host",
            "host.docker.internal:host-gateway",
            "-v",
            &mount,
            "ldk-controller:e2e",
        ])
        .output()
        .expect("failed to run docker container for ldk-controller:e2e");

    if !run.status.success() {
        let stderr = String::from_utf8_lossy(&run.stderr);
        panic!("docker run failed for ldk-controller:e2e: {stderr}");
    }

    DockerContainerGuard { name }
}

fn container_logs(name: &str) -> String {
    let output = Command::new("docker")
        .args(["logs", name])
        .output()
        .expect("failed to run docker logs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("{stdout}\n{stderr}")
}

fn container_running(name: &str) -> bool {
    let output = Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", name])
        .output()
        .expect("failed to run docker inspect");
    output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
}

fn wait_for_controller_ready(name: &str) {
    for _ in 0..40 {
        let logs = container_logs(name);
        if logs.contains("Subscribed to text notes") {
            return;
        }

        if !container_running(name) {
            panic!(
                "ldk-controller container exited before readiness; logs:\n{}",
                logs
            );
        }

        std::thread::sleep(Duration::from_millis(500));
    }

    panic!(
        "timed out waiting for ldk-controller readiness; current logs:\n{}",
        container_logs(name)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_container_stack_boots() {
    let _guard = test_guard();

    let bitcoind = common::bitcoind::BitcoindHarness::start().await;
    let bitcoind_addr = bitcoind.get_new_address().await;
    bitcoind.mine_blocks(1, &bitcoind_addr).await;

    let (_relay_container, relay_url) = start_relay().await;

    ensure_image_exists();

    let relay_ws = relay_url.replace("ws://localhost:", "ws://host.docker.internal:");
    let config_dir = PathBuf::from(format!("/tmp/{}", unique_id("ldk-controller-config")));
    write_config(&relay_ws, &config_dir);

    let controller = start_controller_container(&config_dir);
    wait_for_controller_ready(&controller.name);

    assert!(
        container_running(&controller.name),
        "ldk-controller should still be running after readiness"
    );
}
