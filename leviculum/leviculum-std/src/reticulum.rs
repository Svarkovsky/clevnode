//! Main Reticulum instance
//!
//! High-level entry point that wires together configuration, storage,
//! core NodeCore, and the async runtime (via `ReticulumNode`).

use crate::config::Config;
use crate::driver::{EventReceiver, ReticulumNode, ReticulumNodeBuilder};
use crate::error::Result;

/// Main Reticulum instance
///
/// Wraps a `ReticulumNode` with configuration-driven setup.
pub struct Reticulum {
    /// Configuration
    config: Config,
    /// The underlying node
    node: ReticulumNode,
}

impl Reticulum {
    /// Create a new Reticulum instance with default configuration
    pub fn new() -> Result<Self> {
        let config_path = Config::default_config_path();
        let config = if config_path.exists() {
            Config::load(&config_path)?
        } else {
            Config::default()
        };

        Self::with_config(config)
    }

    /// Create a new Reticulum instance with custom configuration
    ///
    /// The builder reads enable_transport and interface configurations
    /// from the provided config automatically.
    pub fn with_config(config: Config) -> Result<Self> {
        let builder = ReticulumNodeBuilder::new().config(config.clone());

        Ok(Self {
            config,
            node: builder.build_sync()?,
        })
    }

    /// Create a new Reticulum instance in daemon-mode.
    ///
    /// Identical to `with_config` except the application event channel
    /// is not constructed. Use this for daemon-style processes (`lnsd`)
    /// that have no application code consuming `NodeEvent`s. Forwarding
    /// (broadcasts, directed sends, local-client routing) is unaffected,
    /// it runs entirely on `output.actions`.
    ///
    /// `resource_window_policy` selects the resource receive-window
    /// adaptation algorithm (Codeberg #85); the daemon binary reads it from
    /// the `LEVICULUM_RESOURCE_WINDOW_POLICY` environment variable via
    /// [`crate::resource_policy::resource_window_policy_from_env`].
    ///
    /// After this constructor, `take_event_receiver()` returns `None`.
    pub fn with_config_daemon(
        config: Config,
        resource_window_policy: leviculum_core::resource::WindowPolicy,
    ) -> Result<Self> {
        let builder = ReticulumNodeBuilder::new()
            .config(config.clone())
            .resource_window_policy(resource_window_policy)
            .without_events();

        Ok(Self {
            config,
            node: builder.build_sync()?,
        })
    }

    /// Start the Reticulum instance (spawns the event loop)
    pub async fn start(&mut self) -> Result<()> {
        self.node.start().await?;
        Ok(())
    }

    /// Stop the Reticulum instance
    pub async fn stop(&mut self) -> Result<()> {
        self.node.stop().await?;
        tracing::info!("Reticulum stopped");
        Ok(())
    }

    /// Check if the instance is running
    pub fn is_running(&self) -> bool {
        self.node.is_running()
    }

    /// Get the configuration
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Check if transport mode is enabled
    pub fn is_transport_enabled(&self) -> bool {
        self.node.is_transport_enabled()
    }

    /// Return a diagnostic dump of memory usage including process RSS
    pub fn diagnostic_dump(&self) -> String {
        let mut dump = self.node.diagnostic_dump();
        // Read RSS from /proc/self/statm (Linux only)
        if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
            if let Some(rss_pages) = statm.split_whitespace().nth(1) {
                if let Ok(pages) = rss_pages.parse::<u64>() {
                    let rss_bytes = pages * 4096;
                    dump.push_str(&format!("=== Process RSS: {} bytes ===\n", rss_bytes));
                }
            }
        }
        dump
    }

    /// Take the event receiver (can only be called once)
    pub fn take_event_receiver(&mut self) -> Option<EventReceiver> {
        self.node.take_event_receiver()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_instance() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.reticulum.storage_path = Some(td.path().to_path_buf());
        let mut rns = Reticulum::with_config(config).unwrap();

        // Start the node
        rns.start().await.unwrap();
        assert!(rns.is_running());
        assert!(rns.is_transport_enabled());

        // Can take event receiver
        let rx = rns.take_event_receiver();
        assert!(rx.is_some());
        assert!(rns.take_event_receiver().is_none()); // Second call returns None

        rns.stop().await.unwrap();
        assert!(!rns.is_running());
    }

    #[tokio::test]
    async fn test_transport_disabled_via_config() {
        let td = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.reticulum.enable_transport = false;
        config.reticulum.storage_path = Some(td.path().to_path_buf());
        let rns = Reticulum::with_config(config).unwrap();
        assert!(!rns.is_transport_enabled());
    }
}
