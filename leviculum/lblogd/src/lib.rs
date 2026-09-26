//! `lblogd`: a dev-blog server that serves Markdown posts over HTTP/HTTPS and
//! as a NomadNet node over Reticulum.
//!
//! The [`post`] module holds the post/content model (frontmatter parsing,
//! slugs, directory loading), the [`render`] module the Markdown to HTML and
//! Markdown to Micron renderers plus the index and post page templates. The
//! [`content`] module owns the one snapshot both servers read from and the
//! reload path that replaces it, and [`files`] the file area a post's images
//! come from — the same files under `/file/` on the mesh and `/files/` on the
//! web. The [`node`] module is the NomadNet page
//! node: a shared-instance client of a running `lnsd` daemon that serves the
//! rendered Micron pages over Reticulum. The [`web`] module is the clearnet
//! side: an axum server that serves the rendered HTML over HTTP and
//! automatic-HTTPS (Let's Encrypt via rustls-acme). The [`counter`] module
//! holds the per-day request counter both sides feed, and the append-only
//! file it writes — requests and links, named as such, because neither side
//! can honestly count readers. The [`config`] module
//! maps the single TOML config file onto the node and web configs, and
//! [`cli`] holds the binary's argument parser.

pub mod cli;
pub mod config;
pub mod content;
pub mod counter;
pub mod files;
pub mod node;
pub mod post;
pub mod render;
pub mod watcher;
pub mod web;
