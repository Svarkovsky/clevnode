//! Simple send/receive example
//!
//! This example demonstrates basic usage of the ReticulumNode API
//! for sending and receiving messages.
//!
//! # Usage
//!
//! This example requires a running Reticulum daemon (rnsd) to connect to.
//! Start rnsd first, then run:
//!
//! ```sh
//! cargo run --example simple_send
//! ```

use std::time::Duration;

use leviculum_std::driver::ReticulumNodeBuilder;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    println!("Simple Send Example");
    println!("===================");
    println!();

    // Build a node with a TCP interface to a local daemon
    println!("Building node...");
    let mut node = ReticulumNodeBuilder::new()
        .add_tcp_client("127.0.0.1:4242".parse()?)
        .build()
        .await?;

    println!("Starting node...");
    node.start().await?;

    // Take the event receiver to process events
    let mut events = node
        .take_event_receiver()
        .ok_or("Failed to get event receiver")?;

    println!("Node started! Waiting for events...");
    println!();

    // Process events for a limited time
    let timeout = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            println!("Received event: {:?}", event);
        }
    });

    match timeout.await {
        Ok(_) => println!("Event stream ended"),
        Err(_) => println!("Timeout reached (10 seconds)"),
    }

    // Stop the node gracefully
    println!();
    println!("Stopping node...");
    node.stop().await?;

    println!("Done!");
    Ok(())
}
