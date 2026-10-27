//! 本机存储（desktop-shell.md §7.5、§6.1 `prefs_*` / `ui_state_*`、config.md §13.2）。
//!
//! 应用数据目录下三类文件，都只给当前用户（目录 0700、文件 0600），都原子写（临时文件 + 重命名）：
//!
//! - `identity.json`：配对后的非秘密身份（§7.2）。缺失或损坏一律按"未配对"处理，永不因它放行。
//! - `prefs.json`：外观、布局、本机通知开关；严格 schema，未知键拒绝，不得含凭据。
//! - `ui-state/{project_id}.json`：按项目的不透明界面状态；只校验大小与 JSON 形状。
//!
//! 不缓存任何服务端实体（OQ-S6）。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use gantry_core::ids::ProjectId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ipc::{ShellError, ShellErrorCode};
use crate::keys::KeyLabels;

/// 每个项目的界面状态块上限（§10 `ui_state.max_bytes_per_project`）。
pub const UI_STATE_MAX_BYTES_PER_PROJECT: usize = 256 * 1024;
/// 全部项目合计上限（§10 `ui_state.max_total_bytes`）。
pub const UI_STATE_MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;

const IDENTITY_FILE: &str = "identity.json";
const PREFS_FILE: &str = "prefs.json";
const UI_STATE_DIR: &str = "ui-state";

// ---- 偏好 ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Dark,
    Light,
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    Compact,
    Comfortable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Layout {
    pub left_width: u32,
    pub inspector_width: u32,
    pub left_collapsed: bool,
    pub inspector_collapsed: bool,
}

/// 与 `apps/desktop/src/api/shell.ts` 的 `Prefs` 同形（camelCase）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Prefs {
    pub theme: Theme,
    pub density: Density,
    pub language: String,
    pub system_notifications: bool,
    pub layout: Layout,
}

impl Default for Prefs {
    /// config.md §13.2 / desktop-shell.md §10：主题跟随系统、紧凑密度、`zh-CN`、系统通知开。
    fn default() -> Self {
        Self {
            theme: Theme::System,
            density: Density::Compact,
            language: "zh-CN".into(),
            system_notifications: true,
            layout: Layout {
                left_width: 240,
                inspector_width: 300,
                left_collapsed: false,
                inspector_collapsed: false,
            },
        }
    }
}

/// 面板宽度范围，与设置页（`SettingsAppearancePage.tsx`）一致。
const LEFT_WIDTH: (u32, u32) = (180, 480);
const INSPECTOR_WIDTH: (u32, u32) = (220, 520);

impl Prefs {
    fn validate(&self) -> Result<(), ShellError> {
        if !is_language_tag(&self.language) {
            return Err(ShellError::bad_argument("patch.language"));
        }
        let in_range = |v: u32, (lo, hi): (u32, u32)| (lo..=hi).contains(&v);
        if !in_range(self.layout.left_width, LEFT_WIDTH) {
            return Err(ShellError::bad_argument("patch.layout.leftWidth"));
        }
        if !in_range(self.layout.inspector_width, INSPECTOR_WIDTH) {
            return Err(ShellError::bad_argument("patch.layout.inspectorWidth"));
        }
        Ok(())
    }

    /// 应用一个补丁：顶层键整项替换（`layout` 也整项给出，与 `Partial<Prefs>` 一致）。
    /// 未知键、类型错误、越界、形似凭据的值一律拒绝，原偏好不变。
    pub fn patched(&self, patch: &Value) -> Result<Self, ShellError> {
        fn field<T: for<'de> Deserialize<'de>>(key: &str, value: &Value) -> Result<T, ShellError> {
            T::deserialize(value).map_err(|_| ShellError::bad_argument(format!("patch.{key}")))
        }
        let patch = patch
            .as_object()
            .ok_or_else(|| ShellError::bad_argument("patch"))?;
        let mut next = self.clone();
        for (key, value) in patch {
            match key.as_str() {
                "theme" => next.theme = field(key, value)?,
                "density" => next.density = field(key, value)?,
                "language" => next.language = field(key, value)?,
                "systemNotifications" => next.system_notifications = field(key, value)?,
                "layout" => next.layout = field(key, value)?,
                _ => return Err(ShellError::bad_argument(format!("patch.{key}"))),
            }
        }
        next.validate()?;
        Ok(next)
    }
}

/// BCP 47 语言标签的保守子集：`zh-CN`、`en`、`zh-Hans-CN`。语言是偏好里唯一的自由字符串，
/// 限定成这种形态后，凭据、URL、路径都放不进来（config.md §13.2"不得存放任何凭据"）。
fn is_language_tag(tag: &str) -> bool {
    if tag.len() > 35 {
        return false;
    }
    let mut parts = tag.split('-');
    let primary = parts.next().unwrap_or_default();
    (2..=3).contains(&primary.len())
        && primary.bytes().all(|b| b.is_ascii_lowercase())
        && parts.all(|p| (2..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

// ---- 身份 ----

/// `identity.json`（§7.2）：只存非秘密信息，两把私钥以引用名指向硬件密钥库。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityFile {
    pub api_base_url: String,
    /// 离线根 CA 证书 DER 的 SHA-256：`sha256:` + 64 位小写十六进制（P-64）。
    pub ca_fingerprint: String,
    pub device_id: String,
    /// `[叶子, 中间 CA, 根]`。
    pub cert_chain_pem: Vec<String>,
    pub tls_key_label: String,
    pub approval_key_label: String,
    pub paired_at: String,
    pub renewed_at: Option<String>,
    /// 最近见到的服务端事件日志纪元号（P-67）。
    pub log_epoch: Option<u64>,
}

impl IdentityFile {
    pub fn key_labels(&self) -> KeyLabels {
        KeyLabels {
            tls: self.tls_key_label.clone(),
            approval: self.approval_key_label.clone(),
        }
    }

    /// 文件可被本机其他进程篡改（§4），形状不对就当没有。
    fn is_well_formed(&self) -> bool {
        self.api_base_url.starts_with("https://")
            && is_sha256_fingerprint(&self.ca_fingerprint)
            && self.device_id.starts_with("dev_")
            && self.cert_chain_pem.len() == 3
            && self.key_labels().validate().is_ok()
    }
}

fn is_sha256_fingerprint(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

// ---- 存储 ----

/// 应用数据目录。
pub struct AppStore {
    dir: PathBuf,
}

impl AppStore {
    pub fn open(dir: &Path) -> Result<Self, ShellError> {
        create_private_dir(dir)?;
        create_private_dir(&dir.join(UI_STATE_DIR))?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    // 偏好

    /// 缺失或损坏 → 默认偏好（不信任文件内容）。
    pub fn prefs(&self) -> Prefs {
        read_json::<Prefs>(&self.dir.join(PREFS_FILE))
            .filter(|p| p.validate().is_ok())
            .unwrap_or_default()
    }

    pub fn set_prefs(&self, patch: &Value) -> Result<Prefs, ShellError> {
        let next = self.prefs().patched(patch)?;
        write_json(&self.dir.join(PREFS_FILE), &next)?;
        Ok(next)
    }

    // 界面状态

    fn ui_state_path(&self, project_id: &str) -> Result<PathBuf, ShellError> {
        // 只接受合法项目 ID，顺带杜绝路径穿越。
        project_id
            .parse::<ProjectId>()
            .map_err(|_| ShellError::bad_argument("project_id"))?;
        Ok(self
            .dir
            .join(UI_STATE_DIR)
            .join(format!("{project_id}.json")))
    }

    /// 不存在或损坏 → `None`。
    pub fn ui_state_get(&self, project_id: &str) -> Result<Option<Value>, ShellError> {
        Ok(read_json::<Value>(&self.ui_state_path(project_id)?).filter(Value::is_object))
    }

    pub fn ui_state_set(&self, project_id: &str, state: &Value) -> Result<(), ShellError> {
        let path = self.ui_state_path(project_id)?;
        if !state.is_object() {
            return Err(ShellError::bad_argument("state"));
        }
        let bytes = serde_json::to_vec(state).map_err(|_| ShellError::bad_argument("state"))?;
        if bytes.len() > UI_STATE_MAX_BYTES_PER_PROJECT {
            return Err(too_large());
        }
        let others: usize = self
            .ui_state_files()?
            .into_iter()
            .filter(|(id, _)| id != project_id)
            .map(|(_, p)| fs::metadata(p).map(|m| m.len() as usize).unwrap_or(0))
            .sum();
        if others + bytes.len() > UI_STATE_MAX_TOTAL_BYTES {
            return Err(too_large());
        }
        write_bytes(&path, &bytes)
    }

    /// 全部可读的项目状态；损坏的文件跳过。
    pub fn ui_state_list(&self) -> Result<BTreeMap<String, Value>, ShellError> {
        let mut out = BTreeMap::new();
        for (id, path) in self.ui_state_files()? {
            if let Some(state) = read_json::<Value>(&path).filter(Value::is_object) {
                out.insert(id, state);
            }
        }
        Ok(out)
    }

    /// `identity_reset` 时清除：其中的项目 ID 属于旧服务端（§6.1）。
    pub fn ui_state_clear(&self) -> Result<(), ShellError> {
        for (_, path) in self.ui_state_files()? {
            remove_if_exists(&path)?;
        }
        Ok(())
    }

    fn ui_state_files(&self) -> Result<Vec<(String, PathBuf)>, ShellError> {
        let entries = fs::read_dir(self.dir.join(UI_STATE_DIR)).map_err(io_error)?;
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
            else {
                continue;
            };
            if id.parse::<ProjectId>().is_ok() {
                out.push((id.to_string(), path));
            }
        }
        Ok(out)
    }

    // 身份

    /// 缺失、损坏或形状不对 → `None`（未配对）。
    pub fn identity(&self) -> Option<IdentityFile> {
        read_json::<IdentityFile>(&self.dir.join(IDENTITY_FILE))
            .filter(IdentityFile::is_well_formed)
    }

    pub fn save_identity(&self, identity: &IdentityFile) -> Result<(), ShellError> {
        if !identity.is_well_formed() {
            return Err(ShellError::bad_argument("identity"));
        }
        write_json(&self.dir.join(IDENTITY_FILE), identity)
    }

    pub fn delete_identity(&self) -> Result<(), ShellError> {
        remove_if_exists(&self.dir.join(IDENTITY_FILE))
    }
}

fn too_large() -> ShellError {
    ShellError::new(ShellErrorCode::TooLarge, "界面状态超过大小上限")
}

fn io_error(e: std::io::Error) -> ShellError {
    // 路径与系统错误只写日志，不过 IPC。
    let _ = e;
    ShellError::new(ShellErrorCode::Unavailable, "本机存储不可用")
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), ShellError> {
    let bytes = serde_json::to_vec_pretty(value).expect("可序列化");
    write_bytes(path, &bytes)
}

/// 原子写：同目录临时文件（0600）写完 fsync 再重命名。
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), ShellError> {
    let tmp = path.with_extension("json.tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    fs::rename(&tmp, path).map_err(io_error)
}

fn remove_if_exists(path: &Path) -> Result<(), ShellError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error(e)),
    }
}

fn create_private_dir(dir: &Path) -> Result<(), ShellError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700);
        builder.create(dir).map_err(io_error)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    }
    #[cfg(not(unix))]
    builder.create(dir).map_err(io_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_tags() {
        for ok in ["zh-CN", "en", "zh-Hans-CN", "pt-BR"] {
            assert!(is_language_tag(ok), "{ok}");
        }
        for bad in [
            "",
            "Zh-CN",
            "sk-ant-api03-abcdefghijklmnopqrstuvwxyz",
            "https://x",
            "../etc",
            "zh_CN",
            "z",
        ] {
            assert!(!is_language_tag(bad), "{bad}");
        }
    }
}
