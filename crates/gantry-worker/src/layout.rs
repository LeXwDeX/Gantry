//! 沙盒内布局与固定环境变量（worker-protocol §2.1、§2.2）。
//!
//! 路径与 `sandbox` 的挂载点一致；`sandbox` 不依赖本 crate，一致性由本 crate 的测试核对。

/// JobSpec，只读。
pub const JOB_JSON: &str = "/gantry/job.json";
/// 输入材料目录，只读。
pub const INPUTS: &str = "/gantry/inputs";
/// 工作区副本，worker 的 cwd。
pub const WORK: &str = "/gantry/work";
/// 产物目录，启动时为空。
pub const OUT: &str = "/gantry/out";
/// agent CLI 的 `HOME`（tmpfs）。
pub const HOME: &str = "/gantry/home";
/// agent CLI 的 `TMPDIR`（tmpfs）。
pub const TMP: &str = "/gantry/tmp";

/// 预热池下表示输入已就绪的空文件。
pub const READY: &str = "/gantry/inputs/READY";
pub const MANIFEST: &str = "/gantry/inputs/MANIFEST.json";
pub const OUTPUT_SCHEMA: &str = "/gantry/inputs/output.schema.json";

pub const ENV_JOB: &str = "GANTRY_JOB";
pub const ENV_OUT: &str = "GANTRY_OUT";
pub const ENV_WORK: &str = "GANTRY_WORK";

/// `NO_PROXY` 的固定值（worker-protocol §2.2）。
pub const NO_PROXY: &str = "localhost,127.0.0.1";
