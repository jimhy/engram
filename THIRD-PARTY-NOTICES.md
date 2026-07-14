# 第三方组件声明（THIRD-PARTY NOTICES）

本项目（engram 插件，Apache-2.0，见 [LICENSE](./LICENSE)）构建与运行时使用了下列第三方组件。
许可证信息逐个核对自本地依赖源码（cargo registry 中各 crate `Cargo.toml` 的 `license` 字段），
版本号取自 `engine/Cargo.lock` 与 `kb/Cargo.lock` 的当前锁定版本。

## engine（记忆引擎，`engine/Cargo.toml` 直接依赖）

| 组件 | 版本 | 用途 | 许可证 |
|------|------|------|--------|
| redb | 2.6.3 | 嵌入式 KV 存储（记忆库文件 `.redb`） | MIT OR Apache-2.0 |
| serde | 1.0.228 | 序列化/反序列化框架 | MIT OR Apache-2.0 |
| serde_json | 1.0.150 | JSON 编解码（CLI 输入输出、导入导出） | MIT OR Apache-2.0 |
| clap | 4.6.1 | 命令行参数解析 | MIT OR Apache-2.0 |

## kb（知识库 sidecar，`kb/Cargo.toml` 直接依赖）

| 组件 | 版本 | 用途 | 许可证 |
|------|------|------|--------|
| lancedb | 0.30.0 | 本地向量数据库（混合检索） | Apache-2.0 |
| arrow-array | 58.3.0 | Arrow 列式内存数组（与 lancedb 交换数据） | Apache-2.0 AND MIT |
| arrow-schema | 58.3.0 | Arrow schema 定义 | Apache-2.0 |
| fastembed | 5.16.0 | 嵌入推理封装（驱动 ONNX Runtime 跑 bge 模型） | Apache-2.0 |
| text-splitter | 0.31.0 | markdown 语义分块 | MIT |
| tokenizers | 0.23.1（另经 fastembed 传递引入 0.22.2） | HF 分词器（token 计数与 bge 模型对齐） | Apache-2.0 |
| pulldown-cmark | 0.13.4 | markdown 标题树解析（拼标题面包屑） | MIT |
| serde | 1.0.228 | 序列化/反序列化框架 | MIT OR Apache-2.0 |
| serde_json | 1.0.150 | JSON 编解码 | MIT OR Apache-2.0 |
| clap | 4.6.1 | 命令行参数解析 | MIT OR Apache-2.0 |
| tokio | 1.52.3 | 异步运行时（lancedb 需要） | MIT |
| futures | 0.3.32 | 异步组合子 | MIT OR Apache-2.0 |
| sha2 | 0.10.9 | 文档内容 SHA-256（增量入库判断） | MIT OR Apache-2.0 |
| tempfile | 3.27.0 | 测试用临时目录（仅 dev-dependency，不进发布产物） | MIT OR Apache-2.0 |

## 运行时组件（非 crate 依赖，按需下载）

| 组件 | 用途 | 许可证 |
|------|------|--------|
| ONNX Runtime（Microsoft） | 嵌入模型推理引擎；经 ort crate（2.0.0-rc.12，MIT OR Apache-2.0）在构建时下载并静态链接 | MIT |
| bge-small-zh-v1.5（BAAI） | 中文语义嵌入模型；首次使用知识库时从 Hugging Face 按需下载 | MIT（以 Hugging Face 模型卡标注为准） |

> 说明：以上均为直接依赖与显式引入的运行时组件；各 crate 的传递依赖遵循其各自
> 许可证，完整清单可用 `cargo license` / `cargo tree` 在 `engine/`、`kb/` 目录下生成。
