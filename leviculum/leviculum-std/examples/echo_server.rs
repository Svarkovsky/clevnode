//! Echo server example
//!
//! This example demonstrates how to handle incoming connections
//! and echo data back to the sender.
//!
//! # Usage
//!
//! This example requires a running Reticulum daemon (rnsd) to connect to.
//! Start rnsd first, then run:
//!
//! ```sh
//! cargo run --example echo_server
//! ```

use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::NodeEvent;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    println!("Echo Server Example");
    println!("===================");
    println!();

    // Build a node with a TCP interface
    println!("Building echo server node...");
    let mut node = ReticulumNodeBuilder::new()
        .add_tcp_client("127.0.0.1:4242".parse()?)
        .build()
        .await?;

    // Start the node
    println!("Starting node...");
    node.start().await?;

    // Take the event receiver
    let mut events = node
        .take_event_receiver()
        .ok_or("Failed to get event receiver")?;

    println!("Echo server running! Waiting for connections...");
    println!("Press Ctrl+C to stop.");
    println!();

    // Handle events
    loop {
        tokio::select! {
            Some(event) = events.recv() => {
                match event {
                    NodeEvent::LinkEstablished { link_id, is_initiator, .. } => {
                        // Incoming links are accepted and proved automatically by
                        // the core (Python parity). The app just observes this
                        // event; for an incoming link (is_initiator == false) it can
                        // obtain a writable handle with node.link_handle(&link_id).
                        println!("Connection established!");
                        println!("  Link ID: {:02x?}", &link_id.as_bytes()[..4]);
                        println!("  We initiated: {}", is_initiator);
                    }
                    NodeEvent::LinkDataReceived { link_id, data } => {
                        println!("Data received on link {:02x?}", &link_id.as_bytes()[..4]);
                        println!("  {} bytes: {:?}", data.len(), String::from_utf8_lossy(&data));
                        // Echo the data back
                        // In a real application, you would send the data back here
                        println!("  (Would echo back in a real implementation)");
                    }
                    NodeEvent::MessageReceived { link_id, msgtype, sequence, data } => {
                        println!("Message received on link {:02x?}", &link_id.as_bytes()[..4]);
                        println!("  Type: 0x{:04x}, Seq: {}", msgtype, sequence);
                        println!("  {} bytes: {:?}", data.len(), String::from_utf8_lossy(&data));
                    }
                    NodeEvent::LinkClosed { link_id, reason, .. } => {
                        println!("Connection closed: {:02x?}", &link_id.as_bytes()[..4]);
                        println!("  Reason: {:?}", reason);
                    }
                    NodeEvent::AnnounceReceived { announce, interface_index } => {
                        println!("Announce received on interface {}", interface_index);
                        println!("  From: {:02x?}", &announce.destination_hash().as_bytes()[..4]);
                        if let Some(app_data) = announce.app_data_string() {
                            println!("  App data: {}", app_data);
                        }
                    }
                    NodeEvent::PathFound { destination_hash, hops, interface_index } => {
                        println!("Path found to {:02x?}", &destination_hash.as_bytes()[..4]);
                        println!("  Hops: {}, Interface: {}", hops, interface_index);
                    }
                    other => {
                        println!("Other event: {:?}", other);
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!();
                println!("Shutting down...");
                break;
            }
        }
    }

    // Stop the node
    node.stop().await?;
    println!("Echo server stopped.");

    Ok(())
}
