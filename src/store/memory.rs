//! Typed memory model over the schema-agnostic frontmatter codec:
//! fact / decision / rule / session-summary, per docs/FORMAT.md.
//!
//! Validation philosophy: required fields and shapes are errors (typed,
//! actionable, naming file + field + expectation); type-scoping issues
//! (`supersedes` on a non-decision, `source` on a non-session-summary) are
//! warnings — be liberal, warn, never destroy. A future memory type is one
//! enum variant plus a spec paragraph; no format migration.

use crate::error::{Error, Result, Warning};
use crate::store::frontmatter::{FmValue, FrontmatterDoc};
use crate::util::{format_rfc3339_utc, parse_rfc3339_utc};

/// The four memory types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemoryType {
    /// Something true about the world/project ("configs live in etc/").
    Fact,
    /// A choice that was made, and why. May supersede an earlier decision.
    Decision,
    /// A working rule ("always run verify.sh before commit").
    Rule,
    /// A distilled record of an agent session (created by capture).
    SessionSummary,
}

impl MemoryType {
    /// The on-disk spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryType::Fact => "fact",
            MemoryType::Decision => "decision",
            MemoryType::Rule => "rule",
            MemoryType::SessionSummary => "session-summary",
        }
    }

    /// Parse the on-disk spelling.
    pub fn parse(s: &str) -> Option<MemoryType> {
        match s {
            "fact" => Some(MemoryType::Fact),
            "decision" => Some(MemoryType::Decision),
            "rule" => Some(MemoryType::Rule),
            "session-summary" => Some(MemoryType::SessionSummary),
            _ => None,
        }
    }

    /// All four types, for iteration in fixed order.
    pub const ALL: [MemoryType; 4] = [
        MemoryType::Fact,
        MemoryType::Decision,
        MemoryType::Rule,
        MemoryType::SessionSummary,
    ];
}

/// A validated memory. The in-memory twin of one `.md` store file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    /// `<type>-<slug>-<disambiguator>`; also the filename stem. Immutable.
    pub id: String,
    /// One of the four types.
    pub mtype: MemoryType,
    /// Creation instant, epoch seconds UTC. Immutable after creation.
    pub created: i64,
    /// One-line human title.
    pub title: String,
    /// Tags, order preserved.
    pub tags: Vec<String>,
    /// Ids of related memories.
    pub links: Vec<String>,
    /// Capture provenance `<harness>:<session_id>` (session-summary).
    pub source: Option<String>,
    /// Id of the decision this one replaces (decision).
    pub supersedes: Option<String>,
    /// Human-added keys, preserved verbatim in first-seen order.
    pub unknown_keys: Vec<(String, FmValue)>,
    /// The Markdown body.
    pub body: String,
}

impl Memory {
    /// Validate a parsed document into a typed memory.
    ///
    /// Errors name `origin`, the field, and what was expected. Non-fatal
    /// issues (type-scoped fields on the wrong type, scalar where a list
    /// is conventional) come back as structured warnings.
    pub fn from_doc(doc: &FrontmatterDoc, origin: &str) -> Result<(Memory, Vec<Warning>)> {
        let mut warnings = Vec::new();
        let invalid = |message: String| Error::Invalid {
            origin: origin.to_string(),
            message,
        };
        let required_scalar = |key: &str| -> Result<String> {
            match doc.get(key) {
                Some(FmValue::Scalar(s)) if !s.trim().is_empty() => Ok(s.clone()),
                Some(FmValue::Scalar(_)) => {
                    Err(invalid(format!("field '{key}' must not be empty")))
                }
                Some(FmValue::List(_)) => Err(invalid(format!(
                    "field '{key}' must be a scalar, got a list"
                ))),
                None => Err(invalid(format!(
                    "missing required field '{key}' (required: id, type, created, title)"
                ))),
            }
        };

        let id = required_scalar("id")?;
        let type_str = required_scalar("type")?;
        let mtype = MemoryType::parse(&type_str).ok_or_else(|| {
            invalid(format!(
                "unknown type '{type_str}' (expected fact | decision | rule | session-summary)"
            ))
        })?;
        let created_str = required_scalar("created")?;
        let created = parse_rfc3339_utc(&created_str)
            .map_err(|e| invalid(format!("field 'created': {e}")))?;
        let title = required_scalar("title")?;

        // Optional list fields: accept a scalar as a one-element list with
        // a warning (hand-editability: be liberal, warn, don't destroy).
        let mut list_field = |key: &str| -> Vec<String> {
            match doc.get(key) {
                None => Vec::new(),
                Some(FmValue::List(items)) => items.clone(),
                Some(FmValue::Scalar(s)) => {
                    warnings.push(Warning {
                        origin: origin.to_string(),
                        message: format!(
                            "field '{key}' should be a list like `{key}: [a, b]`; treating scalar as a single element"
                        ),
                    });
                    vec![s.clone()]
                }
            }
        };
        let tags = list_field("tags");
        let links = list_field("links");

        let mut scalar_field = |key: &str| -> Option<String> {
            match doc.get(key) {
                None => None,
                Some(FmValue::Scalar(s)) => Some(s.clone()),
                Some(FmValue::List(items)) => {
                    warnings.push(Warning {
                        origin: origin.to_string(),
                        message: format!(
                            "field '{key}' should be a scalar; using the first list element"
                        ),
                    });
                    items.first().cloned()
                }
            }
        };
        let source = scalar_field("source");
        let supersedes = scalar_field("supersedes");

        // Type-scoping: warn, never error.
        if supersedes.is_some() && mtype != MemoryType::Decision {
            warnings.push(Warning {
                origin: origin.to_string(),
                message: format!(
                    "'supersedes' is only meaningful on decision memories (this is a {})",
                    mtype.as_str()
                ),
            });
        }
        if source.is_some() && mtype != MemoryType::SessionSummary {
            warnings.push(Warning {
                origin: origin.to_string(),
                message: format!(
                    "'source' is only meaningful on session-summary memories (this is a {})",
                    mtype.as_str()
                ),
            });
        }

        let unknown_keys: Vec<(String, FmValue)> = doc
            .pairs
            .iter()
            .filter(|(k, _)| !crate::store::frontmatter::SCHEMA_KEY_ORDER.contains(&k.as_str()))
            .cloned()
            .collect();

        Ok((
            Memory {
                id,
                mtype,
                created,
                title,
                tags,
                links,
                source,
                supersedes,
                unknown_keys,
                body: doc.body.clone(),
            },
            warnings,
        ))
    }

    /// Build the canonical document: schema key order, empty/None fields
    /// omitted (no `tags: []` noise), unknown keys last in first-seen order.
    pub fn to_doc(&self) -> FrontmatterDoc {
        let mut pairs: Vec<(String, FmValue)> = vec![
            ("id".to_string(), FmValue::Scalar(self.id.clone())),
            (
                "type".to_string(),
                FmValue::Scalar(self.mtype.as_str().to_string()),
            ),
            (
                "created".to_string(),
                FmValue::Scalar(format_rfc3339_utc(self.created)),
            ),
            ("title".to_string(), FmValue::Scalar(self.title.clone())),
        ];
        if !self.tags.is_empty() {
            pairs.push(("tags".to_string(), FmValue::List(self.tags.clone())));
        }
        if !self.links.is_empty() {
            pairs.push(("links".to_string(), FmValue::List(self.links.clone())));
        }
        if let Some(source) = &self.source {
            pairs.push(("source".to_string(), FmValue::Scalar(source.clone())));
        }
        if let Some(supersedes) = &self.supersedes {
            pairs.push((
                "supersedes".to_string(),
                FmValue::Scalar(supersedes.clone()),
            ));
        }
        pairs.extend(self.unknown_keys.iter().cloned());
        FrontmatterDoc {
            pairs,
            body: self.body.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::frontmatter::parse;

    fn doc(text: &str) -> FrontmatterDoc {
        parse(text, "<t>").unwrap()
    }

    #[test]
    fn valid_memories_of_each_type() {
        for (t, extra) in [
            ("fact", ""),
            ("decision", "supersedes: decision-old-1\n"),
            ("rule", ""),
            ("session-summary", "source: claude-code:abc123\n"),
        ] {
            let text = format!(
                "---\nid: {t}-x-1\ntype: {t}\ncreated: 2026-07-13T12:00:00Z\ntitle: A {t}\n{extra}---\nbody\n"
            );
            let (m, warnings) = Memory::from_doc(&doc(&text), "<t>").unwrap();
            assert_eq!(m.mtype.as_str(), t);
            assert!(warnings.is_empty(), "{t}: unexpected warnings {warnings:?}");
            assert_eq!(
                m.created,
                parse_rfc3339_utc("2026-07-13T12:00:00Z").unwrap()
            );
        }
    }

    #[test]
    fn missing_required_fields_error_naming_the_field() {
        let full = "---\nid: fact-x-1\ntype: fact\ncreated: 2026-07-13T12:00:00Z\ntitle: T\n---\n";
        for missing in ["id", "type", "created", "title"] {
            let text: String = full
                .lines()
                .filter(|l| !l.starts_with(&format!("{missing}:")))
                .map(|l| format!("{l}\n"))
                .collect();
            let e = Memory::from_doc(&doc(&text), "memories/bad.md").unwrap_err();
            let msg = e.to_string();
            assert!(msg.contains(missing), "'{missing}' named in: {msg}");
            assert!(msg.contains("memories/bad.md"), "origin in: {msg}");
        }
    }

    #[test]
    fn bad_type_and_bad_created_are_errors() {
        let e = Memory::from_doc(
            &doc("---\nid: x-1\ntype: opinion\ncreated: 2026-07-13T12:00:00Z\ntitle: T\n---\n"),
            "<t>",
        )
        .unwrap_err();
        assert!(e.to_string().contains("opinion"), "{e}");
        assert!(
            e.to_string().contains("session-summary"),
            "options listed: {e}"
        );

        let e = Memory::from_doc(
            &doc("---\nid: x-1\ntype: fact\ncreated: yesterday\ntitle: T\n---\n"),
            "<t>",
        )
        .unwrap_err();
        assert!(e.to_string().contains("created"), "{e}");
    }

    #[test]
    fn unknown_keys_preserved_and_round_trip() {
        let text = "---\nid: fact-x-1\ntype: fact\ncreated: 2026-07-13T12:00:00Z\ntitle: T\npriority: high\nreviewed_by: pat\n---\nbody\n";
        let (m, w) = Memory::from_doc(&doc(text), "<t>").unwrap();
        assert!(w.is_empty());
        assert_eq!(
            m.unknown_keys,
            vec![
                ("priority".to_string(), FmValue::Scalar("high".to_string())),
                (
                    "reviewed_by".to_string(),
                    FmValue::Scalar("pat".to_string())
                ),
            ]
        );
        assert_eq!(m.to_doc().serialize(), text, "unknown keys survive rewrite");
    }

    #[test]
    fn type_scoped_fields_warn_not_error() {
        let (m, w) = Memory::from_doc(
            &doc("---\nid: fact-x-1\ntype: fact\ncreated: 2026-07-13T12:00:00Z\ntitle: T\nsupersedes: decision-y-1\nsource: claude-code:s1\n---\n"),
            "memories/fact-x-1.md",
        )
        .unwrap();
        assert_eq!(w.len(), 2, "both misplaced fields warned: {w:?}");
        assert!(w.iter().all(|w| w.origin == "memories/fact-x-1.md"));
        // Content preserved regardless.
        assert_eq!(m.supersedes.as_deref(), Some("decision-y-1"));
        assert_eq!(m.source.as_deref(), Some("claude-code:s1"));
        let out = m.to_doc().serialize();
        assert!(out.contains("supersedes: decision-y-1"), "preserved: {out}");
    }

    #[test]
    fn scalar_where_list_expected_warns_and_wraps() {
        let (m, w) = Memory::from_doc(
            &doc("---\nid: fact-x-1\ntype: fact\ncreated: 2026-07-13T12:00:00Z\ntitle: T\ntags: solo\n---\n"),
            "<t>",
        )
        .unwrap();
        assert_eq!(m.tags, vec!["solo".to_string()]);
        assert_eq!(w.len(), 1);
        assert!(w[0].message.contains("tags"), "{w:?}");
    }

    #[test]
    fn to_doc_omits_empty_fields() {
        let m = Memory {
            id: "rule-x-1".to_string(),
            mtype: MemoryType::Rule,
            created: 0,
            title: "T".to_string(),
            tags: vec![],
            links: vec![],
            source: None,
            supersedes: None,
            unknown_keys: vec![],
            body: String::new(),
        };
        assert_eq!(
            m.to_doc().serialize(),
            "---\nid: rule-x-1\ntype: rule\ncreated: 1970-01-01T00:00:00Z\ntitle: T\n---\n"
        );
    }

    #[test]
    fn rfc3339_round_trip_through_model_including_leap_day() {
        let text = "---\nid: fact-x-1\ntype: fact\ncreated: 2024-02-29T23:59:59Z\ntitle: T\n---\n";
        let (m, _) = Memory::from_doc(&doc(text), "<t>").unwrap();
        assert_eq!(m.to_doc().serialize(), text, "leap-day created survives");
    }
}
