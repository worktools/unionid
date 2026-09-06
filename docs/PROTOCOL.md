# 版本化接口与参数

unionid 的稳定网络边界是 JSON Lines 协议 version 1：每个请求和响应各占一个物理行。<code>query</code> 是 JSON string，因此源码中的换行、缩进、引号和管道符都作为数据传输，不参与协议分帧。服务仍暂时接受旧的 <code>{"query":"..."}</code> 和纯文本单行请求，新的客户端应使用本页协议。

## 请求

~~~json
{"version":1,"request_id":"task-42","query":"from tasks\nfilter id == $id\nselect {id, title}","params":{"id":{"type":"int","value":"9007199254740993"}}}
~~~

| 字段 | 规则 |
| --- | --- |
| <code>version</code> | 当前只能是 1；其他值返回 <code>E_PROTOCOL_VERSION</code> |
| <code>request_id</code> | 客户端提供的 string，响应原样返回；它只用于关联请求，不提供去重或 exactly-once |
| <code>query</code> | 完整 unionid 源码，最多 1 MiB |
| <code>params</code> | 可省略的命名 typed value；源码以 <code>$name</code> 引用 |
| <code>schema</code> | 可省略的 <code>{revision, hash}</code>；不等于当前 schema 时，在解析或扫描前返回 <code>E_SCHEMA_CHANGED</code> |

参数名使用与标识符相同的 ASCII 规则，以字母或下划线开头。缺少参数返回 <code>E_PARAM_MISSING</code>，多余参数返回 <code>E_PARAM_EXTRA</code>，wire value 无法解码返回 <code>E_PARAM_TYPE</code>；参数解码后仍由查询上下文做普通类型检查，所以类型不匹配返回 <code>E_TYPE</code>。绑定发生在 AST 上，不通过文本替换，文本参数中的引号、换行、注释符或 pipeline 符号不会改变查询结构。

参数可用于 filter、match condition、derive 算术表达式和 update <code>set</code>。完整 insert/upsert row 使用 <code>insert tasks $row</code> / <code>upsert tasks $row</code>。一个带参数的多语句请求仍是同一个原子批次。过渡 WAL 不能安全重放绑定后的写 AST，因此参数化写入只支持 memory/redb；redb 是正式持久入口。

## 无损值编码

所有协议值都有显式 <code>type</code>。i64、稳定 type ID 和 variant ID 使用十进制 string，避免 JavaScript number 的 53-bit 精度限制。有限 f64 也使用 string，使解码不依赖 JSON number 实现。null、option 的 None、空 list 和 unit variant 具有不同结构。

~~~json
{"type":"int","value":"-9223372036854775808"}
{"type":"float","value":"1.25"}
{"type":"text","value":"hello"}
{"type":"bool","value":true}
{"type":"null"}
{"type":"option","value":null}
{"type":"option","value":{"type":"text","value":"present"}}
{"type":"list","items":[]}
{"type":"tuple","items":[{"type":"int","value":"1"},{"type":"text","value":"x"}]}
{"type":"record","fields":{"id":{"type":"int","value":"1"}}}
{"type":"variant","name":"Running","variant_id":"17","args":[]}
{"type":"named","type_id":"9","value":{"type":"variant","name":"Pending","variant_id":"16","args":[]}}
~~~

客户端构造上下文明确的 ADT 参数时可以将 <code>type_id</code> / <code>variant_id</code> 设为 "0"，由目标字段类型解析名称。查询结果总是返回 catalog 中的稳定 ID，因此不同命名类型下的同名 constructor 不会混淆。普通 JSON 的 number/null/array/object 没有足够信息表达这些区别，不作为 version 1 typed value 的替代格式。

## 响应

~~~json
{
  "version": 1,
  "request_id": "task-42",
  "ok": true,
  "message": "1 row(s)",
  "columns": [{"name":"id","ty":"int"},{"name":"title","ty":"text"}],
  "rows": [{"id":{"type":"int","value":"9007199254740993"},"title":{"type":"text","value":"hello"}}],
  "schema": {"revision":2,"hash":"..."}
}
~~~

<code>columns</code> 决定展示和读取顺序，row object 只承载按名称访问的值。`explain` 响应额外包含 <code>plan</code>：源表、`full_scan`／`primary_key_lookup`／`secondary_index_lookup`、可选索引与 lookup 条件、候选行数、源码顺序 stage 和最终结果 schema；它不执行数据行。失败响应的 <code>error</code> 包含固定 <code>code</code>、可读 <code>message</code> 和可选源码 <code>span</code>。DML 使用 <code>affected_rows</code>，upsert 另有 <code>upsert_action</code>；warnings 不改变 <code>ok</code>。连接在响应前断开时，客户端不能依据断线判断写入是否提交，也不能把相同 <code>request_id</code> 当作服务端幂等键。

## Rust 嵌入接口

<code>Engine::memory()</code> 和 <code>Engine::open_redb(path)</code> 创建数据库；<code>execute</code> 执行无参数原子脚本，<code>execute_with_params</code> 在 AST 上绑定参数。<code>prepare</code> 接受只读 pipeline 或 explain 并记录当前 schema revision/hash，<code>query</code> 或 <code>execute_prepared</code> 执行时若 schema 已变化会返回 <code>E_SCHEMA_CHANGED</code>，调用方可重新 prepare。Rust 调用方可直接读取 <code>QueryPlan</code>、<code>QueryAccessPlan</code> 和对应 enum。migration 继续通过 <code>plan_migrations</code>、<code>apply_migrations</code> 和 <code>migration_status</code> 进入同一个 Engine 提交边界。

可运行示例：

~~~bash
cargo run --example parameters
~~~

TCP 客户端可直接构造 <code>ProtocolRequest</code> 并调用 <code>cli::send_request</code>。<code>WireValue</code> 与 <code>Value</code> 之间提供无损转换；网络 codec、redb 的版本化 binary value codec 和内部 Rust enum 布局彼此独立。连接、执行与响应限制以及优雅关闭行为见[服务运行边界](SERVICE.md)。
