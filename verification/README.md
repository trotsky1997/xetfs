# 实测记录

日期：2026-10-12，Asia/Shanghai。记录包含本机、Docker、公开 Hugging Face Hub 读写和私有 GitHub Actions 的实际结果。本地导入性能使用五组交错配对；CAS 端到端读取对照固定在同一台 Apple M5 Pro 主机，每种配置交错运行五组并使用隔离缓存。

| 验证 | 环境 | 结果与证据 |
| --- | --- | --- |
| 格式、Clippy、Rust 测试、release 构建、Linux FUSE 挂载验收 | GitHub Actions：macOS、Ubuntu、Linux FUSE | 提交 `1c75b54e6433ec3d5ccf2e36ef44fbf7fa184025` 的三个 job 全部通过；[CI 运行记录](https://github.com/trotsky1997/xetfs/actions/runs/38221185791)；本机 macOS 完整测试 31 项通过 |
| 本次 native writer 本地验证 | macOS ARM64；当前改动 | `cargo fmt --all -- --check`、`cargo test --locked --all-targets`（36 项）、`cargo clippy --locked --all-targets -- -D warnings`、`cargo build --locked --release` 和两个 Python 验收脚本语法检查均通过；此项是本机验证，不代表 GitHub Actions 运行 |
| Clippy、Rust 测试、release 构建 | Ubuntu 26.04.1，ARM64，Rust 1.98 | 25 项测试通过；[日志](linux-checks.log) |
| ARM64 随仓库二进制刷新 | macOS ARM64 / Debian 12 ARM64，Rust 1.98.1 | 两个 release 二进制均从当前源码重建；init/put/cat/fsck 与去重冒烟验证通过，Linux 真实 FUSE/远程 FUSE 验收通过；Linux 最高 GLIBC 符号为 `GLIBC_2.34`，依赖 `libgcc_s.so.1`；校验文件：[SHA256SUMS](../bin/SHA256SUMS)，构建元数据：[build.json](build.json) |
| 官方样本 | 63,527,244 字节 CSV、官方 Xorb、固定 dataset revision | 796 个分块边界和块哈希全部一致；文件/Xorb 哈希一致；重建字节一致；[JSON](official-reference.json) |
| Hub 真实读取 | `xet-team/xet-spec-reference-files` 固定 revision；公开只读；macOS ARM64 | 经 Hugging Face Xet 客户端拉取 16 个文件、78,840,265 字节；导出的 CSV SHA-256 一致；fsck 验证 14 个 Xorb、1,041 个块、15 个配方；[JSON](remote-pull.json) |
| 独立 CAS 远程读取 | 同一公开 revision；直接 Hub metadata + Xet read token + CAS reconstruction + 签名 Xorb Range；macOS ARM64 | 全文件 63,527,244 字节，客户端验证 Xet 文件哈希和 SHA-256；偏移 1 MiB 的 64 KiB 范围与全文件切片一致；[JSON](direct-cas-read.json)；复现脚本：[verify_direct_cas.py](../scripts/verify_direct_cas.py) |
| Hub 远程写入与双路径回读 | 用户指定的 `di-zhang-fdu/xet-debug` 数据集；macOS ARM64 | 在公开数据集的唯一目录上传 12 MiB 合成 `.bin`，Hub commit `21ee4d72715d98f60eb97ebb9c88cf4104990088`；独立 CAS 和官方 `hf download` 回读 SHA-256 均为 `8b621a…126037a`；XetFS 本地 fsck 验证 200 块；[JSON](remote-write-acceptance.json)；复现脚本：[remote_write_acceptance.py](../scripts/remote_write_acceptance.py) |
| 可重读流远程写入与双路径回读 | 同一用户指定的公开 dataset；macOS ARM64 | 从本地 CAS 直接流式上传 12 MiB 合成 `.bin`，没有 XetFS 全文件临时副本；CAS 与官方客户端下载的 SHA-256 均为 `8b621a…126037a`；[Hub commit](https://huggingface.co/datasets/di-zhang-fdu/xet-debug/commit/d493abdb11e7af44cb7e0ce0d3ab2b532b385500)；[JSON](remote-write-stream-acceptance.json) |
| XetFS native Xet CAS 写入与双路径回读 | 用户指定的公开 `di-zhang-fdu/xet-debug` dataset；macOS ARM64 | 已提交的 12 MiB 文件由 XetFS 直接上传不可变 Xorb 和 MDB metadata shard，再写 Hub LFS pointer；独立 CAS 与官方 `hf download` 回读 SHA-256 均为 `8b621a…126037a`，fsck 检查 200 chunks；[Hub commit](https://huggingface.co/datasets/di-zhang-fdu/xet-debug/commit/f9106589e55cba3714851199079839da882e1a4e)；[JSON](remote-write-native-acceptance.json) |
| 冷缓存 native Xet CAS 写入 | 用户指定的公开 dataset；macOS ARM64；`huggingface_hub` 1.8.0 官方写入对照 | 修正 DataHash SHA-256 字节序后，新 12 MiB 对象成功完成 LFS batch、CAS、MDB shard 注册与 commit；native push 总耗时 5.94 秒，CAS 重建和官方客户端回读 SHA 均通过。相同大小但独立种子的官方写入为 5.43 秒，单样本 native 慢 9.5%；[native JSON](remote-write-12m-cold-sha256-fix.json)、[official JSON](remote-write-12m-official-cold-benchmark.json) |
| 冷缓存 64 MiB native/官方写入对照 | 同一公开 dataset；macOS ARM64；各一份独立随机 payload | native push 17.15 秒，官方 `HfApi.upload_file`/`hf_xet` 26.33 秒，单样本 native 用时少 34.9%（1.54×）；native CAS 与官方客户端下载 SHA 均通过，官方上传也完整下载校验。[native JSON](remote-write-64m-native-cold-benchmark.json)、[official JSON](remote-write-64m-official-cold-benchmark.json) |
| Warm native CAS Xorb 去重 | 同一公开 dataset；macOS ARM64；重复 64 MiB payload | CAS HEAD 命中 2 个已有 Xorb，上传 0 个；commit 用时 3.76 秒，CAS 与官方客户端回读 SHA 均一致。[JSON](remote-write-64m-warm-dedup.json) |
| 当前发行版 CAS 读取性能对照 | 同一公开 Xet dataset 中的 63.5 MB CSV 与 14.7 MB Xorb；macOS ARM64；官方 `hf` CLI 1.32.0；隔离缓存、交错配对 | 每种配置 5 轮，全部 SHA-256 一致。CSV：1/1 中位数 3.89/4.67 秒（1.20×），8/8 为 3.93/4.79 秒（1.22×）；Xorb：1/1 为 4.13/4.44 秒（1.07×），8/8 为 3.96/4.44 秒（1.12×）；每个配置下 XetFS 均有 4/5 组配对更快。报告：[CSV 1/1](direct-vs-official-current-1w.json)、[CSV 8/8](direct-vs-official-current-8w.json)、[Xorb 1/1](direct-vs-official-xorb-1w.json)、[Xorb 8/8](direct-vs-official-xorb-8w.json)；脚本：[compare_cas_download.py](../scripts/compare_cas_download.py) |
| Hub 并发拉取 | 同一 release 二进制、同一公开 revision、每组 3 次交错运行 | 1 worker 中位数 15.41 秒；8 workers 6.28 秒，减少 59.3%；三组均校验 CSV SHA-256 和 fsck；[JSON](remote-parallel-pull.json)；复现实验脚本：[remote_pull_benchmark.py](../scripts/remote_pull_benchmark.py) |
| 真实 FUSE | Docker Linuxkit 7.0.12，ARM64，root | 读写、链接、目录枚举、元数据、重挂载和 SIGKILL 恢复通过；[JSON](mount-acceptance.json) |
| Hub 按需远程 FUSE | Docker Linuxkit 7.0.12，ARM64；用户指定的 `di-zhang-fdu/xet-debug` 固定 commit；只读挂载 | 12 MiB 官方下载样本中随机偏移读取 64 KiB，字节与 SHA-256 切片一致；只打开一个 4 MiB 对齐窗口（文件逻辑范围的 1/3），写打开被拒绝；[JSON](remote-mount-acceptance.json)；复现脚本：[remote_mount_acceptance.py](../scripts/remote_mount_acceptance.py) |
| 非 root FUSE | 相同 Linux 环境，uid/gid 65534，fusermount3 | 相同挂载流程通过；[JSON](mount-unprivileged.json) |
| CLI 去重 | macOS ARM64，固定种子的 16 MiB 伪随机数据 | 相同副本新增 0 数据对象字节；插入 279 字节后新增 64,664 字节唯一原始块、64,672 字节 Xorb，内容复用 99.6146%；[JSON](dedup-demo.json) |
| 分块热路径前后对比 | Apple M5 Pro，128 MiB 固定种子伪随机数据，交错配对各 5 次 | 完整导入中位吞吐 358.1 → 410.3 MiB/s（+14.6%）；插入版本 721.1 → 1,059.4 MiB/s（+46.9%）；唯一块与回读校验一致；[JSON](performance.json) |

普通 Rust 测试包括成功导入 68 MiB 数据并跨 Xorb 读取、70 MiB 导入中途失败后的旧版本保留与孤儿回收、快照/硬链接/压实、200 步随机写入及截断模型对照、损坏检测及块解码边界。大文件成功导出后还比较了完整 BLAKE3 摘要。

挂载测试实际通过内核调用访问 FUSE。崩溃场景先写入并 fsync，随后追加未提交尾部，向文件系统进程发送 SIGKILL。重新打开仓库后读取到已 fsync 的完整版本，未提交尾部被排除，fsck 通过。这验证该执行场景的进程崩溃恢复，未模拟断电、磁盘故障或所有可能的中断时间点。

性能样本为合成伪随机数据。冷写入对照每种尺寸各只有一份独立内容样本，并非交错配对；12 MiB native 稍慢、64 MiB native 较快，方向并不一致。64 MiB 的一次重复 native push 在当前环境等待远端请求超过六分钟且没有网络活动，未纳入统计。现有数据可证明兼容性和给出该样本耗时，尚不足以断言稳定的远程写入性能优势，更不能推导 Hugging Face 服务端内部性能排名。远程耗时包含网络、Hub API 与 CAS 服务时间。

Hub 并发性能报告只比较 XetFS 同一传输实现的 worker 配置；旧串行版在当前固定 revision 上连续两次未能打开暂存的 `.gitattributes`，因此未用于性能计算。远程数据受网络和服务端状态影响，报告的加速比只代表本次 Apple M5 Pro 实测，不代表服务端 CAS 性能排名。

远程 FUSE 的 4 MiB 结果是 XetFS 内部按需重建窗口的逻辑上界，不是实测 HTTP 线速字节数；本次验收未统计 HTTP 响应流量。与完整下载相比，它为随机访问提供了只读取命中窗口的功能路径；具体线速优势仍需针对更多文件、网络和缓存状态继续测量。

CAS 读取对照测量的是两种客户端经同一个 Hub/CAS 服务端的端到端耗时，不是服务端内部吞吐。当前发行版在一台 Apple M5 Pro 上，对两种文件和两种 worker 配置各做 5 组交错配对，XetFS 中位数领先 1.07–1.22×，每组有 4/5 次配对更快；这支持该样本下的客户端性能优势，但样本仍只覆盖一个主机、一个公开仓库和两种文件尺寸，不能推出所有网络、文件尺寸与负载下都领先。native 写入报告证明 XetFS 自己完成 LFS 协商、Xorb CAS 上传、MDB shard 注册和 Git/LFS commit，官方客户端成功下载回读；冷写入首轮对照结果喜忧参半，warm 去重证明可避免重传已有 Xorb。它们建立协议互操作和特定样本优化证据，尚未构成服务端内部或总体性能排名。

CAS 对照脚本支持指定仓库类型、revision、文件路径、字节数和预期 SHA-256；每轮使用独立 HF 缓存，按交错顺序运行，并校验两个客户端的完整文件内容。

远程写入验收目标为用户指定的公开 dataset。验收只在新的 `xetfs-acceptance/<run-id>/payload.bin` 路径新增合成文件；未覆盖或删除已有文件，也未调整仓库可见性。目标仓库 `di-zhang-fdu/xet-debug` 是公开仓库，报告不含认证 token。

`bin/SHA256SUMS` 标识交付二进制；`build.json` 记录源码摘要、编译器和验证环境。二进制不是官方 Hugging Face 产品。
