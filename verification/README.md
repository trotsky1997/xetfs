# 实测记录

日期：2026-10-12，Asia/Shanghai。所有结果来自本次本机执行和 Docker 内执行；GitHub Actions 工作流已提供，尚未在远程仓库运行。

| 验证 | 环境 | 结果与证据 |
| --- | --- | --- |
| 格式、Clippy、Rust 测试、release 构建 | macOS 26 / Darwin 25.5，ARM64，Rust 1.98 | 25 项测试通过；[日志](native-checks.log) |
| Clippy、Rust 测试、release 构建 | Ubuntu 26.04.1，ARM64，Rust 1.98 | 25 项测试通过；[日志](linux-checks.log) |
| 官方样本 | 63,527,244 字节 CSV、官方 Xorb、固定 dataset revision | 796 个分块边界和块哈希全部一致；文件/Xorb 哈希一致；重建字节一致；[JSON](official-reference.json) |
| 真实 FUSE | Docker Linuxkit 7.0.12，ARM64，root | 读写、链接、目录枚举、元数据、重挂载和 SIGKILL 恢复通过；[JSON](mount-acceptance.json) |
| 非 root FUSE | 相同 Linux 环境，uid/gid 65534，fusermount3 | 相同挂载流程通过；[JSON](mount-unprivileged.json) |
| CLI 去重 | macOS ARM64，固定种子的 16 MiB 伪随机数据 | 相同副本新增 0 数据对象字节；插入 279 字节后新增 64,664 字节唯一原始块、64,672 字节 Xorb，内容复用 99.6146%；[JSON](dedup-demo.json) |

普通 Rust 测试包括成功导入 68 MiB 数据并跨 Xorb 读取、70 MiB 导入中途失败后的旧版本保留与孤儿回收、快照/硬链接/压实、200 步随机写入及截断模型对照、损坏检测及块解码边界。大文件成功导出后还比较了完整 BLAKE3 摘要。

挂载测试实际通过内核调用访问 FUSE。崩溃场景先写入并 fsync，随后追加未提交尾部，向文件系统进程发送 SIGKILL。重新打开仓库后读取到已 fsync 的完整版本，未提交尾部被排除，fsck 通过。这验证该执行场景的进程崩溃恢复，未模拟断电、磁盘故障或所有可能的中断时间点。

去重样本为合成数据；报告中的耗时来自单次本机运行，用于复现该实验，不用于推断模型文件或其他机器的性能。

`bin/SHA256SUMS` 标识交付二进制；`build.json` 记录源码摘要、编译器和验证环境。二进制不是官方 Hugging Face 产品。
