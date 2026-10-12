# 算法与测试数据来源

XetFS 的 Rust 文件系统、SQLite schema、CLI 与验证脚本位于本仓库，采用 Apache-2.0。

实现依据：

- [Hugging Face Xet 协议](https://huggingface.co/docs/xet/index)，含 chunking、hashing、Xorb、远程下载及上传格式。
- [huggingface/xet-core](https://github.com/huggingface/xet-core/tree/f2451ab0e106357ad5d57973bcc3ce0681d1a437)，Apache-2.0。Xet hash keys、Merkle 分组规则及压缩格式依据该 Rust 参考实现；本实现使用独立的数据结构和存储层。
- [huggingface/hf-hub](https://github.com/huggingface/hf-hub)，Apache-2.0。Rust Hub API、Hub commit 和官方 Xet 传输客户端集成。
- [srijs/rust-gearhash](https://github.com/srijs/rust-gearhash)，MIT OR Apache-2.0。通过 `gearhash 0.1.4` 依赖使用其固定 Gear 表。
- [Xet 官方参考数据](https://huggingface.co/datasets/xet-team/xet-spec-reference-files/tree/c4aa3a3f15b1395fff5ce934784bf6c8f2d62de8)，数据集声明 Apache-2.0。`tests/fixtures/official.chunks` 原样来自该固定 revision 的 `Electric_Vehicle_Population_Data_20250917.csv.chunks`；其他大样本由脚本按需下载。

主要直接依赖的许可证：

| 依赖 | 许可证 |
| --- | --- |
| blake3 | CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception |
| fuser | MIT |
| rusqlite | MIT |
| lz4_flex | MIT |
| clap、serde、serde_json、anyhow、thiserror、tempfile | MIT OR Apache-2.0 |
| fs2 | MIT OR Apache-2.0 |
| libc | MIT OR Apache-2.0 |
| hf-hub、hf-xet | Apache-2.0 |

完整解析版本见 `Cargo.lock`。`THIRD_PARTY_LICENSES.txt` 按 `about.toml` 的目标与许可优先级汇集依赖许可文本；安装 `cargo-about 0.9.2` 后运行 `python3 scripts/update_licenses.py` 可重建。SQLite 为 public domain。
