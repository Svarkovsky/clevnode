//! The name recipients see next to our messages.
//!
//! It travels in the delivery announce (`engine.rs`, [`Engine::announce`]) and
//! is the only thing a stranger has to go on: a reference client shows
//! `LXMF.display_name_from_app_data` of the last announce it heard from us.
//! `lnmsg` used to announce the literal string `lnmsg`, which names the tool
//! rather than the person holding it, so every recipient saw the same sender
//! for every operator on every host.
//!
//! # Why the environment is not the first place to look
//!
//! `lnmsg`'s first real consumer is a health monitor started from cron, and
//! cron and systemd units routinely start with a minimal environment in which
//! `$USER` is unset — `env -i` is a fair model of it. A name that is right
//! interactively and wrong from cron is worse than one that is consistently
//! wrong, because it would be discovered late and by a machine. So the
//! authoritative, environment-independent answer comes first
//! ([`leviculum_std::user::passwd_name`]) and `$USER` only backs it up.
//!
//! # Resolution order
//!
//! 1. `--from <NAME>`, then `LNMSG_DISPLAY_NAME`. Explicit operator choices,
//!    and the only two that can fail the command; see [`EmptyDisplayName`].
//! 2. The password-database name of the real uid.
//! 3. `$USER`, then `$LOGNAME`.
//! 4. The literal `lnmsg`. **Resolution never fails**: a status line that does
//!    not go out is worse than one from an oddly-named sender.
//!
//! # What this deliberately does not decide
//!
//! Nothing here is host-aware. The monitor will run as the same user on
//! several machines, so a bare `lew` arriving three times says nothing about
//! which host is in trouble — but which of `hamster`, `lew@schneckenschreck`
//! or something else answers that is the operator's policy, and inventing one
//! here (an automatic hostname suffix, a template) would be us guessing.
//! [`Sources::flag`] and [`Sources::env_override`] are where that choice goes.
//!
//! [`Engine::announce`]: crate::engine

/// Environment override, for the cron case where there is no command line to
/// edit. Loses to `--from`.
pub const DISPLAY_NAME_ENV: &str = "LNMSG_DISPLAY_NAME";

/// The last resort. Only reached when nothing else on the host has a name for
/// the user running us.
pub const FALLBACK: &str = "lnmsg";

/// An override was given and was empty or whitespace only.
///
/// The one way naming can fail, and deliberately not a silent fallback: an
/// operator who set the name explicitly gets told the value cannot be used,
/// rather than discovering weeks later that the announces went out under the
/// default. Only [`Sources::flag`] and [`Sources::env_override`] can produce
/// it — a blank *resolved* username is simply skipped, because nobody asked
/// for it.
#[derive(Debug, PartialEq, Eq)]
pub struct EmptyDisplayName {
    /// How the empty name was given, named the way the operator wrote it.
    pub source: &'static str,
}

impl std::fmt::Display for EmptyDisplayName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is empty: an empty display name is indistinguishable from sending none at all.\n  \
             Give a name, or leave it out to use the account name.",
            self.source
        )
    }
}

impl std::error::Error for EmptyDisplayName {}

/// Everything a resolution may read, as values rather than as lookups.
///
/// Passed in rather than read inside [`resolve`] so the order can be tested
/// without touching the process environment: `cargo` runs test functions in
/// parallel threads of one process, and `set_var` there is a race between
/// tests, not a fixture.
#[derive(Debug, Default, Clone, Copy)]
pub struct Sources<'a> {
    /// `--from`.
    pub flag: Option<&'a str>,
    /// [`DISPLAY_NAME_ENV`].
    pub env_override: Option<&'a str>,
    /// The password-database name of the real uid.
    pub passwd: Option<&'a str>,
    /// `$USER`.
    pub user: Option<&'a str>,
    /// `$LOGNAME`.
    pub logname: Option<&'a str>,
}

/// Which step of the order produced the name.
///
/// Reported in the structured event log, because "the recipient saw the wrong
/// sender" is answered by knowing which source spoke — a cron run that fell
/// through to [`Self::Fallback`] and an interactive one that read `$USER` look
/// identical in the message itself.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Source {
    Flag,
    Env,
    Passwd,
    User,
    Logname,
    Fallback,
}

impl Source {
    /// The log field's value: one lowercase word, no spaces.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Env => "env",
            Self::Passwd => "passwd",
            Self::User => "user",
            Self::Logname => "logname",
            Self::Fallback => "fallback",
        }
    }

    /// How an operator writes this source, for error messages.
    fn spelling(self) -> &'static str {
        match self {
            Self::Flag => "--from",
            Self::Env => DISPLAY_NAME_ENV,
            Self::Passwd => "the password database",
            Self::User => "$USER",
            Self::Logname => "$LOGNAME",
            Self::Fallback => FALLBACK,
        }
    }
}

/// A resolved name and the step that produced it.
#[derive(Debug, PartialEq, Eq)]
pub struct Resolved {
    pub name: String,
    pub source: Source,
}

/// Apply the documented order to a set of sources.
///
/// Surrounding whitespace is dropped from whatever wins: `--from 'lew '` is a
/// typo, not a request for a trailing space in an announce nobody can see it
/// in.
pub fn resolve(sources: Sources<'_>) -> Result<Resolved, EmptyDisplayName> {
    for (value, source) in [
        (sources.flag, Source::Flag),
        (sources.env_override, Source::Env),
    ] {
        if let Some(given) = value {
            let trimmed = given.trim();
            if trimmed.is_empty() {
                return Err(EmptyDisplayName {
                    source: source.spelling(),
                });
            }
            return Ok(Resolved {
                name: trimmed.to_string(),
                source,
            });
        }
    }
    let resolved = [
        (sources.passwd, Source::Passwd),
        (sources.user, Source::User),
        (sources.logname, Source::Logname),
    ]
    .into_iter()
    .filter_map(|(value, source)| value.map(|value| (value.trim(), source)))
    .find(|(candidate, _)| !candidate.is_empty());
    Ok(match resolved {
        Some((name, source)) => Resolved {
            name: name.to_string(),
            source,
        },
        None => Resolved {
            name: FALLBACK.to_string(),
            source: Source::Fallback,
        },
    })
}

/// Resolve against this process: the password database, then the environment.
pub fn from_process(flag: Option<&str>) -> Result<Resolved, EmptyDisplayName> {
    let env_override = std::env::var(DISPLAY_NAME_ENV).ok();
    let passwd = leviculum_std::user::passwd_name();
    let user = std::env::var("USER").ok();
    let logname = std::env::var("LOGNAME").ok();
    resolve(Sources {
        flag,
        env_override: env_override.as_deref(),
        passwd: passwd.as_deref(),
        user: user.as_deref(),
        logname: logname.as_deref(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_database_answers_before_the_environment() {
        let resolved = resolve(Sources {
            passwd: Some("lew"),
            user: Some("someone-else"),
            logname: Some("a-third"),
            ..Sources::default()
        })
        .expect("no override was given, so nothing can fail");
        assert_eq!(resolved.name, "lew");
        assert_eq!(resolved.source, Source::Passwd);
    }

    /// The cron shape: `env -i` leaves only step 1 able to answer. The binary
    /// half of this claim — that a *musl* build's `getpwuid_r` still answers
    /// under `env -i` — is in `tests/send_cli.rs`, since only a real process
    /// can have an empty environment.
    #[test]
    fn with_an_empty_environment_the_password_database_still_answers() {
        let resolved = resolve(Sources {
            passwd: Some("lew"),
            ..Sources::default()
        })
        .expect("resolution never fails without an override");
        assert_eq!(resolved.name, "lew");
        assert_eq!(resolved.source, Source::Passwd);
    }

    #[test]
    fn user_answers_when_the_password_database_cannot() {
        let resolved = resolve(Sources {
            user: Some("lew"),
            logname: Some("a-third"),
            ..Sources::default()
        })
        .expect("resolution never fails without an override");
        assert_eq!(resolved.name, "lew");
        assert_eq!(resolved.source, Source::User);
    }

    #[test]
    fn logname_answers_when_user_is_unset() {
        let resolved = resolve(Sources {
            logname: Some("lew"),
            ..Sources::default()
        })
        .expect("resolution never fails without an override");
        assert_eq!(resolved.name, "lew");
        assert_eq!(resolved.source, Source::Logname);
    }

    /// The last resort exists so that a host with no name for its own uid still
    /// sends. A message nobody gets is worse than one from an odd sender.
    #[test]
    fn a_host_that_can_name_nobody_still_gets_a_name() {
        let resolved = resolve(Sources::default()).expect("the last resort cannot fail");
        assert_eq!(resolved.name, FALLBACK);
        assert_eq!(resolved.source, Source::Fallback);
    }

    /// A blank entry is not an answer — skip it rather than announce a space.
    #[test]
    fn a_blank_resolved_name_is_skipped_rather_than_used() {
        let resolved = resolve(Sources {
            passwd: Some(""),
            user: Some("   "),
            logname: Some("lew"),
            ..Sources::default()
        })
        .expect("blanks are skipped, not refused");
        assert_eq!(resolved.name, "lew");
        assert_eq!(resolved.source, Source::Logname);
    }

    #[test]
    fn the_flag_wins_over_the_environment_variable() {
        let resolved = resolve(Sources {
            flag: Some("hamster"),
            env_override: Some("from-the-env"),
            passwd: Some("lew"),
            ..Sources::default()
        })
        .expect("a non-empty override");
        assert_eq!(resolved.name, "hamster");
        assert_eq!(resolved.source, Source::Flag);
    }

    #[test]
    fn the_environment_variable_wins_over_the_resolved_username() {
        let resolved = resolve(Sources {
            env_override: Some("lew@schneckenschreck"),
            passwd: Some("lew"),
            user: Some("lew"),
            ..Sources::default()
        })
        .expect("a non-empty override");
        assert_eq!(resolved.name, "lew@schneckenschreck");
        assert_eq!(resolved.source, Source::Env);
    }

    #[test]
    fn an_empty_flag_is_refused_and_does_not_fall_back() {
        assert_eq!(
            resolve(Sources {
                flag: Some("  "),
                passwd: Some("lew"),
                ..Sources::default()
            }),
            Err(EmptyDisplayName { source: "--from" }),
            "an explicit empty name must not quietly become the account name"
        );
    }

    #[test]
    fn an_empty_environment_override_is_refused_too() {
        let error = resolve(Sources {
            env_override: Some(""),
            passwd: Some("lew"),
            ..Sources::default()
        })
        .expect_err("an explicitly empty override is an error whichever way it was set");
        assert_eq!(error.source, DISPLAY_NAME_ENV);
        assert!(
            error.to_string().contains("indistinguishable"),
            "the message must say why an empty name is refused: {error}"
        );
    }

    #[test]
    fn surrounding_whitespace_is_not_part_of_the_name() {
        let resolved = resolve(Sources {
            flag: Some("  hamster\n"),
            ..Sources::default()
        })
        .expect("a name with padding is still a name");
        assert_eq!(resolved.name, "hamster");
    }
}
