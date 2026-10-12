# XetFS 0.1 结构与存储约定

## 从 Xet 到文件系统

[Xet](https://huggingface.co/docs/hub/en/xet/index)提供面向大文件的内容寻址、分块去重和重建机制。XetFS 在这些机制上增加 inode、目录树、文件句柄和内核文件系统接口。

```mermaid
flowchart LR
  A[Linux 文件系统调用] --> B[FUSE 适配器]
  C[CLI / Rust API] --> D[FileSystem]
  B --> D
  D --> E[共享 inode 暂存文件]
  E --> F[Gearhash CDC]
  C --> F
  F --> G[Xet keyed BLAKE3 与去重索引]
  G --> H[不可变 Xorb 对象]
  G --> I[文件配方]
  D --> J[SQLite: inode / entries / snapshots]
  I --> J
  H --> K[范围解码与块校验]
  I --> K
  K --> D
```

## 模块

| 模块 | 责任 |
| --- | --- |
| `chunker.rs` | `Read` 输入的流式 CDC，边界独立于读缓冲区大小 |
| `hash.rs` | Xet 块、Xorb、文件和 term verification 哈希；API 十六进制字序 |
| `xorb.rs` | v0 块头、三种压缩类型、解码大小限制 |
| `repository.rs` | CAS 对象、块索引、不可变文件配方、事务、快照、GC、fsck |
| `filesystem.rs` | 目录与 inode 语义、共享写暂存、句柄和 lookup 引用生命周期 |
| `fuse.rs` | Linux FUSE 回调、属性映射、稳定目录句柄与内核权限选项 |
| `main.rs` | 管理、导入导出、范围读取、哈希和挂载命令 |

## 算法对应关系

CDC 使用 [Xet chunking](https://huggingface.co/docs/xet/en/chunking)的固定 Gear 表和 mask `0xffff000000000000`。每个 chunk 重新开始 rolling hash；最后一个块允许短于最小值。标量实现优先保证跨平台一致性。

[哈希](https://huggingface.co/docs/xet/en/hashing)使用协议中的固定 data/internal/verification keys。Merkle 分组合并遵循固定版本参考代码的内容相关切点，文件根再使用全零 key 计算 keyed BLAKE3。空文件采用参考实现的零哈希。外部字符串按四个 little-endian u64 输出，区别于直接把 32 字节逐字节 hex 编码。

Xorb 使用 [v0 块记录格式](https://huggingface.co/docs/xet/en/xorb)：版本 1 字节、压缩长度 3 字节、压缩类型 1 字节、原始长度 3 字节，长度均 little-endian。每块尝试 raw、LZ4 frame、BG4 + LZ4 并选择最小结果。序列化对象总长限制为 64 MiB。

本地对象是无 footer 的块流。`inspect-xorb` 还接受官方固定样本实际包含的 `XETB` 末尾标记，且要求标记后立即 EOF；不把该观察扩展为对所有官方对象 footer 的支持。官方 shard 格式及服务端协议不在 0.1 的兼容范围内。

## 仓库格式

```text
repo/
  LOCK                    进程生命周期的排他 advisory lock
  metadata.sqlite         schema user_version = 1
  metadata.sqlite-wal     SQLite 自行管理
  metadata.sqlite-shm     SQLite 自行管理
  objects/<xet-hash>.xorb  已提交或待回收的不可变数据对象
  tmp/                    暂存文件与待发布对象
```

`schema.sql` 定义格式。`xorbs` 和 `xorb_chunks` 记录物理对象的完整目录；`chunks` 把唯一块哈希映射到一个物理位置。普通 GC 可删除块索引而保留部分使用的对象，完整物理目录仍允许 fsck 校验对象中所有记录。

`contents` 保存不可变版本的 Xet 文件哈希和长度。`file_chunks` 保存顺序、逻辑偏移、长度与块哈希。配方独立于块的当前物理位置，压实只调整块索引。`reconstruction`/`manifest` 将连续的物理块合并成 `(xorb, [chunk_start, chunk_end))` terms，附带逻辑偏移、长度和 verification hash。

`inodes` 保存 inode 属性及内容配方 ID；`entries` 使用 `(parent_inode, raw_name_bytes)` 作为键。同一 inode 可有多个文件目录项；目录不允许硬链接。根 inode 为 1，其余 inode 使用 SQLite AUTOINCREMENT。

快照复制 inode 和目录项元数据，共享不可变配方。它不是独立的数据备份，底层对象仍属于同一仓库。

## 写入和持久化

导入和 fsync 的顺序为：

1. CDC 分块并查询仓库级去重索引。
2. 仅将未知块编码进新 Xorb。
3. 写临时对象并 `sync_all`，以 no-clobber 方式发布，再 fsync 对象目录。
4. 提交块索引与不可变配方的 SQLite 事务。
5. 提交 inode 对新配方的引用、长度及时间戳。

旧 inode 配方在新内容完成提交之前始终有效。导入读错误或未完成的事务不会让旧文件指向半个新对象。

| 中断位置 | 恢复结果 |
| --- | --- |
| 对象仍在 tmp | 下次独占打开仓库时清理暂存 |
| 对象已发布、索引事务未提交 | 对象成为孤儿，GC 清理 |
| 新配方已提交、inode 尚未切换 | 旧文件可读，新配方等候 GC |
| inode 事务已提交 | 新文件配方完整且对象已持久化 |

FUSE 的同一 inode 共享一个磁盘暂存文件。任意句柄读写都观察这一份内容；append 在串行回调中选择当前长度。flush/fsync 提交暂存内容；最后 release 也尝试提交。`O_SYNC`/`O_DSYNC` 写入立即提交。进程被强制终止时未提交的暂存写入可能丢失，已成功 fsync 的内容保存在 CAS 和 SQLite 中。

打开后删除的文件保留 inode，直到文件句柄和内核 lookup 引用均释放。恢复启动会删除无目录项的遗留 inode。重命名和覆盖在单个 namespace 事务中完成，打开的旧目标句柄仍可访问旧 inode。

## 读取、维护与代价

范围读取先通过 `(content, offset)` 索引找到起始块，再遍历交叠的块；仅读取相应的 Xorb 记录。每块解压后检查长度与 keyed BLAKE3 哈希，再返回请求范围。对不可变文件的读取不会把整个 Xorb 或整个文件加载入内存。

GC 的根包括当前 inode 和快照 inode。删除配方与块索引后，完全空闲的 Xorb 可移除；压实将所有存活块写入新对象并提交新位置，最后删除不再引用的旧对象。任何中断只留下可回收对象。执行 GC 时不能持有打开的文件句柄。

`fsck` 检查 SQLite 完整性、外键、物理 Xorb 内容与哈希、块索引、文件配方偏移及根哈希、inode 长度与主要 namespace/snapshot 约束。它检测问题并返回错误，不自动修复数据。

数据缓冲上界为一个至多 64 MiB 的编码对象及少量块；导入的配方和已见块集合占用 O(块数) 内存。当前随机写入的提交仍需扫描整个暂存文件，且 FUSE 使用直接 I/O 和串行回调。后续性能方向包括脏区附近 CDC 重同步、块缓存、批量小文件打包与并行编码；这些尚未实现。
