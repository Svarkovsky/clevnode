//! Command-line argument resolution for the `lnomad` binary.
//!
//! There is one positional argument, the URL to open, and it is optional:
//! started without it on a terminal, `lnomad` opens its start screen with the
//! places panel showing, since node discovery runs continuously in the
//! background and the panel is where its results land. [`resolve_args`] is the
//! pure decision function that maps the raw positional plus the interactivity of
//! the terminal onto a [`Mode`], with no argv or terminal access, so the whole
//! rule set is unit-testable.

/// The resolved intent of an `lnomad` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Fetch and render the page at the given URL string (not yet parsed).
    Page { url: String },
    /// No URL was given: open the browser on its start screen, with the places
    /// panel showing the bookmarks and the nodes discovery has found.
    Start,
}

/// Resolve the positional argument onto a [`Mode`].
///
/// - A positional is the URL to open, whether or not the terminal is
///   interactive (a non-tty invocation prints that page once).
/// - No positional on an interactive terminal opens the start screen.
/// - No positional without an interactive terminal is an error: there is
///   nothing to print and no way to drive the browser.
pub fn resolve_args(positional: Option<&str>, interactive: bool) -> Result<Mode, String> {
    match positional {
        Some(url) => Ok(Mode::Page {
            url: url.to_string(),
        }),
        None if interactive => Ok(Mode::Start),
        None => Err("a page URL is required when the browser cannot run interactively".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_positional_is_the_url() {
        assert_eq!(
            resolve_args(Some("abcd:/page/index.mu"), true),
            Ok(Mode::Page {
                url: "abcd:/page/index.mu".to_string()
            })
        );
    }

    #[test]
    fn a_positional_is_the_url_non_interactively_too() {
        // Piped or redirected: the page is printed once rather than browsed.
        assert_eq!(
            resolve_args(Some("abcd:/page/index.mu"), false),
            Ok(Mode::Page {
                url: "abcd:/page/index.mu".to_string()
            })
        );
    }

    #[test]
    fn no_positional_on_a_terminal_opens_the_start_screen() {
        assert_eq!(resolve_args(None, true), Ok(Mode::Start));
    }

    #[test]
    fn no_positional_without_a_terminal_is_an_error() {
        let err = resolve_args(None, false).unwrap_err();
        assert!(err.contains("page URL is required"), "{err}");
    }
}
