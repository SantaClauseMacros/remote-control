//! Parks as a host and just sits there — used to verify the ping/pong
//! keepalive actually keeps a Cloudflare-Worker-relayed connection alive
//! past whatever idle timeout would otherwise kill it silently.
use rc_transport::relay;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "wss://remote-control.bloxvault8436200.workers.dev/relay".to_string());
    let device_id = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "PARK-HOLD-TEST".to_string());
    println!("parking as {device_id} at {url} ... (Ctrl+C to stop)");
    match relay::park_as_host_ws(&url, &device_id, None).await {
        Ok(_) => println!("a client connected!"),
        Err(e) => println!("park failed: {e}"),
    }
}
