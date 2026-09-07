# 服务运行边界

unionid server 面向本机受信应用：默认监听 <code>127.0.0.1:7878</code>，使用自己的 JSON Lines 协议，不兼容 Redis wire protocol。当前不提供用户认证或 TLS；若要跨主机暴露，应由受控网络或具备认证和 TLS 的代理保护。

## 有界资源

| 资源 | 当前边界 | 超限行为 |
| --- | --- | --- |
| 活动 TCP 连接 | 64 | 新连接收到 <code>E_BUSY</code> 后关闭；拒绝处理不创建 worker |
| 请求 frame | 6 × 1 MiB + 256 bytes | 返回 <code>E_LIMIT</code> 并关闭该连接；该空间容纳 1 MiB 源码最坏 JSON 转义 |
| 查询源码 | 1 MiB / 100,000 tokens / 64 层 | 返回 <code>E_LIMIT</code> 或带位置的语法错误 |
| ADT value | 16 MiB encoded / 64 层 / 1,000,000 collection items | codec、恢复或写入拒绝超限值 |
| 查询 working rows | 250,000 | 返回 <code>E_LIMIT</code>；应增加选择性 indexed filter |
| 查询结果 rows | 100,000 | 返回 <code>E_LIMIT</code>；应增加 filter 或 take |
| Introspection payload | 1 MiB | version 1 请求返回带 request ID 与 schema 的 <code>E_LIMIT</code> |
| TCP response | 16 MiB | 丢弃超限结果，发送小型结构化 <code>E_LIMIT</code> |
| 服务执行 deadline | 25 秒 | 返回 <code>E_TIMEOUT</code>；候选写批次不提交 |
| 空闲连接 / socket write | 30 秒 | 关闭空闲或不读取响应的客户端 |
| match coverage | 100,000 analysis steps | 返回 <code>E_LIMIT</code>，要求简化嵌套 pattern |

查询、写批次与 migration 使用同一个 Engine mutex，最多只有一个请求进入 Engine；其他已接纳连接形成至多 64 个等待者，因此不会产生无界线程或请求队列。读写只观察完整的 Engine 提交。查询扫描和 aggregate 输出定期检查 deadline；其他批次至少在每条语句前后检查，若计算期间越过 deadline，候选状态会被丢弃而不发布。排序受 working-row 上限约束；group/aggregate 另有限制 group 数、accumulator cell 和估算状态内存，查询局部函数限制定义数、调用深度和展开步骤，具体数值见 [QUERY.md](QUERY.md)。`explain` 只绑定查询并读取表／索引元数据和目标 posting，不扫描或复制数据行。

TCP response 使用限长 writer 直接编码，不先创建一个无界 JSON byte buffer。版本化响应超限时仍回显 request ID 与 schema；旧协议得到旧格式的 <code>E_LIMIT</code>。

## 关闭与失败

<code>SIGINT</code> 或 <code>SIGTERM</code> 触发优雅关闭：

1. listener 停止接纳新连接；
2. 正在读取但尚未提交请求的空闲连接被关闭；
3. 已进入 Engine 的请求完成，或因 deadline/error 丢弃候选事务；
4. 所有 worker 退出后释放 Engine 和 redb 独占锁；
5. stderr 输出 accepted/rejected/requests/failed 计数。

优雅关闭完成后可以立即以同一路径重新打开 redb。强制终止、掉电和 commit 结果不确定的恢复边界见 [存储说明](STORAGE.md)；正常 signal 测试不替代那些故障测试。

客户端断开不会回滚一个已经提交或正在提交的请求。request ID 只关联请求与响应，不是幂等键；在未收到响应时不能推断提交结果，也不能盲目把写入当作 exactly-once 重试。需要安全重试时使用应用主键、upsert 或业务幂等标识。

## 嵌入式控制

应用可以调用 <code>server::serve_until(listener, engine, shutdown)</code>，通过共享 <code>AtomicBool</code> 发起同样的关闭流程，并在返回时获得 <code>ServerStats</code>。命令行 <code>server</code> 已把 SIGINT/SIGTERM 连接到这个入口。

服务限制是 v0.1 的明确支持边界，而非容量承诺。1 万/10 万行实际负载、恢复和 migration 数据由 #24 的发布基准记录。
