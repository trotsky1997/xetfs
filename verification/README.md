# 实测记录

日期：2026-10-12，Asia/Shanghai。记录包含本机、Docker、公开 Hugging Face Hub 只读拉取和私有 GitHub Actions 的实际结果。性能对比固定在同一台 Apple M5 Pro 主机，以三次运行中位数比较优化前后的本地 XetFS。

| 验证 | 环境 | 结果与证据 |
| --- | --- | --- |
| 格式、Clippy、Rust 测试、release 构建 | macOS 26 / Darwin 25.5，ARM64，Rust 1.98 | 25 项测试通过；[日志](native-checks.log) |
| Clippy、Rust 测试、release 构建 | Ubuntu 26.04.1，ARM64，Rust 1.98 | 25 项测试通过；[日志](linux-checks.log) |
| 官方样本 | 63,527,244 字节 CSV、官方 Xorb、固定 dataset revision | 796 个分块边界和块哈希全部一致；文件/Xorb 哈希一致；重建字节一致；[JSON](official-reference.json) |
| Hub 真实读取 | `xet-team/xet-spec-reference-files` 固定 revision；公开只读；macOS ARM64 | 经 Hugging Face Xet 客户端拉取 16 个文件、78,840,265 字节；导出的 CSV SHA-256 一致；fsck 验证 14 个 Xorb、1,041 个块、15 个配方；[JSON](remote-pull.json) |
| Hub 并发拉取 | 同一 release 二进制、同一公开 revision、每组 3 次交错运行 | 1 worker 中位数 15.41 秒；8 workers 6.28 秒，减少 59.3%；三组均校验 CSV SHA-256 和 fsck；[JSON](remote-parallel-pull.json)；复现实验脚本：[remote_pull_benchmark.py](../scripts/remote_pull_benchmark.py) |
| 真实 FUSE | Docker Linuxkit 7.0.12，ARM64，root | 读写、链接、目录枚举、元数据、重挂载和 SIGKILL 恢复通过；[JSON](mount-acceptance.json) |
| 非 root FUSE | 相同 Linux 环境，uid/gid 65534，fusermount3 | 相同挂载流程通过；[JSON](mount-unprivileged.json) |
| CLI 去重 | macOS ARM64，固定种子的 16 MiB 伪随机数据 | 相同副本新增 0 数据对象字节；插入 279 字节后新增 64,664 字节唯一原始块、64,672 字节 Xorb，内容复用 99.6146%；[JSON](dedup-demo.json) |
| 分块热路径前后对比 | Apple M5 Pro，128 MiB 固定种子伪随机数据，交错配对各 5 次 | 完整导入中位吞吐 358.1 → 410.3 MiB/s（+14.6%）；插入版本 721.1 → 1,059.4 MiB/s（+46.9%）；唯一块与回读校验一致；[JSON](performance.json) |

普通 Rust 测试包括成功导入 68 MiB 数据并跨 Xorb 读取、70 MiB 导入中途失败后的旧版本保留与孤儿回收、快照/硬链接/压实、200 步随机写入及截断模型对照、损坏检测及块解码边界。大文件成功导出后还比较了完整 BLAKE3 摘要。

挂载测试实际通过内核调用访问 FUSE。崩溃场景先写入并 fsync，随后追加未提交尾部，向文件系统进程发送 SIGKILL。重新打开仓库后读取到已 fsync 的完整版本，未提交尾部被排除，fsck 通过。这验证该执行场景的进程崩溃恢复，未模拟断电、磁盘故障或所有可能的中断时间点。

性能样本为合成伪随机数据。报告比较的是 XetFS 本地实现自身的修改前后耗时，不包含对 Hugging Face CAS 服务端的吞吐对照，因此不据此声称已经超过其云端性能。远程拉取耗时还包含网络、Hub API 和 Xet CAS 服务时间，不作为本地吞吐指标。

Hub 并发性能报告只比较 XetFS 同一传输实现的 worker 配置；旧串行版在当前固定 revision 上连续两次未能打开暂存的 `.gitattributes`，因此未用于性能计算。远程数据受网络和服务端状态影响，报告的加速比只代表本次 Apple M5 Pro 实测，不代表服务端 CAS 性能排名。

`bin/SHA256SUMS` 标识交付二进制；`build.json` 记录源码摘要、编译器和验证环境。二进制不是官方 Hugging Face 产品。
