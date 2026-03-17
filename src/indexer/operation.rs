use crate::query::Weight;
use crate::schema::document::Document;
use crate::schema::{Field, LucivyDocument, Term};
use crate::tokenizer::PreTokenizedString;
use crate::Opstamp;

/// Pre-tokenized field data: per-field list of pre-computed tokens.
///
/// Each entry is (Field, Vec<PreTokenizedString>) — one PreTokenizedString
/// per text value on that field (supports multi-value fields).
/// When present, the SegmentWriter uses PreTokenizedStream instead of
/// re-running the tokenizer — eliminates double tokenization.
pub type PreTokenizedData = Vec<(Field, Vec<PreTokenizedString>)>;

/// Timestamped Delete operation.
pub struct DeleteOperation {
    /// Operation stamp.
    /// It is used to check whether the delete operation
    /// applies to an added document operation.
    pub opstamp: Opstamp,
    /// Weight is used to define the set of documents to be deleted.
    pub target: Box<dyn Weight>,
}

/// Timestamped Add operation.
pub struct AddOperation<D: Document = LucivyDocument> {
    /// Operation stamp.
    pub opstamp: Opstamp,
    /// Document to be added.
    pub document: D,
    /// Optional pre-tokenized fields (from ReaderActor pipeline).
    /// When set, SegmentWriter skips tokenization for these fields.
    pub pre_tokenized: Option<PreTokenizedData>,
}

/// UserOperation is an enum type that encapsulates other operation types.
#[derive(Eq, PartialEq, Debug)]
pub enum UserOperation<D: Document = LucivyDocument> {
    /// Add operation
    Add(D),
    /// Delete operation
    Delete(Term),
}
