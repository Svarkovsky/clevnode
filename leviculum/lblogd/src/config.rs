//! The `lblogd` config file: one TOML file drives both the NomadNet node and
//! the web server.
//!
//! ```toml
//! data_dir  = "/var/lib/lblogd"
//! posts_dir = "/var/lib/lblogd/posts"
//! files_dir = "/var/lib/lblogd/files"   # optional, this is the default
//!
//! [node]
//! instance_name          = "leviculum"
//! display_name           = "leviculum.network dev blog"
//! announce_interval_secs = 21600        # optional
//!
//! [web]
//! domains            = ["leviculum.network"]
//! acme_contact_email = "you@example.org"
//! acme_staging       = true
//! http_bind          = "0.0.0.0:80"     # optional, this is the default
//! https_bind         = "0.0.0.0:443"    # optional, this is the default
//! ```
//!
//! For a local development run, set `acme = false` in `[web]`. The HTTP
//! listener then serves the blog directly and no HTTPS listener is opened,
//! which is the only way to run the server on a machine without a publicly
//! reachable domain:
//!
//! ```toml
//! [web]
//! acme      = false
//! http_bind = "127.0.0.1:8080"
//! ```
//!
//! The per-day request counter is on by default and needs no configuration.
//! `[counter]` exists to move it or switch it off:
//!
//! ```toml
//! [counter]
//! enabled = true                        # optional, this is the default
//! path    = "/var/lib/lblogd/counts.log" # optional, this is the default
//! ```
//!
//! [`Config::blog_node_config`] and [`Config::web_config`] map the file onto
//! the two component configs; the ACME cache directory is derived as
//! `<data_dir>/acme` rather than configured separately, and the counter file
//! from [`Config::counter_path`] the same way.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

use crate::content::Sources;
use crate::counter;
use crate::files::{self, FileArea};
use crate::node::BlogNodeConfig;
use crate::render::BlogMeta;
use crate::web::{AcmeSettings, WebConfig};

/// Errors from loading the config file.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Reading the file failed (missing file, permissions).
    #[error("reading config {path}: {source}")]
    Read {
        /// The config file path.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The file is not valid TOML or is missing a required field.
    #[error("config {path}: {source}")]
    Parse {
        /// The config file path.
        path: String,
        /// The TOML error, which names the offending field or line.
        source: toml::de::Error,
    },
    /// The file parses but the field combination is not usable, e.g. ACME is
    /// enabled without the fields it needs.
    #[error("config {path}: {message}")]
    Invalid {
        /// The config file path.
        path: String,
        /// What is wrong, naming the offending field.
        message: String,
    },
}

/// The parsed config file.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Base data directory: identities, node storage, and the ACME cache
    /// live under it.
    pub data_dir: PathBuf,
    /// Directory of Markdown posts, served by both the node and the web
    /// server.
    pub posts_dir: PathBuf,
    /// Directory of files a post can reference: pictures, mostly.
    ///
    /// Defaults to a `files` sibling of [`posts_dir`](Self::posts_dir). The
    /// directory need not exist; without it the blog simply has no file area.
    /// Served as `/file/<name>` on the mesh and `/files/<name>` on the web
    /// (see [`crate::files`]).
    #[serde(default)]
    pub files_dir: Option<PathBuf>,
    /// The largest single file that will be served, in bytes.
    ///
    /// Defaults to [`crate::files::DEFAULT_MAX_FILE_BYTES`] (10 MiB). A blog is not a file server, and an unbounded transfer over
    /// a LoRa interface denies service to every other reader of the node for
    /// as long as it runs.
    #[serde(default)]
    pub max_file_bytes: Option<u64>,
    /// Reload the posts automatically whenever `posts_dir` changes, instead
    /// of only on SIGHUP.
    ///
    /// Off by default: a deployment publishes deliberately, and reacting to
    /// every write means reacting to half-finished ones too (the watcher
    /// debounces, but the exposure is real). Convenient while writing, which
    /// is what a development config wants.
    #[serde(default)]
    pub watch_posts: bool,
    /// Blog identity: what a reader sees on every page, on both sides.
    #[serde(default)]
    pub blog: BlogSection,
    /// The per-day request counter. The whole section may be omitted, which
    /// is the default: counting on, into `<data_dir>/counts.log`.
    #[serde(default)]
    pub counter: CounterSection,
    /// NomadNet node settings.
    pub node: NodeSection,
    /// Web server settings.
    pub web: WebSection,
}

/// The `[blog]` section: the things a reader needs to know about the blog
/// itself, as opposed to the machinery serving it.
///
/// Every field is optional and the whole section may be left out, in which
/// case the title falls back to `[node] display_name` and the rest is simply
/// not rendered. That keeps configurations written before this section
/// existed working unchanged.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BlogSection {
    /// The blog's name, shown as the heading of every page. Defaults to
    /// `[node] display_name`.
    pub title: Option<String>,
    /// Who writes it. Shown under the title and on each post, unless a post
    /// names its own author.
    pub author: Option<String>,
    /// One sentence on what the blog is about, shown under the title and used
    /// as the HTML meta description.
    pub description: Option<String>,
    /// BCP 47 language tag for the HTML `lang` attribute, e.g. `de`. Wrong
    /// values mislead screen readers and translation offers, so this defaults
    /// to English only because something has to be emitted.
    #[serde(default = "default_language")]
    pub language: String,
    /// Contact address for the about page, rendered as a `mailto:` link.
    ///
    /// A public address gets harvested; setting this is a deliberate choice.
    /// It is deliberately separate from `web.acme_contact_email`, which is an
    /// operator contact for certificate warnings and has no business on a
    /// public page.
    pub email: Option<String>,
    /// LXMF destination hash for the about page, 32 hex characters.
    ///
    /// On the NomadNet side it becomes a `lxmf@<hash>` link, which opens a
    /// conversation with that address.
    pub lxmf: Option<String>,
    /// A Markdown file in the same format as a post, shown on the about page.
    ///
    /// Deliberately not a file in `posts_dir`: a post that is silently kept
    /// out of the index and the feed would be a hidden special case, and it
    /// would collide with a real post that happened to be named `about.md`.
    pub about: Option<PathBuf>,
    /// A stylesheet to use instead of the built-in one. Read at startup and
    /// on every reload, and inlined into each page.
    pub css: Option<PathBuf>,
}

/// How many hex characters an LXMF destination hash has.
///
/// A Reticulum destination hash is `TRUNCATED_HASHLENGTH` (128) bits, and
/// NomadNet rejects a link whose target is any other length.
const LXMF_HASH_HEX_LEN: usize = leviculum_core::constants::TRUNCATED_HASHBYTES * 2;

fn default_language() -> String {
    "en".to_string()
}

/// Hand-written rather than derived so that an omitted `[blog]` section gets
/// the same language default as an empty one; `#[derive(Default)]` would
/// leave the tag blank and emit `lang=""`.
impl Default for BlogSection {
    fn default() -> BlogSection {
        BlogSection {
            title: None,
            author: None,
            description: None,
            language: default_language(),
            email: None,
            lxmf: None,
            about: None,
            css: None,
        }
    }
}

/// The `[counter]` section: where the per-day request counts are written,
/// and whether they are written at all.
///
/// Two keys, both optional, because the counter has no other decisions to
/// expose: what it counts is fixed by what the two sides can honestly see
/// (see [`crate::counter`]), and the flush cadence is a durability constant,
/// not a preference.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CounterSection {
    /// Whether to count at all. On by default: the file holds request totals
    /// and no addresses, it costs about a hundred bytes a day, and a blog
    /// server that cannot say how much it served is the odd default.
    #[serde(default = "default_counter_enabled")]
    pub enabled: bool,
    /// Where to write. Defaults to
    /// [`counter::DEFAULT_FILE_NAME`] under `data_dir`, which is already the
    /// directory `lblogd` owns its state in.
    pub path: Option<PathBuf>,
}

fn default_counter_enabled() -> bool {
    true
}

/// Hand-written for the same reason [`BlogSection`]'s is: an omitted section
/// must behave exactly like an empty one, and `#[derive(Default)]` would make
/// the omitted case mean `enabled = false`.
impl Default for CounterSection {
    fn default() -> CounterSection {
        CounterSection {
            enabled: default_counter_enabled(),
            path: None,
        }
    }
}

/// The `[node]` section.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NodeSection {
    /// Shared instance name of the running `lnsd` daemon; must match the
    /// daemon's `instance_name`.
    pub instance_name: String,
    /// Display name announced over Reticulum. Defaults to `[blog] title`,
    /// since the name a reader sees and the name the mesh sees are normally
    /// the same; set it only when they should differ.
    pub display_name: Option<String>,
    /// Re-announce cadence in seconds; defaults to
    /// [`BlogNodeConfig::default_announce_interval`].
    pub announce_interval_secs: Option<u64>,
}

/// The `[web]` section.
///
/// The three ACME fields are optional at the TOML level but required once
/// [`acme`](Self::acme) is on; [`Config::load`] rejects a config that turns
/// ACME on without them. Keeping them out of serde's required set is what
/// lets a development config omit them entirely instead of filling in
/// meaningless placeholders.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebSection {
    /// Obtain HTTPS certificates from Let's Encrypt. Defaults to `true`, so
    /// existing deployment configs keep working unchanged; set it to `false`
    /// for a local development run on a machine with no publicly reachable
    /// domain.
    #[serde(default = "default_acme")]
    pub acme: bool,
    /// Domains the HTTPS certificate covers. Required when `acme` is on.
    #[serde(default)]
    pub domains: Vec<String>,
    /// Contact email for the ACME account. Required when `acme` is on.
    pub acme_contact_email: Option<String>,
    /// Use the Let's Encrypt staging directory (untrusted test certificates,
    /// generous rate limits). Required when `acme` is on, so production stays
    /// a deliberate choice.
    pub acme_staging: Option<bool>,
    /// Plain HTTP listen address. Redirect-only when `acme` is on, the blog
    /// itself when it is off.
    #[serde(default = "default_http_bind")]
    pub http_bind: SocketAddr,
    /// HTTPS listen address. Unused, and never bound, when `acme` is off.
    #[serde(default = "default_https_bind")]
    pub https_bind: SocketAddr,
}

fn default_acme() -> bool {
    true
}

fn default_http_bind() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 80))
}

fn default_https_bind() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 443))
}

impl Config {
    /// Load, parse and validate the config file at `path`.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        config.validate(path)?;
        Ok(config)
    }

    /// Reject field combinations serde cannot express: with ACME on, the
    /// three certificate fields are mandatory. Without it they are ignored,
    /// so their absence is not an error.
    fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        let invalid = |message: &str| ConfigError::Invalid {
            path: path.display().to_string(),
            message: message.to_string(),
        };
        // The two names default to each other, so at least one has to exist:
        // the blog needs a heading and the announce needs an app_data.
        if self.blog.title.is_none() && self.node.display_name.is_none() {
            return Err(invalid(
                "set blog.title (or node.display_name): the blog needs a name",
            ));
        }
        // A malformed hash would produce a link NomadNet refuses with
        // "Invalid length for LXMF link"; better to say so at startup.
        if let Some(lxmf) = &self.blog.lxmf {
            if lxmf.len() != LXMF_HASH_HEX_LEN || !lxmf.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(invalid(&format!(
                    "blog.lxmf must be {LXMF_HASH_HEX_LEN} hex characters, \
                     got {:?}",
                    lxmf
                )));
            }
        }
        if !self.web.acme {
            return Ok(());
        }
        if self.web.domains.is_empty() {
            return Err(invalid(
                "web.domains must list at least one domain when web.acme is true",
            ));
        }
        if self.web.acme_contact_email.is_none() {
            return Err(invalid(
                "web.acme_contact_email is required when web.acme is true",
            ));
        }
        if self.web.acme_staging.is_none() {
            return Err(invalid(
                "web.acme_staging is required when web.acme is true: \
                 set it to true for Let's Encrypt staging, false for production",
            ));
        }
        Ok(())
    }

    /// The blog's name: `[blog] title`, else `[node] display_name`.
    ///
    /// `validate` guarantees one of them is set, so the fallback is never the
    /// empty string for a config that came through [`Config::load`].
    pub fn blog_title(&self) -> String {
        self.blog
            .title
            .clone()
            .or_else(|| self.node.display_name.clone())
            .unwrap_or_default()
    }

    /// What every page says about the blog.
    ///
    /// `nomadnet_address` is the destination hash, which the caller resolves
    /// from the persistent identity; passing it in rather than deriving it
    /// here keeps this free of filesystem access.
    pub fn blog_meta(&self, nomadnet_address: Option<String>) -> BlogMeta {
        BlogMeta {
            title: self.blog_title(),
            author: self.blog.author.clone(),
            description: self.blog.description.clone(),
            language: self.blog.language.clone(),
            // The first configured domain is the blog's canonical name,
            // whether or not certificates are being fetched for it right now.
            // Tying this to `acme` would mean the feed and the cross-links
            // could never be checked before deployment: with ACME off there
            // would be no URL, and with it on the HTTP listener only
            // redirects. A development run that sets `domains` gets the real
            // links; one that does not gets none, and no feed.
            web_url: self.web.domains.first().map(|d| format!("https://{d}")),
            nomadnet_address,
            email: self.blog.email.clone(),
            lxmf: self.blog.lxmf.clone(),
            has_about: self.has_about_page(),
        }
    }

    /// Whether there is an about page to link to.
    ///
    /// It exists as soon as there is anything to show: a contact address, an
    /// LXMF hash, or a text file. Linking the author's name to a page with
    /// nothing on it would be worse than not linking it.
    pub fn has_about_page(&self) -> bool {
        self.blog.email.is_some() || self.blog.lxmf.is_some() || self.blog.about.is_some()
    }

    /// Where the content comes from: the posts directory plus the optional
    /// stylesheet, about text and file area.
    ///
    /// The file area is always configured, because it is optional by
    /// existence rather than by declaration: a blog that has no `files`
    /// directory simply serves no files, and one that grows one later starts
    /// serving them on the next reload without a config change.
    pub fn content_sources(&self) -> Sources {
        let dir = self
            .files_dir
            .clone()
            .unwrap_or_else(|| files::default_files_dir(&self.posts_dir));
        Sources::new(&self.posts_dir)
            .with_css(self.blog.css.clone())
            .with_about(self.blog.about.clone())
            .with_files(Some(FileArea {
                dir,
                max_bytes: self.max_file_bytes.unwrap_or(files::DEFAULT_MAX_FILE_BYTES),
            }))
    }

    /// The file the per-day counts are appended to, or `None` when counting
    /// is switched off.
    pub fn counter_path(&self) -> Option<PathBuf> {
        self.counter.enabled.then(|| {
            self.counter
                .path
                .clone()
                .unwrap_or_else(|| self.data_dir.join(counter::DEFAULT_FILE_NAME))
        })
    }

    /// The NomadNet node config this file describes.
    pub fn blog_node_config(&self) -> BlogNodeConfig {
        BlogNodeConfig {
            instance_name: self.node.instance_name.clone(),
            data_dir: self.data_dir.clone(),
            display_name: self
                .node
                .display_name
                .clone()
                .unwrap_or_else(|| self.blog_title()),
            announce_interval: self
                .node
                .announce_interval_secs
                .map(Duration::from_secs)
                .unwrap_or_else(BlogNodeConfig::default_announce_interval),
        }
    }

    /// The web server config this file describes. `acme: None` selects the
    /// plaintext development mode.
    ///
    /// The fallbacks on the two ACME options are unreachable for a config
    /// that came through [`Config::load`], which rejects `acme = true`
    /// without them.
    pub fn web_config(&self) -> WebConfig {
        WebConfig {
            acme: self.web.acme.then(|| AcmeSettings {
                domains: self.web.domains.clone(),
                cache_dir: self.data_dir.join("acme"),
                contact_email: self.web.acme_contact_email.clone().unwrap_or_default(),
                staging: self.web.acme_staging.unwrap_or(true),
            }),
            http_bind: self.web.http_bind,
            https_bind: self.web.https_bind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        data_dir  = "/var/lib/lblogd"
        posts_dir = "/var/lib/lblogd/posts"

        [node]
        instance_name          = "leviculum"
        display_name           = "leviculum.network dev blog"
        announce_interval_secs = 3600

        [web]
        domains            = ["leviculum.network", "www.leviculum.network"]
        acme_contact_email = "ops@example.org"
        acme_staging       = true
        http_bind          = "127.0.0.1:8080"
        https_bind         = "127.0.0.1:8443"
    "#;

    /// A development config: no ACME, so none of the certificate fields.
    const DEV_SAMPLE: &str = r#"
        data_dir  = "/tmp/lblogd-dev"
        posts_dir = "/tmp/lblogd-dev/posts"

        [node]
        instance_name = "lblogd-dev"
        display_name  = "dev blog"

        [web]
        acme      = false
        http_bind = "127.0.0.1:8080"
    "#;

    /// Write `text` to a temp file and run it through the real [`Config::load`],
    /// which is what applies [`Config::validate`].
    fn load_from_str(text: &str) -> Result<Config, ConfigError> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lblogd.toml");
        std::fs::write(&path, text).unwrap();
        Config::load(&path)
    }

    #[test]
    fn sample_parses() {
        let config: Config = toml::from_str(SAMPLE).unwrap();
        assert_eq!(config.data_dir, PathBuf::from("/var/lib/lblogd"));
        assert_eq!(config.posts_dir, PathBuf::from("/var/lib/lblogd/posts"));
        assert_eq!(config.node.instance_name, "leviculum");
        assert_eq!(
            config.node.display_name.as_deref(),
            Some("leviculum.network dev blog")
        );
        assert_eq!(config.node.announce_interval_secs, Some(3600));
        assert_eq!(
            config.web.domains,
            vec!["leviculum.network", "www.leviculum.network"]
        );
        assert_eq!(
            config.web.acme_contact_email.as_deref(),
            Some("ops@example.org")
        );
        assert_eq!(config.web.acme_staging, Some(true));
        assert_eq!(config.web.http_bind, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.web.https_bind, "127.0.0.1:8443".parse().unwrap());
    }

    #[test]
    fn the_blog_section_may_be_omitted_entirely() {
        // SAMPLE predates the section, which is exactly the case that has to
        // keep working.
        assert!(!SAMPLE.contains("[blog]"));
        let config = load_from_str(SAMPLE).unwrap();
        assert_eq!(config.blog_title(), "leviculum.network dev blog");
        assert_eq!(config.blog.language, "en", "a blank lang= would be wrong");
        let meta = config.blog_meta(None);
        assert_eq!(meta.author, None);
        assert_eq!(meta.description, None);
    }

    #[test]
    fn the_two_names_default_to_each_other() {
        // Only a blog title: the announce uses it too.
        let sample = SAMPLE.replace(
            "display_name           = \"leviculum.network dev blog\"",
            "",
        ) + "\n[blog]\ntitle = \"Just the Blog\"\n";
        let config = load_from_str(&sample).unwrap();
        assert_eq!(config.blog_title(), "Just the Blog");
        assert_eq!(config.blog_node_config().display_name, "Just the Blog");

        // Only a display name: the pages use it as the heading.
        let config = load_from_str(SAMPLE).unwrap();
        assert_eq!(config.blog_title(), "leviculum.network dev blog");

        // Both: each keeps its own, which is the point of allowing both.
        let sample = format!("{SAMPLE}\n[blog]\ntitle = \"Reader Facing\"\n");
        let config = load_from_str(&sample).unwrap();
        assert_eq!(config.blog_title(), "Reader Facing");
        assert_eq!(
            config.blog_node_config().display_name,
            "leviculum.network dev blog"
        );
    }

    #[test]
    fn a_blog_with_no_name_at_all_is_invalid() {
        let sample = SAMPLE.replace(
            "display_name           = \"leviculum.network dev blog\"",
            "",
        );
        let err = load_from_str(&sample).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
        assert!(err.to_string().contains("blog.title"), "{err}");
    }

    #[test]
    fn blog_meta_carries_the_reader_facing_fields() {
        let sample = format!(
            "{SAMPLE}\n[blog]\ntitle = \"Reader Facing\"\nauthor = \"Someone\"\n\
             description = \"About things\"\nlanguage = \"de\"\n"
        );
        let meta = load_from_str(&sample)
            .unwrap()
            .blog_meta(Some("abc123".to_string()));
        assert_eq!(meta.title, "Reader Facing");
        assert_eq!(meta.author.as_deref(), Some("Someone"));
        assert_eq!(meta.description.as_deref(), Some("About things"));
        assert_eq!(meta.language, "de");
        assert_eq!(meta.nomadnet_address.as_deref(), Some("abc123"));
        // The first certificate domain is the canonical public URL.
        assert_eq!(
            meta.web_url.as_deref(),
            Some("https://leviculum.network"),
            "the micron side needs somewhere to point"
        );
    }

    #[test]
    fn the_public_url_comes_from_domains_not_from_acme() {
        // No domains: nothing to build absolute links from, so no feed and no
        // cross-link. A development bind address would be worse than none.
        let meta = load_from_str(DEV_SAMPLE).unwrap().blog_meta(None);
        assert_eq!(meta.web_url, None);

        // Domains without ACME: the canonical name is known, which is what
        // makes the feed checkable before deployment.
        let sample = DEV_SAMPLE.replace(
            "acme      = false",
            "acme      = false
        domains   = [\"leviculum.network\"]",
        );
        let meta = load_from_str(&sample).unwrap().blog_meta(None);
        assert_eq!(meta.web_url.as_deref(), Some("https://leviculum.network"));
    }

    #[test]
    fn an_about_page_exists_as_soon_as_there_is_anything_on_it() {
        let config = load_from_str(SAMPLE).unwrap();
        assert!(!config.has_about_page(), "nothing configured, no page");
        assert!(!config.blog_meta(None).has_about);

        for field in [
            "email = \"a@b.example\"",
            "lxmf  = \"0ec84236630cea839d80a71c39fb41ce\"",
            "about = \"/etc/lblogd/about.md\"",
        ] {
            let sample = format!("{SAMPLE}\n[blog]\n{field}\n");
            let config = load_from_str(&sample).unwrap();
            assert!(config.has_about_page(), "{field} should create the page");
            assert!(config.blog_meta(None).has_about, "{field}");
        }
    }

    #[test]
    fn a_malformed_lxmf_hash_is_rejected_at_startup() {
        // NomadNet refuses a link whose target is not exactly the hash
        // length, so emitting one would produce a link that silently fails
        // for every reader. Better to refuse the config.
        for bad in ["deadbeef", "0ec84236630cea839d80a71c39fb41ce00", "zz"] {
            let sample = format!("{SAMPLE}\n[blog]\nlxmf = \"{bad}\"\n");
            let err = load_from_str(&sample).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { .. }), "{bad}: {err}");
            assert!(err.to_string().contains("blog.lxmf"), "{err}");
        }

        let sample = format!("{SAMPLE}\n[blog]\nlxmf = \"0EC84236630CEA839D80A71C39FB41CE\"\n");
        assert!(
            load_from_str(&sample).is_ok(),
            "upper-case hex is still a hash"
        );
    }

    #[test]
    fn acme_defaults_to_on_so_deployment_configs_are_unchanged() {
        // SAMPLE carries no `acme` key, exactly like a config written before
        // the field existed.
        assert!(!SAMPLE.contains("acme ="));
        let config = load_from_str(SAMPLE).unwrap();
        assert!(config.web.acme);
        assert!(config.web_config().acme.is_some());
    }

    #[test]
    fn counting_is_on_by_default_and_lands_under_data_dir() {
        // SAMPLE carries no `[counter]` section, exactly like a config
        // written before it existed.
        assert!(!SAMPLE.contains("[counter]"));
        let config = load_from_str(SAMPLE).unwrap();
        assert_eq!(
            config.counter_path(),
            Some(PathBuf::from("/var/lib/lblogd/counts.log"))
        );
    }

    #[test]
    fn the_counter_can_be_moved_or_switched_off() {
        let sample = format!("{SAMPLE}\n[counter]\npath = \"/var/log/lblogd-counts\"\n");
        assert_eq!(
            load_from_str(&sample).unwrap().counter_path(),
            Some(PathBuf::from("/var/log/lblogd-counts"))
        );

        // Off means off, even with a path set: an operator turning it off has
        // not asked for the file to keep growing.
        let sample =
            format!("{SAMPLE}\n[counter]\nenabled = false\npath = \"/var/log/lblogd-counts\"\n");
        assert_eq!(load_from_str(&sample).unwrap().counter_path(), None);
    }

    #[test]
    fn blog_node_config_maps_fields() {
        let config: Config = toml::from_str(SAMPLE).unwrap();
        let node = config.blog_node_config();
        assert_eq!(node.instance_name, "leviculum");
        assert_eq!(node.data_dir, PathBuf::from("/var/lib/lblogd"));
        assert_eq!(node.display_name, "leviculum.network dev blog");
        assert_eq!(node.announce_interval, Duration::from_secs(3600));
    }

    #[test]
    fn announce_interval_defaults_when_omitted() {
        let sample = SAMPLE.replace("announce_interval_secs = 3600", "");
        let config: Config = toml::from_str(&sample).unwrap();
        assert_eq!(config.node.announce_interval_secs, None);
        assert_eq!(
            config.blog_node_config().announce_interval,
            BlogNodeConfig::default_announce_interval()
        );
    }

    #[test]
    fn web_config_maps_fields_and_derives_acme_cache_dir() {
        let config: Config = toml::from_str(SAMPLE).unwrap();
        let web = config.web_config();
        let acme = web.acme.expect("SAMPLE enables acme");
        assert_eq!(
            acme.domains,
            vec!["leviculum.network", "www.leviculum.network"]
        );
        assert_eq!(acme.cache_dir, PathBuf::from("/var/lib/lblogd/acme"));
        assert_eq!(acme.contact_email, "ops@example.org");
        assert!(acme.staging);
        assert_eq!(web.http_bind, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(web.https_bind, "127.0.0.1:8443".parse().unwrap());
    }

    #[test]
    fn dev_config_needs_no_acme_fields() {
        let config = load_from_str(DEV_SAMPLE).unwrap();
        assert!(!config.web.acme);
        assert!(config.web.domains.is_empty());
        assert_eq!(config.web.acme_contact_email, None);
        assert_eq!(config.web.acme_staging, None);
        assert_eq!(config.web.http_bind, "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn web_config_without_acme_selects_plaintext_mode() {
        let web = load_from_str(DEV_SAMPLE).unwrap().web_config();
        assert!(web.acme.is_none());
        assert_eq!(web.http_bind, "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn acme_without_domains_is_invalid() {
        let sample = SAMPLE.replace(
            "domains            = [\"leviculum.network\", \"www.leviculum.network\"]",
            "domains = []",
        );
        let err = load_from_str(&sample).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
        assert!(err.to_string().contains("web.domains"), "{err}");
    }

    #[test]
    fn acme_without_contact_email_is_invalid() {
        let sample = SAMPLE.replace("acme_contact_email = \"ops@example.org\"", "");
        let err = load_from_str(&sample).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
        assert!(err.to_string().contains("web.acme_contact_email"), "{err}");
    }

    #[test]
    fn acme_without_staging_choice_is_invalid() {
        // Staging vs production must stay a deliberate decision, so dropping
        // the key is an error rather than silently defaulting.
        let sample = SAMPLE.replace("acme_staging       = true", "");
        let err = load_from_str(&sample).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
        assert!(err.to_string().contains("web.acme_staging"), "{err}");
    }

    #[test]
    fn acme_off_ignores_missing_certificate_fields() {
        // The same config that is invalid with acme on loads cleanly with it
        // off: the fields are meaningless there, not merely optional.
        let sample = SAMPLE
            .replace("acme_contact_email = \"ops@example.org\"", "")
            .replace("acme_staging       = true", "")
            .replace("[web]", "[web]\n        acme = false");
        let config = load_from_str(&sample).unwrap();
        assert!(config.web_config().acme.is_none());
    }

    #[test]
    fn binds_default_to_80_and_443() {
        let sample = SAMPLE
            .replace("http_bind          = \"127.0.0.1:8080\"", "")
            .replace("https_bind         = \"127.0.0.1:8443\"", "");
        let config: Config = toml::from_str(&sample).unwrap();
        assert_eq!(config.web.http_bind, "0.0.0.0:80".parse().unwrap());
        assert_eq!(config.web.https_bind, "0.0.0.0:443".parse().unwrap());
    }

    #[test]
    fn missing_file_is_a_read_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        let err = Config::load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }), "{err}");
        assert!(err.to_string().contains("does-not-exist.toml"), "{err}");
    }

    #[test]
    fn invalid_toml_is_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "data_dir = [unclosed").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        assert!(err.to_string().contains("bad.toml"), "{err}");
    }

    #[test]
    fn missing_required_field_names_the_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.toml");
        let sample = SAMPLE.replace("instance_name          = \"leviculum\"", "");
        std::fs::write(&path, sample).unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
        assert!(err.to_string().contains("instance_name"), "{err}");
    }

    #[test]
    fn unknown_field_is_a_parse_error() {
        let sample = format!("{SAMPLE}\ntypo_field = 1\n");
        let err = toml::from_str::<Config>(&sample).unwrap_err();
        assert!(err.to_string().contains("typo_field"), "{err}");
    }
}
