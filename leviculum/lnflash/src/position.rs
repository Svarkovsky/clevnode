//! `--set-position` / `--clear-position` — a user-set fixed position as
//! the node's telemetry source.
//!
//! The board-facing half is one control-envelope frame
//! (`TYPE_FIXED_POSITION`); everything in this module is the human-facing
//! half: parse what a person types for a coordinate, say what will be
//! sent, and put the bytes on an already-opened port. Nothing here opens
//! a port — the open lives with [`crate::flow`], which proves it landed
//! on the intended board — so the parsing and the wire bytes are all
//! testable without a board.
//!
//! What the parser accepts, stated because "say what you accepted" is the
//! flag's contract: decimal degrees, latitude then longitude then an
//! optional altitude in metres, separated by commas or whitespace. Each
//! coordinate takes a sign (`-13.4`) or a hemisphere letter (`13.4S`,
//! `S13.4`, case-insensitive; N/S on the latitude, E/W on the longitude),
//! but not both at once. Degrees/minutes/seconds notation is not
//! accepted — every consumer map app shows decimal degrees, and a silent
//! misparse of `52°31'12"` as three tokens would be worse than naming
//! the expected shape.

use std::io;

use leviculum_core::envelope::{
    FixedPositionWire, FIXED_POSITION_MAX_LAT_E6, FIXED_POSITION_MAX_LON_E6, TYPE_FIXED_POSITION,
};

use crate::envelope::{self, SessionReply};
use crate::sys::Fd;

/// Send the fixed position (`None` clears it) on an already-opened
/// transport port. Takes the fd rather than a path for the same reason
/// [`crate::telemetry::send_configured`] does: the open and its
/// board-binding proof live with the caller. The set frame is 19 bytes —
/// Reticulum's minimum packet size — so it must only ever travel behind
/// the capability probe, which [`envelope::probed`] enforces.
pub fn send_configured(fd: &Fd, position: Option<&FixedPositionWire>) -> io::Result<SessionReply> {
    envelope::probed(fd, TYPE_FIXED_POSITION, |fd| {
        envelope::send_fixed_position(fd, position)
    })
}

/// One `key=value` line describing what will be sent, for the transcript
/// and for grep.
pub fn describe(position: &FixedPositionWire) -> String {
    let mut line = format!(
        "lat={} lon={}",
        fmt_scaled(position.latitude_e6 as i64, 1_000_000, 6),
        fmt_scaled(position.longitude_e6 as i64, 1_000_000, 6),
    );
    if let Some(alt) = position.altitude_e2 {
        line.push_str(&format!(" alt_m={}", fmt_scaled(alt as i64, 100, 2)));
    }
    line
}

/// A scaled integer back as the decimal the user typed, sign handled
/// before the split so `-0.5` keeps its minus.
fn fmt_scaled(value: i64, scale: i64, places: usize) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let abs = value.abs();
    format!("{sign}{}.{:0places$}", abs / scale, abs % scale)
}

/// Which axis a coordinate token belongs to; the axis owns its hemisphere
/// letters and its range.
#[derive(Clone, Copy)]
enum Axis {
    Latitude,
    Longitude,
}

impl Axis {
    const fn name(self) -> &'static str {
        match self {
            Self::Latitude => "latitude",
            Self::Longitude => "longitude",
        }
    }

    /// (positive, negative) hemisphere letters.
    const fn letters(self) -> (char, char) {
        match self {
            Self::Latitude => ('N', 'S'),
            Self::Longitude => ('E', 'W'),
        }
    }

    const fn max_e6(self) -> i64 {
        match self {
            Self::Latitude => FIXED_POSITION_MAX_LAT_E6 as i64,
            Self::Longitude => FIXED_POSITION_MAX_LON_E6 as i64,
        }
    }
}

/// Parse what `--set-position` was given: `<lat>,<lon>[,<alt>]`.
pub fn parse_position(text: &str) -> Result<FixedPositionWire, String> {
    const SHAPE: &str = "a position is <latitude>,<longitude>[,<altitude in metres>] in decimal \
                         degrees (comma or space separated; sign or hemisphere letter)";
    let tokens: Vec<&str> = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .collect();
    let (lat, lon, alt) = match tokens.as_slice() {
        [lat, lon] => (lat, lon, None),
        [lat, lon, alt] => (lat, lon, Some(alt)),
        _ => {
            return Err(format!(
                "{SHAPE}; {:?} has {} value(s)",
                text.trim(),
                tokens.len()
            ))
        }
    };
    Ok(FixedPositionWire {
        latitude_e6: parse_coordinate(lat, Axis::Latitude)?,
        longitude_e6: parse_coordinate(lon, Axis::Longitude)?,
        altitude_e2: match alt {
            Some(alt) => Some(parse_altitude(alt)?),
            None => None,
        },
    })
}

/// One coordinate token: decimal degrees with a sign or a hemisphere
/// letter (prefix or suffix), scaled to degrees × 1e6 and range-checked.
fn parse_coordinate(token: &str, axis: Axis) -> Result<i32, String> {
    let (positive, negative) = axis.letters();
    let mut rest = token;
    let mut hemisphere: Option<char> = None;
    for (letter, at_start) in [
        (rest.chars().next(), true),
        (rest.chars().next_back(), false),
    ] {
        if let Some(letter) = letter.map(|c| c.to_ascii_uppercase()) {
            if letter == positive || letter == negative {
                hemisphere = Some(letter);
                rest = if at_start {
                    &rest[1..]
                } else {
                    &rest[..rest.len() - 1]
                };
                break;
            }
        }
    }
    if hemisphere.is_some() && rest.starts_with(['+', '-']) {
        return Err(format!(
            "{:?}: a {} takes a sign or a hemisphere letter, not both",
            token,
            axis.name()
        ));
    }
    let degrees: f64 = rest.parse().map_err(|_| {
        format!(
            "{:?} is not a {} in decimal degrees (like 52.52, -13.4 or 13.4{})",
            token,
            axis.name(),
            negative
        )
    })?;
    if !degrees.is_finite() {
        return Err(format!("{:?} is not a {}", token, axis.name()));
    }
    let mut scaled = (degrees * 1e6).round() as i64;
    if hemisphere == Some(negative) {
        scaled = -scaled;
    }
    if scaled.abs() > axis.max_e6() {
        return Err(format!(
            "{:?}: a {} is at most {}°",
            token,
            axis.name(),
            axis.max_e6() / 1_000_000
        ));
    }
    Ok(scaled as i32)
}

/// The altitude in metres, scaled to metres × 1e2. No range beyond the
/// wire's own: unlike a coordinate, an unusual altitude is not
/// self-evidently a typo, and the board reports what it was given.
fn parse_altitude(token: &str) -> Result<i32, String> {
    let metres: f64 = token
        .parse()
        .map_err(|_| format!("{token:?} is not an altitude in metres (like 34 or -2.5)"))?;
    if !metres.is_finite() {
        return Err(format!("{token:?} is not an altitude in metres"));
    }
    let scaled = (metres * 100.0).round();
    if !(i32::MIN as f64..=i32::MAX as f64).contains(&scaled) {
        return Err(format!(
            "{token:?} does not fit the wire's altitude field (±21 474 836 m)"
        ));
    }
    Ok(scaled as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::testing::{
        envelope_firmware_stub, fixed_position_frame, old_firmware_stub, pre_236_firmware_stub,
        reporterless_firmware_stub, seen,
    };
    use crate::sys::testpty::Pty;
    use leviculum_core::envelope::REFUSE_UNSUPPORTED;

    fn berlin() -> FixedPositionWire {
        FixedPositionWire {
            latitude_e6: 52_520_008,
            longitude_e6: 13_404_954,
            altitude_e2: Some(3_400),
        }
    }

    // -----------------------------------------------------------------
    // What a human may type
    // -----------------------------------------------------------------

    #[test]
    fn the_accepted_spellings_all_parse_to_the_same_position() {
        // The parse table: decimal degrees, comma or space, sign or
        // hemisphere letter, prefix or suffix, any case.
        for typed in [
            "52.520008,13.404954,34",
            "52.520008, 13.404954, 34",
            "52.520008 13.404954 34",
            "N52.520008,E13.404954,34",
            "52.520008N 13.404954E 34",
            "52.520008n,13.404954e,34",
            "+52.520008,+13.404954,34.0",
        ] {
            assert_eq!(parse_position(typed).unwrap(), berlin(), "{typed}");
        }
    }

    #[test]
    fn the_altitude_is_optional() {
        assert_eq!(
            parse_position("52.520008,13.404954").unwrap(),
            FixedPositionWire {
                altitude_e2: None,
                ..berlin()
            }
        );
    }

    #[test]
    fn south_and_west_negate_exactly_like_a_sign() {
        let southern = parse_position("-36.84846,-73.04444,-4.3").unwrap();
        for typed in ["36.84846S,73.04444W,-4.3", "s36.84846 w73.04444 -4.3"] {
            assert_eq!(parse_position(typed).unwrap(), southern, "{typed}");
        }
        assert_eq!(southern.latitude_e6, -36_848_460);
        assert_eq!(southern.longitude_e6, -73_044_440);
        assert_eq!(southern.altitude_e2, Some(-430));
    }

    #[test]
    fn garbage_is_refused_with_the_shape_that_was_expected() {
        for typed in [
            "",
            "52.52",                      // one value
            "52.52,13.40,34,9",           // four values
            "here,there",                 // words
            "91,13.40",                   // latitude beyond the pole
            "-90.000001,0",               // just beyond, negative
            "52.52,180.000001",           // longitude beyond the antimeridian
            "52.52,13.40,inf",            // not an altitude
            "nan,13.40",                  // not a coordinate
            "N-52.52,13.40",              // letter and sign at once
            "52.52S N,13.40",             // letter with junk
            "E52.52,13.40",               // longitude letter on the latitude
            "52.52,13.40,fortythree",     // words again
            "52.52,13.40,99999999999999", // altitude beyond the wire
        ] {
            assert!(parse_position(typed).is_err(), "{typed:?} was accepted");
        }
        // The refusals name the shape, not just "invalid".
        assert!(parse_position("here,there")
            .unwrap_err()
            .contains("decimal degrees"));
        assert!(parse_position("91,13.40").unwrap_err().contains("90"));
        assert!(parse_position("N-52.52,13.40")
            .unwrap_err()
            .contains("not both"));
        assert!(parse_position("52.52").unwrap_err().contains("latitude"));
    }

    #[test]
    fn the_poles_and_the_antimeridian_are_places() {
        assert_eq!(parse_position("90,180").unwrap().latitude_e6, 90_000_000);
        assert_eq!(
            parse_position("90S,180W").unwrap().longitude_e6,
            -180_000_000
        );
    }

    #[test]
    fn the_transcript_prints_back_what_was_parsed() {
        assert_eq!(
            describe(&berlin()),
            "lat=52.520008 lon=13.404954 alt_m=34.00"
        );
        assert_eq!(
            describe(&parse_position("-0.5,-0.25").unwrap()),
            "lat=-0.500000 lon=-0.250000"
        );
    }

    // -----------------------------------------------------------------
    // The bytes that reach the board
    // -----------------------------------------------------------------

    #[test]
    fn a_set_position_reaches_the_board_and_is_acked() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let position = parse_position("52.520008,13.404954,34").unwrap();
        assert_eq!(
            send_configured(&fd, Some(&position)).unwrap(),
            SessionReply::Acked
        );
        // What went on the wire is what the firmware's own decision
        // function decodes: the typed coordinates, scaled, altitude flag
        // explicitly present.
        assert_eq!(fixed_position_frame(&seen), Some(Some(berlin())));
    }

    #[test]
    fn a_clear_reaches_the_board_as_the_one_byte_payload() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(send_configured(&fd, None).unwrap(), SessionReply::Acked);
        assert_eq!(fixed_position_frame(&seen), Some(None));
    }

    #[test]
    fn a_reporterless_board_refuses_instead_of_acking_what_nothing_reports() {
        // The fc60b95 capability gate, on the new frame: the position is
        // read only by the reporter, so a binary without one answers the
        // named refusal — never an ack for a pin that would never appear.
        let pty = Pty::open();
        let seen = seen();
        reporterless_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let reply = send_configured(&fd, Some(&berlin())).unwrap();
        assert_eq!(reply, SessionReply::Refused(REFUSE_UNSUPPORTED));
        assert!(!reply.took_it(), "took_it drives the non-zero exit");
        assert!(fixed_position_frame(&seen).is_some());
    }

    #[test]
    fn an_old_board_is_reported_as_such_rather_than_written_to_blind() {
        // The set frame is 19 bytes — exactly Reticulum's minimum packet
        // size — so pre-envelope firmware would read it as a packet. The
        // probe's silence must stop the conversation, not start it.
        let pty = Pty::open();
        let seen = seen();
        old_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        assert_eq!(
            send_configured(&fd, Some(&berlin())).unwrap(),
            SessionReply::ProbeSilent
        );
        assert_eq!(fixed_position_frame(&seen), None);
    }

    #[test]
    fn a_board_without_the_consumer_is_named_not_guessed_at() {
        let pty = Pty::open();
        let seen = seen();
        pre_236_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        assert_eq!(
            send_configured(&fd, Some(&berlin())).unwrap(),
            SessionReply::NotAccepted
        );
        assert_eq!(fixed_position_frame(&seen), None);
    }
}
