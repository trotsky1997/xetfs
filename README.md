# XetFS

Rust 编写的本地内容寻址文件系统，提供 Linux FUSE 挂载，以及 macOS/Linux 均可使用的仓库 CLI 与 Rust 库。当前版本为 **0.1.0**。

实现依据是 [Hugging Face Xet 概览](https://huggingface.co/docs/hub/en/xet/index)、[开放协议](https://huggingface.co/docs/xet/index)及其 Rust 参考实现。分块、块哈希、Merkle 哈希和 Xorb 块编码已经用固定版本的官方数据核对。

## 已实现

| 能力 | 行为 |
| --- | --- |
| 内容定义分块 | Gearhash；8 KiB 最小、64 KiB 目标、128 KiB 最大；流式输入；SIMD 加速且保持官方边界 |
| 内容寻址 | Xet keyed BLAKE3 块哈希、文件 Merkle 哈希、Xorb 哈希、term verification hash |
| 去重与压缩 | 仓库范围的跨文件、跨版本去重；raw、LZ4 frame、BG4 + LZ4 自动择优 |
| 数据组织 | 不超过 64 MiB 的不可变 Xorb；文件保存块配方；`manifest` 展示合并后的 Xorb 范围 |
| 文件操作 | 顺序与随机读写、追加、截断、零填充空洞、目录、重命名、硬链接、符号链接 |
| 元数据 | inode、原始字节文件名、权限位、uid/gid、纳秒时间戳、链接数 |
| 版本与维护 | 命名快照、快照读取、整树恢复、标记清理、Xorb 压实、`fsck` |
| Hub 远程传输 | 通过 Hugging Face 官方 Rust Xet 客户端拉取文件、上传文件并创建 Hub commit |
| 持久化 | SQLite WAL + FULL synchronous；对象先 fsync，再提交引用；仓库进程锁 |

## 构建与运行

需要 Rust 1.95 或更新版本；本次验证使用 Rust 1.98。Hugging Face Xet 远程客户端依赖 `sysinfo 0.39`，其最低 Rust 版本为 1.95。`Cargo.lock` 已包含在源码中。仓库中的 ARM64 预编译二进制来自上一版，不含下文的 Hub 远程命令；远程同步请使用本源码构建的程序。

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

## Hugging Face Hub 远程读写

先用 Hugging Face CLI 登录，或按 `hf-hub` 的标准方式提供 `HF_TOKEN`。读取私有仓库需要 read 权限；上传需要 write 权限。`remote pull` 按指定 revision 下载 Xet 文件并导入本地 CAS，`remote push` 将本地文件导出到临时目录，再通过官方 Xet 客户端上传并写入一个 Hub commit：

```sh
hf auth login
$XETFS_BIN --repo ./demo.repo init
$XETFS_BIN --repo ./demo.repo remote pull owner/dataset \
  --repo-type dataset --revision main --prefix train --workers 8
$XETFS_BIN remote cat owner/dataset train/weights.bin \
  --repo-type dataset --revision main --offset 1048576 --length 65536 > weights.range
$XETFS_BIN --repo ./demo.repo ls /train
$XETFS_BIN --repo ./demo.repo remote push owner/dataset \
  --repo-type dataset --revision main --prefix train \
  --message "Update training data"
```

`--prefix` 是 Hub 仓库内的相对目录，导入后在本地保留同一路径；push 时选择本地同名目录，上传到 Hub 同一路径。pull 遇到已存在且 Xet 文件哈希与长度都相同的文件会跳过；其他同名文件需要 `--force` 才会覆盖。push 只新增或更新文件，不会删除 Hub 上多余的文件；Hub 不存储空目录，符号链接会跳过并计入结果。传输完成后数据保存在本地仓库，因此后续读取不依赖网络；当前 FUSE 挂载本身仍挂载本地仓库。

pull 使用 Hub 批量快照接口并发获取文件；`--workers` 可设为 1 到 64，默认 8。报告中的 `workers` 记录本次配置。

`remote cat` 直接调用 Xet CAS reconstruction API 和签名 Xorb range URL，把指定字节范围写到 stdout，不需要先拉取整文件或初始化本地仓库。单次最多读取 64 MiB；目标必须是 Xet 存储的文件。完整文件读取会核对 Xet file hash。`--workers` 控制并发签名 Xorb 请求，范围为 1–64，默认 1；大文件可按网络与 CAS 响应情况调高。Hub 元数据仍由 `hf-hub` 获取，CAS token 按 Hugging Face CLI 的环境变量/本地 token 文件规则加载，短期 CAS token 和签名 URL 不落盘。

Hub 客户端负责授权、Xet CAS 传输和 Hub commit 格式。Hub 判定为 Xet/LFS 的文件经 Xet shard/CAS 路径上传；其他小文件按 Hub 的常规 inline 路径处理。XetFS 使用本地 Xet 算法重新摄取下载字节，并把本地文件内容交给官方客户端上传；本地 SQLite 仓库格式不会直接作为 Hub shard 上传。

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

实测包含：官方 CSV 的 796 个分块边界及块哈希、文件/Xorb/verification 哈希、官方 Xorb 解码后的完整字节比对、跨 Xorb 大文件读取、导入中断回滚、快照与 GC、真实内核挂载及 SIGKILL 后 fsync 数据恢复。性能基准对同一 128 MiB 数据集运行三次，记录优化前后的中位数；Hub 只读拉取也在固定公开 revision 上完成并核对 SHA-256。具体环境和指标见 [验证记录](verification/README.md)。

## 当前边界

- Hub 同步目前通过显式 `remote pull` / `remote push` 工作，不提供 Hub 文件的按需远程挂载、双向变更检测或删除镜像。每次传输会在宿主机临时目录暂存文件，峰值临时空间可能接近待传输数据总量。
- 挂载目前支持 Linux；macOS 已验证核心库与 CLI，尚未提供 macFUSE 挂载。
- 可写文件首次修改会在磁盘建立完整暂存文件；flush/fsync/最后关闭时流式重分块整个文件。数据对象会去重，但提交计算仍为 O(文件大小)，暂存空间也可能接近文件大小。
- 文件数据缓冲限制在一个 Xorb 和少量 chunk，配方及本次导入的块集合占用 O(块数) 内存。
- 当前使用直接 I/O 和串行 FUSE 回调；尚未提供 mmap、xattr/ACL、POSIX 文件锁、配额、特殊设备节点、分布式多写者与跨机器同步。读取不自动更新时间戳。

结构、提交顺序和格式约束见 [ARCHITECTURE.md](ARCHITECTURE.md)。代码采用 Apache-2.0；算法依据与测试数据来源见 [THIRD_PARTY.md](THIRD_PARTY.md)。
