//! Full-text search over ticket titles and bodies.
//!
//! An inverted index is built from the scanned tickets and lives and dies with
//! its `Store`; the files stay the only source of truth. Matching is
//! case-insensitive and token-based: a token is a run of letters and digits.
//! Every word and every quoted phrase of the query must match (AND). Words
//! match whole tokens only (no stemming or prefixes); a word that holds
//! punctuation, such as `ST-12`, must match its tokens next to each other.

use std::collections::{BTreeMap, BTreeSet};

/// Score of one occurrence in a title.
const TITLE_WEIGHT: u32 = 5;
/// Score of one occurrence in a body.
const BODY_WEIGHT: u32 = 1;
/// Characters of context kept before the first match in a snippet.
const SNIPPET_BEFORE: usize = 50;
/// Characters of context kept from the first match on.
const SNIPPET_AFTER: usize = 110;

/// A token with the byte range it covers in the original text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    text: String,
    start: usize,
}

fn tokens(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut current: Option<(String, usize)> = None;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            current
                .get_or_insert_with(|| (String::new(), i))
                .0
                .extend(c.to_lowercase());
        } else if let Some((text, start)) = current.take() {
            out.push(Token { text, start });
        }
    }
    if let Some((text, start)) = current {
        out.push(Token { text, start });
    }
    out
}

/// The lower-case tokens of `text`.
pub fn tokenize(text: &str) -> Vec<String> {
    tokens(text).into_iter().map(|t| t.text).collect()
}

/// A parsed search query: phrases that must all occur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// Each phrase is a sequence of tokens that must appear next to each other.
    phrases: Vec<Vec<String>>,
}

impl Query {
    /// Parses words and `"quoted phrases"`. An unterminated quote runs to the end.
    pub fn parse(text: &str) -> Result<Query, String> {
        let mut parts: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        for c in text.chars() {
            match c {
                '"' => {
                    parts.push(std::mem::take(&mut current));
                    quoted = !quoted;
                }
                c if c.is_whitespace() && !quoted => parts.push(std::mem::take(&mut current)),
                c => current.push(c),
            }
        }
        parts.push(current);
        let mut phrases: Vec<Vec<String>> = parts
            .iter()
            .map(|p| tokenize(p))
            .filter(|p| !p.is_empty())
            .collect();
        phrases.dedup();
        if phrases.is_empty() {
            return Err("the search query has no searchable words".into());
        }
        Ok(Query { phrases })
    }
}

/// Which part of a ticket a snippet comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Body,
}

impl Field {
    pub fn key(self) -> &'static str {
        match self {
            Field::Title => "title",
            Field::Body => "body",
        }
    }
}

/// One matching ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub id: String,
    pub score: u32,
    pub field: Field,
    pub snippet: String,
}

/// Token positions of one token in one ticket.
#[derive(Debug, Default, Clone)]
struct Posting {
    title: Vec<u32>,
    body: Vec<u32>,
}

#[derive(Debug, Clone)]
struct Document {
    title: String,
    body: String,
    body_tokens: Vec<Token>,
}

#[derive(Debug, Default)]
pub struct SearchIndex {
    postings: BTreeMap<String, BTreeMap<String, Posting>>,
    docs: BTreeMap<String, Document>,
}

impl SearchIndex {
    /// Builds the index from `(id, title, body)` triples.
    pub fn build<'a>(items: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>) -> SearchIndex {
        let mut index = SearchIndex::default();
        for (id, title, body) in items {
            let doc = Document {
                title: title.to_string(),
                body: body.to_string(),
                body_tokens: tokens(body),
            };
            for (pos, t) in tokens(title).iter().enumerate() {
                index.posting(&t.text, id).title.push(pos as u32);
            }
            for (pos, t) in doc.body_tokens.iter().enumerate() {
                index.posting(&t.text, id).body.push(pos as u32);
            }
            index.docs.insert(id.to_string(), doc);
        }
        index
    }

    fn posting(&mut self, token: &str, id: &str) -> &mut Posting {
        self.postings
            .entry(token.to_string())
            .or_default()
            .entry(id.to_string())
            .or_default()
    }

    /// The tickets matching every phrase of `query`, best first. Ties are
    /// left in ID order; callers that want natural ID order sort by score first.
    pub fn search(&self, query: &Query) -> Vec<Hit> {
        let mut candidates: Option<BTreeSet<&str>> = None;
        for phrase in &query.phrases {
            for token in phrase {
                let ids: BTreeSet<&str> = self
                    .postings
                    .get(token)
                    .map(|p| p.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                candidates = Some(match candidates {
                    Some(c) => c.intersection(&ids).copied().collect(),
                    None => ids,
                });
            }
        }
        let mut hits = Vec::new();
        for id in candidates.unwrap_or_default() {
            if let Some(hit) = self.score(id, query) {
                hits.push(hit);
            }
        }
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        hits
    }

    fn score(&self, id: &str, query: &Query) -> Option<Hit> {
        let mut score = 0;
        let mut first_body: Option<u32> = None;
        for phrase in &query.phrases {
            let title = self.occurrences(id, phrase, Field::Title);
            let body = self.occurrences(id, phrase, Field::Body);
            if title.is_empty() && body.is_empty() {
                return None;
            }
            score += title.len() as u32 * TITLE_WEIGHT + body.len() as u32 * BODY_WEIGHT;
            first_body = first_body.into_iter().chain(body.first().copied()).min();
        }
        let doc = self.docs.get(id)?;
        let (field, snippet) = match first_body {
            Some(pos) => (Field::Body, snippet(&doc.body, &doc.body_tokens, pos)),
            None => (Field::Title, doc.title.clone()),
        };
        Some(Hit {
            id: id.to_string(),
            score,
            field,
            snippet,
        })
    }

    /// Start positions of `phrase` in one field of one ticket.
    fn occurrences(&self, id: &str, phrase: &[String], field: Field) -> Vec<u32> {
        let positions = |token: &String| -> &[u32] {
            match self.postings.get(token).and_then(|p| p.get(id)) {
                Some(p) if field == Field::Title => &p.title,
                Some(p) => &p.body,
                None => &[],
            }
        };
        let Some((first, rest)) = phrase.split_first() else {
            return Vec::new();
        };
        positions(first)
            .iter()
            .copied()
            .filter(|start| {
                rest.iter()
                    .enumerate()
                    .all(|(i, t)| positions(t).binary_search(&(start + 1 + i as u32)).is_ok())
            })
            .collect()
    }
}

/// A single-line excerpt around the token at `pos`, with `...` where text was cut.
fn snippet(text: &str, tokens: &[Token], pos: u32) -> String {
    let at = tokens.get(pos as usize).map_or(0, |t| t.start);
    let before: Vec<(usize, char)> = text[..at].char_indices().collect();
    let from = before
        .len()
        .checked_sub(SNIPPET_BEFORE)
        .map_or(0, |i| before[i].0);
    let to = text[at..]
        .char_indices()
        .nth(SNIPPET_AFTER)
        .map_or(text.len(), |(i, _)| at + i);
    let mut excerpt = text[from..to]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if from > 0 {
        excerpt.insert_str(0, "...");
    }
    if to < text.len() {
        excerpt.push_str("...");
    }
    excerpt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> SearchIndex {
        SearchIndex::build([
            (
                "A",
                "Quarterly report",
                "Collect the numbers.\nWrite the report draft.",
            ),
            ("B", "Send mail", "Mail the Quarterly numbers to finance"),
            (
                "C",
                "Plan",
                "report quarterly in that order; quarterly again",
            ),
            ("D", "Ünïcode Straße", "Größe matters"),
        ])
    }

    fn ids(q: &str) -> Vec<String> {
        index()
            .search(&Query::parse(q).unwrap())
            .into_iter()
            .map(|h| h.id)
            .collect()
    }

    #[test]
    fn tokenizes_case_insensitively_on_non_alphanumerics() {
        assert_eq!(tokenize("Hello, World-12!"), ["hello", "world", "12"]);
        assert_eq!(tokenize("Straße"), ["straße"]);
        assert!(tokenize("  --  ").is_empty());
    }

    #[test]
    fn all_words_must_match_in_any_order() {
        assert_eq!(ids("quarterly numbers"), ["A", "B"]);
        assert_eq!(ids("numbers quarterly"), ["A", "B"]);
    }

    #[test]
    fn phrases_need_adjacent_tokens() {
        assert_eq!(ids("\"quarterly report\""), ["A"]);
        assert_eq!(ids("\"report quarterly\""), ["C"]);
        assert!(ids("\"report numbers\"").is_empty());
        assert_eq!(ids("\"Collect the numbers\""), ["A"]);
    }

    #[test]
    fn unterminated_quote_runs_to_the_end() {
        assert_eq!(ids("\"quarterly report"), ["A"]);
    }

    #[test]
    fn hyphenated_word_is_a_phrase() {
        let idx = SearchIndex::build([("X", "ST-12 fix", ""), ("Y", "12 ST", "")]);
        let hits = idx.search(&Query::parse("st-12").unwrap());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "X");
    }

    #[test]
    fn titles_outrank_bodies_and_more_hits_outrank_fewer() {
        let hits = index().search(&Query::parse("quarterly").unwrap());
        let order: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(order, ["A", "C", "B"]);
        assert!(hits[0].score > hits[1].score && hits[1].score > hits[2].score);
    }

    #[test]
    fn order_is_deterministic_on_ties() {
        let idx = SearchIndex::build([("B", "x", "foo"), ("A", "y", "foo")]);
        let hits = idx.search(&Query::parse("foo").unwrap());
        assert_eq!(hits[0].id, "A");
    }

    #[test]
    fn unicode_and_case_match() {
        assert_eq!(ids("STRASSE"), Vec::<String>::new());
        assert_eq!(ids("straße"), ["D"]);
        assert_eq!(ids("GRÖSSE"), Vec::<String>::new());
        assert_eq!(ids("größe"), ["D"]);
    }

    #[test]
    fn words_match_whole_tokens_only() {
        assert!(ids("quart").is_empty());
        assert!(ids("reports").is_empty());
    }

    #[test]
    fn empty_queries_are_rejected() {
        assert!(Query::parse("").is_err());
        assert!(Query::parse("  \"\"  ").is_err());
        assert!(Query::parse("-- !!").is_err());
    }

    #[test]
    fn snippets_come_from_the_body_around_the_first_match() {
        let hits = index().search(&Query::parse("draft").unwrap());
        assert_eq!(hits[0].field, Field::Body);
        assert_eq!(
            hits[0].snippet,
            "Collect the numbers. Write the report draft."
        );
        let long = format!("{} needle {}", "word ".repeat(40), "tail ".repeat(60));
        let idx = SearchIndex::build([("L", "t", long.as_str())]);
        let hit = &idx.search(&Query::parse("needle").unwrap())[0];
        assert!(hit.snippet.starts_with("...") && hit.snippet.ends_with("..."));
        assert!(hit.snippet.contains("needle"));
        assert!(hit.snippet.len() < 220);
    }

    #[test]
    fn title_only_matches_use_the_title_as_snippet() {
        let hits = index().search(&Query::parse("send").unwrap());
        assert_eq!(hits[0].field, Field::Title);
        assert_eq!(hits[0].snippet, "Send mail");
    }
}
