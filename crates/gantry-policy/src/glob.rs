//! gitignore 风格的路径通配符（policy.md §7.2；config.md §5.2）。
//!
//! 只做匹配，不读文件系统。语义取 gitignore 的子集，区分大小写：
//!
//! - 模式里除末尾外不含 `/`：与路径的**任一组件**比较（`package.json` 也命中
//!   `a/package.json`；命中目录即命中其下全部文件）。
//! - 含 `/`：相对仓库根锚定（开头的 `/` 可省略）；模式命中路径的前若干组件
//!   时，视为命中该目录，其下全部文件都算命中。
//! - 末尾 `/`：只匹配目录，即路径必须还有更深的组件。
//! - 组件内：`*` 匹配任意个非 `/` 字符，`?` 匹配一个，`[...]` 为字符类（支持
//!   区间与 `!` / `^` 取反）；整个组件为 `**` 时匹配零个或多个组件，其余位置的
//!   连续 `*` 按单个 `*` 处理。
//! - **不支持** `!` 取反与 `\` 转义（取反会让 `deny` 规则不可判定，V15）。

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GlobError {
    #[error("glob must not be empty")]
    Empty,
    #[error("glob negation `!` is not supported")]
    Negation,
    #[error("glob escape `\\` is not supported")]
    Escape,
    #[error("glob has an empty component")]
    EmptyComponent,
    #[error("glob has an unterminated character class")]
    UnterminatedClass,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Glob {
    source: String,
    segments: Vec<String>,
    anchored: bool,
    dir_only: bool,
}

impl Glob {
    pub fn parse(pattern: &str) -> Result<Self, GlobError> {
        if pattern.is_empty() {
            return Err(GlobError::Empty);
        }
        if pattern.starts_with('!') {
            return Err(GlobError::Negation);
        }
        if pattern.contains('\\') {
            return Err(GlobError::Escape);
        }
        let mut body = pattern;
        let dir_only = body.ends_with('/');
        if dir_only {
            body = &body[..body.len() - 1];
        }
        let anchored = body.contains('/');
        let body = body.strip_prefix('/').unwrap_or(body);
        if body.is_empty() {
            return Err(GlobError::Empty);
        }
        let segments: Vec<String> = body.split('/').map(str::to_owned).collect();
        for segment in &segments {
            if segment.is_empty() {
                return Err(GlobError::EmptyComponent);
            }
            check_classes(segment)?;
        }
        Ok(Self {
            source: pattern.to_owned(),
            segments,
            anchored,
            dir_only,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// `path` 是仓库相对 POSIX 路径（已规范化，没有空组件）。
    pub fn matches(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        if self.anchored {
            return match_prefix(&self.segments, &parts, self.dir_only);
        }
        // 无斜杠模式只有一个组件，与任一组件比较；目录命中覆盖其下文件。
        let segment = &self.segments[0];
        parts.iter().enumerate().any(|(index, part)| {
            let is_dir = index + 1 < parts.len();
            (is_dir || !self.dir_only) && match_component(segment, part)
        })
    }
}

impl fmt::Debug for Glob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Glob").field(&self.source).finish()
    }
}

/// 一组通配符，任一命中即命中。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GlobSet {
    globs: Vec<Glob>,
}

impl GlobSet {
    pub fn parse<S: AsRef<str>>(patterns: &[S]) -> Result<Self, GlobError> {
        let globs = patterns
            .iter()
            .map(|pattern| Glob::parse(pattern.as_ref()))
            .collect::<Result<_, _>>()?;
        Ok(Self { globs })
    }

    pub fn matches(&self, path: &str) -> bool {
        self.globs.iter().any(|glob| glob.matches(path))
    }

    pub fn patterns(&self) -> impl Iterator<Item = &str> {
        self.globs.iter().map(Glob::as_str)
    }
}

/// 锚定模式：模式的全部组件命中路径的一段前缀。前缀短于路径时，被命中的是目录。
fn match_prefix(segments: &[String], parts: &[&str], dir_only: bool) -> bool {
    match segments.split_first() {
        None => !(dir_only && parts.is_empty()),
        Some((first, rest)) if first == "**" => {
            // `**` 吃掉零个或多个组件。
            (0..=parts.len()).any(|skip| match_prefix(rest, &parts[skip..], dir_only))
        }
        Some((first, rest)) => match parts.split_first() {
            Some((part, tail)) => {
                match_component(first, part) && match_prefix(rest, tail, dir_only)
            }
            None => false,
        },
    }
}

fn match_component(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    match_chars(&pattern, &text)
}

fn match_chars(pattern: &[char], text: &[char]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some(('*', rest)) => {
            let rest = skip_stars(rest);
            (0..=text.len()).any(|skip| match_chars(rest, &text[skip..]))
        }
        Some(('?', rest)) => !text.is_empty() && match_chars(rest, &text[1..]),
        Some(('[', _)) => {
            let Some((class, rest)) = parse_class(pattern) else {
                return false;
            };
            match text.split_first() {
                Some((ch, tail)) => class.contains(*ch) && match_chars(rest, tail),
                None => false,
            }
        }
        Some((literal, rest)) => match text.split_first() {
            Some((ch, tail)) => ch == literal && match_chars(rest, tail),
            None => false,
        },
    }
}

fn skip_stars(mut pattern: &[char]) -> &[char] {
    while let Some(('*', rest)) = pattern.split_first() {
        pattern = rest;
    }
    pattern
}

struct CharClass {
    negated: bool,
    items: Vec<(char, char)>,
}

impl CharClass {
    fn contains(&self, ch: char) -> bool {
        let hit = self.items.iter().any(|(lo, hi)| *lo <= ch && ch <= *hi);
        hit != self.negated
    }
}

/// `pattern[0] == '['`；返回字符类与其后的剩余模式。
fn parse_class(pattern: &[char]) -> Option<(CharClass, &[char])> {
    let mut index = 1;
    let negated = matches!(pattern.get(index), Some('!' | '^'));
    if negated {
        index += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    while let Some(&ch) = pattern.get(index) {
        if ch == ']' && !first {
            return Some((CharClass { negated, items }, &pattern[index + 1..]));
        }
        first = false;
        if pattern.get(index + 1) == Some(&'-') && pattern.get(index + 2).is_some_and(|c| *c != ']')
        {
            items.push((ch, pattern[index + 2]));
            index += 3;
        } else {
            items.push((ch, ch));
            index += 1;
        }
    }
    None
}

fn check_classes(segment: &str) -> Result<(), GlobError> {
    let chars: Vec<char> = segment.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '[' {
            let (_, rest) = parse_class(&chars[index..]).ok_or(GlobError::UnterminatedClass)?;
            index = chars.len() - rest.len();
        } else {
            index += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(pattern: &str, path: &str) -> bool {
        Glob::parse(pattern).expect("valid glob").matches(path)
    }

    #[test]
    fn slashless_pattern_matches_any_component() {
        assert!(hit("package.json", "package.json"));
        assert!(hit("package.json", "web/package.json"));
        assert!(hit("*.pem", "certs/server.pem"));
        assert!(hit(".env.*", ".env.local"));
        assert!(hit("requirements*.txt", "requirements-dev.txt"));
        assert!(hit(".devcontainer", ".devcontainer/devcontainer.json"));
        assert!(!hit("package.json", "package.json5"));
    }

    #[test]
    fn anchored_pattern_is_rooted_and_covers_directories() {
        assert!(hit(".github/workflows/**", ".github/workflows/ci.yml"));
        assert!(hit(
            ".github/workflows/**",
            ".github/workflows/nested/x.yml"
        ));
        assert!(hit(".github/workflows", ".github/workflows/ci.yml"));
        assert!(!hit(".github/workflows/**", "x/.github/workflows/ci.yml"));
        assert!(hit("/CODEOWNERS", "CODEOWNERS"));
        assert!(!hit("/CODEOWNERS", "docs/CODEOWNERS"));
        assert!(hit("**/Cargo.lock", "Cargo.lock"));
        assert!(hit("**/Cargo.lock", "a/b/Cargo.lock"));
        assert!(hit("**", "any/where.txt"));
    }

    #[test]
    fn matching_is_case_sensitive() {
        assert!(!hit("package.json", "Package.json"));
        assert!(!hit(".github/workflows/**", ".GitHub/workflows/ci.yml"));
    }

    #[test]
    fn dir_only_pattern_needs_deeper_component() {
        assert!(hit("build/", "build/out.o"));
        assert!(!hit("build/", "build"));
    }

    #[test]
    fn character_classes() {
        assert!(hit("file[0-9].txt", "file7.txt"));
        assert!(!hit("file[!0-9].txt", "file7.txt"));
        assert!(hit("file[!0-9].txt", "filex.txt"));
    }

    #[test]
    fn unsupported_syntax_is_rejected() {
        assert_eq!(Glob::parse("!keep"), Err(GlobError::Negation));
        assert_eq!(Glob::parse(r"a\*"), Err(GlobError::Escape));
        assert_eq!(Glob::parse(""), Err(GlobError::Empty));
        assert_eq!(Glob::parse("a//b"), Err(GlobError::EmptyComponent));
        assert_eq!(Glob::parse("a[b"), Err(GlobError::UnterminatedClass));
    }
}
