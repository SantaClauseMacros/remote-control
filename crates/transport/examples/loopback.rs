//! Exercises the transport end to end in one process: XXpsk0 first pairing,
//! control round-trip, a large "video" frame, RTT, a code-less *resume* with
//! the pinned key, and rejection of a wrong code / a bogus host key.
//!
//! ```text
//! cargo run -p rc-transport --example loopback
//! ```

use std::time::Duration;

use rc_transport::lan::{ClientAuth, LanListener, LanSession};
use rc_transport::{EncodedFrame, Session};

fn secret(seed: u8) -> [u8; 32] {
    let mut k = [seed; 32];
    k[0] ^= 0x55;
    k
}
fn short(k: &[u8; 32]) -> String {
    k.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

    let host_sk = secret(1);
    let client_sk = secret(2);
    let client_pub = rc_crypto::x25519_public_from_secret(&client_sk);
    let code = "482193";

    let listener = LanListener::bind("127.0.0.1:0", code, host_sk).await?;
    let addr = listener.local_addr()?;
    println!("listening on {addr}");

    // ── 1. first pairing (with the code) ──────────────────────────────────
    let (client, host) = tokio::join!(
        LanSession::connect(addr, &client_sk, ClientAuth::Pair { code: code.into() }),
        listener.accept(),
    );
    let client = client?;
    let (host, _peer) = host?;
    assert!(host.was_pairing() && client.was_pairing());
    assert_eq!(host.peer_key(), client_pub);
    let host_key = client.peer_key();
    println!("[pair] client pinned host {}, host saw client {}", short(&host_key), short(&host.peer_key()));

    client.control().send(b"ping".to_vec()).await?;
    assert_eq!(host.control().recv().await?, b"ping");
    host.control().send(b"pong".to_vec()).await?;
    assert_eq!(client.control().recv().await?, b"pong");

    host.video()
        .send(EncodedFrame { data: vec![0xAB; 400 * 1024], key_frame: true, timestamp_us: 42 })
        .await?;
    let f = client.video().recv().await?;
    assert!(f.data.len() == 400 * 1024 && f.data.iter().all(|&b| b == 0xAB));
    println!("[pair] control + {} KB video intact", f.data.len() / 1024);

    tokio::time::sleep(Duration::from_secs(4)).await;
    // Both ends, deliberately. Keepalives used to be originated by the
    // initiator alone, and since the reply is what fills in the *sender's*
    // rtt_ms, the host's reading sat at zero forever — checking only the
    // client here is why that went unnoticed. The host is the end whose
    // congestion control steers by this number.
    let (c_rtt, h_rtt) = (client.video().feedback().rtt_ms, host.video().feedback().rtt_ms);
    println!("[pair] rtt client={c_rtt:.2}ms host={h_rtt:.2}ms");
    assert!(c_rtt > 0.0, "client measured no RTT");
    assert!(h_rtt > 0.0, "host measured no RTT");
    drop((client, host));

    // ── 2. resume with NO code, using the pinned key ─────────────────────
    let (resumed, host2) = tokio::join!(
        LanSession::connect(addr, &client_sk, ClientAuth::Resume { host_key }),
        listener.accept(),
    );
    let resumed = resumed?;
    let (host2, _) = host2?;
    assert!(!resumed.was_pairing() && !host2.was_pairing());
    assert_eq!(host2.peer_key(), client_pub);
    println!("[resume] code-less reconnect OK");
    drop((resumed, host2));

    // ── 3. resume with a bogus host key is rejected ──────────────────────
    let (bad, _srv) = tokio::join!(
        LanSession::connect(addr, &client_sk, ClientAuth::Resume { host_key: secret(9) }),
        listener.accept(),
    );
    match bad {
        Err(e) => println!("[reject] wrong host key: {e}"),
        Ok(_) => panic!("resume with a bogus host key was accepted"),
    }

    // ── 4. first pairing with the wrong code is rejected ─────────────────
    let l4 = LanListener::bind("127.0.0.1:0", "000000", host_sk).await?;
    let a4 = l4.local_addr()?;
    let acc = tokio::spawn(async move { l4.accept().await });
    match LanSession::connect(a4, &client_sk, ClientAuth::Pair { code: "999999".into() }).await {
        Err(e) => println!("[reject] wrong pairing code: {e}"),
        Ok(_) => panic!("wrong pairing code was accepted"),
    }
    assert!(acc.await?.is_err());

    println!("\nALL TRANSPORT CHECKS PASSED");
    Ok(())
}
