//! Who this process is running as, without asking the environment.
//!
//! `$USER` is set by a login shell, and a program started by cron or by a
//! systemd unit has no login shell behind it: `env -i` is a fair model of what
//! those units hand a child. A name that is right interactively and wrong from
//! cron is worse than one that is consistently wrong, because it is discovered
//! late and by a machine — so the authoritative answer comes first, and the
//! environment is only a fallback for callers that want one.
//!
//! The authoritative answer is the password database entry for the real user
//! id. In the static musl binaries this workspace ships, musl's `getpwuid_r`
//! parses `/etc/passwd` itself instead of dlopen-ing NSS modules, so the lookup
//! works in a fully static binary for the local users the rig and the
//! workstations have.

/// The name of the real user id, from the password database.
///
/// `None` when the uid has no entry at all (a container with an empty
/// `/etc/passwd`, an id that only exists in a directory service a static binary
/// cannot reach). Never an error: a caller that needs a name always has
/// somewhere else to go, and this returning nothing is that signal.
#[cfg(unix)]
pub fn passwd_name() -> Option<String> {
    use std::ffi::CStr;
    use std::mem::MaybeUninit;

    // SAFETY: `getuid` takes no arguments, touches no memory of ours and is
    // documented as always succeeding.
    let uid = unsafe { libc::getuid() };

    // The suggested buffer size, when the platform offers one. musl returns
    // -1 for `_SC_GETPW_R_SIZE_MAX`; 1 KiB covers every realistic entry, and
    // the ERANGE loop below covers the rest.
    // SAFETY: `sysconf` reads a constant and writes nothing.
    let mut size = match unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) } {
        suggested if suggested > 0 => suggested as usize,
        _ => 1024,
    };

    // A pathological `/etc/passwd` line must not turn a name lookup into an
    // unbounded allocation loop; past 64 KiB there is no name worth having.
    while size <= 64 * 1024 {
        let mut buffer = vec![0u8; size];
        let mut entry = MaybeUninit::<libc::passwd>::uninit();
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `entry` and `found` are live for the call, and `buffer` is
        // `size` writable bytes, which is exactly what is passed as the
        // length. `getpwuid_r` writes the entry through the pointers it is
        // given and stores no state of its own.
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast::<libc::c_char>(),
                buffer.len(),
                &mut found,
            )
        };
        if code == libc::ERANGE {
            size *= 2;
            continue;
        }
        if code != 0 || found.is_null() {
            // A non-zero code is a lookup error and a null `found` is "no such
            // uid". Neither is worth distinguishing here: both mean the
            // password database has no name for us.
            return None;
        }
        // SAFETY: `getpwuid_r` returning 0 with a non-null result has
        // initialised `entry`, whose string fields point into `buffer` — still
        // alive for the rest of this block.
        let entry = unsafe { entry.assume_init() };
        if entry.pw_name.is_null() {
            return None;
        }
        // SAFETY: `pw_name` is a NUL-terminated C string inside `buffer`.
        let name = unsafe { CStr::from_ptr(entry.pw_name) };
        return name.to_str().ok().map(str::to_owned);
    }
    None
}

/// Non-unix hosts have no password database; the caller falls back.
#[cfg(not(unix))]
pub fn passwd_name() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the lookup: it answers with nothing in the environment.
    ///
    /// This runs in-process, so it cannot clear the environment the way `env -i`
    /// does — the binary-level check lives in `lnmsg`'s CLI tests. What it does
    /// prove is that the call itself works on this host and returns a plausible
    /// name rather than an empty string.
    #[test]
    #[cfg(unix)]
    fn the_password_database_names_the_user_running_the_tests() {
        let Some(name) = passwd_name() else {
            // A build host with no passwd entry for its own uid is unusual but
            // legal, and the function is specified to say so rather than fail.
            return;
        };
        assert!(!name.is_empty(), "a name from the passwd file is not empty");
        assert!(
            !name.contains('\0') && !name.contains(':'),
            "a passwd name carries neither NUL nor the field separator: {name:?}"
        );
        if let Ok(from_env) = std::env::var("USER") {
            assert_eq!(
                name, from_env,
                "the passwd entry and $USER disagree about who is running this"
            );
        }
    }
}
