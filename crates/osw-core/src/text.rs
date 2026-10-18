use crate::settings::TranscribeSettings;

/// A decoded whisper segment; timestamps are in centiseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub start_cs: i64,
    pub end_cs: i64,
    pub text: String,
}

/// Builds the final text from decoded segments.
///
/// Whisper segments are decoder boundaries, not paragraph boundaries. Their
/// text already contains the token-level whitespace needed between adjacent
/// segments, so they are joined without a separator. Timestamp mode stays
/// line-oriented for readability.
pub fn assemble(segments: &[Segment], settings: &TranscribeSettings) -> String {
    let joined = if settings.show_timestamps {
        segments
            .iter()
            .map(|s| {
                format!(
                    "[{:.1}->{:.1}] {}",
                    s.start_cs as f32 / 100.0,
                    s.end_cs as f32 / 100.0,
                    s.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        segments.iter().map(|s| s.text.as_str()).collect()
    };

    let cleaned = joined
        .replace("[MUSIC]", "")
        .replace("[BLANK_AUDIO]", "")
        .trim()
        .to_string();

    if settings.should_apply_asian_autocorrect() && !cleaned.is_empty() {
        autocorrect::format(&cleaned)
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(text: &str) -> Segment {
        Segment { start_cs: 0, end_cs: 150, text: text.into() }
    }

    #[test]
    fn joins_without_inventing_whitespace() {
        let s = TranscribeSettings::default();
        assert_eq!(assemble(&[seg(" Hello"), seg(" world.")], &s), "Hello world.");
        assert_eq!(assemble(&[seg(" foo"), seg("bar")], &s), "foobar");
    }

    #[test]
    fn strips_blank_markers() {
        let s = TranscribeSettings::default();
        assert_eq!(assemble(&[seg(" [BLANK_AUDIO]")], &s), "");
        assert_eq!(assemble(&[seg(" [MUSIC] hi")], &s), "hi");
    }

    #[test]
    fn timestamps_are_line_oriented() {
        let s = TranscribeSettings { show_timestamps: true, ..Default::default() };
        assert_eq!(assemble(&[seg("a"), seg("b")], &s), "[0.0->1.5] a\n[0.0->1.5] b");
    }

    #[test]
    fn autocorrect_only_for_asian_languages() {
        let zh = TranscribeSettings { language: "zh".into(), ..Default::default() };
        assert_eq!(assemble(&[seg("你好world")], &zh), "你好 world");
        let en = TranscribeSettings::default();
        assert_eq!(assemble(&[seg("你好world")], &en), "你好world");
    }
}
