//! 补丁闸门的机械部分（R5，repo.md §7.4 的 P2–P5、P7、P11）。
//!
//! 原则（worker-protocol §8.3）：不自己写补丁解析器，事实来自 git 自己的输出。
//! 顺序是"原始字节 → git 预解析（不落盘）→ 应用 → 应用后枚举"；规则判定（P1 上限、
//! P6、P8、P9）归 `policy`。补丁里的路径、内容与 git 的 stderr 都是不可信数据。
//!
//! 唯一的例外是 [`classify_headers`]：只扫描 diff 头部行里的路径，**只用于拒绝**，
//! 从不据此放行任何东西（失败闭合）。两种用途：git 自己拒绝解析的补丁（例如含空组件
//! 的路径）本该归 `path_invalid`，用它细分原因；git 解析成功时也再查一遍，因为 git 会
//! 规整路径（引号路径里的 `\000` 被截断成前半段）。

use std::collections::BTreeMap;
use std::path::Path;

use gantry_core::canon::GitOid;
use gantry_core::ports::{AddedLineSource, RepoError};
use gantry_core::text::{normalize_untrusted, NormalizeLimits};
use gantry_core::worker::{FileMode, FileStatus, PatchFile, PatchRejectReason, RepoPath};
use unicode_normalization::UnicodeNormalization;

use crate::git::{GitCmd, GitEnv};
use crate::mirror::io_error;

/// 读取补丁的上限；体积硬上限由 sandbox intake 保证（repo.md §7.4 P1）。
pub const MAX_PATCH_BYTES: usize = 64 * 1024 * 1024;

/// 一次机械拒绝。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mechanical {
    pub reason: PatchRejectReason,
    pub detail: String,
}

impl Mechanical {
    fn new(reason: PatchRejectReason, detail: impl AsRef<str>) -> Self {
        Self {
            reason,
            detail: sanitize(detail.as_ref()),
        }
    }
}

/// 不可信文本进事件 / 日志前去掉控制符与 bidi 字符（repo.md §12.D 末项）。
pub fn sanitize(text: &str) -> String {
    let limits = NormalizeLimits {
        max_bytes: 4 * 1024,
        head_bytes: 2 * 1024,
        tail_bytes: 2 * 1024,
    };
    normalize_untrusted(text.as_bytes(), limits)
        .text
        .replace('\n', " ")
}

// ---------------------------------------------------------------------------
// P4 / P5：路径规范
// ---------------------------------------------------------------------------

/// HFS+ 比较文件名时忽略的字符（git `is_hfs_dotgit` 的同一张表）。
fn hfs_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{200C}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{206A}'..='\u{206F}' | '\u{FEFF}'
    )
}

/// 把一个路径组件折叠成"文件系统眼里的名字"：去 HFS 可忽略字符，去 NTFS 备用数据流
/// （`:` 之后）与结尾的点 / 空格，转小写。
fn fold_component(component: &str) -> String {
    let stripped: String = component.chars().filter(|c| !hfs_ignorable(*c)).collect();
    let base = stripped.split(':').next().unwrap_or_default();
    base.trim_end_matches(['.', ' ']).to_lowercase()
}

fn is_dotgit_component(component: &str) -> bool {
    let folded = fold_component(component);
    // NTFS 8.3 短名：`GIT~1`（及其他序号）指向 `.git`。
    folded == ".git"
        || (folded.starts_with("git~") && folded[4..].chars().all(|c| c.is_ascii_digit()))
}

fn is_gitmodules(path: &str) -> bool {
    path.split('/')
        .next_back()
        .map(fold_component)
        .is_some_and(|name| name == ".gitmodules" || name.starts_with("gitmod~"))
}

/// P4：UTF-8、非绝对、无 `..` / `.` / 空组件、无 `\` / NUL、无 `.git` 组件。
pub fn check_path(raw: &[u8]) -> Result<RepoPath, Mechanical> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| Mechanical::new(PatchRejectReason::PathInvalid, "path is not UTF-8"))?;
    let invalid =
        |why: &str| Mechanical::new(PatchRejectReason::PathInvalid, format!("{why}: {text}"));
    if text.is_empty() {
        return Err(invalid("empty path"));
    }
    if text.starts_with('/') {
        return Err(invalid("absolute path"));
    }
    if text.contains('\\') || text.contains('\0') {
        return Err(invalid("forbidden character in path"));
    }
    if text.chars().any(|c| c.is_control()) {
        return Err(invalid("control character in path"));
    }
    for component in text.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(invalid("invalid path component"));
        }
    }
    if text.split('/').any(is_dotgit_component) {
        return Err(Mechanical::new(
            PatchRejectReason::Dotgit,
            format!(".git component: {text}"),
        ));
    }
    RepoPath::parse(text).map_err(|_| invalid("invalid path"))
}

/// 大小写 / Unicode 规范化冲突检查用的键（NFC + 小写）。
pub fn collision_key(path: &str) -> String {
    path.nfc().collect::<String>().to_lowercase()
}

// ---------------------------------------------------------------------------
// 预解析（git apply --numstat / --summary，不碰文件系统）
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct Preparsed {
    /// 每条记录的路径（重命名 / 复制有两条）与是否二进制（numstat 为 `-`）。
    pub entries: Vec<(Vec<Vec<u8>>, bool)>,
    /// summary 里出现的 (路径, 模式)。
    pub modes: Vec<(Vec<u8>, FileMode)>,
}

/// 运行预解析；git 自己拒绝时返回 `Err(stderr)`。
pub async fn preparse(
    env: &GitEnv,
    cwd: &Path,
    patch: &[u8],
) -> Result<Result<Preparsed, String>, RepoError> {
    let numstat = GitCmd::new("apply-numstat", cwd)
        .args(["apply", "--numstat", "-z", "-p1", "-"])
        .stdin(patch.to_vec())
        .output(env)
        .await?;
    if !numstat.success() {
        return Ok(Err(numstat.stderr_tail.as_untrusted_str().to_owned()));
    }
    let summary = GitCmd::new("apply-summary", cwd)
        .args(["apply", "--summary", "-p1", "-"])
        .stdin(patch.to_vec())
        .output(env)
        .await?;
    if !summary.success() {
        return Ok(Err(summary.stderr_tail.as_untrusted_str().to_owned()));
    }
    Ok(Ok(Preparsed {
        entries: parse_numstat_z(&numstat.stdout),
        modes: parse_summary(&summary.stdout),
    }))
}

/// `added\tdeleted\tpath\0`；重命名 / 复制为 `added\tdeleted\t\0old\0new\0`。
fn parse_numstat_z(out: &[u8]) -> Vec<(Vec<Vec<u8>>, bool)> {
    let mut entries = Vec::new();
    let mut fields = out.split(|b| *b == 0).peekable();
    while let Some(head) = fields.next() {
        if head.is_empty() {
            continue;
        }
        let mut parts = head.splitn(3, |b| *b == b'\t');
        let added = parts.next().unwrap_or_default();
        let _deleted = parts.next();
        let path = parts.next().unwrap_or_default();
        let binary = added == b"-";
        let paths = if path.is_empty() {
            let old = fields.next().unwrap_or_default().to_vec();
            let new = fields.next().unwrap_or_default().to_vec();
            vec![old, new]
        } else {
            vec![path.to_vec()]
        };
        entries.push((paths, binary));
    }
    entries
}

/// ` create mode M path` / ` delete mode M path` / ` mode change A => B path`。
fn parse_summary(out: &[u8]) -> Vec<(Vec<u8>, FileMode)> {
    let mut modes = Vec::new();
    for line in out.split(|b| *b == b'\n') {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_start();
        let rest = line
            .strip_prefix("create mode ")
            .or_else(|| line.strip_prefix("delete mode "));
        if let Some(rest) = rest {
            if let Some((mode, path)) = rest.split_once(' ') {
                if let Ok(mode) = FileMode::parse(mode) {
                    modes.push((unquote(path), mode));
                }
            }
        } else if let Some(rest) = line.strip_prefix("mode change ") {
            let mut parts = rest.splitn(4, ' ');
            let (Some(old), Some(_), Some(new), Some(path)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            for mode in [old, new] {
                if let Ok(mode) = FileMode::parse(mode) {
                    modes.push((unquote(path), mode));
                }
            }
        }
    }
    modes
}

/// git 的 C 风格路径引号（`core.quotePath`）。
fn unquote(path: &str) -> Vec<u8> {
    let Some(inner) = path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
        return path.as_bytes().to_vec();
    };
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' || i + 1 >= bytes.len() {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let next = bytes[i + 1];
        let simple = match next {
            b'a' => Some(0x07),
            b'b' => Some(0x08),
            b't' => Some(b'\t'),
            b'n' => Some(b'\n'),
            b'v' => Some(0x0b),
            b'f' => Some(0x0c),
            b'r' => Some(b'\r'),
            b'"' => Some(b'"'),
            b'\\' => Some(b'\\'),
            _ => None,
        };
        if let Some(byte) = simple {
            out.push(byte);
            i += 2;
        } else if i + 3 < bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let value = (bytes[i + 1] - b'0') as u32 * 64
                + (bytes[i + 2] - b'0') as u32 * 8
                + (bytes[i + 3] - b'0') as u32;
            out.push(value as u8);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// 预解析事实上的 P4 / P5 / P7。
pub fn check_preparsed(pre: &Preparsed, raw: &[u8]) -> Result<(), Mechanical> {
    if mentions_binary(raw) || pre.entries.iter().any(|(_, binary)| *binary) {
        return Err(Mechanical::new(PatchRejectReason::Binary, "binary patch"));
    }
    for (paths, _) in &pre.entries {
        for path in paths {
            let path = check_path(path)?;
            if is_gitmodules(path.as_str()) {
                return Err(Mechanical::new(
                    PatchRejectReason::Submodule,
                    format!(".gitmodules change: {path}"),
                ));
            }
        }
    }
    for (path, mode) in &pre.modes {
        mode_rejection(*mode, &String::from_utf8_lossy(path))?;
    }
    Ok(())
}

fn mode_rejection(mode: FileMode, path: &str) -> Result<(), Mechanical> {
    match mode {
        FileMode::Mode120000 => Err(Mechanical::new(
            PatchRejectReason::Symlink,
            format!("symlink: {path}"),
        )),
        FileMode::Mode160000 => Err(Mechanical::new(
            PatchRejectReason::Submodule,
            format!("gitlink: {path}"),
        )),
        _ => Ok(()),
    }
}

/// P7 的另一半：补丁里出现 `GIT binary patch` / `Binary files` 行。
fn mentions_binary(raw: &[u8]) -> bool {
    raw.split(|b| *b == b'\n')
        .any(|line| line.starts_with(b"GIT binary patch") || line.starts_with(b"Binary files "))
}

/// 只看头部行里的路径找拒绝原因；找不到 → `None`。
pub fn classify_headers(raw: &[u8]) -> Option<Mechanical> {
    if mentions_binary(raw) {
        return Some(Mechanical::new(PatchRejectReason::Binary, "binary patch"));
    }
    for line in raw.split(|b| *b == b'\n') {
        let line = String::from_utf8_lossy(line);
        let mut candidates: Vec<String> = Vec::new();
        if let Some(rest) = line.strip_prefix("diff --git ") {
            candidates.extend(rest.split(' ').map(str::to_owned));
        } else if let Some(rest) = line
            .strip_prefix("--- ")
            .or_else(|| line.strip_prefix("+++ "))
        {
            candidates.push(rest.split('\t').next().unwrap_or_default().to_owned());
        } else {
            for prefix in ["rename from ", "rename to ", "copy from ", "copy to "] {
                if let Some(rest) = line.strip_prefix(prefix) {
                    candidates.push(format!("a/{rest}"));
                }
            }
        }
        for candidate in candidates {
            if candidate == "/dev/null" {
                continue;
            }
            let bytes = unquote(&candidate);
            let path = bytes
                .strip_prefix(b"a/")
                .or_else(|| bytes.strip_prefix(b"b/"))
                .unwrap_or(&bytes);
            if let Err(rejection) = check_path(path) {
                return Some(rejection);
            }
        }
    }
    None
}

/// P4 末项（应用前）：补丁里的路径与 base 树或补丁里另一条路径仅大小写或 NFC / NFD
/// 不同。放在应用前，是因为大小写不敏感的文件系统上 `git apply` 会先失败，
/// 这类补丁就会被误归为 `not_applicable`。
pub async fn collisions_before_apply(
    env: &GitEnv,
    mirror: &Path,
    base: &GitOid,
    pre: &Preparsed,
) -> Result<Option<Mechanical>, RepoError> {
    let listed = GitCmd::new("ls-tree", mirror)
        .args(["ls-tree", "-r", "-z", "--name-only", "--full-tree"])
        .arg(base.to_string())
        .run(env)
        .await?;
    let base_paths = listed
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned());
    let patch_paths: Vec<String> = pre
        .entries
        .iter()
        .flat_map(|(paths, _)| paths.iter())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    Ok(find_collision(
        base_paths.chain(patch_paths.iter().cloned()),
        &patch_paths,
    ))
}

/// 在 `all` 的每一级路径前缀上建折叠键，`touched` 里任一前缀撞上不同写法即冲突。
fn find_collision(all: impl Iterator<Item = String>, touched: &[String]) -> Option<Mechanical> {
    let mut by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in all {
        for prefix in prefixes(&path) {
            let entry = by_key.entry(collision_key(&prefix)).or_default();
            if !entry.contains(&prefix) {
                entry.push(prefix);
            }
        }
    }
    for path in touched {
        for prefix in prefixes(path) {
            if let Some(names) = by_key.get(&collision_key(&prefix)) {
                if names.len() > 1 {
                    return Some(Mechanical::new(
                        PatchRejectReason::PathInvalid,
                        format!("case / unicode collision: {}", names.join(", ")),
                    ));
                }
            }
        }
    }
    None
}

fn prefixes(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut prefix = String::new();
    for component in path.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        out.push(prefix.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// 应用后枚举（P3）
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Enumerated {
    pub files: Vec<PatchFile>,
    pub total_changed_lines: u64,
}

pub async fn enumerate(
    env: &GitEnv,
    scratch: &Path,
) -> Result<Result<Enumerated, Mechanical>, RepoError> {
    let raw = GitCmd::new("diff-raw", scratch)
        .args([
            "diff",
            "--cached",
            "--raw",
            "-z",
            "--no-renames",
            "--full-index",
            "--no-ext-diff",
            "--no-textconv",
            "HEAD",
        ])
        .run(env)
        .await?;
    let numstat = GitCmd::new("diff-numstat", scratch)
        .args([
            "diff",
            "--cached",
            "--numstat",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "HEAD",
        ])
        .run(env)
        .await?;
    let mut counts: BTreeMap<Vec<u8>, Option<(u32, u32)>> = BTreeMap::new();
    for (paths, _) in parse_numstat_z(&numstat.stdout) {
        // --no-renames：每条只有一个路径。
        if let Some(path) = paths.into_iter().next() {
            counts.insert(path, None);
        }
    }
    for chunk in numstat.stdout.split(|b| *b == 0) {
        let mut parts = chunk.splitn(3, |b| *b == b'\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let parse = |field: &[u8]| std::str::from_utf8(field).ok()?.parse::<u32>().ok();
        if let (Some(a), Some(d)) = (parse(added), parse(deleted)) {
            counts.insert(path.to_vec(), Some((a, d)));
        }
    }

    let mut files = Vec::new();
    let mut new_blobs = Vec::new();
    let mut fields = raw.stdout.split(|b| *b == 0);
    while let Some(meta) = fields.next() {
        if meta.is_empty() {
            continue;
        }
        let Some(path) = fields.next() else {
            break;
        };
        let meta = String::from_utf8_lossy(meta);
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        if parts.len() < 5 {
            return Err(io_error("unexpected git diff --raw output"));
        }
        let repo_path = match check_path(path) {
            Ok(path) => path,
            Err(rejection) => return Ok(Err(rejection)),
        };
        let old_mode = parse_mode(parts[0]);
        let new_mode = parse_mode(parts[1]);
        let status = match parts[4].chars().next() {
            Some('A') => FileStatus::Added,
            Some('D') => FileStatus::Deleted,
            Some('M') if parts[2] == parts[3] => FileStatus::ModeOnly,
            _ => FileStatus::Modified,
        };
        let is_symlink = [old_mode, new_mode].contains(&Some(FileMode::Mode120000));
        let is_submodule = [old_mode, new_mode].contains(&Some(FileMode::Mode160000))
            || is_gitmodules(repo_path.as_str());
        let count = counts.get(path).copied().flatten();
        let is_binary = counts.contains_key(path) && count.is_none();
        if new_mode.is_some_and(FileMode::is_regular) {
            new_blobs.push((files.len(), parts[3].to_owned()));
        }
        files.push(PatchFile {
            path: repo_path,
            status,
            old_mode,
            new_mode,
            added: count.map(|(a, _)| a),
            deleted: count.map(|(_, d)| d),
            is_binary,
            is_symlink,
            is_submodule,
            size_after: None,
        });
    }

    for file in &files {
        if file.is_submodule {
            return Ok(Err(Mechanical::new(
                PatchRejectReason::Submodule,
                format!("submodule change: {}", file.path),
            )));
        }
        if file.is_symlink {
            return Ok(Err(Mechanical::new(
                PatchRejectReason::Symlink,
                format!("symlink: {}", file.path),
            )));
        }
        if file.is_binary {
            return Ok(Err(Mechanical::new(
                PatchRejectReason::Binary,
                format!("binary: {}", file.path),
            )));
        }
    }

    if !new_blobs.is_empty() {
        let input: String = new_blobs
            .iter()
            .map(|(_, oid)| format!("{oid}\n"))
            .collect();
        let sizes = GitCmd::new("cat-file-sizes", scratch)
            .args(["cat-file", "--batch-check=%(objectsize)"])
            .stdin(input.into_bytes())
            .run(env)
            .await?;
        let text = sizes.stdout_str()?;
        for ((index, _), line) in new_blobs.iter().zip(text.lines()) {
            files[*index].size_after = line.trim().parse().ok();
        }
    }

    if let Some(rejection) = collisions(env, scratch, &files).await? {
        return Ok(Err(rejection));
    }

    let total_changed_lines = files
        .iter()
        .map(|file| u64::from(file.added.unwrap_or(0)) + u64::from(file.deleted.unwrap_or(0)))
        .sum();
    Ok(Ok(Enumerated {
        files,
        total_changed_lines,
    }))
}

fn parse_mode(mode: &str) -> Option<FileMode> {
    if mode == "000000" {
        None
    } else {
        FileMode::parse(mode).ok()
    }
}

/// P4 末项（应用后复核）：以 index 为准再查一次。
async fn collisions(
    env: &GitEnv,
    scratch: &Path,
    files: &[PatchFile],
) -> Result<Option<Mechanical>, RepoError> {
    let listed = GitCmd::new("ls-files", scratch)
        .args(["ls-files", "-z", "--cached"])
        .run(env)
        .await?;
    let all = listed
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned());
    let touched: Vec<String> = files
        .iter()
        .filter(|f| f.status != FileStatus::Deleted)
        .map(|f| f.path.as_str().to_owned())
        .collect();
    Ok(find_collision(all, &touched))
}

// ---------------------------------------------------------------------------
// 新增行（交 policy 的 P9 扫描）
// ---------------------------------------------------------------------------

/// `GitAddedLines` 的预取版本：在 scratch 检出存活期间，对每个文件运行
/// `git diff --cached -U0 --no-color --no-ext-diff --no-textconv --no-renames HEAD -- <path>`，
/// 只取 hunk 内以 `+` 开头的行——解析的是 git 规整过的输出，不是沙盒给的补丁文本。
/// `AddedLineSource` 是同步接口，所以在调用 `policy` 之前取齐。
#[derive(Debug, Default)]
pub struct AddedLines {
    lines: BTreeMap<RepoPath, Vec<String>>,
}

impl AddedLines {
    pub async fn collect(
        env: &GitEnv,
        scratch: &Path,
        files: &[PatchFile],
    ) -> Result<Self, RepoError> {
        let mut lines = BTreeMap::new();
        for file in files.iter().filter(|f| f.added.unwrap_or(0) > 0) {
            let out = GitCmd::new("diff-added", scratch)
                .args([
                    "--literal-pathspecs",
                    "diff",
                    "--cached",
                    "-U0",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    "HEAD",
                    "--",
                ])
                .arg(file.path.as_str())
                .run(env)
                .await?;
            lines.insert(file.path.clone(), added_from_diff(&out.stdout));
        }
        Ok(Self { lines })
    }
}

fn added_from_diff(diff: &[u8]) -> Vec<String> {
    let mut in_hunk = false;
    let mut out = Vec::new();
    for line in diff.split(|b| *b == b'\n') {
        if line.starts_with(b"@@") {
            in_hunk = true;
            continue;
        }
        if line.starts_with(b"diff --git ") {
            in_hunk = false;
            continue;
        }
        if in_hunk {
            if let Some(rest) = line.strip_prefix(b"+") {
                out.push(String::from_utf8_lossy(rest).into_owned());
            }
        }
    }
    out
}

impl AddedLineSource for AddedLines {
    fn added_lines<'a>(
        &'a self,
        path: &'a RepoPath,
    ) -> Box<dyn Iterator<Item = std::io::Result<String>> + 'a> {
        match self.lines.get(path) {
            Some(lines) => Box::new(lines.iter().cloned().map(Ok)),
            None => Box::new(std::iter::empty()),
        }
    }
}

// ---------------------------------------------------------------------------
// 一次性检出
// ---------------------------------------------------------------------------

/// `git worktree add --detach <scratch>/<job_id> <base_sha>`。
pub async fn add_scratch(
    env: &GitEnv,
    mirror: &Path,
    dir: &Path,
    base: &GitOid,
) -> Result<(), RepoError> {
    crate::mirror::remove_dir_if_exists(dir)?;
    GitCmd::new("worktree-add", mirror)
        .args(["worktree", "add", "--quiet", "--detach", "--no-checkout"])
        .arg(dir)
        .arg(base.to_string())
        .run(env)
        .await?;
    // 先只填 index 再按 index 检出：不运行 post-checkout（hooksPath=/dev/null），
    // 也不初始化子模块。
    GitCmd::new("read-tree", dir)
        .args(["read-tree", "--reset", "-u", "HEAD"])
        .run(env)
        .await?;
    Ok(())
}

/// 删除一次性检出与其 worktree 元数据；成功、失败、超时都调用。
pub async fn remove_scratch(env: &GitEnv, mirror: &Path, dir: &Path) {
    let _ = GitCmd::new("worktree-remove", mirror)
        .args(["worktree", "remove", "--force"])
        .arg(dir)
        .output(env)
        .await;
    let _ = crate::mirror::remove_dir_if_exists(dir);
    let _ = GitCmd::new("worktree-prune", mirror)
        .args(["worktree", "prune"])
        .output(env)
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotgit_variants_are_detected() {
        for path in [
            ".git/config",
            ".GIT/config",
            "a/.Git/hooks/x",
            "GIT~1/config",
            ".git::$INDEX_ALLOCATION/x",
            ".g\u{200c}it/config",
            ".git./x",
            ".git /x",
        ] {
            let err = check_path(path.as_bytes()).unwrap_err();
            assert_eq!(err.reason, PatchRejectReason::Dotgit, "{path:?}");
        }
        assert!(check_path(b".github/workflows/ci.yml").is_ok());
        assert!(check_path(b"src/git/mod.rs").is_ok());
    }

    #[test]
    fn invalid_paths_are_detected() {
        for path in [
            "../x",
            "a/../../x",
            "/abs",
            "a\\b",
            "a//b",
            "a/./b",
            "",
            "a\0b",
        ] {
            let err = check_path(path.as_bytes()).unwrap_err();
            assert_eq!(err.reason, PatchRejectReason::PathInvalid, "{path:?}");
        }
        assert_eq!(
            check_path(&[0xff, 0xfe]).unwrap_err().reason,
            PatchRejectReason::PathInvalid
        );
    }

    #[test]
    fn unquote_handles_c_escapes() {
        assert_eq!(unquote("\"a\\tb\""), b"a\tb");
        assert_eq!(unquote("\"\\303\\251\""), "é".as_bytes());
        assert_eq!(unquote("plain"), b"plain");
    }

    #[test]
    fn added_lines_keep_plus_prefixed_content() {
        let diff = b"diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -0,0 +1,2 @@\n+++ looks like a header\n+plain\n";
        assert_eq!(
            added_from_diff(diff),
            vec!["++ looks like a header", "plain"]
        );
    }

    #[test]
    fn sanitize_strips_control_and_bidi() {
        let text = sanitize("a\u{202e}b\u{1b}[31mc\u{7}d\ne");
        assert!(!text.contains('\u{202e}') && !text.contains('\u{1b}') && !text.contains('\u{7}'));
        assert!(!text.contains('\n'));
    }
}
