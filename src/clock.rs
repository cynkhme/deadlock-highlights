//! The clock value a reader answered, and the VOD time it was read at.

/// A clock reading that passed validation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ClockReading {
    pub vod_time: f64,
    pub clock_s: u32,
}

/// `M:SS` or `MM:SS` with seconds below 60, as total seconds.
pub(crate) fn parse_clock(s: &str) -> Option<u32> {
    let (m, sec) = s.trim().split_once(':')?;
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if !(1..=2).contains(&m.len()) || sec.len() != 2 || !digits(m) || !digits(sec) {
        return None;
    }
    let (m, sec): (u32, u32) = (m.parse().ok()?, sec.parse().ok()?);
    (sec < 60).then_some(m * 60 + sec)
}

/// The clock of a reader answer `{"clock": "M:SS"}`, which models often wrap in a code fence.
pub(crate) fn parse_answer(raw: &str) -> Option<u32> {
    let mut body = raw.trim();
    if let Some(rest) = body.strip_prefix("```") {
        let rest = rest.strip_suffix("```")?;
        // The fence may name a language on its first line.
        body = rest.split_once('\n').map_or(rest, |(_, b)| b).trim();
    }
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    parse_clock(v.get("clock")?.as_str()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_formats() {
        assert_eq!(parse_clock("5:17"), Some(317));
        assert_eq!(parse_clock(" 14:39 "), Some(879));
        assert_eq!(parse_clock("0:09"), Some(9));
        for bad in [
            "5:60", "123:00", "5:7", "", "5:17:00", ":17", "a:17", "+5:17",
        ] {
            assert_eq!(parse_clock(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn answers_with_and_without_fence() {
        assert_eq!(parse_answer(r#"{"clock":"5:17"}"#), Some(317));
        assert_eq!(
            parse_answer("```json\n{\"clock\": \"5:17\"}\n```"),
            Some(317)
        );
        assert_eq!(parse_answer("```\n{\"clock\":\"5:17\"}\n```"), Some(317));
        assert_eq!(parse_answer("  {\"clock\":\"5:17\"}\n"), Some(317));
        for bad in [
            r#"{"clock":""}"#,
            "I see 5:17",
            "[]",
            r#"{"clock":517}"#,
            "",
            "```json\n```",
        ] {
            assert_eq!(parse_answer(bad), None, "{bad:?}");
        }
    }
}
