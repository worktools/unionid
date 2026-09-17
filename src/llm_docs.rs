use serde::Serialize;

pub const LLM_QUERY_DOCS_VERSION: u32 = 1;
pub const QUERY_LANGUAGE_VERSION: &str = "0.7";

const QUERY_REFERENCE: &str = include_str!("../docs/LLM_QUERY.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DocsCategory {
    Learn,
    Language,
    Application,
    Lifecycle,
    Integration,
    Operations,
}

impl DocsCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Learn => "learn",
            Self::Language => "language",
            Self::Application => "application",
            Self::Lifecycle => "lifecycle",
            Self::Integration => "integration",
            Self::Operations => "operations",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct BundledDocsTopic {
    pub name: &'static str,
    pub category: DocsCategory,
    pub title: &'static str,
    pub summary: &'static str,
    #[serde(skip)]
    content: &'static str,
}

#[derive(Debug, Serialize)]
pub struct BundledDocsCatalog {
    pub schema_version: u32,
    pub software_version: &'static str,
    pub categories: &'static [&'static str],
    pub topics: Vec<BundledDocsTopic>,
}

#[derive(Debug, Serialize)]
pub struct BundledDocument {
    pub schema_version: u32,
    pub software_version: &'static str,
    pub topic: &'static str,
    pub category: DocsCategory,
    pub title: &'static str,
    pub content: &'static str,
}

pub const DOCS_CATEGORIES: &[&str] = &[
    "learn",
    "language",
    "application",
    "lifecycle",
    "integration",
    "operations",
];

const TOPICS: &[BundledDocsTopic] = &[
    BundledDocsTopic {
        name: "getting-started",
        category: DocsCategory::Learn,
        title: "Getting started / 入门",
        summary: "Install Unionid and complete the first local ADT database workflow.",
        content: include_str!("../docs/GETTING_STARTED.md"),
    },
    BundledDocsTopic {
        name: "cli",
        category: DocsCategory::Learn,
        title: "CLI and REPL / 命令行与交互环境",
        summary: "Use local, remote, scripted, interactive, and diagnostic CLI flows.",
        content: include_str!("../docs/CLI.md"),
    },
    BundledDocsTopic {
        name: "language",
        category: DocsCategory::Language,
        title: "Language overview / 语言概览",
        summary: "Read the executable, Rust-shaped, semicolon-free language surface.",
        content: include_str!("../docs/LANGUAGE.md"),
    },
    BundledDocsTopic {
        name: "query",
        category: DocsCategory::Language,
        title: "Query language / 查询语言",
        summary: "Use pipelines, expressions, ADT matching, aggregation, and bounded reads.",
        content: include_str!("../docs/QUERY.md"),
    },
    BundledDocsTopic {
        name: "llm-query",
        category: DocsCategory::Language,
        title: "LLM query context / LLM 查询上下文",
        summary: "Provide compact query-generation rules; use `docs query` for examples.",
        content: QUERY_REFERENCE,
    },
    BundledDocsTopic {
        name: "schema",
        category: DocsCategory::Language,
        title: "Schema and evolution / Schema 与演进",
        summary: "Define ADTs, tables, stable identities, defaults, and evolution contracts.",
        content: include_str!("../docs/SCHEMA.md"),
    },
    BundledDocsTopic {
        name: "scalars",
        category: DocsCategory::Language,
        title: "Production scalars / 生产标量",
        summary: "Use decimal, temporal, UUID, bytes, and related typed values.",
        content: include_str!("../docs/SCALARS.md"),
    },
    BundledDocsTopic {
        name: "application-data",
        category: DocsCategory::Application,
        title: "Application ADTs / 应用数据类型",
        summary: "Move structs, enums, options, tuples, and lists across application boundaries.",
        content: include_str!("../docs/APPLICATION_DATA.md"),
    },
    BundledDocsTopic {
        name: "migrations",
        category: DocsCategory::Lifecycle,
        title: "Migrations / 数据迁移",
        summary: "Plan, apply, resume, inspect, and validate schema migrations.",
        content: include_str!("../docs/MIGRATIONS.md"),
    },
    BundledDocsTopic {
        name: "upgrading",
        category: DocsCategory::Lifecycle,
        title: "Upgrading / 版本升级",
        summary: "Upgrade binaries, storage formats, codecs, and source compatibility safely.",
        content: include_str!("../docs/UPGRADING.md"),
    },
    BundledDocsTopic {
        name: "service",
        category: DocsCategory::Integration,
        title: "Service integration / 服务集成",
        summary: "Embed or serve Unionid with request, concurrency, and shutdown boundaries.",
        content: include_str!("../docs/SERVICE.md"),
    },
    BundledDocsTopic {
        name: "protocol",
        category: DocsCategory::Integration,
        title: "Wire protocol / 传输协议",
        summary: "Integrate through versioned typed request, response, and stream contracts.",
        content: include_str!("../docs/PROTOCOL.md"),
    },
    BundledDocsTopic {
        name: "http",
        category: DocsCategory::Integration,
        title: "HTTP adapter / HTTP 适配器",
        summary: "Expose bounded query and stream operations through the HTTP adapter.",
        content: include_str!("../docs/HTTP.md"),
    },
    BundledDocsTopic {
        name: "deployment",
        category: DocsCategory::Operations,
        title: "Deployment / 部署",
        summary: "Run a durable service with explicit resource and maintenance boundaries.",
        content: include_str!("../docs/DEPLOYMENT.md"),
    },
    BundledDocsTopic {
        name: "backup",
        category: DocsCategory::Operations,
        title: "Backup and restore / 备份与恢复",
        summary: "Create, verify, restore, and retain logical and incremental backups.",
        content: include_str!("../docs/BACKUP.md"),
    },
    BundledDocsTopic {
        name: "storage",
        category: DocsCategory::Operations,
        title: "Storage / 存储",
        summary: "Understand redb durability, formats, checks, compaction, and limits.",
        content: include_str!("../docs/STORAGE.md"),
    },
    BundledDocsTopic {
        name: "metrics",
        category: DocsCategory::Operations,
        title: "Metrics / 指标",
        summary: "Export bounded-cardinality process metrics without exposing data values.",
        content: include_str!("../docs/METRICS.md"),
    },
    BundledDocsTopic {
        name: "observability",
        category: DocsCategory::Operations,
        title: "Observability / 可观测性",
        summary: "Configure value-free terminal and slow-query observation safely.",
        content: include_str!("../docs/OBSERVABILITY.md"),
    },
];

pub fn docs_catalog(category: Option<DocsCategory>) -> BundledDocsCatalog {
    BundledDocsCatalog {
        schema_version: 1,
        software_version: env!("CARGO_PKG_VERSION"),
        categories: DOCS_CATEGORIES,
        topics: TOPICS
            .iter()
            .copied()
            .filter(|topic| category.is_none_or(|category| topic.category == category))
            .collect(),
    }
}

pub fn bundled_document(topic: &str) -> Option<BundledDocument> {
    TOPICS
        .iter()
        .find(|candidate| candidate.name == topic)
        .map(|topic| BundledDocument {
            schema_version: 1,
            software_version: env!("CARGO_PKG_VERSION"),
            topic: topic.name,
            category: topic.category,
            title: topic.title,
            content: topic.content,
        })
}

pub fn render_bundled_document_markdown(document: &BundledDocument) -> String {
    format!(
        "---\ndocument_schema_version: {}\nsoftware_version: {}\ntopic: {}\ncategory: {}\n---\n\n{}",
        document.schema_version,
        document.software_version,
        document.topic,
        document.category.as_str(),
        document.content.trim_start()
    )
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct LlmQueryExample {
    pub name: &'static str,
    pub description: &'static str,
    pub source: &'static str,
}

#[derive(Debug, Serialize)]
pub struct LlmQueryDocs {
    pub schema_version: u32,
    pub software_version: &'static str,
    pub language_version: &'static str,
    pub topic: &'static str,
    pub reference: &'static str,
    pub examples: &'static [LlmQueryExample],
}

const EXAMPLES: &[LlmQueryExample] = &[
    LlmQueryExample {
        name: "adt-query",
        description: "Define, write, match, derive, sort, and project an enum-backed row.",
        source: include_str!("../examples/llm/adt-query.unid"),
    },
    LlmQueryExample {
        name: "nested-values",
        description: "Query nested records, options, enum payloads, lists, and arrow closures.",
        source: include_str!("../examples/llm/nested-values.unid"),
    },
    LlmQueryExample {
        name: "mutation",
        description: "Perform a typed update with simultaneous assignments and returning.",
        source: include_str!("../examples/llm/mutation.unid"),
    },
    LlmQueryExample {
        name: "set-operations",
        description: "Combine schema-identical ADT projections with stable typed deduplication.",
        source: include_str!("../examples/llm/set-operations.unid"),
    },
];

pub fn query_docs() -> LlmQueryDocs {
    LlmQueryDocs {
        schema_version: LLM_QUERY_DOCS_VERSION,
        software_version: env!("CARGO_PKG_VERSION"),
        language_version: QUERY_LANGUAGE_VERSION,
        topic: "query",
        reference: QUERY_REFERENCE,
        examples: EXAMPLES,
    }
}

pub fn render_query_docs_markdown() -> String {
    let docs = query_docs();
    let mut output = format!(
        "---\ndocument_schema_version: {}\nsoftware_version: {}\nlanguage_version: {}\ntopic: {}\n---\n\n{}",
        docs.schema_version,
        docs.software_version,
        docs.language_version,
        docs.topic,
        docs.reference.trim_end()
    );
    output.push_str("\n\n# Runnable examples\n");
    for example in docs.examples {
        output.push_str(&format!(
            "\n## {}\n\n{}\n\n```text\n{}```\n",
            example.name, example.description, example.source
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_examples_are_canonical_and_executable() {
        for example in EXAMPLES {
            assert_eq!(
                crate::format_source(example.source).unwrap(),
                example.source,
                "{}",
                example.name
            );
            let mut engine = crate::Engine::memory();
            let response = engine.execute(example.source);
            assert!(response.ok, "{}: {}", example.name, response.message);
            assert!(!response.rows.is_empty(), "{}", example.name);
        }
    }

    #[test]
    fn markdown_bundle_has_versioned_front_matter_and_examples() {
        let markdown = render_query_docs_markdown();
        assert!(markdown.starts_with("---\ndocument_schema_version: 1\n"));
        assert!(markdown.contains("language_version: 0.7"));
        assert!(markdown.contains("## adt-query"));
        assert!(markdown.contains("Pending =>"));
        assert!(markdown.ends_with("```\n"));
    }

    #[test]
    fn bundled_docs_are_categorized_and_versioned() {
        let catalog = docs_catalog(None);
        assert_eq!(catalog.schema_version, 1);
        assert_eq!(catalog.topics.len(), 18);
        assert_eq!(catalog.topics[0].name, "getting-started");
        assert!(catalog.topics.iter().any(|topic| topic.name == "query"));
        assert!(
            catalog
                .topics
                .iter()
                .any(|topic| topic.name == "migrations")
        );
        assert!(
            catalog
                .topics
                .iter()
                .any(|topic| topic.name == "deployment")
        );

        let language = docs_catalog(Some(DocsCategory::Language));
        assert_eq!(language.topics.len(), 5);
        assert!(
            language
                .topics
                .iter()
                .all(|topic| topic.category == DocsCategory::Language)
        );

        let query = bundled_document("query").unwrap();
        assert_eq!(query.category, DocsCategory::Language);
        assert!(query.content.contains("# 查询语言"));
        let rendered = render_bundled_document_markdown(&query);
        assert!(rendered.starts_with("---\ndocument_schema_version: 1\n"));
        assert!(rendered.contains("topic: query\ncategory: language"));
        assert!(bundled_document("missing").is_none());
    }
}
