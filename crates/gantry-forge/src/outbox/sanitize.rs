//! 出站文本净化（FA §2.4 第二道防线；forge.md §7.11）。
//!
//! 顺序：去 HTML 标签与注释 → `@` 提及包反引号 → 跨仓库引用中和 → 行首 `/word` 转义 →
//! 关闭关键字中和 → 截断。密钥扫描由调用方经 `PolicyPort::scan_text` 做（命中即
//! `validation_failed(secret_detected)`），标记最后由 forge 自己追加。
//! 本模块是纯函数，不做 IO。

/// 行首命令串前插入的零宽空格。
const ZWSP: char = '\u{200B}';

const CLOSING_KEYWORDS: [&str; 9] = [
    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
];

/// 净化入队方提供的文本。`repo` 是本 Project 的 `owner/name`，同仓库的 `#N` 保持原样。
pub(crate) fn outbound(text: &str, repo: &str, max_chars: usize) -> String {
    let text = strip_html(text);
    let text = neutralize_tokens(&text, repo);
    let text = escape_commands(&text);
    truncate(&text, max_chars)
}

/// 去掉 HTML 注释与标签。未闭合的注释 / 标签从起点截掉，宁可少写不可夹带。
fn strip_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let comment = rest.find("<!--");
        let tag = find_tag_start(rest);
        let next = match (comment, tag) {
            (Some(c), Some(t)) => Some(c.min(t)),
            (one, other) => one.or(other),
        };
        let Some(start) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let end = if let Some(comment) = after.strip_prefix("<!--") {
            comment.find("-->").map(|end| 4 + end + 3)
        } else {
            after.find('>').map(|end| end + 1)
        };
        match end {
            Some(end) => rest = &after[end..],
            None => return out,
        }
    }
}

/// `<` 后跟字母、`/` 或 `!` 才算标签起点；`a < b` 之类的比较保持原样。
fn find_tag_start(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    (0..bytes.len()).find(|&i| {
        bytes[i] == b'<'
            && bytes
                .get(i + 1)
                .is_some_and(|next| next.is_ascii_alphabetic() || *next == b'/' || *next == b'!')
    })
}

/// 按空白切词处理提及、跨仓库引用与关闭关键字；保留原有空白。
fn neutralize_tokens(text: &str, repo: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut previous_word = String::new();
    for piece in split_keep_whitespace(text) {
        if piece.chars().all(char::is_whitespace) {
            out.push_str(piece);
            continue;
        }
        let neutralized = if is_mention(piece) || is_cross_repo_ref(piece, repo) {
            wrap(piece)
        } else if is_issue_ref(piece) && is_closing_keyword(&previous_word) {
            // 入队方文本里的关闭关键字一律中和；forge 自己生成的 `Closes #N` 在净化之后追加。
            wrap(piece)
        } else {
            piece.to_owned()
        };
        out.push_str(&neutralized);
        previous_word = piece.trim_end_matches([':', ',']).to_ascii_lowercase();
    }
    out
}

fn split_keep_whitespace(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut in_space = None;
    for (index, ch) in text.char_indices() {
        let space = ch.is_whitespace();
        if in_space.is_some_and(|was| was != space) {
            pieces.push(&text[start..index]);
            start = index;
        }
        in_space = Some(space);
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

fn wrap(token: &str) -> String {
    // 去掉已有反引号再包，避免 "``" 拼接后被提前闭合。
    format!("`{}`", token.replace('`', ""))
}

fn trimmed(token: &str) -> &str {
    token.trim_matches(|c: char| {
        matches!(
            c,
            '(' | ')' | '[' | ']' | ',' | '.' | ';' | ':' | '!' | '?' | '"' | '\''
        )
    })
}

/// `@user`、`@org/team`。邮箱（`a@b.c`）不算提及。
fn is_mention(token: &str) -> bool {
    let token = trimmed(token);
    let Some(name) = token.strip_prefix('@') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '/' || c == '_')
}

/// `owner/repo#N`、`owner/repo@sha`、指向其他仓库 Issue / PR 的 github.com 链接。
fn is_cross_repo_ref(token: &str, repo: &str) -> bool {
    let token = trimmed(token);
    let lower = token.to_ascii_lowercase();
    let repo = repo.to_ascii_lowercase();
    if let Some(path) = lower
        .strip_prefix("https://github.com/")
        .or_else(|| lower.strip_prefix("http://github.com/"))
    {
        let mut parts = path.split('/');
        let (Some(owner), Some(name), Some(kind)) = (parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        return matches!(kind, "issues" | "pull" | "commit") && format!("{owner}/{name}") != repo;
    }
    for sep in ['#', '@'] {
        if let Some((target, rest)) = lower.split_once(sep) {
            let looks_like_repo = target.split('/').count() == 2
                && target
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
            if looks_like_repo && !rest.is_empty() && target != repo {
                return true;
            }
        }
    }
    false
}

fn is_issue_ref(token: &str) -> bool {
    let token = trimmed(token);
    token
        .strip_prefix('#')
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

fn is_closing_keyword(word: &str) -> bool {
    CLOSING_KEYWORDS.contains(&word)
}

/// 行首（忽略缩进）的 `/word` 前插入零宽空格，防止被当成命令（自触发，Q-04）。
fn escape_commands(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let indent = line.len() - line.trim_start().len();
        let rest = &line[indent..];
        if rest.starts_with('/') && rest[1..].starts_with(|c: char| c.is_ascii_alphabetic()) {
            out.push_str(&line[..indent]);
            out.push(ZWSP);
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
    }
    out
}

/// 按字符数截断并注明（对象长度上限需核实，FA OQ-8）。
fn truncate(text: &str, max_chars: usize) -> String {
    const NOTE: &str = "\n\n…(truncated by Gantry)";
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let keep = max_chars.saturating_sub(NOTE.chars().count());
    let mut out: String = text.chars().take(keep).collect();
    out.push_str(NOTE);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "acme/shop-api";

    fn clean(text: &str) -> String {
        outbound(text, REPO, 65_536)
    }

    #[test]
    fn strips_html_and_forged_markers() {
        assert_eq!(
            clean("a <b>bold</b> <!-- gantry:v1 act=act_x --> c"),
            "a bold  c"
        );
        assert_eq!(clean("x <!-- unclosed"), "x ");
        assert_eq!(clean("1 < 2 and 3 > 2"), "1 < 2 and 3 > 2");
    }

    #[test]
    fn wraps_mentions_but_not_emails() {
        assert_eq!(
            clean("ping @alice and @acme/core."),
            "ping `@alice` and `@acme/core.`"
        );
        assert_eq!(clean("mail a@b.example"), "mail a@b.example");
    }

    #[test]
    fn neutralizes_cross_repo_refs_and_keeps_same_repo() {
        assert_eq!(clean("see other/repo#12"), "see `other/repo#12`");
        assert_eq!(clean("see other/repo@abc123"), "see `other/repo@abc123`");
        assert_eq!(
            clean("https://github.com/evil/x/issues/1"),
            "`https://github.com/evil/x/issues/1`"
        );
        assert_eq!(clean("acme/shop-api#3 and #4"), "acme/shop-api#3 and #4");
        assert_eq!(
            clean("https://github.com/acme/shop-api/pull/9"),
            "https://github.com/acme/shop-api/pull/9"
        );
    }

    #[test]
    fn escapes_line_start_commands() {
        assert_eq!(
            clean("/gantry retry\n  /approve\nnot /here"),
            "\u{200B}/gantry retry\n  \u{200B}/approve\nnot /here"
        );
    }

    #[test]
    fn neutralizes_closing_keywords() {
        assert_eq!(
            clean("Fixes #999 and closes: #7"),
            "Fixes `#999` and closes: `#7`"
        );
        assert_eq!(clean("mentions #5"), "mentions #5");
    }

    #[test]
    fn truncates_with_note() {
        let long = "x".repeat(100);
        let out = outbound(&long, REPO, 50);
        assert_eq!(out.chars().count(), 50);
        assert!(out.ends_with("(truncated by Gantry)"));
    }
}
