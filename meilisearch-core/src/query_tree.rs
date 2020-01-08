use std::borrow::Cow;
use std::collections::HashMap;
use std::time::Instant;
use std::{cmp, fmt, iter::once};

use sdset::{Set, SetBuf, SetOperation};
use slice_group_by::StrGroupBy;
use itertools::{EitherOrBoth, merge_join_by};
use fst::{IntoStreamer, Streamer};

use crate::database::MainT;
use crate::{store, DocumentId, DocIndex, MResult};
use crate::automaton::{build_dfa, build_prefix_dfa, build_exact_dfa};

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Operation {
    And(Vec<Operation>),
    Or(Vec<Operation>),
    Query(Query),
}

impl fmt::Debug for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn pprint_tree(f: &mut fmt::Formatter<'_>, op: &Operation, depth: usize) -> fmt::Result {
            match op {
                Operation::And(children) => {
                    writeln!(f, "{:1$}AND", "", depth * 2)?;
                    children.iter().try_for_each(|c| pprint_tree(f, c, depth + 1))
                },
                Operation::Or(children) => {
                    writeln!(f, "{:1$}OR", "", depth * 2)?;
                    children.iter().try_for_each(|c| pprint_tree(f, c, depth + 1))
                },
                Operation::Query(query) => writeln!(f, "{:2$}{:?}", "", query, depth * 2),
            }
        }

        pprint_tree(f, self, 0)
    }
}

pub type QueryId = usize;

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Query {
    pub id: QueryId,
    pub prefix: bool,
    pub kind: QueryKind,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueryKind {
    Tolerant(String),
    Exact(String),
    Phrase(Vec<String>),
}

impl Query {
    fn tolerant(id: QueryId, prefix: bool, s: &str) -> Query {
        Query { id, prefix, kind: QueryKind::Tolerant(s.to_string()) }
    }

    fn exact(id: QueryId, prefix: bool, s: &str) -> Query {
        Query { id, prefix, kind: QueryKind::Exact(s.to_string()) }
    }

    fn phrase2(id: QueryId, prefix: bool, (left, right): (&str, &str)) -> Query {
        Query { id, prefix, kind: QueryKind::Phrase(vec![left.to_owned(), right.to_owned()]) }
    }
}

impl fmt::Debug for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Query { id, prefix, kind } = self;
        let prefix = if *prefix { String::from("Prefix") } else { String::default() };
        match kind {
            QueryKind::Exact(word) => {
                f.debug_struct(&(prefix + "Exact")).field("id", &id).field("word", &word).finish()
            },
            QueryKind::Tolerant(word) => {
                f.debug_struct(&(prefix + "Tolerant")).field("id", &id).field("word", &word).finish()
            },
            QueryKind::Phrase(words) => {
                f.debug_struct(&(prefix + "Phrase")).field("id", &id).field("words", &words).finish()
            },
        }
    }
}

#[derive(Debug, Default)]
pub struct PostingsList {
    docids: SetBuf<DocumentId>,
    matches: SetBuf<DocIndex>,
}

#[derive(Debug, Default)]
pub struct Context {
    pub synonyms: HashMap<Vec<String>, Vec<Vec<String>>>,
    pub postings: HashMap<String, PostingsList>,
}

fn split_best_frequency<'a>(
    reader: &heed::RoTxn<MainT>,
    postings_lists: store::PostingsLists,
    word: &'a str,
) -> MResult<Option<(&'a str, &'a str)>>
{
    let chars = word.char_indices().skip(1);
    let mut best = None;

    for (i, _) in chars {
        let (left, right) = word.split_at(i);

        let left_freq = postings_lists.postings_list(reader, left.as_bytes())?.map(|pl| pl.len()).unwrap_or(0);
        let right_freq = postings_lists.postings_list(reader, right.as_bytes())?.map(|pl| pl.len()).unwrap_or(0);

        let min_freq = cmp::min(left_freq, right_freq);
        if min_freq != 0 && best.map_or(true, |(old, _, _)| min_freq > old) {
            best = Some((min_freq, left, right));
        }
    }

    Ok(best.map(|(_, l, r)| (l, r)))
}

fn fetch_synonyms(
    reader: &heed::RoTxn<MainT>,
    synonyms: store::Synonyms,
    words: &[&str],
) -> MResult<Vec<Vec<String>>>
{
    let words = words.join(" "); // TODO ugly
    // synonyms.synonyms(reader, words.as_bytes()).cloned().unwrap_or_default()
    Ok(vec![])
}

fn is_last<I: IntoIterator>(iter: I) -> impl Iterator<Item=(bool, I::Item)> {
    let mut iter = iter.into_iter().peekable();
    core::iter::from_fn(move || {
        iter.next().map(|item| (iter.peek().is_none(), item))
    })
}

fn create_operation<I, F>(iter: I, f: F) -> Operation
where I: IntoIterator<Item=Operation>,
      F: Fn(Vec<Operation>) -> Operation,
{
    let mut iter = iter.into_iter();
    match (iter.next(), iter.next()) {
        (Some(first), None) => first,
        (first, second) => f(first.into_iter().chain(second).chain(iter).collect()),
    }
}

const MAX_NGRAM: usize = 3;

pub fn create_query_tree(
    reader: &heed::RoTxn<MainT>,
    postings_lists: store::PostingsLists,
    synonyms: store::Synonyms,
    query: &str,
) -> MResult<Operation>
{
    let query = query.to_lowercase();

    let words = query.linear_group_by_key(char::is_whitespace).map(ToOwned::to_owned);
    let words = words.filter(|s| !s.contains(char::is_whitespace)).enumerate();
    let words: Vec<_> = words.collect();

    let mut ngrams = Vec::new();
    for ngram in 1..=MAX_NGRAM {
        let ngiter = words.windows(ngram).enumerate().map(|(i, group)| {
            let before = words[..i].windows(1);
            let after = words[i + ngram..].windows(1);
            before.chain(Some(group)).chain(after)
        });

        for group in ngiter {
            let mut ops = Vec::new();

            for (is_last, words) in is_last(group) {
                let mut alts = Vec::new();
                match words {
                    [(id, word)] => {
                        let phrase = split_best_frequency(reader, postings_lists, word)?
                            .map(|ws| Query::phrase2(*id, is_last, ws))
                            .map(Operation::Query);

                        let synonyms = fetch_synonyms(reader, synonyms, &[word])?.into_iter().map(|alts| {
                            let iter = alts.into_iter().map(|w| Query::exact(*id, false, &w)).map(Operation::Query);
                            create_operation(iter, Operation::And)
                        });

                        let query = Query::tolerant(*id, is_last, word);

                        alts.push(Operation::Query(query));
                        alts.extend(synonyms.chain(phrase));
                    },
                    words => {
                        let id = words[0].0;
                        let words: Vec<_> = words.iter().map(|(_, s)| s.as_str()).collect();

                        for synonym in fetch_synonyms(reader, synonyms, &words)? {
                            let synonym = synonym.into_iter().map(|s| Operation::Query(Query::exact(id, false, &s)));
                            let synonym = create_operation(synonym, Operation::And);
                            alts.push(synonym);
                        }

                        let query = Query::exact(id, is_last, &words.concat());
                        alts.push(Operation::Query(query));
                    }
                }

                ops.push(create_operation(alts, Operation::Or));
            }

            ngrams.push(create_operation(ops, Operation::And));
            if ngram == 1 { break }
        }
    }

    Ok(create_operation(ngrams, Operation::Or))
}

pub struct QueryResult<'o, 'txn> {
    pub docids: SetBuf<DocumentId>,
    pub queries: HashMap<&'o Query, Cow<'txn, Set<DocIndex>>>,
}

pub type Postings<'o, 'txn> = HashMap<&'o Query, Cow<'txn, Set<DocIndex>>>;
pub type Cache<'o, 'c> = HashMap<&'o Operation, SetBuf<DocumentId>>;

pub fn traverse_query_tree<'o, 'txn>(
    reader: &'txn heed::RoTxn<MainT>,
    words_set: &fst::Set,
    postings_lists: store::PostingsLists,
    tree: &'o Operation,
) -> MResult<QueryResult<'o, 'txn>>
{
    fn execute_and<'o, 'txn>(
        reader: &'txn heed::RoTxn<MainT>,
        words_set: &fst::Set,
        pls: store::PostingsLists,
        cache: &mut Cache<'o, 'txn>,
        postings: &mut Postings<'o, 'txn>,
        depth: usize,
        operations: &'o [Operation],
    ) -> MResult<SetBuf<DocumentId>>
    {
        println!("{:1$}AND", "", depth * 2);

        let before = Instant::now();
        let mut results = Vec::new();

        for op in operations {
            if cache.get(op).is_none() {
                let docids = match op {
                    Operation::And(ops) => execute_and(reader, words_set, pls, cache, postings, depth + 1, &ops)?,
                    Operation::Or(ops) => execute_or(reader, words_set, pls, cache, postings, depth + 1, &ops)?,
                    Operation::Query(query) => execute_query(reader, words_set, pls, postings, depth + 1, &query)?,
                };
                cache.insert(op, docids);
            }
        }

        for op in operations {
            if let Some(docids) = cache.get(op) {
                results.push(docids.as_ref());
            }
        }

        let op = sdset::multi::Intersection::new(results);
        let docids = op.into_set_buf();

        println!("{:3$}--- AND fetched {} documents in {:.02?}", "", docids.len(), before.elapsed(), depth * 2);

        Ok(docids)
    }

    fn execute_or<'o, 'txn>(
        reader: &'txn heed::RoTxn<MainT>,
        words_set: &fst::Set,
        pls: store::PostingsLists,
        cache: &mut Cache<'o, 'txn>,
        postings: &mut Postings<'o, 'txn>,
        depth: usize,
        operations: &'o [Operation],
    ) -> MResult<SetBuf<DocumentId>>
    {
        println!("{:1$}OR", "", depth * 2);

        let before = Instant::now();
        let mut ids = Vec::new();

        for op in operations {
            let docids = match cache.get(op) {
                Some(docids) => docids,
                None => {
                    let docids = match op {
                        Operation::And(ops) => execute_and(reader, words_set, pls, cache, postings, depth + 1, &ops)?,
                        Operation::Or(ops) => execute_or(reader, words_set, pls, cache, postings, depth + 1, &ops)?,
                        Operation::Query(query) => execute_query(reader, words_set, pls, postings, depth + 1, &query)?,
                    };
                    cache.entry(op).or_insert(docids)
                }
            };

            ids.extend_from_slice(docids.as_ref());
        }

        let docids = SetBuf::from_dirty(ids);

        println!("{:3$}--- OR fetched {} documents in {:.02?}", "", docids.len(), before.elapsed(), depth * 2);

        Ok(docids)
    }

    fn execute_query<'o, 'txn>(
        reader: &'txn heed::RoTxn<MainT>,
        words_set: &fst::Set,
        pls: store::PostingsLists,
        postings: &mut Postings<'o, 'txn>,
        depth: usize,
        query: &'o Query,
    ) -> MResult<SetBuf<DocumentId>>
    {
        let before = Instant::now();

        // let byte = query.as_bytes()[0];
        // let mut stream = if byte == u8::max_value() {
        //     words.search(&dfa).ge(&[byte]).into_stream()
        // } else {
        //     words.search(&dfa).ge(&[byte]).lt(&[byte + 1]).into_stream()
        // };

        let Query { id, prefix, kind } = query;
        let docids = match kind {
            QueryKind::Tolerant(word) => {
                let dfa = if *prefix { build_prefix_dfa(word) } else { build_dfa(word) };

                let mut docids = Vec::new();
                let mut stream = words_set.search(&dfa).into_stream();
                while let Some(input) = stream.next() {
                    if let Some(matches) = pls.postings_list(reader, input)? {
                        docids.extend(matches.iter().map(|d| d.document_id))
                    }
                }

                SetBuf::from_dirty(docids)
            },
            QueryKind::Exact(word) => {
                // TODO support prefix and non-prefix exact DFA
                let dfa = build_exact_dfa(word);

                let mut docids = Vec::new();
                let mut stream = words_set.search(&dfa).into_stream();
                while let Some(input) = stream.next() {
                    if let Some(matches) = pls.postings_list(reader, input)? {
                        docids.extend(matches.iter().map(|d| d.document_id))
                    }
                }

                SetBuf::from_dirty(docids)
            },
            QueryKind::Phrase(words) => {
                // TODO support prefix and non-prefix exact DFA
                if let [first, second] = words.as_slice() {
                    let first = pls.postings_list(reader, first.as_bytes())?.unwrap_or_default();
                    let second = pls.postings_list(reader, second.as_bytes())?.unwrap_or_default();

                    let iter = merge_join_by(first.as_slice(), second.as_slice(), |a, b| {
                        let x = (a.document_id, a.attribute, (a.word_index as u32) + 1);
                        let y = (b.document_id, b.attribute, b.word_index as u32);
                        x.cmp(&y)
                    });

                    let matches: Vec<_> = iter
                        .filter_map(EitherOrBoth::both)
                        .flat_map(|(a, b)| once(*a).chain(Some(*b)))
                        .collect();

                    let mut docids: Vec<_> = matches.iter().map(|m| m.document_id).collect();
                    docids.dedup();

                    println!("{:2$}matches {:?}", "", matches, depth * 2);

                    SetBuf::new(docids).unwrap()
                } else {
                    println!("{:2$}{:?} skipped", "", words, depth * 2);
                    SetBuf::default()
                }
            },
        };

        println!("{:4$}{:?} fetched {:?} documents in {:.02?}", "", query, docids.len(), before.elapsed(), depth * 2);

        // postings.insert(query, matches);
        Ok(docids)
    }

    let mut cache = Cache::new();
    let mut postings = Postings::new();

    let docids = match tree {
        Operation::And(ops) => execute_and(reader, words_set, postings_lists, &mut cache, &mut postings, 0, &ops)?,
        Operation::Or(ops) => execute_or(reader, words_set, postings_lists, &mut cache, &mut postings, 0, &ops)?,
        Operation::Query(query) => execute_query(reader, words_set, postings_lists, &mut postings, 0, &query)?,
    };

    Ok(QueryResult { docids, queries: postings })
}