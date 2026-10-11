//! Per-language adapter data modules, plus the lookup surface.
//!
//! Every module here is data only: `&'static` slices and real enums, no
//! engine imports. The grammar layer validates each adapter's declared
//! kinds and fields against its linked grammar at initialization, so
//! adapter/grammar drift fails loudly at startup.

pub mod c;
pub mod cpp;
pub mod global;
pub mod go;
pub mod java;
pub mod javascript;
pub mod js_family;
pub mod python;
pub mod rows;
pub mod rust;
pub mod tsx;
pub mod typescript;

use crate::lang::adapter::LanguageAdapter;
use crate::lang::ids::LanguageId;

pub use global::GLOBAL_DIRECTIVES;

/// Every adapter, in [`LanguageId::ALL`](crate::lang::ids::LanguageId::ALL)
/// order (pinned by a unit test).
pub static ALL_ADAPTERS: &[LanguageAdapter] = &[
    typescript::ADAPTER,
    tsx::ADAPTER,
    javascript::ADAPTER,
    python::ADAPTER,
    go::ADAPTER,
    java::ADAPTER,
    c::ADAPTER,
    cpp::ADAPTER,
    rust::ADAPTER,
];

/// The adapter for a language.
pub fn adapter_for(language: LanguageId) -> &'static LanguageAdapter {
    &ALL_ADAPTERS[language.index()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_has_an_adapter_and_vice_versa() {
        for id in LanguageId::ALL {
            assert_eq!(
                adapter_for(id).id,
                id,
                "adapter_for({}) is {}",
                id.as_str(),
                id.as_str(),
            );
        }
    }

    #[test]
    fn all_adapters_are_in_language_order() {
        assert_eq!(ALL_ADAPTERS.len(), LanguageId::ALL.len());
        for (position, adapter) in ALL_ADAPTERS.iter().enumerate() {
            assert_eq!(
                adapter.id.index(),
                position,
                "ALL_ADAPTERS order must match LanguageId::ALL",
            );
        }
    }

    #[test]
    fn comment_syntax_data_is_sane_for_every_adapter() {
        for adapter in ALL_ADAPTERS {
            assert!(
                !adapter.comment_node_kinds.is_empty(),
                "{}: comment node kinds",
                adapter.id.as_str(),
            );
            assert!(
                !adapter.line_prefixes.is_empty(),
                "{}: line prefixes",
                adapter.id.as_str(),
            );
            // Every language except Python has block comments; Python has
            // none and must say so.
            if adapter.id == LanguageId::Python {
                assert!(adapter.block_delimiters.is_none());
            } else {
                assert!(adapter.block_delimiters.is_some());
            }
            assert_eq!(adapter.block_comments_nest, adapter.id == LanguageId::Rust,);
        }
    }

    #[test]
    fn doc_policies_match_the_locked_decisions() {
        let policies = [
            (
                LanguageId::Typescript,
                Some(crate::lang::adapter::DocPolicy::Prose),
            ),
            (
                LanguageId::Tsx,
                Some(crate::lang::adapter::DocPolicy::Prose),
            ),
            (
                LanguageId::Javascript,
                Some(crate::lang::adapter::DocPolicy::ExportProtected),
            ),
            (
                LanguageId::Java,
                Some(crate::lang::adapter::DocPolicy::AlwaysProtected),
            ),
            (
                LanguageId::Rust,
                Some(crate::lang::adapter::DocPolicy::AlwaysProtected),
            ),
            (LanguageId::Python, None),
            (LanguageId::Go, None),
            (LanguageId::C, None),
            (LanguageId::Cpp, None),
        ];
        for (id, expected) in policies {
            let adapter = adapter_for(id);
            assert_eq!(
                adapter.doc.as_ref().map(|doc| doc.policy),
                expected,
                "{} doc policy",
                id.as_str(),
            );
        }
        // Exported rules exist only where the policy is ExportProtected.
        for adapter in ALL_ADAPTERS {
            let protected = adapter
                .doc
                .as_ref()
                .is_some_and(|doc| doc.policy == crate::lang::adapter::DocPolicy::ExportProtected);
            assert_eq!(
                adapter.exported_rule.is_some(),
                protected,
                "{}: exported rule only under ExportProtected",
                adapter.id.as_str(),
            );
        }
    }

    #[test]
    fn only_javascript_defines_an_exported_rule() {
        for adapter in ALL_ADAPTERS {
            if adapter.id == LanguageId::Javascript {
                assert_eq!(
                    adapter.exported_rule,
                    Some(crate::lang::adapter::ExportedRule::JsModule),
                );
            } else {
                assert!(adapter.exported_rule.is_none());
            }
        }
    }

    #[test]
    fn anchor_wrappers_match_the_pinned_shapes() {
        use crate::lang::adapter::AnchorWrapper;
        let wrappers = [
            (
                LanguageId::Javascript,
                vec![AnchorWrapper::Field {
                    kind: "export_statement",
                    field: "declaration",
                }],
            ),
            (
                LanguageId::Typescript,
                vec![AnchorWrapper::Field {
                    kind: "export_statement",
                    field: "declaration",
                }],
            ),
            (
                LanguageId::Tsx,
                vec![AnchorWrapper::Field {
                    kind: "export_statement",
                    field: "declaration",
                }],
            ),
            (
                LanguageId::Python,
                vec![AnchorWrapper::Field {
                    kind: "decorated_definition",
                    field: "definition",
                }],
            ),
            (
                LanguageId::Rust,
                vec![AnchorWrapper::PrefixSibling {
                    kind: "attribute_item",
                }],
            ),
            (LanguageId::Go, vec![]),
            (LanguageId::Java, vec![]),
            (LanguageId::C, vec![]),
            (LanguageId::Cpp, vec![]),
        ];
        for (id, expected) in wrappers {
            let adapter = adapter_for(id);
            assert_eq!(
                adapter.anchor_wrappers.to_vec(),
                expected,
                "{} anchor wrappers",
                id.as_str(),
            );
        }
    }

    #[test]
    fn closing_tokens_are_pinned_per_language() {
        for adapter in ALL_ADAPTERS {
            if adapter.id == LanguageId::Python {
                assert!(adapter.closing_tokens.is_empty());
            } else {
                assert_eq!(adapter.closing_tokens, &["}", ")", "]"]);
            }
        }
    }
}
