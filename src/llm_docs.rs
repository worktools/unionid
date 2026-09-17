use serde::Serialize;

pub const LLM_QUERY_DOCS_VERSION: u32 = 1;
pub const QUERY_LANGUAGE_VERSION: &str = "0.7";

const QUERY_REFERENCE: &str = include_str!("../docs/LLM_QUERY.md");

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
}
