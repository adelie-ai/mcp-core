//! The log-field sanitiser this crate re-exports to the fleet.
//!
//! Thirteen servers reach `Safe` through `mcp-core` rather than depending on
//! `adelie-telemetry` themselves, so this crate owns the contract they are held
//! to. The set of characters below is that contract written as data: if the
//! shared predicate is ever narrowed, one of these tests fails and names the
//! character that stopped being replaced.
//!
//! The caps and the markers are written out here rather than read from the
//! crate. Reading them would compare a constant to itself and pass whatever
//! the numbers became.

use mcp_core::telemetry::Safe;

/// What replaces a character that could change what a reader sees.
const REPLACEMENT: char = '\u{fffd}';

/// What marks a value the cap cut short.
const TRUNCATED: &str = "...";

/// The most bytes of a caller-chosen name a log field keeps.
const MAX_NAME_BYTES: usize = 128;

/// The most bytes of a diagnostic message a log field keeps.
const MAX_MESSAGE_BYTES: usize = 1024;

/// Every character a log field must not carry through, and why.
///
/// Three groups, and they fail in different ways.
///
/// - Category Cc - C0, DEL and C1. A newline ends the log line early and
///   starts one that reads as genuine; an escape drives the terminal.
/// - Categories Zl and Zp - U+2028 and U+2029. `char::is_control` does not
///   cover them, and some log viewers and every JSON consumer read them as a
///   line break.
/// - The bidi controls, category Cf. They leave the bytes honest and reverse
///   what a terminal shows, so a name renders as something it is not. This is
///   the Trojan-source class. The set is the marks, the embeddings, the
///   overrides, the isolates and the two pops.
fn deceptive_characters() -> Vec<char> {
    let mut set: Vec<char> = Vec::new();
    set.extend((0x00..=0x1f).filter_map(char::from_u32));
    set.push('\u{7f}');
    set.extend((0x80..=0x9f).filter_map(char::from_u32));
    set.push('\u{2028}');
    set.push('\u{2029}');
    set.extend(['\u{061c}', '\u{200e}', '\u{200f}']);
    set.extend((0x202a..=0x202e).filter_map(char::from_u32));
    set.extend((0x2066..=0x2069).filter_map(char::from_u32));
    set
}

/// AC: every server can reach the sanitiser through `mcp-core`, so none of
/// them writes its own.
///
/// A hand-written copy drifts. The one that existed used a 128-byte cap where
/// this crate uses 1024 for a message, so the same value already read two ways
/// depending on which binary logged it.
#[test]
fn the_sanitiser_is_reachable_through_this_crate() {
    assert_eq!(Safe::name("search").to_string(), "search");
    assert_eq!(Safe::message("not found").to_string(), "not found");
}

/// AC: no character in the deceptive set survives a name field.
#[test]
fn every_deceptive_character_is_replaced_in_a_name() {
    for character in deceptive_characters() {
        let rendered = Safe::name(format!("a{character}b")).to_string();
        assert_eq!(
            rendered,
            format!("a{REPLACEMENT}b"),
            "U+{:04X} survived a name field, so a caller can change what a reader sees",
            character as u32
        );
    }
}

/// AC: no character in the deceptive set survives a message field either.
///
/// The two caps differ, so the two paths are separate and both are checked.
#[test]
fn every_deceptive_character_is_replaced_in_a_message() {
    for character in deceptive_characters() {
        let rendered = Safe::message(format!("a{character}b")).to_string();
        assert_eq!(
            rendered,
            format!("a{REPLACEMENT}b"),
            "U+{:04X} survived a message field, so a caller can change what a reader sees",
            character as u32
        );
    }
}

/// The upper boundary of the predicate.
///
/// U+200D ZERO WIDTH JOINER is category Cf, like the bidi controls, and it
/// carries the emoji sequences a person legitimately wants to read. This test
/// fails if the predicate ever widens to all of Cf.
#[test]
fn a_zero_width_joiner_survives_the_sanitiser() {
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    assert_eq!(Safe::name(family).to_string(), family);
}

/// AC: a name is capped at 128 bytes, so one request cannot ship as much as it
/// likes into a span field that leaves the process with `otel` on.
#[test]
fn a_name_is_capped_at_128_bytes() {
    let long = "n".repeat(4096);
    let rendered = Safe::name(&long).to_string();

    assert_eq!(
        rendered,
        format!("{}{TRUNCATED}", "n".repeat(MAX_NAME_BYTES))
    );
}

/// AC: a message is capped at 1024 bytes - wider than a name, because the text
/// is mostly what this crate or the server wrote and is worth keeping whole.
#[test]
fn a_message_is_capped_at_1024_bytes() {
    let long = "m".repeat(4096);
    let rendered = Safe::message(&long).to_string();

    assert_eq!(
        rendered,
        format!("{}{TRUNCATED}", "m".repeat(MAX_MESSAGE_BYTES))
    );
}

/// The cap counts bytes after substitution, not characters.
///
/// A replacement is three bytes where the character it replaced may be one, so
/// a value made only of newlines must not overrun the cap by tripling.
#[test]
fn a_replacement_counts_against_the_cap_at_its_own_width() {
    let rendered = Safe::name("\n".repeat(MAX_NAME_BYTES)).to_string();

    let replacements = MAX_NAME_BYTES / REPLACEMENT.len_utf8();
    assert_eq!(
        rendered,
        format!(
            "{}{TRUNCATED}",
            REPLACEMENT.to_string().repeat(replacements)
        )
    );
    assert!(rendered.len() <= MAX_NAME_BYTES + TRUNCATED.len());
}

/// The cap never cuts a character in half.
#[test]
fn the_cap_never_splits_a_character() {
    let four_byte = "\u{1f600}";
    let almost_full = "x".repeat(MAX_NAME_BYTES - 2);
    let rendered = Safe::name(format!("{almost_full}{four_byte}")).to_string();

    assert_eq!(rendered, format!("{almost_full}{TRUNCATED}"));
}

/// AC: a JSON value goes through the same wrapper as any other value.
///
/// The wrapper takes anything that can be displayed, which is what lets the
/// tool-arguments line drop a JSON-only wrapper of its own. A JSON rendering
/// escapes the C0 controls itself, which is what made that path look safe; it
/// does not escape U+2028, U+2029 or a bidi control, and it bounds nothing.
#[test]
fn a_json_value_is_sanitised_like_any_other_value() {
    let hostile = serde_json::json!({
        "path": format!("/tmp/a{}b", '\u{202e}'),
        "note": format!("x{}y", '\u{2028}'),
    });

    let rendered = Safe::message(&hostile).to_string();

    assert!(
        !rendered.contains('\u{202e}') && !rendered.contains('\u{2028}'),
        "a JSON value carried a deceptive character through: {rendered:?}"
    );
    assert!(
        rendered.contains(REPLACEMENT),
        "the deceptive characters must be replaced, not dropped: {rendered:?}"
    );
}

/// A JSON value writes itself in pieces, so the cap has to hold across the
/// whole rendering rather than per piece.
#[test]
fn a_json_value_is_capped_like_any_other_message() {
    let bulk = serde_json::json!({ "bulk": "b".repeat(64 * 1024) });

    let rendered = Safe::message(&bulk).to_string();

    assert_eq!(rendered.len(), MAX_MESSAGE_BYTES + TRUNCATED.len());
    assert!(rendered.ends_with(TRUNCATED));
}

/// Rendering a value through the wrapper must equal rendering it to a string
/// first and wrapping that.
///
/// This is what makes the piecewise `Display` of a JSON value safe: the cap and
/// the substitution see one stream, not one call per piece. Both caps are
/// checked, because the dispatch path uses each of them on a JSON value - the
/// name cap on a request id, the message cap on the arguments payload - and
/// the tighter cap is where the two ways of rendering would first diverge.
#[test]
fn wrapping_a_value_equals_wrapping_its_own_rendering() {
    let cases = [
        serde_json::json!({ "name": format!("a{}b", '\u{202e}') }),
        serde_json::json!({ "bulk": "b".repeat(4096) }),
        serde_json::json!([1, 2, 3]),
        serde_json::json!(null),
        serde_json::json!("a-request-id"),
        serde_json::json!(format!("id-{}-{}", '\u{2028}', "x".repeat(200))),
        serde_json::json!("x".repeat(MAX_NAME_BYTES)),
        serde_json::json!(7),
    ];

    for value in cases {
        assert_eq!(
            Safe::name(&value).to_string(),
            Safe::name(value.to_string()).to_string(),
            "the piecewise rendering of {value} disagreed with its whole rendering, at the \
             name cap"
        );
        assert_eq!(
            Safe::message(&value).to_string(),
            Safe::message(value.to_string()).to_string(),
            "the piecewise rendering of {value} disagreed with its whole rendering, at the \
             message cap"
        );
    }
}
