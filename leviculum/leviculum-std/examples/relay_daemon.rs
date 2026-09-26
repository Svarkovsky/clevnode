//! Transport relay daemon example
//!
//! This example demonstrates a Rust Reticulum node acting as a transport relay.
//! It connects to remote Reticulum nodes as a TCP client and routes traffic
//! between all connected peers.
//!
//! # Usage
//!
//! ```sh
//! # Connect to a peer at 127.0.0.1:4242
//! cargo run --example relay_daemon -- 127.0.0.1:4242
//! ```

use leviculum_std::driver::ReticulumNodeBuilder;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:4242".to_string());

    let peer: std::net::SocketAddr = addr.parse()?;

    // Build a transport-enabled node that connects to the peer
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(peer)
        .build()
        .await?;

    node.start().await?;
    println!("Relay daemon connected to {}", peer);

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    println!("\nShutting down...");

    let stats = node.transport_stats();
    println!(
        "Stats: received={}, forwarded={}, sent={}",
        stats.packets_received(),
        stats.packets_forwarded(),
        stats.packets_sent()
    );

    node.stop().await?;
    println!("Relay daemon stopped");

    Ok(())
}
