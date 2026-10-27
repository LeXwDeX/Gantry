//! 方式 B：经运行时归档导出读取 `out/`（sandbox.md §7.8）。
//!
//! tar 由 Podman 生成，但条目名、类型、大小都由沙盒决定，所以只接受 ustar 的普通
//! 文件条目；符号链接、硬链接、目录、设备、FIFO、PAX / GNU 扩展头、稀疏文件一律
//! 记入 `unexpected`。原始归档的总字节数封顶，读到上限即中止，不无限缓冲；
//! 头部校验和不对、大小字段不是八进制、数据提前结束都视为归档损坏并中止。

use std::io::{self, Read};

use super::{pre_check, Collector, IntakePlan, IntakeResult, Reject, MAX_ENTRIES};

const BLOCK: usize = 512;

/// 归档里的头部与填充开销：每个条目至少一个头块，再加两个结束块。
pub fn archive_cap(plan: &IntakePlan) -> u64 {
    plan.total_max_bytes
        .saturating_add(((MAX_ENTRIES as u64) + 2) * 2 * BLOCK as u64)
}

/// 解析导出的归档。`root` 是归档里 `out/` 目录自身的名字（`podman cp ctr:/gantry/out -`
/// 产出的条目形如 `out/`、`out/result.json`）；为 `None` 时条目直接位于顶层。
pub fn read_out_tar<R: Read>(reader: R, plan: &IntakePlan, root: Option<&str>) -> IntakeResult {
    let mut reader = Capped {
        inner: reader,
        remaining: archive_cap(plan),
        exceeded: false,
    };
    let mut collector = Collector::default();
    let mut entries = 0usize;
    loop {
        let mut header = [0u8; BLOCK];
        match read_block(&mut reader, &mut header) {
            Ok(true) => {}
            Ok(false) => break, // 干净的 EOF（没有结束块也接受）
            Err(_) => {
                abort(&mut collector, &reader);
                break;
            }
        }
        if header.iter().all(|b| *b == 0) {
            break;
        }
        entries += 1;
        if entries > MAX_ENTRIES + 1 {
            collector.note("<archive>", Reject::TooManyEntries);
            break;
        }
        let Some(entry) = parse_header(&header) else {
            collector.note("<archive>", Reject::ArchiveMalformed);
            break;
        };
        let outcome = handle_entry(&mut reader, &entry, plan, root, &mut collector);
        if outcome.is_err() {
            abort(&mut collector, &reader);
            break;
        }
    }
    collector.finish()
}

fn abort<R>(collector: &mut Collector, reader: &Capped<R>) {
    let reason = if reader.exceeded {
        Reject::ArchiveTotalExceeded
    } else {
        Reject::ArchiveMalformed
    };
    collector.note("<archive>", reason);
}

struct Entry {
    name: Vec<u8>,
    typeflag: u8,
    size: u64,
}

fn handle_entry<R: Read>(
    reader: &mut Capped<R>,
    entry: &Entry,
    plan: &IntakePlan,
    root: Option<&str>,
    collector: &mut Collector,
) -> io::Result<()> {
    let padded = entry.size.div_ceil(BLOCK as u64) * BLOCK as u64;
    let kind = match entry.typeflag {
        b'0' | 0 => None,
        b'1' => Some(Reject::Hardlink),
        b'2' => Some(Reject::Symlink),
        b'3' | b'4' => Some(Reject::Device),
        b'5' => Some(Reject::Directory),
        b'6' => Some(Reject::Fifo),
        b'x' | b'g' | b'L' | b'K' => Some(Reject::ArchiveExtensionHeader),
        b'S' => Some(Reject::ArchiveSparse),
        _ => Some(Reject::OtherType),
    };
    let name = match relative_name(&entry.name, root) {
        // `out/` 目录自身的条目是归档的正常组成部分。
        RelName::Root if entry.typeflag == b'5' => return skip(reader, padded),
        RelName::Root | RelName::Outside => {
            collector.reject(&entry.name, Reject::InvalidName);
            return skip(reader, padded);
        }
        RelName::Inside(name) => name,
    };
    if let Some(reason) = kind {
        collector.reject(name, reason);
        return skip(reader, padded);
    }
    let artifact = match pre_check(name, plan) {
        Ok(artifact) => artifact,
        Err(reason) => {
            collector.reject(name, reason);
            return skip(reader, padded);
        }
    };
    if entry.size > plan.limit(artifact) {
        collector.reject(name, Reject::TooLarge);
        return skip(reader, padded);
    }
    let mut bytes = vec![0u8; entry.size as usize];
    reader.read_exact(&mut bytes)?;
    skip(reader, padded - entry.size)?;
    collector.offer(artifact, bytes, plan);
    Ok(())
}

enum RelName<'a> {
    Root,
    Inside(&'a [u8]),
    Outside,
}

fn relative_name<'a>(name: &'a [u8], root: Option<&str>) -> RelName<'a> {
    let name = name.strip_prefix(b"./").unwrap_or(name);
    let Some(root) = root else {
        return if name.is_empty() || name == b"." {
            RelName::Root
        } else {
            RelName::Inside(name)
        };
    };
    let root = root.as_bytes();
    if name == root
        || (name.len() == root.len() + 1 && name.starts_with(root) && name.ends_with(b"/"))
    {
        return RelName::Root;
    }
    match name
        .strip_prefix(root)
        .and_then(|rest| rest.strip_prefix(b"/"))
    {
        // 子目录、`..`、尾随 `/` 都留给 pre_check 判 InvalidName。
        Some(rest) => RelName::Inside(rest),
        None => RelName::Outside,
    }
}

fn parse_header(block: &[u8; BLOCK]) -> Option<Entry> {
    let stored = parse_octal(&block[148..156])?;
    let computed: u64 = block
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                u64::from(b' ')
            } else {
                u64::from(*b)
            }
        })
        .sum();
    if stored != computed {
        return None;
    }
    let magic = &block[257..263];
    let ustar = magic == b"ustar\0";
    if !ustar && magic != b"ustar " {
        return None;
    }
    let size = parse_octal(&block[124..136])?;
    let mut name = field(&block[0..100]).to_vec();
    let prefix = field(&block[345..500]);
    if ustar && !prefix.is_empty() {
        let mut full = prefix.to_vec();
        full.push(b'/');
        full.extend_from_slice(&name);
        name = full;
    }
    Some(Entry {
        name,
        typeflag: block[156],
        size,
    })
}

fn field(bytes: &[u8]) -> &[u8] {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    &bytes[..end]
}

/// 只接受 NUL / 空格结尾的八进制；GNU 的 base-256 大小视为损坏。
fn parse_octal(bytes: &[u8]) -> Option<u64> {
    let text = bytes
        .iter()
        .copied()
        .skip_while(|b| *b == b' ')
        .take_while(|b| *b != 0 && *b != b' ');
    let mut value: u64 = 0;
    let mut digits = 0;
    for b in text {
        if !(b'0'..=b'7').contains(&b) {
            return None;
        }
        value = value.checked_mul(8)?.checked_add(u64::from(b - b'0'))?;
        digits += 1;
    }
    (digits > 0).then_some(value)
}

/// 读满一个块；块开头即 EOF 返回 `Ok(false)`，块中途 EOF 是错误。
fn read_block<R: Read>(reader: &mut R, block: &mut [u8; BLOCK]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < BLOCK {
        let n = reader.read(&mut block[filled..])?;
        if n == 0 {
            return if filled == 0 {
                Ok(false)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        filled += n;
    }
    Ok(true)
}

fn skip<R: Read>(reader: &mut R, len: u64) -> io::Result<()> {
    let copied = io::copy(&mut reader.by_ref().take(len), &mut io::sink())?;
    if copied != len {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

/// 原始归档的总字节上限；超出即报错，读取在此中止。
struct Capped<R> {
    inner: R,
    remaining: u64,
    exceeded: bool,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            // 再探一个字节：真的还有数据才算超限。
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => {
                    self.exceeded = true;
                    Err(io::Error::other("archive exceeds the total cap"))
                }
            };
        }
        let max = buf
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let n = self.inner.read(&mut buf[..max])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}
