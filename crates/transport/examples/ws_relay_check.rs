//! Manual check: does a full Noise session establish over the WebSocket
//! relay path (`park_as_host_ws` / `connect_as_client_ws`)? Point this at a
//! local `wrangler dev` instance of `cf-worker` before running:
//!
//! ```text
//! cd cf-worker && npx wrangler dev --port 8787
//! cargo run -p rc-transport --example ws_relay_check -- ws://127.0.0.1:8787/relay
//! ```
use rc_crypto::DeviceIdentity;
use rc_transport::lan::{ClientAuth, LanSession};
use rc_transport::relay;
use rc_transport::Session as _;

fn secret(seed: &[u8]) -> [u8; 32] {
    let mut s = [0u8; 32];
    s[..seed.len().min(32)].copy_from_slice(&seed[..seed.len().min(32)]);
    *DeviceIdentity::from_seed(&s).unwrap().x25519_secret()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ws://127.0.0.1:8787/relay".to_string());
    let device_id = "WS-CHECK-DEVICE";
    let code = "424242";

    let host_sk = secret(b"host-seed");
    let client_sk = secret(b"client-seed");

    let host_url = url.clone();
    let host_task = tokio::spawn(async move {
        let stream = relay::park_as_host_ws(&host_url, device_id, None)
            .await
            .expect("host park failed");
        println!("[host] parked + spliced, running Noise responder...");
        let sess = LanSession::over_stream_responder(stream, &host_sk, "424242")
            .await
            .expect("host handshake failed");
        println!("[host] session established! peer_key={:02x?}", sess.peer_key());
        sess.control().send(b"ping from host".to_vec()).await.unwrap();
        let reply = sess.control().recv().await.unwrap();
        println!("[host] got reply: {}", String::from_utf8_lossy(&reply));
    });

    // give the host a moment to park before the client asks for it
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let stream = relay::connect_as_client_ws(&url, device_id, None)
        .await
        .expect("client connect failed");
    println!("[client] spliced, running Noise initiator...");
    let sess = LanSession::over_stream_initiator(
        stream,
        &client_sk,
        ClientAuth::Pair {
            code: code.to_string(),
        },
    )
    .await
    .expect("client handshake failed");
    println!(
        "[client] session established! peer_key={:02x?} was_pairing={}",
        sess.peer_key(),
        sess.was_pairing()
    );
    let msg = sess.control().recv().await.unwrap();
    println!("[client] got: {}", String::from_utf8_lossy(&msg));
    sess.control()
        .send(b"pong from client".to_vec())
        .await
        .unwrap();

    host_task.await.unwrap();
    println!("ALL WS RELAY CHECKS PASSED");
}
