use std::sync::{Mutex, OnceLock};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
    GenericImage,
};

/// Starts a fresh strfry relay container and returns (container, relay_url).
/// The container is automatically removed when dropped.
pub async fn start_relay() -> (testcontainers::ContainerAsync<GenericImage>, String) {
    let container = GenericImage::new("strfry-strfry", "latest")
        // strfry listens on port 7777 inside the container
        .with_exposed_port(7777.tcp())
        // Wait until strfry logs that the websocket server is ready
        .with_wait_for(WaitFor::message_on_stderr("Started websocket server"))
        .start()
        .await
        .expect("Failed to start strfry container");

    // testcontainers maps the container's port to a random host port
    let host_port = container
        .get_host_port_ipv4(7777)
        .await
        .expect("Failed to get mapped port");

    let relay_url = format!("ws://localhost:{}", host_port);
    println!("Strfry relay started on {}", relay_url);

    (container, relay_url)
}

pub fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("test lock poisoned")
}
