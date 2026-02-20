#[path = "common/mod.rs"]
mod common;

use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::os::unix::fs::PermissionsExt;

use common::{start_relay, test_guard};
use ldk_controller::lightning::{LdkService, LdkServiceConfig};
use ldk_controller::{MethodAccessRule, UsageProfile};
use nostr_sdk::prelude::*;
use nwc::nostr::nips::nip04;
use nwc::nostr::nips::nip47::{
    ErrorCode, MakeInvoiceRequest, Method, NostrWalletConnectUri, PayKeysendRequest, Request,
    Response,
};
use serde_json::{json, Value};

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

fn write_config(
    relay_url: &str,
    dir: &PathBuf,
    private_key: &str,
    bitcoind: Option<(&str, u16, &str, &str)>,
) {
    let mut config = format!(
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
    if let Some((rpc_host, rpc_port, rpc_user, rpc_password)) = bitcoind {
        config.push_str(&format!(
            r#"
[bitcoind]
rpc_host = "{rpc_host}"
rpc_port = {rpc_port}
rpc_user = "{rpc_user}"
rpc_password = "{rpc_password}"
"#
        ));
    }

    fs::create_dir_all(dir).expect("failed to create test config directory");
    fs::set_permissions(dir, fs::Permissions::from_mode(0o777))
        .expect("failed to set config directory permissions");
    let data_dir = dir.join("data");
    fs::create_dir_all(&data_dir).expect("failed to create test data directory");
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o777))
        .expect("failed to set data directory permissions");
    fs::write(dir.join("config.toml"), config).expect("failed to write config.toml");
}

fn start_controller_container(
    config_dir: &PathBuf,
    published_ldk_port: Option<u16>,
) -> DockerContainerGuard {
    let name = unique_id("ldk-controller-e2e");
    let mount = format!("{}:/var/lib/ldk-controller", config_dir.display());
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        name.clone(),
        "--add-host".to_string(),
        "host.docker.internal:host-gateway".to_string(),
        "-v".to_string(),
        mount,
    ];
    if let Some(port) = published_ldk_port {
        args.push("-p".to_string());
        args.push(format!("{port}:9735"));
    }
    args.push("ldk-controller:e2e".to_string());

    let run = Command::new("docker")
        .args(args)
        .output()
        .expect("failed to run docker container for ldk-controller:e2e");

    if !run.status.success() {
        let stderr = String::from_utf8_lossy(&run.stderr);
        panic!("docker run failed for ldk-controller:e2e: {stderr}");
    }

    DockerContainerGuard { name }
}

fn free_local_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("read local addr").port();
    drop(listener);
    port
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

    let timeout = Duration::from_secs(45);
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

async fn send_control_request_and_wait_response(
    controller: &Client,
    controller_secret: &SecretKey,
    service_pubkey: PublicKey,
    payload: Value,
) -> Value {
    let encrypted = nip04::encrypt(controller_secret, &service_pubkey, payload.to_string())
        .expect("failed to encrypt control request");
    let request_event = EventBuilder::new(Kind::Custom(ldk_controller::CONTROL_REQUEST_KIND), encrypted)
        .tag(Tag::public_key(service_pubkey));
    controller
        .send_event_builder(request_event)
        .await
        .expect("failed to publish control request event");

    let timeout = Duration::from_secs(15);
    let response_event = tokio::time::timeout(timeout, async {
        let mut notifications = controller.notifications();
        while let Some(notification) = notifications.next().await {
            if let ClientNotification::Event { event, .. } = notification {
                let event = event.as_ref();
                if event.kind == Kind::Custom(ldk_controller::CONTROL_RESPONSE_KIND)
                    && event.pubkey == service_pubkey
                {
                    return event.clone();
                }
            }
        }
        panic!("notification stream ended before control response");
    })
    .await
    .expect("timed out waiting for control response event");

    let decrypted = nip04::decrypt(controller_secret, &service_pubkey, &response_event.content)
        .expect("failed to decrypt control response");
    serde_json::from_str(&decrypted).expect("failed to parse control response JSON")
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
    write_config(&relay_ws, &config_dir, "invalid-for-tests", None);

    let controller = start_controller_container(&config_dir, None);
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
    write_config(&relay_ws_for_container, &config_dir, &service_secret, None);

    let controller = start_controller_container(&config_dir, None);
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
    write_config(&relay_ws_for_container, &config_dir, &service_secret, None);

    let controller = start_controller_container(&config_dir, None);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_control_list_channels_roundtrip() -> Result<()> {
    let _guard = test_guard();

    let _bitcoind = common::bitcoind::BitcoindHarness::start().await;
    let (_relay_container, relay_url) = start_relay().await;
    ensure_image_built();

    let service_keys = Keys::generate();
    let service_secret = service_keys.secret_key().to_bech32()?;
    let service_pubkey = service_keys.public_key();

    let relay_ws_for_container = relay_url.replace("ws://localhost:", "ws://host.docker.internal:");
    let config_dir = PathBuf::from(format!("/tmp/{}", unique_id("ldk-controller-config")));
    write_config(&relay_ws_for_container, &config_dir, &service_secret, None);

    let controller_container = start_controller_container(&config_dir, None);
    wait_for_controller_ready(&controller_container.name);

    let controller_keys = Keys::generate();
    let controller_secret = controller_keys.secret_key().clone();
    let controller_pubkey = controller_keys.public_key();

    let mut control = HashMap::new();
    control.insert(
        "list_channels".to_string(),
        MethodAccessRule {
            access_rate: None,
        },
    );
    let profile = UsageProfile {
        quota: None,
        methods: None,
        control: Some(control),
    };
    let owner_keys = Keys::generate();
    common::grant_usage_profile(
        &owner_keys,
        &relay_url,
        service_pubkey,
        controller_pubkey,
        &profile,
    )
    .await?;

    let controller_client = Client::builder().signer(controller_keys).build();
    controller_client.add_relay(&relay_url).await?;
    controller_client.connect().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    controller_client
        .subscribe(
            Filter::new()
                .kind(Kind::Custom(ldk_controller::CONTROL_RESPONSE_KIND))
                .author(service_pubkey),
        )
        .await?;

    tokio::time::sleep(Duration::from_secs(2)).await;
    let response = send_control_request_and_wait_response(
        &controller_client,
        &controller_secret,
        service_pubkey,
        json!({
            "method": "list_channels",
            "params": {}
        }),
    )
    .await;

    assert_eq!(response["result_type"], "list_channels");
    assert!(response["error"].is_null(), "unexpected control error: {:?}", response["error"]);
    assert!(
        response["result"].is_array(),
        "list_channels result should be an array, got: {:?}",
        response["result"]
    );

    assert!(
        container_running(&controller_container.name),
        "ldk-controller should still be running after control roundtrip"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "WIP: reverse-direction payment from containerized Alice to Bob times out intermittently"]
async fn e2e_control_open_channel_and_bidirectional_payment() -> Result<()> {
    let _guard = test_guard();

    let bitcoind = common::bitcoind::BitcoindHarness::start().await;
    let (_relay_container, relay_url) = start_relay().await;
    ensure_image_built();
    let miner_address = bitcoind.get_new_address().await;
    bitcoind.mine_blocks(101, &miner_address).await;

    let service_keys = Keys::generate();
    let service_secret = service_keys.secret_key().to_bech32()?;
    let service_pubkey = service_keys.public_key();

    let relay_ws_for_container = relay_url.replace("ws://localhost:", "ws://host.docker.internal:");
    let config_dir = PathBuf::from(format!("/tmp/{}", unique_id("ldk-controller-config")));
    let published_alice_port = free_local_port();
    write_config(
        &relay_ws_for_container,
        &config_dir,
        &service_secret,
        Some((
            "host.docker.internal",
            bitcoind.rpc_port(),
            bitcoind.rpc_user(),
            bitcoind.rpc_password(),
        )),
    );
    let controller_container = start_controller_container(&config_dir, Some(published_alice_port));
    wait_for_controller_ready(&controller_container.name);

    let controller_keys = Keys::generate();
    let controller_secret = controller_keys.secret_key().clone();
    let controller_pubkey = controller_keys.public_key();

    let mut methods = HashMap::new();
    methods.insert(Method::GetInfo, MethodAccessRule { access_rate: None });
    methods.insert(Method::GetBalance, MethodAccessRule { access_rate: None });
    methods.insert(Method::MakeInvoice, MethodAccessRule { access_rate: None });
    methods.insert(Method::PayInvoice, MethodAccessRule { access_rate: None });
    methods.insert(Method::PayKeysend, MethodAccessRule { access_rate: None });
    let mut control = HashMap::new();
    control.insert("list_channels".to_string(), MethodAccessRule { access_rate: None });
    control.insert("connect_peer".to_string(), MethodAccessRule { access_rate: None });
    let usage_profile = UsageProfile {
        quota: None,
        methods: Some(methods),
        control: Some(control),
    };

    let owner_keys = Keys::generate();
    common::grant_usage_profile(
        &owner_keys,
        &relay_url,
        service_pubkey,
        controller_pubkey,
        &usage_profile,
    )
    .await?;

    let nwc_client = Client::builder().signer(controller_keys).build();
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
    nwc_client
        .subscribe(
            Filter::new()
                .kind(Kind::Custom(ldk_controller::CONTROL_RESPONSE_KIND))
                .author(service_pubkey),
        )
        .await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    let relay = RelayUrl::parse(&relay_url)?;
    let uri =
        NostrWalletConnectUri::new(service_pubkey, vec![relay], controller_secret.clone(), None);

    let alice_info = send_nwc_request_and_wait_response(
        &nwc_client,
        &uri,
        service_pubkey,
        Method::GetInfo,
        Request::get_info(),
    )
    .await
    .to_get_info()
    .expect("get_info should decode");
    let alice_pubkey = alice_info.pubkey.expect("alice pubkey should be present");

    let bob_listen_port = free_local_port();
    let bob_cfg = LdkServiceConfig {
        network: "regtest".to_string(),
        bitcoind_rpc_host: bitcoind.rpc_host().to_string(),
        bitcoind_rpc_port: bitcoind.rpc_port(),
        bitcoind_rpc_user: bitcoind.rpc_user().to_string(),
        bitcoind_rpc_password: bitcoind.rpc_password().to_string(),
        ldk_storage_dir: format!("/tmp/{}", unique_id("e2e-bob-ldk")),
        ldk_listen_addr: Some(format!("127.0.0.1:{bob_listen_port}")),
    };
    let bob = LdkService::start_from_config(&bob_cfg).expect("start bob ldk");

    let bob_funding_addr = bob.new_onchain_address().expect("bob funding address");
    bitcoind.send_to_address(&bob_funding_addr, 0.05).await;
    bitcoind.mine_blocks(1, &miner_address).await;

    let sync_timeout = Duration::from_secs(20);
    let sync_start = tokio::time::Instant::now();
    loop {
        bob.sync_wallets().expect("bob sync");
        if bob.get_balance_msat().expect("bob balance") >= 5_000_000_000 {
            break;
        }
        assert!(sync_start.elapsed() <= sync_timeout, "bob funding timeout");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    bob.open_channel(
        &alice_pubkey,
        &format!("127.0.0.1:{published_alice_port}"),
        2_000_000,
        Some(1_000_000_000),
    )
    .expect("bob opens channel to alice");
    bitcoind.mine_blocks(1, &miner_address).await;

    let ready_timeout = Duration::from_secs(45);
    let ready_start = tokio::time::Instant::now();
    loop {
        bob.sync_wallets().expect("bob sync after open");
        let bob_ready = bob.has_ready_channel_with(&alice_pubkey);
        let list_response = send_control_request_and_wait_response(
            &nwc_client,
            &controller_secret,
            service_pubkey,
            json!({
                "method": "list_channels",
                "params": {}
            }),
        )
        .await;
        let bob_node_id = bob.node_id();
        let alice_ready = list_response["error"].is_null()
            && list_response["result"]
                .as_array()
                .map(|channels| {
                    channels.iter().any(|entry| {
                        entry["counterparty_pubkey"]
                            .as_str()
                            .map(|pk| pk == bob_node_id)
                            .unwrap_or(false)
                            && entry["is_channel_ready"].as_bool().unwrap_or(false)
                    })
                })
                .unwrap_or(false);
        if bob_ready && alice_ready {
            break;
        }
        assert!(
            ready_start.elapsed() <= ready_timeout,
            "channel did not become ready in time"
        );
        bitcoind.mine_blocks(1, &miner_address).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let connect_response = send_control_request_and_wait_response(
        &nwc_client,
        &controller_secret,
        service_pubkey,
        json!({
            "method": "connect_peer",
            "params": {
                "pubkey": bob.node_id(),
                "host": "host.docker.internal",
                "port": bob_listen_port
            }
        }),
    )
    .await;
    assert!(
        connect_response["error"].is_null(),
        "connect_peer should succeed before reverse payment: {:?}",
        connect_response
    );

    let make_invoice_response = send_nwc_request_and_wait_response(
        &nwc_client,
        &uri,
        service_pubkey,
        Method::MakeInvoice,
        Request::make_invoice(MakeInvoiceRequest {
            amount: 5_000_000,
            description: Some("bob-to-alice".to_string()),
            description_hash: None,
            expiry: Some(3600),
        }),
    )
    .await;
    assert!(
        make_invoice_response.error.is_none(),
        "make_invoice should succeed: {:?}",
        make_invoice_response.error
    );
    let alice_invoice = make_invoice_response
        .to_make_invoice()
        .expect("make_invoice response should decode")
        .invoice;
    bob.pay_invoice(&alice_invoice, None)
        .expect("bob pays alice invoice");

    // After Bob->Alice payment succeeds, Alice should have spendable channel balance.
    let balance_after_receive = send_nwc_request_and_wait_response(
        &nwc_client,
        &uri,
        service_pubkey,
        Method::GetBalance,
        Request::get_balance(),
    )
    .await;
    assert!(
        balance_after_receive.error.is_none(),
        "get_balance after receive should succeed: {:?}",
        balance_after_receive.error
    );

    let pay_response = send_nwc_request_and_wait_response(
        &nwc_client,
        &uri,
        service_pubkey,
        Method::PayKeysend,
        Request::pay_keysend(PayKeysendRequest {
            id: None,
            amount: 120_000,
            pubkey: bob.node_id(),
            preimage: None,
            tlv_records: vec![],
        }),
    )
    .await;
    assert!(
        pay_response.error.is_none(),
        "alice->bob pay_keysend should succeed: {:?}",
        pay_response.error
    );

    assert!(
        container_running(&controller_container.name),
        "ldk-controller should stay running after scenario"
    );

    bob.stop().expect("stop bob");
    Ok(())
}
