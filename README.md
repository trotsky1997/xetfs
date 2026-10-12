# XetFS

Rust 编写的本地内容寻址文件系统，提供 Linux FUSE 挂载，以及 macOS/Linux 均可使用的仓库 CLI 与 Rust 库。当前版本为 **0.1.0**。

实现依据是 [Hugging Face Xet 概览](https://huggingface.co/docs/hub/en/xet/index)、[开放协议](https://huggingface.co/docs/xet/index)及其 Rust 参考实现。分块、块哈希、Merkle 哈希和 Xorb 块编码已经用固定版本的官方数据核对。

## 已实现

| 能力 | 行为 |
| --- | --- |
| 内容定义分块 | Gearhash；8 KiB 最小、64 KiB 目标、128 KiB 最大；流式输入 |
| 内容寻址 | Xet keyed BLAKE3 块哈希、文件 Merkle 哈希、Xorb 哈希、term verification hash |
| 去重与压缩 | 仓库范围的跨文件、跨版本去重；raw、LZ4 frame、BG4 + LZ4 自动择优 |
| 数据组织 | 不超过 64 MiB 的不可变 Xorb；文件保存块配方；`manifest` 展示合并后的 Xorb 范围 |
| 文件操作 | 顺序与随机读写、追加、截断、零填充空洞、目录、重命名、硬链接、符号链接 |
| 元数据 | inode、原始字节文件名、权限位、uid/gid、纳秒时间戳、链接数 |
| 版本与维护 | 命名快照、快照读取、整树恢复、标记清理、Xorb 压实、`fsck` |
| 持久化 | SQLite WAL + FULL synchronous；对象先 fsync，再提交引用；仓库进程锁 |

## 构建与运行

需要 Rust 1.87 或更新版本；本次验证使用 Rust 1.98。`Cargo.lock` 已包含在源码中。

```sh
cargo build --release --locked
cargo test --locked --all-targets
```

也可直接使用 `bin/xetfs-macos-arm64` 或 `bin/xetfs-linux-arm64`。macOS 二进制提供仓库命令；内核挂载当前支持 Linux。附带的 Linux 二进制使用 GNU libc，需要 glibc 2.39 及以上和 libgcc；其他 Linux 环境请从源码构建。macOS 二进制的最低部署版本为 macOS 11，实测环境为 macOS 26。

以下示例创建独立仓库，导入本地文件并读取：

```sh
XETFS_BIN=./target/release/xetfs
printf 'hello XetFS\n' > sample.txt
$XETFS_BIN --repo ./demo.repo init
$XETFS_BIN --repo ./demo.repo mkdir -p /models
$XETFS_BIN --repo ./demo.repo put sample.txt /models/sample.txt
$XETFS_BIN --repo ./demo.repo cat /models/sample.txt
$XETFS_BIN --repo ./demo.repo cat /models/sample.txt --offset 6 --length 5
$XETFS_BIN --repo ./demo.repo cp /models/sample.txt /models/copy.txt
$XETFS_BIN --repo ./demo.repo manifest /models/sample.txt
$XETFS_BIN --repo ./demo.repo stats
$XETFS_BIN --repo ./demo.repo fsck
```

`put SOURCE PATH` 从宿主机导入，`get PATH DESTINATION` 导出到宿主机。仓库内的相对路径均相对虚拟根目录。`cp` 创建独立 inode 并共享内容配方；`ln` 创建共享 inode 的硬链接。修改副本不会改变原文件。

随机修改、元数据与链接：

```sh
$XETFS_BIN --repo ./demo.repo patch sample.txt /models/copy.txt --offset 1024
$XETFS_BIN --repo ./demo.repo truncate /models/copy.txt 4096
$XETFS_BIN --repo ./demo.repo chmod /models/copy.txt 640
$XETFS_BIN --repo ./demo.repo ln /models/sample.txt /hard-link
$XETFS_BIN --repo ./demo.repo ln -s models/sample.txt /symbolic-link
$XETFS_BIN --repo ./demo.repo mv /models/copy.txt /models/renamed.txt
```

## 挂载

Linux 需要内核 FUSE、`/dev/fuse` 和 `fusermount3`（通常由发行版的 `fuse3` 包提供）。`fuser` 使用 Rust 挂载实现，无需链接 libfuse 开发库。

```sh
mkdir ./mountpoint
$XETFS_BIN --repo ./demo.repo mount ./mountpoint
```

此命令在前台运行。另一个终端可使用 `cp`、`cat`、`mv`、`ln`、Python 文件 API 等访问挂载目录。卸载：

```sh
fusermount3 -u ./mountpoint
```

挂载点必须为空，并与仓库目录分离。挂载使用 `default_permissions`、`nodev`、`nosuid`。仓库同一时间由一个进程持有，挂载期间其他仓库 CLI 命令返回 Busy；先卸载，再进行快照和 GC 管理。CLI/Rust 库是仓库管理接口，文件权限的内核检查发生在 FUSE 挂载上。

## 快照与回收

```sh
$XETFS_BIN --repo ./demo.repo snapshot create baseline
$XETFS_BIN --repo ./demo.repo snapshot list
$XETFS_BIN --repo ./demo.repo cat /models/sample.txt --snapshot baseline
$XETFS_BIN --repo ./demo.repo snapshot restore baseline
$XETFS_BIN --repo ./demo.repo snapshot drop baseline
$XETFS_BIN --repo ./demo.repo gc
$XETFS_BIN --repo ./demo.repo gc --compact
```

`restore` 用快照中的完整目录树替换当前目录树。快照共享不可变文件配方与数据块，因此 GC 会保留快照引用的内容。普通 `gc` 删除不可达配方、块索引和完全空闲的 Xorb；`--compact` 还会重打包存活块，回收部分使用的 Xorb。回收报告的 `reclaimed_bytes` 是对象文件总长度的净减少量，不含数据库收缩，也不等同于底层文件系统分配空间。

## 验证

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
python3 scripts/dedup_demo.py ./target/release/xetfs
python3 scripts/verify_reference.py ./target/release/xetfs --cache ./reference-cache
python3 scripts/mount_acceptance.py ./target/release/xetfs  # Linux，实际挂载
```

官方样本验证下载约 78 MB 数据；普通 Rust 测试不需要联网。官方向量固定在 dataset revision `c4aa3a3f15b1395fff5ce934784bf6c8f2d62de8`。`verification/` 保存此次实测报告。

实测包含：官方 CSV 的 796 个分块边界及块哈希、文件/Xorb/verification 哈希、官方 Xorb 解码后的完整字节比对、跨 Xorb 大文件读取、导入中断回滚、快照与 GC、真实内核挂载及 SIGKILL 后 fsync 数据恢复。具体环境和指标见 [验证记录](verification/README.md)。

## 当前边界

- 本地存储后端；HF Hub 鉴权、远程 CAS、全局去重服务以及官方 shard 文件传输尚未实现。SQLite 元数据格式属于 XetFS，不能直接作为官方 shard 上传。
- 挂载目前支持 Linux；macOS 已验证核心库与 CLI，尚未提供 macFUSE 挂载。
- 可写文件首次修改会在磁盘建立完整暂存文件；flush/fsync/最后关闭时流式重分块整个文件。数据对象会去重，但提交计算仍为 O(文件大小)，暂存空间也可能接近文件大小。
- 文件数据缓冲限制在一个 Xorb 和少量 chunk，配方及本次导入的块集合占用 O(块数) 内存。
- 当前使用直接 I/O 和串行 FUSE 回调；尚未提供 mmap、xattr/ACL、POSIX 文件锁、配额、特殊设备节点、分布式多写者与跨机器同步。读取不自动更新时间戳。

结构、提交顺序和格式约束见 [ARCHITECTURE.md](ARCHITECTURE.md)。代码采用 Apache-2.0；算法依据与测试数据来源见 [THIRD_PARTY.md](THIRD_PARTY.md)。
