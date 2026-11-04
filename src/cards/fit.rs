//! Fitting text into a card box: wrap, step the size down, cut at a word + "…".

#[derive(Debug, Clone, PartialEq)]
pub struct Fit {
    pub size: f32,
    pub lines: Vec<String>,
}

/// Greedy word wrap at `size`. A word wider than the line is split by chars.
pub fn wrap(text: &str, width: f32, size: f32, measure: impl Fn(&str, f32) -> f32) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if measure(&candidate, size) <= width {
            line = candidate;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        for c in word.chars() {
            line.push(c);
            if measure(&line, size) > width && line.chars().count() > 1 {
                line.pop();
                lines.push(std::mem::take(&mut line));
                line.push(c);
            }
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The largest of `sizes` (descending) that fits the box; else the smallest, cut at a word + "…".
pub fn fit(
    text: &str,
    width: f32,
    height: f32,
    sizes: &[f32],
    line_height: f32,
    measure: impl Fn(&str, f32) -> f32,
) -> Fit {
    let max_lines = |size: f32| ((height / (size * line_height)).floor() as usize).max(1);
    for &size in sizes {
        let lines = wrap(text, width, size, &measure);
        if lines.len() <= max_lines(size) {
            return Fit { size, lines };
        }
    }
    let size = *sizes.last().expect("at least one size");
    let mut lines = wrap(text, width, size, &measure);
    lines.truncate(max_lines(size));
    if let Some(last) = lines.last_mut() {
        *last = ellipsize(last, width, size, &measure);
    }
    Fit { size, lines }
}

/// `line` + "…", dropping trailing words (then chars) until it fits.
fn ellipsize(line: &str, width: f32, size: f32, measure: impl Fn(&str, f32) -> f32) -> String {
    let mut words: Vec<&str> = line.split(' ').collect();
    while words.len() > 1 && measure(&format!("{}…", words.join(" ")), size) > width {
        words.pop();
    }
    let mut out = words.join(" ");
    while out.chars().count() > 1 && measure(&format!("{out}…"), size) > width {
        out.pop();
    }
    format!(
        "{}…",
        out.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':'))
    )
}

/// At most `max` chars, cut at the last whole word + "…".
pub fn clip_words(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    let cut = match head.rfind(' ') {
        Some(i) if i > 0 => head[..i].trim_end().to_string(),
        _ => head,
    };
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every char is half an em wide.
    fn mono(s: &str, size: f32) -> f32 {
        s.chars().count() as f32 * size * 0.5
    }

    const SIZES: [f32; 4] = [44.0, 40.0, 36.0, 32.0];

    #[test]
    fn a_one_liner_reads_as_a_pull_quote() {
        let f = fit("gm nostr", 1000.0, 300.0, &SIZES, 1.3, mono);
        assert_eq!(f.size, 44.0);
        assert_eq!(f.lines, vec!["gm nostr"]);
    }

    #[test]
    fn steps_down_before_cutting() {
        // 300 chars: 7 lines of 45 at 44px (5 fit); 6 of 55 at 36px (6 fit).
        let text = "word ".repeat(60);
        let f = fit(text.trim(), 1000.0, 300.0, &SIZES, 1.3, mono);
        assert_eq!(f.size, 36.0);
        assert!(!f.lines.join(" ").ends_with('…'));
        assert_eq!(f.lines.join(" "), text.trim());
    }

    #[test]
    fn overflow_cuts_at_a_whole_word_with_an_ellipsis() {
        let text = (0..200)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let f = fit(&text, 1000.0, 300.0, &SIZES, 1.3, mono);
        assert_eq!(f.size, 32.0);
        assert_eq!(f.lines.len(), 7);
        let last = f.lines.last().unwrap();
        assert!(last.ends_with('…'), "got {last}");
        for line in &f.lines {
            assert!(mono(line, 32.0) <= 1000.0, "overflowing line {line}");
        }
        // Every drawn word is a whole word of the source.
        let words: Vec<&str> = text.split(' ').collect();
        let drawn = f.lines.join(" ");
        let drawn = drawn.trim_end_matches('…');
        for w in drawn.split(' ') {
            assert!(words.contains(&w), "partial word {w}");
        }
        assert!(text.starts_with(drawn));
    }

    #[test]
    fn an_unbreakable_word_is_split_rather_than_overflowing() {
        let text = "a".repeat(100);
        let f = fit(&text, 300.0, 1000.0, &SIZES, 1.3, mono);
        for line in &f.lines {
            assert!(mono(line, f.size) <= 300.0, "overflowing line {line}");
        }
        assert_eq!(f.lines.concat(), text);
    }

    #[test]
    fn clip_words_cuts_on_a_word_boundary() {
        assert_eq!(clip_words("short", 200), "short");
        assert_eq!(clip_words("one two three", 9), "one two…");
        assert_eq!(clip_words("abcdefghij", 5), "abcd…");
        assert_eq!(clip_words("さとう さとう", 5), "さとう…");
    }
}
