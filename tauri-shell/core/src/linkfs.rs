//! 链接原语与能力探测。
//!
//! 共享资产（skill 目录、规则文件）在 Windows 上的「一份内容、多处入口」靠三种
//! 目录项实现：符号链接、目录联接（junction）、硬链接。三者在这台机器上的实测
//! 行为决定了下面的每一条实现选择，结论来自 `link-probe`（临时 bin，已按计划
//! 删除），运行环境：Windows 11 x64、普通用户会话、`symlink_dir` 未被策略禁用。
//!
//! ## 实测结论（2026-11-02，本机复测）
//!
//! | 问题 | 结果 |
//! |---|---|
//! | `std::os::windows::fs::symlink_dir` 普通会话 | **成功**；`symlink_metadata().file_type().is_symlink()` = true，`read_link` 读回绝对路径，可透过链接读文件 |
//! | FSCTL 建的 junction | **成功**；`symlink_metadata().is_symlink()` = **true**，`metadata().is_dir()` = true，`read_link` **也能读回**，`attributes` = 0x410（DIRECTORY \| REPARSE_POINT） |
//! | junction 上的 `remove_dir` | **只删链接**，库内容完好 |
//! | 跟随链接的最终路径 | `GetFinalPathNameByHandleW`（不带 `FILE_FLAG_OPEN_REPARSE_POINT`）对 symlink 与 junction 都返回库内路径，且与 `canonicalize(库内)` 逐字相同（同为 `\\?\C:\…` 形态） |
//! | `hard_link` | 同卷成功；跨卷 `ERROR_NOT_SAME_DEVICE (17)`，`kind = CrossesDevices` |
//! | 在**已存在**的目录上建链接 | symlink 与 junction 都返回 `ERROR_ALREADY_EXISTS (183)`；因此「替换」必须先删后建 |
//! | 相对目标 | `symlink_dir(r".\library")` 原样存下 `.\library`；**所以一律只接受绝对路径** |
//! | 无写权限的目录（`C:\Windows\System32`） | 两种方式都失败，`CreateDirectoryW` 报 `ERROR_ACCESS_DENIED (5)` |
//!
//! 由此得到三条实现约束：
//!
//! 1. **`read_link` 可以读 junction，但不能只用它**：不同 Windows / Rust 版本上
//!    junction 的 `is_symlink()` 与 `read_link()` 行为都变过（文档里流传的「false / 报错」
//!    在本机复现不出来）。`entry_link_kind` 因此**先读重解析点、再看 symlink 标志**，
//!    两条路都不依赖单一 API 的版本行为。
//! 2. **junction 必须按 0x410 识别**：在目录枚举层它是一条「带重解析点的目录」，
//!    `file_type().is_dir()` 是 false，`exists()`/`metadata()` 又会跟随进库内。所以
//!    任何「这是不是链接」的判断都走本模块，不要直接看 `DirEntry::file_type()`。
//! 3. **降级链是 symlink → junction（目录）/ symlink → hardlink（文件）**：
//!    symlink 是本机可用的最简单原语；junction 在 symlink 被策略禁用时顶上，
//!    但它只解决目录。`copy` 不在这里产生 —— 它是 `store::Op::copy_dir` 的语义，
//!    由调用方显式选择（见 `docs/DESIGN.md` §8.3 链接能力与降级链）。
//!
//! ## 为什么不用 std 建 junction
//!
//! `std` 没有 junction API，只有 symlink。junction 必须自己发
//! `FSCTL_SET_REPARSE_POINT`，否则「symlink 被拒时的回退」这一层根本不存在。
//! 重解析点数据的字段偏移是**相对于 `PathBuffer` 起点的字节数**，长度字段**不含**
//! 结尾 NUL —— 这两点是这块代码唯一容易写错的地方，单测里用往返断言把它钉住。

use std::io;
use std::path::{Path, PathBuf};

use crate::store;

/// `IO_REPARSE_TAG_MOUNT_POINT`：junction 的标签。常数住在 `Win32_System_Ioctl`
/// 模块里，不在 `Win32_Storage_FileSystem`。
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// 符号链接重解析点的标签（低 4 位是相对/绝对标志，比较前先掩掉）。
const SYMLINK_TAG: u32 = 0xA000_000C;
const SYMLINK_TAG_MASK: u32 = 0xFFFF_FFF0;
/// symlink 标签里的「相对目标」标志位。我们从不产生相对链接（`require_absolute`），
/// 但它决定了标签的低 4 位，`entry_link_kind` 比较时必须先掩掉。
pub const SYMLINK_RELATIVE_FLAG: u32 = 0x1;
/// 重解析点数据的固定头：Tag(4) + DataLength(2) + Reserved(2) + 四个 u16 字段(8)。
const REPARSE_HEADER: usize = 16;
const GENERIC_WRITE: u32 = 0x4000_0000;

/// 一条链接实际是用什么建的。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkMethod {
    /// 目录符号链接（`std::os::windows::fs::symlink_dir` / `symlink_file`）
    Symlink,
    /// 目录联接：`FSCTL_SET_REPARSE_POINT` + `IO_REPARSE_TAG_MOUNT_POINT`
    Junction,
    /// `std::fs::hard_link`，仅文件
    Hardlink,
    /// 不是链接，是受管副本（"降级到复制"）
    Copy,
}

impl LinkMethod {
    /// 写进 `links.json` 与审计日志的字符串，**改它就是改数据格式**。
    pub fn as_str(self) -> &'static str {
        match self {
            LinkMethod::Symlink => "symlink",
            LinkMethod::Junction => "junction",
            LinkMethod::Hardlink => "hardlink",
            LinkMethod::Copy => "copy",
        }
    }

    /// 严格匹配，未知返回 `None`。
    pub fn parse(s: &str) -> Option<LinkMethod> {
        match s {
            "symlink" => Some(LinkMethod::Symlink),
            "junction" => Some(LinkMethod::Junction),
            "hardlink" => Some(LinkMethod::Hardlink),
            "copy" => Some(LinkMethod::Copy),
            _ => None,
        }
    }

    /// 是不是真的链接（copy 是一个真实副本，不占目录项里的重解析点）。
    pub fn is_link(self) -> bool {
        !matches!(self, LinkMethod::Copy)
    }
}

/// 本机支持哪些方式，以及两种用途各自的优先顺序。
pub struct LinkCap {
    pub symlink_dir: bool,
    pub symlink_file: bool,
    pub junction: bool,
    pub hardlink: bool,
    /// 顺序已排好：目录用 `dir_order`，单文件用 `file_order`。末尾可能带 `Copy`
    /// （只在能力探测被明确要求「允许降级到副本」时）。
    pub dir_order: Vec<LinkMethod>,
    pub file_order: Vec<LinkMethod>,
    /// 探测用的临时目录，用于在 UI 上解释「为什么是 copy 模式」。
    pub probed_at: String,
}

impl LinkCap {
    /// 目录的首选方式；一个都不能用时返回 `None`（调用方据此给出可读的拒绝）。
    pub fn preferred_dir(&self) -> Option<LinkMethod> {
        self.dir_order.iter().copied().find(|m| m.is_link()).or_else(|| {
            self.dir_order
                .iter()
                .copied()
                .find(|m| *m == LinkMethod::Copy)
        })
    }

    /// 单文件的首选方式；口径同 [`LinkCap::preferred_dir`]。
    pub fn preferred_file(&self) -> Option<LinkMethod> {
        self.file_order.iter().copied().find(|m| m.is_link()).or_else(|| {
            self.file_order
                .iter()
                .copied()
                .find(|m| *m == LinkMethod::Copy)
        })
    }

    /// 全都要靠副本时（没有任何链接能力），UI 顶部要常驻横幅。
    pub fn copy_only(&self) -> bool {
        !self.dir_order.iter().any(|m| m.is_link())
    }
}

/// 探测本机能建哪些链接。
///
/// `probe_root` 是探测用的临时目录（由调用方保证可写、事后清理）；`cache` 是一份
/// 可选的结果缓存文件，内容匹配时就跳过探测。探测失败**不**返回 `Err` ——
/// 与 `settings::ensure_store` 的语气一致：能力不足是一种可解释的状态
/// （`dir_order` 里连 `Copy` 都没有），不是一个可以让启动失败的错误。
pub fn capability(probe_root: &Path, cache: Option<&Path>) -> LinkCap {
    if let Some(c) = cache {
        if let Some(cap) = read_cap_cache(c, probe_root) {
            return cap;
        }
    }
    let cap = probe(probe_root);
    if let Some(c) = cache {
        write_cap_cache(c, &cap);
    }
    cap
}

/// 真正的探测。公开给测试与 `ak-scan --shared` 使用，以便绕过缓存。
pub fn probe(probe_root: &Path) -> LinkCap {
    let probed_at = store::native_str(probe_root);
    let mut cap = LinkCap {
        symlink_dir: false,
        symlink_file: false,
        junction: false,
        hardlink: false,
        dir_order: Vec::new(),
        file_order: Vec::new(),
        probed_at: probed_at.clone(),
    };
    let Some(work) = make_work_dir(probe_root) else {
        // 连工作目录都建不出来：不开任何能力。调用方据此报 copy 模式。
        return cap;
    };
    let src_dir = work.join("src-dir");
    let src_file = work.join("src-file.txt");
    let _ = std::fs::create_dir_all(&src_dir);
    let _ = std::fs::write(&src_file, b"probe");

    // 目录：symlink → junction
    let s1 = work.join("dir-symlink");
    cap.symlink_dir = try_dir_symlink(&src_dir, &s1).is_ok();
    let _ = unlink(&s1);

    let s2 = work.join("dir-junction");
    cap.junction = create_junction(&src_dir, &s2).is_ok();
    let _ = unlink(&s2);

    // 文件：symlink → hardlink（两者都要 src 与 dst 在同一卷）
    let f1 = work.join("file-symlink.txt");
    cap.symlink_file = symlink_file_raw(&src_file, &f1).is_ok();
    let _ = unlink(&f1);

    let f2 = work.join("file-hardlink.txt");
    cap.hardlink = std::fs::hard_link(&src_file, &f2).is_ok();
    let _ = unlink(&f2);

    cap.dir_order = dir_chain(&cap);
    cap.file_order = file_chain(&cap);
    store::rmrf(&work);
    cap
}

fn dir_chain(cap: &LinkCap) -> Vec<LinkMethod> {
    let mut out = Vec::new();
    if cap.symlink_dir {
        out.push(LinkMethod::Symlink);
    }
    if cap.junction {
        out.push(LinkMethod::Junction);
    }
    out
}

fn file_chain(cap: &LinkCap) -> Vec<LinkMethod> {
    let mut out = Vec::new();
    if cap.symlink_file {
        out.push(LinkMethod::Symlink);
    }
    if cap.hardlink {
        out.push(LinkMethod::Hardlink);
    }
    out
}

/// 每次探测一个独立子目录，避免两个进程同时探测时互相踩。
fn make_work_dir(probe_root: &Path) -> Option<PathBuf> {
    std::fs::create_dir_all(probe_root).ok()?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = probe_root.join(format!("probe-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/* ------------------------------------------------------------------ */
/* 创建                                                                */
/* ------------------------------------------------------------------ */

/// 建一个指向 `src` 的目录链接。`prefer` 指定时只试这一种，否则按
/// [`LinkCap`] 的顺序逐个试。返回实际用掉的方式。
pub fn create_dir_link(
    src: &Path,
    dst: &Path,
    prefer: Option<LinkMethod>,
) -> Result<LinkMethod, String> {
    require_absolute(src, dst)?;
    if !src.is_dir() {
        return Err(format!("link source is not a directory: {}", src.display()));
    }
    if dst.exists() || is_link(dst) {
        return Err(format!("link target already exists: {}", dst.display()));
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("the parent directory could not be created ({}): {}", parent.display(), e))?;
    }
    let order: Vec<LinkMethod> = match prefer {
        Some(LinkMethod::Symlink) => vec![LinkMethod::Symlink],
        Some(LinkMethod::Junction) => vec![LinkMethod::Junction],
        Some(other) => {
            return Err(format!(
                "{} cannot be used for a directory link",
                other.as_str()
            ))
        }
        None => vec![LinkMethod::Symlink, LinkMethod::Junction],
    };
    let mut errors: Vec<String> = Vec::new();
    for m in order {
        let r = match m {
            LinkMethod::Symlink => try_dir_symlink(src, dst),
            LinkMethod::Junction => create_junction(src, dst),
            // order 里只有上两种；走到这里就是本文件的 bug，而不是用户的输入。
            _ => unreachable!(),
        };
        match r {
            Ok(()) => {
                if read_target(dst).map(|t| same_target(&t, src)).unwrap_or(false) {
                    return Ok(m);
                }
                // 建出来了但读不回同一个目标：宁可失败，也不要留下一条读不懂的链接。
                let _ = unlink(dst);
                errors.push(format!("{} was created but reads back to another target", m.as_str()));
            }
            Err(e) => errors.push(format!("{} failed: {}", m.as_str(), e)),
        }
    }
    Err(describe_failure(dst, &errors))
}

/// 建一个指向 `src` 的文件链接（symlink 优先，回退 hardlink）。
///
/// `src` 为空表示「不知道原来指向哪」—— 这正是**硬链接**的常态：它没有目标可读。
/// 此时只尝试 hardlink，且不校验目标（`std::fs::hard_link` 会自己拒绝空路径）。
/// 还原路径依赖这条语义：快照里一条 `target` 为空的记录，唯一合理的重建方式
/// 就是按原种类尽力重造，而不是报「数据坏了」。
pub fn create_file_link(
    src: &Path,
    dst: &Path,
    prefer: Option<LinkMethod>,
) -> Result<LinkMethod, String> {
    let targetless = src.as_os_str().is_empty();
    if targetless {
        if !dst.is_absolute() {
            return Err(format!(
                "links need absolute paths, got dst {}",
                dst.display()
            ));
        }
    } else {
        require_absolute(src, dst)?;
        if !src.is_file() {
            return Err(format!("link source is not a file: {}", src.display()));
        }
    }
    if dst.exists() || is_link(dst) {
        return Err(format!("link target already exists: {}", dst.display()));
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("the parent directory could not be created ({}): {}", parent.display(), e))?;
    }
    let order: Vec<LinkMethod> = match (prefer, targetless) {
        (_, true) => vec![LinkMethod::Hardlink],
        (Some(LinkMethod::Symlink), _) => vec![LinkMethod::Symlink],
        (Some(LinkMethod::Hardlink), _) => vec![LinkMethod::Hardlink],
        (Some(other), _) => {
            return Err(format!(
                "{} cannot be used for a file link",
                other.as_str()
            ))
        }
        (None, _) => vec![LinkMethod::Symlink, LinkMethod::Hardlink],
    };
    let mut errors: Vec<String> = Vec::new();
    for m in order {
        let r = match m {
            LinkMethod::Symlink => symlink_file_raw(src, dst),
            // hardlink 没有「目标」可读：同一 inode 就是同一份内容，读回来的
            // 路径是它自己。所以这里只验证内容一致，不去 `read_target`。
            LinkMethod::Hardlink => match std::fs::hard_link(src, dst) {
                Ok(()) => Ok(()),
                Err(e) => Err(e),
            },
            // order 里只有上两种；走到这里就是本文件的 bug，而不是用户的输入。
            _ => unreachable!(),
        };
        match r {
            Ok(()) => {
                if m == LinkMethod::Hardlink {
                    return Ok(m);
                }
                if read_target(dst).map(|t| same_target(&t, src)).unwrap_or(false) {
                    return Ok(m);
                }
                let _ = unlink(dst);
                errors.push(format!("{} was created but reads back to another target", m.as_str()));
            }
            Err(e) => errors.push(format!("{} failed: {}", m.as_str(), e)),
        }
    }
    Err(describe_failure(dst, &errors))
}

/// 链接创建失败时给用户看的一句话：两种方式都试过的话，要把两种原因都说出来 ——
/// 只说「失败」会让「开发者模式没开」和「目标已存在」看起来是同一个问题。
fn describe_failure(dst: &Path, errors: &[String]) -> String {
    format!(
        "no link could be created at {} ({})",
        dst.display(),
        errors.join("; ")
    )
}

fn require_absolute(src: &Path, dst: &Path) -> Result<(), String> {
    if !src.is_absolute() || !dst.is_absolute() {
        // 相对目标会被原样存进重解析点，之后从别的进程看就解析到别处去了。
        return Err(format!(
            "links need absolute paths, got src {} and dst {}",
            src.display(),
            dst.display()
        ));
    }
    Ok(())
}fn try_dir_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(src, dst)
}

fn symlink_file_raw(src: &Path, dst: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(src, dst)
}

/* ------------------------------------------------------------------ */
/* 读取                                                                */
/* ------------------------------------------------------------------ */

/// 读回链接目标。**junction 与 symlink 都要能读**。
///
/// 先按硬链接判空（硬链接没有目标），再读重解析点，最后退回 `read_link`。
pub fn read_target(p: &Path) -> Option<PathBuf> {
    if let Some(t) = read_reparse_target(p) {
        return Some(t);
    }
    std::fs::read_link(p).ok()
}

/// 这条路径是不是链接（symlink、junction 或硬链接）。
pub fn is_link(p: &Path) -> bool {
    link_kind(p).is_some()
}

/// 判定链接类型，用于快照/还原时保持原类型。
///
/// 硬链接排在最后：它对 `symlink_metadata` 没有任何可观察的差别，
/// 只能靠 `nlink > 1`；一个普通的、被别处硬链接过的文件同样会命中，
/// 这是已知的过度判定（见 `docs/DESIGN.md` §8.3 链接能力与降级链）。
pub fn link_kind(p: &Path) -> Option<LinkMethod> {
    if let Some(tag) = reparse_tag(p) {
        return Some(if tag == IO_REPARSE_TAG_MOUNT_POINT {
            LinkMethod::Junction
        } else {
            LinkMethod::Symlink
        });
    }
    // 读不到重解析点时（权限、卷不支持），退回到类型与文件系统标志。
    // 注意 `file_type().is_dir()` 对 junction 是 **false**，所以先问 symlink；
    // 再问「带重解析点的目录」—— 那是 junction 在枚举层的真实样子。
    let meta = std::fs::symlink_metadata(p).ok()?;
    if meta.file_type().is_symlink() {
        return Some(LinkMethod::Symlink);
    }
    if has_reparse_attribute(p) {
        // 有重解析点却读不出标签：junction 与 symlink 都可能，按标签缺失的
        // 常见形态记为 Junction（它只可能是目录级联接）。
        return Some(LinkMethod::Junction);
    }
    if meta.is_file() && hardlink_count(p) > 1 {
        return Some(LinkMethod::Hardlink);
    }
    None
}

/// 目录项是否带 `FILE_ATTRIBUTE_REPARSE_POINT`（0x400）。
///
/// 用 `GetFileAttributesW` 而不是 `metadata()`：后者会**跟随**链接，问到的
/// 是库内那个对象的属性 —— 这正是「链接被当成真实目录」的来源。
fn has_reparse_attribute(p: &Path) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let w = wide(p);
    let attrs = unsafe { GetFileAttributesW(w.as_ptr()) };
    attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// 目录里某一条目的链接类型。
///
/// 这是**目录枚举必须用的那一个**：`DirEntry::file_type()` 对 junction 返回的不是
/// `is_symlink()`，而是「一个带重解析点的目录」，`is_dir()` 同样是 false。
/// 用它取 `file_type()` 之后，调用方还得自己 `metadata()` 才知道是不是目录，
/// 那一步又会跟随进库内、把链接当成普通目录 —— 于是链接会被当成真实内容快照。
pub fn entry_link_kind(p: &Path) -> Option<LinkMethod> {
    link_kind(p)
}

/// 解除链接本身，不跟随（junction 与 symlink 都用 `RemoveDirectoryW`，硬链接用 `remove_file`）。
///
/// 两条 Windows 陷阱都在这一行上：
/// 1. `std::fs::remove_dir` 在 junction 上会**跟随进库内**删掉内容（实测库副本被清空）；
/// 2. `symlink_metadata().is_dir()` 对目录符号链接是 **false**，于是「不是目录就
///    `remove_file`」会报 `Access denied`。所以这里不问类型，一律先试
///    `RemoveDirectoryW`，失败再试 `remove_file`。
pub fn unlink(p: &Path) -> io::Result<()> {
    match link_kind(p) {
        Some(LinkMethod::Hardlink) => {
            // 硬链接的「解除」只能是删掉这个目录项；另一个名字仍然持有内容。
            if !std::fs::symlink_metadata(p)?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("a hard link must be a file: {}", p.display()),
                ));
            }
            std::fs::remove_file(p)
        }
        Some(_) => {
            let dir_err = match remove_directory_entry(p) {
                Ok(()) => return Ok(()),
                Err(e) => e,
            };
            match std::fs::remove_file(p) {
                Ok(()) => Ok(()),
                // 顺序很重要：**目录中的符号链接**上 `RemoveDirectoryW` 成功、
                // 而 `remove_file` 要求 FILE_ATTRIBUTE_DIRECTORY；**文件符号链接**
                // 上正好相反。两种都试，以成功的那个为准，但绝不能只试一种。
                Err(_) => Err(dir_err),
            }
        }
        // 没有任何链接特征：**不要猜**。`remove_dir` 会顺着重解析点进库内把内容
        // 删掉，这是整个模块里唯一能造成数据丢失的一行，所以这里只拒绝。
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a link: {}", p.display()),
        )),
    }
}

/// 把 `p` 这个目录项换成指向 `target` 的硬链接。`p` 必须已存在且可读。
///
/// 用于**还原**：硬链接没有目标可读，重构它必须借助一个 donor（同 inode 的另一个
/// 名字）。`hard_link` 不接受已存在的目标，所以先把这个目录项删掉 —— 删的是目录项，
/// 内容还在 donor 那一侧。
pub fn replace_with_hardlink(p: &Path, target: &Path) -> io::Result<()> {
    if !p.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a hard link must be a file: {}", p.display()),
        ));
    }
    let bytes = std::fs::read(p)?;
    if same_target(p, target) {
        // 对端就是它自己：链上去只会得到一条读起来是 FilesystemLoop 的死链接。
        return Ok(());
    }
    std::fs::remove_file(p)?;
    match std::fs::hard_link(target, p) {
        Ok(()) => Ok(()),
        Err(e) => {
            // 重建失败时不能让这个条目**消失**：它可能承载着唯一的副本。
            let _ = std::fs::write(p, bytes);
            Err(e)
        }
    }
}

/// 在 `p` 处用 `content` 重建一个硬链接，donor 取**同目录**里内容相同的那个条目。
///
/// 用于还原：快照里的硬链接没有目标可读，只能靠「同一目录里还有谁持有同一份字节」
/// 来推断它原本与谁共享 inode。找不到（内容唯一的那个名字）时退回普通文件 ——
/// 内容正确但不再是硬链接，是这里能给的最好结果。
pub fn restore_hardlink(p: &Path, content: &[u8]) -> io::Result<()> {
    // 排序的依据：优先选**普通文件**做 donor。目录里的硬链接对端（`SKILL.md`）
    // 在还原阶段可能已经被写成指向别处的 symlink，链到它会让这条链接也跟着指向
    // 别处；而按内容写回普通文件时，同目录的其他硬链接名字**恰好**也都是同一份
    // 内容，优先用它们能让「同 innode」这个结构重新出现。
    if let Some(donor) = hardlink_sibling(p, content) {
        // 先删掉刚写回的普通文件再链：`hard_link` 不接受已存在的目标。
        if std::fs::remove_file(p).is_ok() && std::fs::hard_link(&donor, p).is_ok() {
            return Ok(());
        }
        // 链不上就把内容写回去 —— 条目不能消失。
    }
    remove_if_exists(p);
    std::fs::write(p, content)
}

fn remove_if_exists(p: &Path) {
    if p.exists() {
        let _ = std::fs::remove_file(p);
    }
}

/// 同目录里内容与 `content` 完全相同、且**不是** `p` 自己的文件。
///
/// 只在目录内查找：硬链接不能跨卷，而同一个资产目录里的副本才可能是它的原始对端。
/// 比内容而不是比 inode，是因为还原时对端可能已经被写成了一个**新**文件（快照里
/// 它排在前面，先被写了回去），此时 inode 已经不同。
fn hardlink_sibling(p: &Path, content: &[u8]) -> Option<PathBuf> {
    let parent = p.parent()?;
    let self_name = native_lower(p);
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(parent).ok()?.flatten() {
        let q = entry.path();
        if native_lower(&q) == self_name {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&q) else {
            continue;
        };
        // 只要**真正的普通文件**：symlink 的 `is_file()` 会因为跟随而变真，
        // 用它当 donor 会把「指向别处」这个属性传染给这条链接。
        if !meta.file_type().is_file() || meta.len() != content.len() as u64 {
            continue;
        }
        if std::fs::canonicalize(&q).ok() == std::fs::canonicalize(p).ok() {
            // 它自己（比如 `\\?\` 前缀、大小写不同）：不是对端。
            continue;
        }
        if std::fs::read(&q).map(|b| b == content).unwrap_or(false) {
            candidates.push(q);
        }
    }
    // 稳定挑选：优先硬链接（说明它本来就是这个 inode 的另一个名字），其次按名字。
    candidates.sort_by_key(|q| {
        (
            crate::linkfs::link_kind(q) != Some(LinkMethod::Hardlink),
            native_lower(q),
        )
    });
    candidates.into_iter().next()
}

fn native_lower(p: &Path) -> String {
    store::native_str(p).to_lowercase()
}

/// 用 `RemoveDirectoryW` 删掉目录项本身（不跟随重解析点）。
fn remove_directory_entry(p: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::RemoveDirectoryW;
    let w = wide(p);
    let ok = unsafe { RemoveDirectoryW(w.as_ptr()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 同一物理对象？（realpath 归一后比较）
///
/// Windows 路径大小写不敏感，而 `canonicalize` 不统一盘符大小写，也不归一
/// UAC 目录别名；所以只做「canonicalize + 小写化」，不做更聪明的猜测。
/// 悬空链接的 `canonicalize` 会失败，此时退回「父目录 + 文件名」。
pub fn same_target(a: &Path, b: &Path) -> bool {
    normalize_for_compare(a) == normalize_for_compare(b)
}

fn normalize_for_compare(p: &Path) -> String {
    let resolved = match std::fs::canonicalize(p) {
        Ok(c) => c,
        Err(_) => match (p.parent(), p.file_name()) {
            (Some(parent), Some(name)) => match std::fs::canonicalize(parent) {
                Ok(par) => par.join(name),
                Err(_) => p.to_path_buf(),
            },
            _ => p.to_path_buf(),
        },
    };
    let mut s = store::native_str(&resolved);
    // `\\?\` 前缀只影响表示，不影响身份；统一去掉，免得「带前缀」与「不带前缀」
    // 的两个字符串被判成不同对象。
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    s.to_lowercase()
}

/// 一条链接自身的链接数（>1 即还有别的目录项指向同一份内容）。
///
/// `MetadataExt::number_of_links` 在 1.99 还是 `windows_by_handle` 不稳定特性，
/// 所以这里直接问 Win32：`CreateFileW`（带 `FILE_FLAG_OPEN_REPARSE_POINT`，
/// 不跟随）+ `GetFileInformationByHandle`。
fn hardlink_count(p: &Path) -> u32 {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let Ok(h) = open_reparse(p, 0) else {
        return 0;
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(h, &mut info) };
    unsafe { CloseHandle(h) };
    if ok == 0 {
        0
    } else {
        info.nNumberOfLinks
    }
}

/* ------------------------------------------------------------------ */
/* 重解析点（junction）                                                */
/* ------------------------------------------------------------------ */

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_REPARSE_POINT, FSCTL_SET_REPARSE_POINT};

fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn open_reparse(p: &Path, access: u32) -> io::Result<isize> {
    let w = wide(p);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            0,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(h)
}

/// 读重解析点标签。不是重解析点（或读不出来）时返回 `None`。
fn reparse_tag(p: &Path) -> Option<u32> {
    let buf = read_reparse_buffer(p)?;
    if buf.len() < 4 {
        return None;
    }
    Some(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

/// 读 junction / symlink 的目标。只有 MOUNT_POINT 与 SYMLINK 两种标签解析目标；
/// 其它标签（云盘占位文件之类）返回 `None`，交给调用方的其它判断。
fn read_reparse_target(p: &Path) -> Option<PathBuf> {
    let buf = read_reparse_buffer(p)?;
    let tag = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    match tag {
        IO_REPARSE_TAG_MOUNT_POINT => mount_point_target(&buf),
        t if t & SYMLINK_TAG_MASK == SYMLINK_TAG & SYMLINK_TAG_MASK => symlink_target(&buf),
        _ => None,
    }
}

fn read_reparse_buffer(p: &Path) -> Option<Vec<u8>> {
    let h = open_reparse(p, 0).ok()?;
    // 目标路径是 UTF-16，加上两段名字与头部；16 KiB 足够覆盖 MAX_PATH 与长路径。
    let mut buf = vec![0u8; 16 * 1024];
    let mut ret: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            h,
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null(),
            0,
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            buf.len() as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(h) };
    if ok == 0 || (ret as usize) < 8 {
        return None;
    }
    buf.truncate(ret as usize);
    Some(buf)
}

/// MOUNT_POINT 的 `PathBuffer` 布局（全为相对 `PathBuffer` 起点的字节数）：
/// 四个 u16 字段依次为：`SubstituteNameOffset(2) SubstituteNameLength(2) PrintNameOffset(2) PrintNameLength(2)`。
fn mount_point_target(buf: &[u8]) -> Option<PathBuf> {
    if buf.len() < REPARSE_HEADER {
        return None;
    }
    let sub_off = u16::from_le_bytes([buf[8], buf[9]]) as usize;
    let sub_len = u16::from_le_bytes([buf[10], buf[11]]) as usize;
    let print_off = u16::from_le_bytes([buf[12], buf[13]]) as usize;
    let print_len = u16::from_le_bytes([buf[14], buf[15]]) as usize;
    let print = utf16_at(buf, REPARSE_HEADER + print_off, print_len);
    // 优先用 PrintName（用户可读的路径）；空的话退回 SubstituteName 去掉 \??\ 前缀。
    if let Some(p) = print.filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let units = utf16_at(buf, REPARSE_HEADER + sub_off, sub_len)?;
    Some(PathBuf::from(strip_nt_prefix(&units)))
}

/// 去掉 NT / 长路径前缀。`\\?\` 只影响表示，不影响身份。
fn strip_nt_prefix(s: &str) -> String {
    let mut out = s;
    if let Some(rest) = out.strip_prefix(r"\??\") {
        out = rest;
    }
    if let Some(rest) = out.strip_prefix(r"\\?\") {
        out = rest;
    }
    out.to_string()
}

/// SYMLINK 的布局相同，但 SubstituteName 已是普通路径。
fn symlink_target(buf: &[u8]) -> Option<PathBuf> {
    if buf.len() < REPARSE_HEADER + 4 {
        return None;
    }
    // **Windows 10 起 SYMBOLIC_LINK_REPARSE_BUFFER 多一个 4 字节 Flags 字段**，
    // 它吃掉 PathBuffer 的前两个 UTF-16 单元（相对/绝对标志就在那儿）。漏掉它，
    // 读回来的目标前面会多两个字符（实测 `md\??\C:\…`）：偏移对、长度对，
    // 只有起点错了 4 字节。junction 没有这个字段，所以两边的基数不同。
    let path_buffer = REPARSE_HEADER + 4;
    let sub_off = u16::from_le_bytes([buf[8], buf[9]]) as usize;
    let sub_len = u16::from_le_bytes([buf[10], buf[11]]) as usize;
    let mut s = utf16_at(buf, path_buffer + sub_off, sub_len)?;
    if let Some(rest) = s.strip_prefix(r"\??\") {
        s = rest.to_string();
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    if s.is_empty() {
        return None;
    }
    // 相对符号链接需要相对链接所在目录解析，这里只报告存下的字符串 ——
    // 我们的写入路径从不产生相对链接（`require_absolute`），所以这条分支只服务
    // 用户手建的链接：读得出原文，就足以让 doctor 解释它。
    Some(PathBuf::from(s))
}

fn utf16_at(buf: &[u8], start: usize, len: usize) -> Option<String> {
    if start + len > buf.len() || len % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = buf[start..start + len]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// 建一个 junction。`target` 必须是绝对路径。
///
/// 步骤：建空目录 → 打开（带 `FILE_FLAG_OPEN_REPARSE_POINT`）→ 组 buffer →
/// `FSCTL_SET_REPARSE_POINT`。任何一步失败都把自己建的空目录清掉，
/// 不在用户目录里留垃圾。
fn create_junction(target: &Path, link: &Path) -> io::Result<()> {
    if !target.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a junction needs an absolute target",
        ));
    }
    let l = wide(link);
    let made = unsafe { CreateDirectoryW(l.as_ptr(), std::ptr::null()) };
    if made == 0 {
        return Err(io::Error::last_os_error());
    }
    let outcome = write_junction(link, target);
    if outcome.is_err() {
        let _ = std::fs::remove_dir(link);
    }
    outcome
}

fn write_junction(link: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let h = open_reparse(link, GENERIC_WRITE)?;

    // SubstituteName = \??\C:\target（NT 命名空间），PrintName = C:\target（用户可读）。
    let sub: Vec<u16> = r"\??\"
        .encode_utf16()
        .chain(target.as_os_str().encode_wide())
        .collect();
    let print: Vec<u16> = target.as_os_str().encode_wide().collect();
    let sub_bytes = sub.len() * 2;
    let print_bytes = print.len() * 2;

    let mut buf = vec![0u8; REPARSE_HEADER + sub_bytes + 2 + print_bytes + 2];
    buf[0..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    // DataLength 覆盖头部之后的全部字段：四个 u16 + 两段名字（各带结尾 NUL）。
    let data_len = (8 + sub_bytes + 2 + print_bytes + 2) as u16;
    buf[4..6].copy_from_slice(&data_len.to_le_bytes());
    buf[8..10].copy_from_slice(&0u16.to_le_bytes());
    buf[10..12].copy_from_slice(&(sub_bytes as u16).to_le_bytes());
    buf[12..14].copy_from_slice(&((sub_bytes + 2) as u16).to_le_bytes());
    buf[14..16].copy_from_slice(&(print_bytes as u16).to_le_bytes());
    let mut o = REPARSE_HEADER;
    for u in sub.iter().copied().chain(std::iter::once(0)) {
        buf[o..o + 2].copy_from_slice(&u.to_le_bytes());
        o += 2;
    }
    for u in print.iter().copied().chain(std::iter::once(0)) {
        buf[o..o + 2].copy_from_slice(&u.to_le_bytes());
        o += 2;
    }

    let mut ret: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            h,
            FSCTL_SET_REPARSE_POINT,
            buf.as_ptr() as *const std::ffi::c_void,
            buf.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    let err = if ok == 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    unsafe { CloseHandle(h) };
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/* ------------------------------------------------------------------ */
/* 能力缓存                                                            */
/* ------------------------------------------------------------------ */

/// 缓存文件格式：一行一个 `key=value`。坏了就当没有 —— 探测结果本来就廉价，
/// 而一个解析不了的缓存绝不该阻止应用启动。
fn read_cap_cache(cache: &Path, probe_root: &Path) -> Option<LinkCap> {
    let raw = std::fs::read_to_string(cache).ok()?;
    let mut cap = LinkCap {
        symlink_dir: false,
        symlink_file: false,
        junction: false,
        hardlink: false,
        dir_order: Vec::new(),
        file_order: Vec::new(),
        probed_at: String::new(),
    };
    for line in raw.lines() {
        let (k, v) = line.split_once('=')?;
        match k {
            "symlink_dir" => cap.symlink_dir = v == "1",
            "symlink_file" => cap.symlink_file = v == "1",
            "junction" => cap.junction = v == "1",
            "hardlink" => cap.hardlink = v == "1",
            "probed_at" => cap.probed_at = v.to_string(),
            _ => {}
        }
    }
    if cap.probed_at.is_empty() {
        return None;
    }
    // 缓存记录的是「在哪个盘上探测的」：换了卷（U 盘、网络盘）就要重测，
    // 因为 hardlink 的结果随卷而变。
    let same_volume = same_volume(&cap.probed_at, probe_root);
    cap.probed_at = store::native_str(probe_root);
    if !same_volume {
        return None;
    }
    cap.dir_order = dir_chain(&cap);
    cap.file_order = file_chain(&cap);
    Some(cap)
}

fn write_cap_cache(cache: &Path, cap: &LinkCap) {
    let body = format!(
        "symlink_dir={}\nsymlink_file={}\njunction={}\nhardlink={}\nprobed_at={}\n",
        u8::from(cap.symlink_dir),
        u8::from(cap.symlink_file),
        u8::from(cap.junction),
        u8::from(cap.hardlink),
        cap.probed_at
    );
    if let Some(dir) = cache.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = crate::settings::atomic_write(cache, &body);
}

fn same_volume(a: &str, b: &Path) -> bool {
    let pa = Path::new(a);
    let vol = |p: &Path| p.components().next().map(|c| c.as_os_str().to_string_lossy().to_lowercase());
    vol(pa).is_some() && vol(pa) == vol(b)
}

impl LinkCap {
    /// 把能力探测结果序列化成界面用的 `cap`。
    ///
    /// 键名是 camelCase（渲染层直接读）；`copyOnly` 由 `dir_order` 现算，不单独
    /// 存字段 —— 存两份会在缓存命中和现探测之间出现互相矛盾的值。
    ///
    /// 这个 impl 只读字段、不做探测：调用方仍必须先走 [`capability`]。
    pub fn to_json(&self) -> serde_json::Value {
        let order = |methods: &[LinkMethod]| {
            serde_json::Value::Array(
                methods
                    .iter()
                    .map(|m| serde_json::Value::String(m.as_str().to_string()))
                    .collect(),
            )
        };
        let mut map = serde_json::Map::new();
        map.insert(
            "symlinkDir".to_string(),
            serde_json::Value::Bool(self.symlink_dir),
        );
        map.insert(
            "symlinkFile".to_string(),
            serde_json::Value::Bool(self.symlink_file),
        );
        map.insert(
            "junction".to_string(),
            serde_json::Value::Bool(self.junction),
        );
        map.insert(
            "hardlink".to_string(),
            serde_json::Value::Bool(self.hardlink),
        );
        map.insert("dirOrder".to_string(), order(&self.dir_order));
        map.insert("fileOrder".to_string(), order(&self.file_order));
        map.insert(
            "probedAt".to_string(),
            serde_json::Value::String(self.probed_at.clone()),
        );
        map.insert(
            "copyOnly".to_string(),
            serde_json::Value::Bool(self.copy_only()),
        );
        serde_json::Value::Object(map)
    }
}

/* ------------------------------------------------------------------ */
/* 测试                                                                */
/* ------------------------------------------------------------------ */

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ak-linkfs-{}-{}-{}", tag, std::process::id(), nanos));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn seed(dir: &Path) -> PathBuf {
        let src = dir.join("library").join("pdf");
        std::fs::create_dir_all(&src).expect("src dir");
        std::fs::write(src.join("SKILL.md"), b"body").expect("src file");
        src
    }

    #[test]
    fn method_strings_round_trip_and_reject_unknown() {
        for m in [
            LinkMethod::Symlink,
            LinkMethod::Junction,
            LinkMethod::Hardlink,
            LinkMethod::Copy,
        ] {
            assert_eq!(LinkMethod::parse(m.as_str()), Some(m));
        }
        assert_eq!(LinkMethod::parse("Junction"), None);
        assert_eq!(LinkMethod::parse(""), None);
        assert_eq!(LinkMethod::parse("junction "), None);
        assert!(!LinkMethod::Copy.is_link());
        assert!(LinkMethod::Junction.is_link());
    }

    #[test]
    fn probe_reports_a_usable_dir_chain() {
        // 这是链接内核验收 1 的内核版本：普通会话下必须至少有一种非 copy 的目录方式。
        let root = temp("probe");
        let cap = probe(&root);
        assert!(
            cap.dir_order.iter().any(|m| m.is_link()),
            "no directory link method is available: {:?}",
            cap.dir_order
        );
        assert!(!cap.copy_only());
        assert!(cap.preferred_dir().is_some());
        store::rmrf(&root);
    }

    #[test]
    fn probe_survives_an_unwritable_root() {
        // 探测失败不得 panic，也不得返回半截结果。
        let cap = probe(Path::new(r"C:\Windows\System32\ak-probe-should-fail"));
        // 具体能力取决于权限，但结构必须自洽：order 里每一项都得有对应的布尔位。
        for m in &cap.dir_order {
            assert!(*m == LinkMethod::Symlink && cap.symlink_dir
                || *m == LinkMethod::Junction && cap.junction);
        }
    }

    #[test]
    fn dir_symlink_round_trips_and_nothing_leaks() {
        let root = temp("symdir");
        let src = seed(&root);
        let dst = root.join("agent").join("pdf");
        let m = create_dir_link(&src, &dst, None).expect("create link");
        assert!(m.is_link(), "{:?}", m);
        assert!(is_link(&dst));
        assert_eq!(link_kind(&dst), Some(m));
        assert_eq!(link_kind(&dst), entry_link_kind(&dst));
        assert_eq!(
            std::fs::read_to_string(dst.join("SKILL.md")).expect("read through"),
            "body"
        );
        let target = read_target(&dst).expect("read target");
        assert!(same_target(&target, &src), "{} vs {}", target.display(), src.display());
        unlink(&dst).expect("unlink");
        assert!(!is_link(&dst));
        assert!(src.join("SKILL.md").exists(), "the library copy must survive");
        store::rmrf(&root);
    }

    #[test]
    fn junction_round_trips_through_the_reparse_buffer() {
        let root = temp("junc");
        let src = seed(&root);
        let dst = root.join("junc");
        create_junction(&src, &dst).expect("create junction");
        assert_eq!(link_kind(&dst), Some(LinkMethod::Junction));
        // 偏移算错的典型表现就是这里读回乱码或 None。
        let target = read_target(&dst).expect("read back");
        assert!(same_target(&target, &src), "{} vs {}", target.display(), src.display());
        assert_eq!(
            std::fs::read_to_string(dst.join("SKILL.md")).expect("read through"),
            "body"
        );
        // `unlink` 不能跟随：删完库内容还得在。
        unlink(&dst).expect("unlink junction");
        assert!(src.join("SKILL.md").exists());
        store::rmrf(&root);
    }

    #[test]
    fn creating_over_an_existing_path_is_refused() {
        let root = temp("exists");
        let src = seed(&root);
        let dst = root.join("busy");
        std::fs::create_dir_all(&dst).expect("busy dir");
        let e = create_dir_link(&src, &dst, None).expect_err("must refuse");
        assert!(e.contains("already exists"), "{e}");
        // 拒绝之后那个目录必须原样还在（没有被清掉）。
        assert!(dst.is_dir());
        store::rmrf(&root);
    }

    #[test]
    fn relative_paths_are_refused() {
        let root = temp("relative");
        seed(&root);
        let e = create_dir_link(Path::new("library"), Path::new("agent"), None).expect_err("must refuse");
        assert!(e.contains("absolute"), "{e}");
        store::rmrf(&root);
    }

    #[test]
    fn file_links_records_whichever_method_worked_and_keeps_content() {
        // 首选由实测定（本机通常是 symlink），但断言的是「记录下来的方式与盘上
        // 真实情况一致」，而不是某一种具体方式 —— 那才是 doctor 依赖的东西。
        let root = temp("filelink");
        let src = seed(&root);
        let file = src.join("SKILL.md");
        let dst = root.join("rules").join("AGENTS.md");
        let m = create_file_link(&file, &dst, None).expect("create file link");
        assert!(m.is_link(), "{:?}", m);
        assert_eq!(link_kind(&dst), Some(m), "the recorded method must match the disk");
        assert_eq!(std::fs::read_to_string(&dst).expect("read"), "body");
        unlink(&dst).expect("unlink");
        assert!(file.exists());
        store::rmrf(&root);
    }

    #[test]
    fn a_usable_file_method_is_recorded_on_this_machine() {
        let root = temp("filecap");
        let cap = probe(&root);
        assert!(
            cap.file_order.iter().any(|m| m.is_link()),
            "no file link method is available: {:?}",
            cap.file_order
        );
        assert!(cap.preferred_file().is_some());
        store::rmrf(&root);
    }

    #[test]
    fn hardlink_is_reported_as_a_link_but_has_no_target() {
        let root = temp("hardlink");
        let src = seed(&root);
        let a = src.join("SKILL.md");
        let b = root.join("hard-b.md");
        if std::fs::hard_link(&a, &b).is_err() {
            store::rmrf(&root);
            return; // 跨卷的临时目录：这条用例不适用
        }
        assert_eq!(link_kind(&b), Some(LinkMethod::Hardlink));
        assert!(read_target(&b).is_none(), "a hard link has no target to read back");
        unlink(&b).expect("unlink hard link");
        assert!(a.exists(), "the other name still holds the content");
        store::rmrf(&root);
    }

    #[test]
    fn a_plain_directory_is_not_a_link() {
        let root = temp("plain");
        let src = seed(&root);
        assert!(!is_link(&src));
        assert_eq!(link_kind(&src), None);
        assert!(read_target(&src).is_none());
        store::rmrf(&root);
    }

    #[test]
    fn same_target_normalises_case_and_prefixes() {
        let root = temp("same");
        let src = seed(&root);
        assert!(same_target(&src, &src));
        let upper = PathBuf::from(store::native_str(&src).to_uppercase());
        assert!(same_target(&src, &upper), "Windows paths are case-insensitive");
        let prefixed = PathBuf::from(format!(r"\\?\\{}", store::native_str(&src)));
        assert!(same_target(&src, &prefixed), "the \\\\?\\ prefix is a presentation detail");
        store::rmrf(&root);
    }

    #[test]
    fn capability_cache_round_trips_per_volume() {
        let root = temp("cache");
        let work = root.join("probe");
        let cache = root.join("cache").join("linkcap.txt");
        let first = capability(&work, Some(&cache));
        let second = capability(&work, Some(&cache));
        assert_eq!(first.symlink_dir, second.symlink_dir);
        assert_eq!(first.dir_order, second.dir_order);
        assert_eq!(first.file_order, second.file_order);
        assert!(!second.probed_at.is_empty());
        store::rmrf(&root);
    }

    #[test]
    fn a_corrupt_cache_is_ignored_not_fatal() {
        let root = temp("cache-corrupt");
        let work = root.join("probe");
        let cache = root.join("linkcap.txt");
        std::fs::write(&cache, "{").expect("write garbage");
        let cap = capability(&work, Some(&cache));
        assert!(cap.dir_order.iter().any(|m| m.is_link()), "a broken cache must not erase the capability");
        store::rmrf(&root);
    }

    #[test]
    fn tag_constants_match_the_windows_values() {
        // 这三个常数是协议值，不是我们的选择；钉住它们，免得有人「顺手整理」。
        // 顺带记录一个容易写错的位运算：标签的低 4 位是标志位，所以标签本身
        // **不能**写成 `0xA000_000C & 0xFFFF_FFF0` —— 那会得到 0xA000_0000。
        assert_eq!(IO_REPARSE_TAG_MOUNT_POINT, 0xA000_0003);
        assert_eq!(SYMLINK_TAG, 0xA000_000C);
        assert_eq!(REPARSE_HEADER, 16);
        assert_eq!(SYMLINK_TAG_MASK, 0xFFFF_FFF0);
        let absolute = SYMLINK_TAG & !SYMLINK_RELATIVE_FLAG;
        let relative = SYMLINK_TAG | SYMLINK_RELATIVE_FLAG;
        assert_eq!(relative & !SYMLINK_RELATIVE_FLAG, absolute);
        assert_eq!((relative - SYMLINK_RELATIVE_FLAG), absolute);
    }
}
