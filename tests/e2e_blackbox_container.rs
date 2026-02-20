#[path = "common/mod.rs"]
mod common;

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{start_relay, test_guard};
use ldk_controller::{MethodAccessRule, UsageProfile};
use nostr_sdk::prelude::*;
use nwc::nostr::nips::nip47::{ErrorCode, Method, NostrWalletConnectUri, Request, Response};

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

fn ensure_image_built() {
    static IMAGE_BUILT: OnceLock<()> = OnceLock::new();
    IMAGE_BUILT.get_or_init(|| {
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
    });
}

fn write_config(relay_url: &str, dir: &PathBuf, private_key: &str) {
    let config = format!(
        r#"[node]
network = "regtest"
listening_port = 9735
data_dir = "/var/lib/ldk-controller/data"

[nostr]
relay = "{relay_url}"
private_key = "{private_key}"

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
        if logs.contains("Press Ctrl+C to stop.") {
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

async fn send_nwc_request_and_wait_response(
    nwc_client: &Client,
    uri: &NostrWalletConnectUri,
    service_pubkey: PublicKey,
    method: Method,
    request: Request,
) -> Response {
    let request_event = request
        .to_event(uri)
        .expect("failed to create NWC request event");
    nwc_client
        .send_event(&request_event)
        .await
        .expect("failed to publish NWC request event");

    let timeout = Duration::from_secs(15);
    let uri_clone = uri.clone();
    tokio::time::timeout(timeout, async {
        let mut notifications = nwc_client.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::WalletConnectResponse && event.pubkey == service_pubkey {
                    let response =
                        Response::from_event(&uri_clone, event).expect("failed to parse response");
                    if response.result_type == method {
                        return response;
                    }
                }
            }
        }
        panic!("notification stream ended before NWC response");
    })
    .await
    .expect("timed out waiting for NWC response")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_container_stack_boots() {
    let _guard = test_guard();

    let bitcoind = common::bitcoind::BitcoindHarness::start().await;
    let bitcoind_addr = bitcoind.get_new_address().await;
    bitcoind.mine_blocks(1, &bitcoind_addr).await;

    let (_relay_container, relay_url) = start_relay().await;

    ensure_image_built();

    let relay_ws = relay_url.replace("ws://localhost:", "ws://host.docker.internal:");
    let config_dir = PathBuf::from(format!("/tmp/{}", unique_id("ldk-controller-config")));
    write_config(&relay_ws, &config_dir, "invalid-for-tests");

    let controller = start_controller_container(&config_dir);
    wait_for_controller_ready(&controller.name);

    assert!(
        container_running(&controller.name),
        "ldk-controller should still be running after readiness"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_nwc_get_info_get_balance_roundtrip() -> Result<()> {
    let _guard = test_guard();

    let _bitcoind = common::bitcoind::BitcoindHarness::start().await;
    let (_relay_container, relay_url) = start_relay().await;
    ensure_image_built();

    let service_keys = Keys::generate();
    let service_secret = service_keys.secret_key().to_bech32()?;
    let service_pubkey = service_keys.public_key();

    let relay_ws_for_container = relay_url.replace("ws://localhost:", "ws://host.docker.internal:");
    let config_dir = PathBuf::from(format!("/tmp/{}", unique_id("ldk-controller-config")));
    write_config(&relay_ws_for_container, &config_dir, &service_secret);

    let controller = start_controller_container(&config_dir);
    wait_for_controller_ready(&controller.name);

    let client_secret = Keys::generate().secret_key().clone();
    let client_keys = Keys::new(client_secret.clone());
    let client_pubkey = client_keys.public_key();

    let owner_keys = Keys::generate();
    let usage_profile = UsageProfile {
        quota: None,
        methods: None,
        control: None,
    };
    common::grant_usage_profile(
        &owner_keys,
        &relay_url,
        service_pubkey,
        client_pubkey,
        &usage_profile,
    )
    .await?;

    let nwc_client = Client::builder().signer(client_keys).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    nwc_client
        .subscribe(
            Filter::new()
                .kind(Kind::WalletConnectResponse)
                .author(service_pubkey),
        )
        .await?;

    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(service_pubkey, vec![relay], client_secret, None);

    let info_response = send_nwc_request_and_wait_response(
        &nwc_client,
        &uri,
        service_pubkey,
        Method::GetInfo,
        Request::get_info(),
    )
    .await;
    assert!(
        info_response.error.is_none(),
        "get_info returned error: {:?}",
        info_response.error
    );
    let info = info_response
        .to_get_info()
        .expect("get_info response should decode");
    assert_eq!(info.network, Some("regtest".to_string()));
    assert!(
        info.pubkey.as_ref().is_some_and(|p| !p.is_empty()),
        "get_info pubkey should be non-empty"
    );

    let balance_response = send_nwc_request_and_wait_response(
        &nwc_client,
        &uri,
        service_pubkey,
        Method::GetBalance,
        Request::get_balance(),
    )
    .await;
    assert!(
        balance_response.error.is_none(),
        "get_balance returned error: {:?}",
        balance_response.error
    );
    let balance = balance_response
        .to_get_balance()
        .expect("get_balance response should decode");
    let _balance_msat = balance.balance;

    assert!(
        container_running(&controller.name),
        "ldk-controller should still be running after NWC roundtrip"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_grant_authorization_enforced() -> Result<()> {
    let _guard = test_guard();

    let _bitcoind = common::bitcoind::BitcoindHarness::start().await;
    let (_relay_container, relay_url) = start_relay().await;
    ensure_image_built();

    let service_keys = Keys::generate();
    let service_secret = service_keys.secret_key().to_bech32()?;
    let service_pubkey = service_keys.public_key();

    let relay_ws_for_container = relay_url.replace("ws://localhost:", "ws://host.docker.internal:");
    let config_dir = PathBuf::from(format!("/tmp/{}", unique_id("ldk-controller-config")));
    write_config(&relay_ws_for_container, &config_dir, &service_secret);

    let controller = start_controller_container(&config_dir);
    wait_for_controller_ready(&controller.name);

    let client_secret = Keys::generate().secret_key().clone();
    let client_keys = Keys::new(client_secret.clone());
    let client_pubkey = client_keys.public_key();

    let nwc_client = Client::builder().signer(client_keys.clone()).build();
    nwc_client.add_relay(&relay_url).await?;
    nwc_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    nwc_client
        .subscribe(
            Filter::new()
                .kind(Kind::WalletConnectResponse)
                .author(service_pubkey),
        )
        .await?;

    let relay = RelayUrl::parse(&relay_url)?;
    let uri = NostrWalletConnectUri::new(service_pubkey, vec![relay], client_secret, None);

    let mut allowed_methods: HashMap<Method, MethodAccessRule> = HashMap::new();
    allowed_methods.insert(
        Method::GetInfo,
        MethodAccessRule {
            access_rate: None,
        },
    );
    let usage_profile = UsageProfile {
        quota: None,
        methods: Some(allowed_methods),
        control: None,
    };

    // Old d-tag format should not apply any grant.
    let owner_keys = Keys::generate();
    let owner_client = Client::builder().signer(owner_keys.clone()).build();
    owner_client.add_relay(&relay_url).await?;
    owner_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    let old_d_content = serde_json::to_string(&usage_profile).expect("serialize usage profile");
    let old_d_value = client_pubkey.to_string();
    let old_d_event = EventBuilder::new(Kind::Custom(30078), old_d_content)
        .tag(Tag::parse(["d", old_d_value.as_str()]).expect("create old d tag"))
        .tag(Tag::public_key(service_pubkey));
    owner_client.send_event_builder(old_d_event).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let response_without_valid_grant = send_nwc_request_and_wait_response(
        &nwc_client,
        &uri,
        service_pubkey,
        Method::GetInfo,
        Request::get_info(),
    )
    .await;
    let err = response_without_valid_grant
        .error
        .expect("old d-tag grant should not authorize");
    assert_eq!(err.code, ErrorCode::Unauthorized);

    // Node-based d-tag format should authorize get_info for the same client.
    common::grant_usage_profile(
        &owner_keys,
        &relay_url,
        service_pubkey,
        client_pubkey,
        &usage_profile,
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let allowed_response = send_nwc_request_and_wait_response(
        &nwc_client,
        &uri,
        service_pubkey,
        Method::GetInfo,
        Request::get_info(),
    )
    .await;
    assert!(
        allowed_response.error.is_none(),
        "get_info should be allowed once node-based d grant is present: {:?}",
        allowed_response.error
    );

    let denied_response = send_nwc_request_and_wait_response(
        &nwc_client,
        &uri,
        service_pubkey,
        Method::GetBalance,
        Request::get_balance(),
    )
    .await;
    let denied_error = denied_response
        .error
        .expect("non-granted method should return restricted");
    assert_eq!(denied_error.code, ErrorCode::Restricted);

    assert!(
        container_running(&controller.name),
        "ldk-controller should still be running after auth checks"
    );

    Ok(())
}
