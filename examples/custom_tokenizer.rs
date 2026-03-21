// # Defining a tokenizer pipeline
//
// In this example, we'll see how to define a tokenizer
// by composing filters into a custom pipeline.
use ld_lucivy::collector::TopDocs;
use ld_lucivy::query::QueryParser;
use ld_lucivy::schema::*;
use ld_lucivy::tokenizer::{
    CamelCaseSplitFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer,
};
use ld_lucivy::{doc, Index, IndexWriter};

fn main() -> ld_lucivy::Result<()> {
    let mut schema_builder = Schema::builder();

    // We want a custom tokenizer that splits camelCase identifiers
    // and lowercases everything — useful for code search.
    let text_field_indexing = TextFieldIndexing::default()
        .set_tokenizer("code")
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let text_options = TextOptions::default()
        .set_indexing_options(text_field_indexing)
        .set_stored();
    let title = schema_builder.add_text_field("title", text_options);
    let body = schema_builder.add_text_field("body", TEXT);

    let schema = schema_builder.build();
    let index = Index::create_in_ram(schema.clone());

    // Register a custom tokenizer: SimpleTokenizer → CamelCaseSplit → LowerCaser
    let code_tokenizer = TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(CamelCaseSplitFilter)
        .filter(LowerCaser)
        .build();
    index.tokenizers().register("code", code_tokenizer);

    let mut index_writer: IndexWriter = index.writer(50_000_000)?;
    index_writer.add_document(doc!(
        title => "getElementById",
        body => "Returns the Element that has an ID attribute with the given value."
    ))?;
    index_writer.add_document(doc!(
        title => "HTMLParser",
        body => "A parser for HTML documents that produces a DOM tree."
    ))?;
    index_writer.add_document(doc!(
        title => "createElement",
        body => "Creates the HTML element specified by tagName."
    ))?;
    index_writer.commit()?;

    let reader = index.reader()?;
    let searcher = reader.searcher();

    let query_parser = QueryParser::for_index(&index, vec![title, body]);

    // "element" matches both "getElementById" and "createElement"
    // because CamelCaseSplitFilter splits them into tokens.
    let query = query_parser.parse_query("element")?;
    let top_docs = searcher.search(&query, &TopDocs::with_limit(10).order_by_score())?;

    for (_, doc_address) in top_docs {
        let retrieved_doc: LucivyDocument = searcher.doc(doc_address)?;
        println!("{}", retrieved_doc.to_json(&schema));
    }

    Ok(())
}
