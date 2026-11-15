//! manifest：commit 树的扁平虚拟命名空间（path → oid/mode/size）。
//!
//! 一次 `git ls-tree -r -l -z <commit>` 装载全部文件行；打开 O(文件数)，
//! 之后 stat/lookup 纯内存查询（与 B1 的 LowerIndex 同构，但来源是对象库
//! 的树而不是 SQLite 模板清单）。
//!
//! * `-z`：记录以 NUL 分隔、路径原样字节（不转义、不带引号）；记录内
//!   `<mode> <type> <oid> <size>\t<path>`，首个 `\t` 即 meta 结束（meta
//!   段不含制表符）。
//! * mode（八进制文本）：100644 / 100755 / 120000（symlink，size = blob
//!   大小）/ 160000（gitlink，size 列为 `-`，按 0 记账；对象库无对应
//!   blob，读路径报缺）。
//! * 目录集合与每目录直接子项索引由文件行派生（树不输出目录行）；
//!   `""` = 根。
//! * 大小写：git 树是大小写敏感命名空间，manifest 按原样字节作键
//!   （大小写敏感查找）；ino 派生（[`crate::file_id`]）另行小写化，与
//!   wtfs-core 的 case-insensitive ino 规则一致。
//! * 非法路径（[`crate::validate_relative_path`] 拒绝的形状）装载即报错
//!   ——与 B1 模板创建的拒绝规则同一口径，绝不把上游树里的危险路径
//!   带进视图。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use crate::gitx::{require_ok, run_git};
use crate::{Error, Result, validate_relative_path};

/// manifest 内单个文件（树内 blob 条目）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFile {
    /// repo 相对路径：UTF-8，正斜杠 `/` 分隔，git 原样字节（大小写敏感键）。
    pub path: String,
    /// blob oid（40 位小写 hex sha1）。
    pub oid: String,
    /// Git 模式（[`crate::MODE_REGULAR`] / [`MODE_EXEC`] / [`MODE_SYMLINK`] /
    /// [`MODE_GITLINK`]）。
    pub mode: u32,
    /// blob 字节数（gitlink 恒 0，size 列为 `-`）。
    pub size: u64,
}

/// manifest：文件表 + 目录集合 + 每目录直接子项（构建一次，只读共享）。
#[derive(Debug, Clone)]
pub struct Manifest {
    commit: String,
    files: BTreeMap<String, ManifestFile>,
    dirs: HashSet<String>,
    children: HashMap<String, BTreeMap<String, EntryKind>>,
    total_size: u64,
}

/// 目录索引里的子项类型：文件行 = [`EntryKind::File`]，派生的中间目录 =
/// [`EntryKind::Directory`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

impl Manifest {
    /// 从仓库对象库装载 `<commit>` 的完整文件清单（递归展开子树）。
    pub fn load(repo: &Path, commit: &str) -> Result<Self> {
        let out = run_git(repo, &["ls-tree", "-r", "-l", "-z", commit])?;
        require_ok(&out, "git ls-tree -r -l -z")?;
        Self::parse(&out.stdout, commit)
    }

    /// 从 `ls-tree -r -l -z` 原始输出解析（测试可直接喂构造的字节）。
    pub fn parse(raw: &[u8], commit: &str) -> Result<Self> {
        let mut files = BTreeMap::new();
        let mut total_size = 0u64;
        for record in raw.split(|&b| b == 0u8) {
            if record.is_empty() {
                continue;
            }
            let Some(tab) = record.iter().position(|&b| b == b'\t') else {
                return Err(Error::Cas(format!(
                    "ls-tree record without TAB separator: {:?}",
                    String::from_utf8_lossy(record)
                )));
            };
            // split_at(tab) 的后半从 TAB 本身开始；路径段须跳过分隔符。
            let meta = &record[..tab];
            let path_bytes = &record[tab + 1..];
            let path = std::str::from_utf8(path_bytes)
                .map_err(|_| Error::Cas("ls-tree path is not UTF-8".into()))?;
            validate_relative_path(path)?;
            let meta = std::str::from_utf8(meta)
                .map_err(|_| Error::Cas("ls-tree meta is not UTF-8".into()))?;
            let (mode, oid, size) = parse_meta(meta, path)?;
            total_size += size;
            files.insert(
                path.to_string(),
                ManifestFile {
                    path: path.to_string(),
                    oid,
                    mode,
                    size,
                },
            );
        }
        Ok(Self {
            commit: commit.to_string(),
            dirs: derive_dirs(&files),
            children: derive_children(&files),
            files,
            total_size,
        })
    }

    /// manifest 对应的 commit（完整 sha）。
    pub fn commit(&self) -> &str {
        &self.commit
    }

    /// 文件条目数。
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// 全部 blob 字节数（缓存预算与基准报告用）。
    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    /// 文件表（路径序 BTreeMap）。
    pub fn files(&self) -> &BTreeMap<String, ManifestFile> {
        &self.files
    }

    /// 单文件查询（精确路径，大小写敏感）。
    pub fn file(&self, path: &str) -> Option<&ManifestFile> {
        self.files.get(path)
    }

    /// 路径是否为派生目录（不含根；根恒为目录）。
    pub fn has_dir(&self, path: &str) -> bool {
        path.is_empty() || self.dirs.contains(path)
    }

    /// 全部派生目录（不含根；基准遍历与 M18 卷视图用）。
    pub fn dirs(&self) -> &HashSet<String> {
        &self.dirs
    }

    /// 目录的直接子项（name → kind），按名字序；`""` = 根。
    /// 不存在的目录返回 `None`（与 [`Self::has_dir`] 一致性由调用方保证）。
    pub fn children_of(&self, dir: &str) -> Option<&BTreeMap<String, EntryKind>> {
        self.children.get(dir)
    }

    /// 路径或其任一祖先目录之下是否有 manifest 内容（删除时是否需要
    /// whiteout 的判定）。
    pub fn has_file_or_dir(&self, path: &str) -> bool {
        self.files.contains_key(path) || self.dirs.contains(path)
    }
}

/// meta 段：`<mode> <type> <oid> <size>`（`-l` 的 size 列按列宽右对齐，
/// 前导空格填充——按空白切分）。
fn parse_meta(meta: &str, path: &str) -> Result<(u32, String, u64)> {
    let bad = || Error::Cas(format!("ls-tree meta {meta:?} unparseable for {path:?}"));
    let mut parts = meta.split_whitespace();
    let mode_text = parts.next().ok_or_else(bad)?;
    let ty = parts.next().ok_or_else(bad)?;
    let oid = parts.next().ok_or_else(bad)?;
    let size_text = parts.next().ok_or_else(bad)?;
    if parts.next().is_some() {
        return Err(bad());
    }
    let mode = u32::from_str_radix(mode_text, 8).map_err(|_| bad())?;
    match mode {
        crate::MODE_REGULAR | crate::MODE_EXEC | crate::MODE_SYMLINK => {
            if ty != "blob" {
                return Err(bad());
            }
        }
        crate::MODE_GITLINK => {
            if ty != "commit" {
                return Err(bad());
            }
        }
        _ => return Err(bad()),
    }
    if oid.len() != 40 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    let size = if size_text == "-" {
        0
    } else {
        size_text.parse::<u64>().map_err(|_| bad())?
    };
    Ok((mode, oid.to_string(), size))
}

/// 文件路径 → 全部祖先目录集合（不含根）。
fn derive_dirs(files: &BTreeMap<String, ManifestFile>) -> HashSet<String> {
    let mut dirs = HashSet::new();
    for path in files.keys() {
        let comps: Vec<&str> = path.split('/').collect();
        let mut parent = String::new();
        for comp in &comps[..comps.len() - 1] {
            parent = if parent.is_empty() {
                (*comp).to_string()
            } else {
                format!("{parent}/{comp}")
            };
            dirs.insert(parent.clone());
        }
    }
    dirs
}

/// 目录 → 直接子项索引（`""` = 根；与 wtfs-core LowerIndex 同一算法）。
fn derive_children(
    files: &BTreeMap<String, ManifestFile>,
) -> HashMap<String, BTreeMap<String, EntryKind>> {
    let mut children: HashMap<String, BTreeMap<String, EntryKind>> = HashMap::new();
    for path in files.keys() {
        let comps: Vec<&str> = path.split('/').collect();
        let mut parent = String::new();
        for (i, comp) in comps.iter().enumerate() {
            let is_last = i + 1 == comps.len();
            children.entry(parent.clone()).or_default().insert(
                (*comp).to_string(),
                if is_last {
                    EntryKind::File
                } else {
                    EntryKind::Directory
                },
            );
            if !is_last {
                parent = if parent.is_empty() {
                    (*comp).to_string()
                } else {
                    format!("{parent}/{comp}")
                };
            }
        }
    }
    children
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

    fn rec(mode: &str, ty: &str, oid: &str, size: &str, path: &str) -> Vec<u8> {
        let mut v = format!("{mode} {ty} {oid} {size}\t").into_bytes();
        v.extend_from_slice(path.as_bytes());
        v.push(0);
        v
    }

    #[test]
    fn parses_entries_sizes_modes_and_derives_dirs() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&rec("100644", "blob", OID, "0", "a/empty.txt"));
        raw.extend_from_slice(&rec("100644", "blob", OID, "123", "a/b/deep.txt"));
        raw.extend_from_slice(&rec("100755", "blob", OID, "7", "run.sh"));
        raw.extend_from_slice(&rec("120000", "blob", OID, "9", "link.txt"));
        raw.extend_from_slice(&rec(
            "160000",
            "commit",
            "1234567890123456789012345678901234567890",
            "-",
            "submod",
        ));
        let m = Manifest::parse(&raw, "commitsha").unwrap();

        assert_eq!(m.len(), 5);
        assert_eq!(m.commit(), "commitsha");
        let f = m.file("a/empty.txt").unwrap();
        assert_eq!((f.mode, f.size), (crate::MODE_REGULAR, 0));
        assert_eq!(m.file("run.sh").unwrap().mode, crate::MODE_EXEC);
        assert_eq!(m.file("link.txt").unwrap().mode, crate::MODE_SYMLINK);
        assert_eq!(m.file("link.txt").unwrap().size, 9);
        let g = m.file("submod").unwrap();
        assert_eq!((g.mode, g.size), (crate::MODE_GITLINK, 0));
        assert_eq!(m.total_size(), 123 + 7 + 9);

        assert!(m.has_dir("a"));
        assert!(m.has_dir("a/b"));
        assert!(!m.has_dir("a/b/deep.txt"));
        assert!(m.has_dir(""));
        assert_eq!(
            m.children_of("")
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["a", "link.txt", "run.sh", "submod"]
        );
        assert_eq!(
            m.children_of("a")
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["b", "empty.txt"]
        );
        assert_eq!(
            m.children_of("a/b").unwrap().get("deep.txt"),
            Some(&EntryKind::File)
        );
    }

    #[test]
    fn rejects_bad_records_and_paths() {
        // 缺 TAB
        let mut raw = format!("100644 blob {OID} 1 nopath").into_bytes();
        raw.push(0);
        assert!(Manifest::parse(&raw, "c").is_err());
        // 未知 mode
        assert!(Manifest::parse(&rec("999999", "blob", OID, "1", "x"), "c").is_err());
        // gitlink 但 type=blob
        assert!(Manifest::parse(&rec("160000", "blob", OID, "1", "x"), "c").is_err());
        // 保留名
        assert!(Manifest::parse(&rec("100644", "blob", OID, "1", "CON"), "c").is_err());
        // dot 组件
        assert!(Manifest::parse(&rec("100644", "blob", OID, "1", "a/../b"), "c").is_err());
        // 空
        let m = Manifest::parse(b"", "c").unwrap();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert!(m.has_dir(""));
        assert!(
            m.children_of("").is_none(),
            "空 manifest 无任何子项（根键不存在）"
        );
    }

    /// 路径含 `\t` / 非 ASCII：`-z` 下原样字节，首个 `\t` 为分隔符。
    #[test]
    fn keeps_raw_bytes_after_first_tab() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&rec("100644", "blob", OID, "3", "数据/表.txt"));
        raw.extend_from_slice(&rec("100644", "blob", OID, "3", "tab\tin.txt"));
        let m = Manifest::parse(&raw, "c").unwrap();
        assert!(m.file("数据/表.txt").is_some());
        assert!(m.file("tab\tin.txt").is_some());
        assert_eq!(
            m.children_of("")
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["tab\tin.txt", "数据"]
        );
    }
}
