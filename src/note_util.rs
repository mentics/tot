//! Helpers for note previews and clickable URLs.

/// Collapse a note body to a single preview line, truncated to `max_chars`.
pub fn one_line_preview(body: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let flat: String = body
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    let count = flat.chars().count();
    if count <= max_chars {
        return flat.to_string();
    }
    let take = max_chars.saturating_sub(1);
    let mut out: String = flat.chars().take(take).collect();
    out.push('…');
    out
}

/// A URL span in a string, measured in Unicode scalar indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlSpan {
    pub start: usize,
    pub end: usize,
    pub url: String,
}

/// Find `http://` / `https://` URLs in `text`. Trailing punctuation is trimmed from matches.
pub fn find_urls(text: &str) -> Vec<UrlSpan> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let rest = chars.len() - i;
        let is_https = rest >= 8
            && chars[i] == 'h'
            && chars[i + 1] == 't'
            && chars[i + 2] == 't'
            && chars[i + 3] == 'p'
            && chars[i + 4] == 's'
            && chars[i + 5] == ':'
            && chars[i + 6] == '/'
            && chars[i + 7] == '/';
        let is_http = !is_https
            && rest >= 7
            && chars[i] == 'h'
            && chars[i + 1] == 't'
            && chars[i + 2] == 't'
            && chars[i + 3] == 'p'
            && chars[i + 4] == ':'
            && chars[i + 5] == '/'
            && chars[i + 6] == '/';
        if !is_https && !is_http {
            i += 1;
            continue;
        }
        let start = i;
        i += if is_https { 8 } else { 7 };
        while i < chars.len() {
            let c = chars[i];
            if c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`' | '|' | '{' | '}') {
                break;
            }
            i += 1;
        }
        let mut end = i;
        while end > start {
            let last = chars[end - 1];
            if matches!(
                last,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '(' | '['
            ) {
                end -= 1;
            } else {
                break;
            }
        }
        if end > start {
            let url: String = chars[start..end].iter().collect();
            out.push(UrlSpan { start, end, url });
        }
        i = end.max(start + 1);
    }
    out
}

/// Screen hit region for a clickable URL (absolute terminal coordinates).
#[derive(Debug, Clone)]
pub struct LinkHit {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub url: String,
}

/// One display row produced by [`layout_note`].
#[derive(Debug, Clone)]
pub struct DisplayRow {
    pub text: String,
    /// Inclusive-exclusive char index range in the original note body for each column.
    /// `col_to_src[col]` is the source char index for that display column.
    pub col_to_src: Vec<usize>,
}

/// Hard-wrap `text` to `width` columns, preserving explicit newlines.
///
/// Each display column maps back to a source character index for link hit-testing.
pub fn layout_note(text: &str, width: usize) -> Vec<DisplayRow> {
    if width == 0 {
        return vec![DisplayRow {
            text: String::new(),
            col_to_src: Vec::new(),
        }];
    }
    let chars: Vec<char> = text.chars().collect();
    let mut rows = Vec::new();
    let mut idx = 0usize;

    while idx < chars.len() {
        if chars[idx] == '\n' {
            rows.push(DisplayRow {
                text: String::new(),
                col_to_src: Vec::new(),
            });
            idx += 1;
            continue;
        }
        let mut text_line = String::new();
        let mut col_to_src = Vec::new();
        while idx < chars.len() && chars[idx] != '\n' && col_to_src.len() < width {
            text_line.push(chars[idx]);
            col_to_src.push(idx);
            idx += 1;
        }
        rows.push(DisplayRow {
            text: text_line,
            col_to_src,
        });
        if idx < chars.len() && chars[idx] == '\n' {
            idx += 1;
        }
    }

    if rows.is_empty() {
        rows.push(DisplayRow {
            text: String::new(),
            col_to_src: Vec::new(),
        });
    }
    rows
}

/// Build styled-ready rows and absolute link hit boxes for the visible viewport.
pub fn visible_link_hits(
    rows: &[DisplayRow],
    urls: &[UrlSpan],
    origin_x: u16,
    origin_y: u16,
    scroll: u16,
    visible_rows: u16,
) -> Vec<LinkHit> {
    let mut hits = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let visual = row_idx as i32 - scroll as i32;
        if visual < 0 || visual >= visible_rows as i32 {
            continue;
        }
        let y = origin_y.saturating_add(visual as u16);
        for url in urls {
            let mut run_start: Option<usize> = None;
            for (col, &src) in row.col_to_src.iter().enumerate() {
                let inside = src >= url.start && src < url.end;
                if inside {
                    if run_start.is_none() {
                        run_start = Some(col);
                    }
                } else if let Some(start_col) = run_start.take() {
                    hits.push(LinkHit {
                        x: origin_x.saturating_add(start_col as u16),
                        y,
                        width: (col - start_col) as u16,
                        url: url.url.clone(),
                    });
                }
            }
            if let Some(start_col) = run_start {
                hits.push(LinkHit {
                    x: origin_x.saturating_add(start_col as u16),
                    y,
                    width: (row.col_to_src.len() - start_col) as u16,
                    url: url.url.clone(),
                });
            }
        }
    }
    hits
}

/// True when `(x, y)` falls inside any hit; returns the URL.
pub fn hit_test(hits: &[LinkHit], x: u16, y: u16) -> Option<&str> {
    hits.iter()
        .find(|h| y == h.y && x >= h.x && x < h.x.saturating_add(h.width))
        .map(|h| h.url.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_collapses_whitespace_and_truncates() {
        assert_eq!(one_line_preview("hello\nworld", 20), "hello world");
        assert_eq!(one_line_preview("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn finds_https_and_strips_trailing_punct() {
        let spans = find_urls("see https://example.com/path), please");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "https://example.com/path");
    }

    #[test]
    fn finds_multiple_urls() {
        let spans = find_urls("a http://a.test b https://b.test/x");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].url, "http://a.test");
        assert_eq!(spans[1].url, "https://b.test/x");
    }

    #[test]
    fn layout_maps_columns_to_source() {
        let rows = layout_note("hi\nthere", 10);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "hi");
        assert_eq!(rows[0].col_to_src, vec![0, 1]);
        assert_eq!(rows[1].text, "there");
        assert_eq!(rows[1].col_to_src, vec![3, 4, 5, 6, 7]);
    }

    #[test]
    fn link_hit_covers_url_cells() {
        let text = "go https://x.test now";
        let rows = layout_note(text, 40);
        let urls = find_urls(text);
        let hits = visible_link_hits(&rows, &urls, 2, 5, 0, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://x.test");
        assert_eq!(hits[0].x, 2 + 3); // "go " then URL
        assert_eq!(hits[0].y, 5);
        assert_eq!(hits[0].width, "https://x.test".chars().count() as u16);
        assert_eq!(hit_test(&hits, hits[0].x, 5), Some("https://x.test"));
        assert_eq!(hit_test(&hits, 2, 5), None);
    }
}
