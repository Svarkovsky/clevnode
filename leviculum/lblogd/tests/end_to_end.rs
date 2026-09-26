//! End-to-end: parse a full sample post (frontmatter plus mixed Markdown) and
//! render it through both pipelines without error.

use lblogd::post::{parse_post, PostDefaults};
use lblogd::render::{
    render_index_html, render_index_micron, render_post_html, render_post_micron, BlogMeta,
    DEFAULT_STYLE,
};
use leviculum_micron::{parse, Block};

const SAMPLE: &str = r#"+++
title = "Bringing Up the LoRa Rig"
date = "2026-07-12"
+++

The rig is **finally** stable. Setup was *mostly* painless:

# Hardware

- two LNodes
- three RNodes

## Flashing

Run `just flash` and wait:

```
$ just flash
flashing T114 on /dev/ttyACM0
```

See [the docs](https://example.com/rig) for details.

![rig photo](rig.png)

| board | count |
|-------|-------|
| LNode | 2     |
"#;

#[test]
fn sample_post_renders_to_both_formats() {
    let post = parse_post(SAMPLE, &fixture_defaults()).unwrap();
    assert_eq!(post.title, "Bringing Up the LoRa Rig");
    assert_eq!(post.slug, "bringing-up-the-lora-rig");
    assert_eq!(post.date.to_string(), "2026-07-12");

    let html = render_post_html(&fixture_meta(), DEFAULT_STYLE, &post);
    assert!(html.contains("<title>Bringing Up the LoRa Rig</title>"));
    assert!(html.contains("2026-07-12"));
    assert!(html.contains("<strong>finally</strong>"));
    assert!(html.contains("<pre><code>"));

    let micron = render_post_micron(&fixture_meta(), &post);
    let doc = parse(&micron);
    assert!(matches!(doc.blocks[0], Block::Heading { depth: 1, .. }));
    assert!(doc
        .blocks
        .iter()
        .any(|b| matches!(b, Block::Heading { depth: 2, .. })));
    assert!(doc
        .blocks
        .iter()
        .any(|b| matches!(b, Block::LiteralBlock { .. })));
    assert!(micron.contains("2026-07-12"));

    let index_html = render_index_html(&fixture_meta(), DEFAULT_STYLE, std::slice::from_ref(&post));
    assert!(index_html.contains("/posts/bringing-up-the-lora-rig"));

    let index_micron = render_index_micron(&fixture_meta(), std::slice::from_ref(&post));
    assert!(index_micron.contains(":/page/bringing-up-the-lora-rig.mu"));
    assert!(index_micron.contains("2026-07-12"));
}

/// Defaults for fixtures that always set title and date themselves.
fn fixture_defaults() -> PostDefaults {
    PostDefaults {
        title: "fixture".to_string(),
        date: "2000-01-01".parse().unwrap(),
    }
}

/// Blog metadata for fixtures that are about rendering, not about identity.
fn fixture_meta() -> BlogMeta {
    BlogMeta {
        title: "Test Blog".to_string(),
        author: None,
        description: None,
        language: "en".to_string(),
        web_url: None,
        nomadnet_address: None,
        email: None,
        lxmf: None,
        has_about: false,
    }
}
