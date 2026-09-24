# 本地存储 M0 验证

隔离的 Rust 工程，直接调用 `sqlx`、`lancedb`、`kuzu`，不连接数据库服务、不进入应用运行路径。候选版本与完整依赖分别固定在 Cargo.toml 和 Cargo.lock；这些版本尚不等于应用已支持的后端。

Kuzu 0.11.3 固定 `cxx = 1.0.138`，但其 `cxx-build` 范围过宽。本工程同时约束生成器为 1.0.138：1.0.202 生成的 `cxxbridge1$202$...` 符号与旧运行库不匹配，已在 Windows 最终链接阶段复现失败。不能只用 `cargo check` 判断此组合可发布。

Kuzu 保留默认扩展特性。在本机关闭默认特性时，CMake 仍构建默认扩展加载器，但 Rust 不链接对应扩展，最终出现 algo/fts/json/vector 的 4 个未解析符号。LanceDB 则关闭默认云端特性，使用本地文件接口。

## Windows 构建

需要 Rust、MSVC C++ Build Tools、CMake 和 Ninja。`build.ps1` 优先使用 PATH 中的工具，否则从 Visual Studio 安装目录定位 CMake/Ninja。Lance 所需的 protoc 由可选构建辅助 crate 提供，不要求另行安装，也不是运行时服务。

在仓库根目录执行：

```powershell
cargo fetch --manifest-path tools/storage-probe/Cargo.toml --locked --target x86_64-pc-windows-msvc
./tools/storage-probe/build.ps1
python tools/storage-probe/verify.py
```

最低 Rust 版本须先安装 `1.88.0` 工具链，再单独构建和运行：

```powershell
./tools/storage-probe/build.ps1 -Toolchain 1.88.0 -TargetDirectory target/storage-probe-msrv -Jobs 8
python tools/storage-probe/verify.py --binary target/storage-probe-msrv/debug/opencontext-storage-probe.exe
```

若 Windows 默认执行策略禁止 `.ps1`，可仅对本次构建进程使用 `powershell -NoProfile -ExecutionPolicy Bypass -File tools/storage-probe/build.ps1`，在后面追加上面的工具链参数；不需要修改系统执行策略。

只检查某个后端可加 `--backend sqlite`、`--backend lancedb` 或 `--backend kuzu`。检查结果和临时数据库保留在 `target/storage-probe/runs/run-*/`，不访问应用数据库。

## 检查范围

- 重复初始化、读写、关闭后重新打开、按 tenant/workspace 删除，以及同 ID 跨 scope（含引号字符串）。
- LanceDB 按 scope 预过滤的精确向量 top-1 查询，长连接读取其他进程提交的数据，两个写进程的并发追加。
- SQLite WAL 下读写并发、写锁竞争、杀死未提交事务后的恢复。
- Kuzu 实体/边及删除、读写进程的文件锁、两个只读进程、杀死未提交事务后的恢复。
- Kuzu 同进程共享 Database，在写事务未提交时由另一线程的连接读取，再验证提交后的值。
- 三种后端在已确认写入后被强制杀死，重新打开验证数据。
- Kuzu 若只允许一个读写进程，用 JSON-lines 向持有嵌入式库的进程读写；这只是验证访问边界的测试驱动，不是生产存储服务或 RPC 实现。

实际通过项、构建失败和未验证项以 [验证记录](../../docs/VALIDATION.md) 与运行生成的 report.json 为准。脚本失败即返回非零状态；不会把预期支持当作已验证。

## 范围限制

探针只有最小表结构；重复初始化不等于生产结构完整性检查。LanceDB 的 `put` 是追加，用唯一测试 ID 验证并发，尚未实现业务幂等 upsert。向量测试未创建 ANN 索引；SQLite 的 `search` 只是 scoped 查询，未验证 FTS。没有实现生产 scope 授权、来源有效性检查、跨库账本、队列或 Worker OS 文件锁。这些属于 M1–M4，不能用本探针替代验收。

Rust 1.88、其他 OS 和 release 构建须单独记录实测结果；Cargo 的版本解析成功不能代替最低 Rust 版本编译。
