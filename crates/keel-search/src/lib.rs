//! Code search: a trigram index over the files of each repository's default
//! branch.
//!
//! Every file (path and content) is indexed as the set of its lowercase
//! three-character substrings. A query is split the same way and documents
//! containing all of its trigrams are candidates; the caller then confirms
//! real matches by scanning the candidates' content. This is how Google Code
//! Search and zoekt work, and unlike word-based search it finds any
//! substring, including punctuation such as `fn main(` or `->`.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery, TermSetQuery};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions,
    Value,
};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, Term, doc};

/// Queries need at least one trigram.
pub const MIN_QUERY_CHARS: usize = 3;
const TOKENIZER: &str = "trigram";
/// One indexing thread with a modest buffer: indexing is incremental and
/// rare, so memory matters more than throughput.
const WRITER_MEMORY: usize = 32 * 1024 * 1024;

struct Fields {
    repo_id: Field,
    /// `<repo id>:<path>`, to replace or delete a file's document.
    key: Field,
    path: Field,
    blob: Field,
    grams: Field,
}

pub struct SearchIndex {
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    f: Fields,
}

/// A file that contains all trigrams of a query (not necessarily the
/// query itself).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub repo_id: u64,
    pub path: String,
    /// Hex id of the blob that was indexed.
    pub blob: String,
}

/// A file to (re)index.
pub struct FileDoc {
    pub path: String,
    pub blob: String,
    pub content: String,
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let grams = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(TOKENIZER)
            // Only which documents contain a trigram matters.
            .set_index_option(IndexRecordOption::Basic),
    );
    let f = Fields {
        repo_id: b.add_u64_field("repo_id", INDEXED | FAST | STORED),
        key: b.add_text_field("key", STRING),
        path: b.add_text_field("path", STRING | STORED),
        blob: b.add_text_field("blob", STORED),
        grams: b.add_text_field("grams", grams),
    };
    (b.build(), f)
}

/// The distinct lowercase trigrams of `s`, by characters.
pub fn trigrams(s: &str) -> BTreeSet<String> {
    let chars: Vec<char> = s.chars().flat_map(char::to_lowercase).collect();
    chars.windows(3).map(|w| w.iter().collect()).collect()
}

impl SearchIndex {
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let (schema, f) = schema();
        let directory = tantivy::directory::MmapDirectory::open(dir)?;
        let index = Index::open_or_create(directory, schema).context("opening search index")?;
        index.tokenizers().register(
            TOKENIZER,
            TextAnalyzer::builder(NgramTokenizer::new(3, 3, false)?)
                .filter(LowerCaser)
                .build(),
        );
        let writer = index.writer_with_num_threads(1, WRITER_MEMORY)?;
        let reader = index.reader()?;
        Ok(Self {
            reader,
            writer: Mutex::new(writer),
            f,
        })
    }

    fn key(repo_id: u64, path: &str) -> String {
        format!("{repo_id}:{path}")
    }

    /// Removes `deleted` paths and (re)indexes `files` of a repository in
    /// one commit. Searches see either all or none of it.
    pub fn update(
        &self,
        repo_id: u64,
        deleted: &[String],
        files: impl IntoIterator<Item = FileDoc>,
    ) -> anyhow::Result<usize> {
        let mut writer = self.writer.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        for path in deleted {
            writer.delete_term(Term::from_field_text(self.f.key, &Self::key(repo_id, path)));
        }
        let mut added = 0;
        for file in files {
            let key = Self::key(repo_id, &file.path);
            writer.delete_term(Term::from_field_text(self.f.key, &key));
            let mut text = file.path.clone();
            text.push('\n');
            text.push_str(&file.content);
            writer.add_document(doc!(
                self.f.repo_id => repo_id,
                self.f.key => key,
                self.f.path => file.path,
                self.f.blob => file.blob,
                self.f.grams => text,
            ))?;
            added += 1;
        }
        writer.commit()?;
        drop(writer);
        self.reader.reload()?;
        Ok(added)
    }

    pub fn delete_repo(&self, repo_id: u64) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        writer.delete_term(Term::from_field_u64(self.f.repo_id, repo_id));
        writer.commit()?;
        drop(writer);
        self.reader.reload()?;
        Ok(())
    }

    /// Files in `repos` that contain every trigram of `query`.
    pub fn candidates(&self, query: &str, repos: &[u64], limit: usize) -> anyhow::Result<Vec<Candidate>> {
        let grams = trigrams(query);
        if grams.is_empty() || repos.is_empty() {
            return Ok(Vec::new());
        }
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = grams
            .iter()
            .map(|g| {
                let q = TermQuery::new(Term::from_field_text(self.f.grams, g), IndexRecordOption::Basic);
                (Occur::Must, Box::new(q) as Box<dyn Query>)
            })
            .collect();
        let repo_terms = repos.iter().map(|id| Term::from_field_u64(self.f.repo_id, *id));
        clauses.push((Occur::Must, Box::new(TermSetQuery::new(repo_terms))));
        let query = BooleanQuery::new(clauses);

        let searcher = self.reader.searcher();
        let top = searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;
        top.into_iter()
            .map(|(_, address)| {
                let doc: TantivyDocument = searcher.doc(address)?;
                let text = |field| doc.get_first(field).and_then(|v| v.as_str()).unwrap_or_default().to_owned();
                Ok(Candidate {
                    repo_id: doc.get_first(self.f.repo_id).and_then(|v| v.as_u64()).unwrap_or_default(),
                    path: text(self.f.path),
                    blob: text(self.f.blob),
                })
            })
            .collect()
    }

    /// Number of indexed files.
    pub fn len(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, content: &str) -> FileDoc {
        FileDoc {
            path: path.into(),
            blob: format!("blob-of-{path}"),
            content: content.into(),
        }
    }

    fn paths(index: &SearchIndex, q: &str, repos: &[u64]) -> Vec<String> {
        let mut p: Vec<String> = index.candidates(q, repos, 100).unwrap().into_iter().map(|c| c.path).collect();
        p.sort();
        p
    }

    #[test]
    fn trigram_search() {
        assert_eq!(trigrams("aBcd").into_iter().collect::<Vec<_>>(), ["abc", "bcd"]);
        assert!(trigrams("ab").is_empty());

        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        assert!(index.is_empty());
        index
            .update(1, &[], [
                file("src/main.rs", "fn main() {\n    println!(\"Hello\");\n}\n"),
                file("README.md", "# Engine\nRun it with cargo.\n"),
            ])
            .unwrap();
        index.update(2, &[], [file("lib.rs", "pub fn main_loop() {}")]).unwrap();
        assert_eq!(index.len(), 3);

        assert_eq!(paths(&index, "fn main(", &[1, 2]), ["src/main.rs"]);
        assert_eq!(paths(&index, "FN MAIN", &[1, 2]), ["lib.rs", "src/main.rs"]);
        assert_eq!(paths(&index, "fn main", &[2]), ["lib.rs"], "repo filter");
        assert_eq!(paths(&index, "readme", &[1]), ["README.md"], "paths are indexed");
        assert!(paths(&index, "nothing like this", &[1, 2]).is_empty());
        assert!(paths(&index, "fn", &[1, 2]).is_empty(), "too short");

        // Updating a file replaces it; deleting removes it.
        index.update(1, &["README.md".into()], [file("src/main.rs", "fn start() {}")]).unwrap();
        assert!(paths(&index, "fn main(", &[1]).is_empty());
        assert_eq!(paths(&index, "fn start", &[1]), ["src/main.rs"]);
        assert_eq!(index.len(), 2);

        index.delete_repo(1).unwrap();
        assert_eq!(index.len(), 1);
        drop(index);

        // Survives reopening.
        let index = SearchIndex::open(dir.path()).unwrap();
        assert_eq!(paths(&index, "main_loop", &[2]), ["lib.rs"]);
    }
}
