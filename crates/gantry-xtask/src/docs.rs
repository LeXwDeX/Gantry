//! 契约文档的机械读取：JSON 代码块与 api.md §4.1 路由表。
//!
//! 读取只做行号、原始文本与哈希，不解释示例语义。

use std::fs;
use std::path::Path;

use crate::util::sha256_hex;

pub struct Fence {
    pub source: String,
    pub line: usize,
    pub context: String,
    pub body: String,
    pub sha256: String,
}

pub fn json_fences(root: &Path, relative: &str) -> Result<Vec<Fence>, String> {
    let path = root.join(relative);
    let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    let mut fences = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let trimmed = lines[index].trim();
        if !(trimmed.eq_ignore_ascii_case("```json") || trimmed.eq_ignore_ascii_case("``` json")) {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut context = String::new();
        let mut cursor = index;
        while cursor > 0 {
            cursor -= 1;
            if lines[cursor].starts_with('#') {
                context = lines[cursor].trim().to_owned();
                break;
            }
        }
        index += 1;
        let mut body = Vec::new();
        while index < lines.len() && !lines[index].trim().starts_with("```") {
            body.push(lines[index]);
            index += 1;
        }
        let body = body.join("\n");
        fences.push(Fence {
            source: relative.to_owned(),
            line: start,
            context,
            sha256: sha256_hex(body.as_bytes()),
            body,
        });
        index += 1;
    }
    Ok(fences)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Route {
    pub method: String,
    pub path: String,
}

pub fn api_routes(root: &Path) -> Result<Vec<Route>, String> {
    let path = root.join("docs/interfaces/api.md");
    let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut routes = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !in_table {
            if trimmed.starts_with("| # | 方法 | 路径 |") {
                in_table = true;
            }
            continue;
        }
        if !trimmed.starts_with('|') {
            if !routes.is_empty() {
                break;
            }
            continue;
        }
        let cells: Vec<&str> = trimmed.split('|').map(str::trim).collect();
        // 首尾各有空串；data cells: 1..#, 2..method, 3..path
        if cells.len() < 5 {
            continue;
        }
        let Some(method) = normalize_method(cells[2]) else {
            continue;
        };
        let Some(path) = normalize_path(cells[3]) else {
            continue;
        };
        routes.push(Route { method, path });
    }
    if routes.is_empty() {
        return Err("api.md §4.1 路由表未找到".to_owned());
    }
    Ok(routes)
}

fn normalize_method(cell: &str) -> Option<String> {
    let method = cell.trim();
    if method.is_empty()
        || !method
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch == '-')
    {
        return None;
    }
    Some(method.to_owned())
}

fn normalize_path(cell: &str) -> Option<String> {
    let mut path = cell.trim().replace('`', "");
    if let Some(position) = path.find('（') {
        path.truncate(position);
    }
    let path = path.trim().to_owned();
    if !path.starts_with('/') {
        return None;
    }
    Some(path)
}
