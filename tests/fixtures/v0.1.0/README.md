# v0.1.0 持久数据夹具 / v0.1.0 durable fixture

## 中文说明

这个夹具由公开的 unionid v0.1.0 macOS ARM64 原生产物创建，用于验证后续版本读取和升级真实发布格式，而不只测试手工构造的 meta 表。

- Release：<https://github.com/worktools/unionid/releases/tag/v0.1.0>
- Archive：`unionid-v0.1.0-aarch64-apple-darwin.tar.gz`
- GitHub 与 sidecar SHA-256：`c2a34a9615407f0bae53e088991283de61eba9ec8e71ccacdd69e18bf808ec1c`
- 生成器：解包后的 `unionid 0.1.0`
- `database.redb` SHA-256：`db8cdceec848b977cffea00f12077cce2f97327abf63b01d5eed3910e4979e9d`
- `backup.json` SHA-256：`eb0091c3c4e2d5a33faf1d874f3324e3c1cb0ba7a9a6702e0af7ab9d11df49f5`
- Schema：revision 1，`sha256:2f7b412f82bdbff4222841fb38f8560b9130a99d54a239581efe703b894ac6a7`
- Migration：`m0001_fixture`，checksum `sha256:5aad0f44ef589706244cf04ec37cebcda0647cb258dced590a76345d9e27ee3f`

生成步骤是先对空库应用 `migrations/0001_fixture.uid`，再执行 `rows.uid`，然后由旧二进制运行 `check` 和 `backup`。库包含三个带稳定主键的 Task rows、命名 record/sum、嵌套 option/list，以及主键和 `owner.email` 索引。旧可执行文件不进入仓库。

`database.redb` 和 `backup.json` 是生成证据，在 `.gitattributes` 中标记为 generated 且不展开文本 diff。测试每次复制数据库后再升级，绝不修改源夹具。

## English Description

This fixture was created with the public unionid v0.1.0 macOS ARM64 native artifact. It verifies that later versions read and upgrade the real released format rather than only hand-crafted legacy metadata.

- Release: <https://github.com/worktools/unionid/releases/tag/v0.1.0>
- Archive: `unionid-v0.1.0-aarch64-apple-darwin.tar.gz`
- GitHub and sidecar SHA-256: `c2a34a9615407f0bae53e088991283de61eba9ec8e71ccacdd69e18bf808ec1c`
- Generator: the extracted `unionid 0.1.0`
- `database.redb` SHA-256: `db8cdceec848b977cffea00f12077cce2f97327abf63b01d5eed3910e4979e9d`
- `backup.json` SHA-256: `eb0091c3c4e2d5a33faf1d874f3324e3c1cb0ba7a9a6702e0af7ab9d11df49f5`
- Schema: revision 1, `sha256:2f7b412f82bdbff4222841fb38f8560b9130a99d54a239581efe703b894ac6a7`
- Migration: `m0001_fixture`, checksum `sha256:5aad0f44ef589706244cf04ec37cebcda0647cb258dced590a76345d9e27ee3f`

Generation applied `migrations/0001_fixture.uid` to an empty database, executed `rows.uid`, and then ran `check` and `backup` with the old binary. The database contains three stable-primary-key Task rows, named record/sum values, nested option/list values, and primary plus `owner.email` indexes. The old executable is not committed.

`database.redb` and `backup.json` are generated evidence, marked generated and non-diffable in `.gitattributes`. Tests always copy the database before upgrading and never modify the source fixture.
