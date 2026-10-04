//! STUB(4-D): replaced when 4-D (acquisition orchestration) lands with the port of
//! `Services/Common/ExternalSearchService.cs`. Only the constant `SearchBudget` reads is here.

/// How many enriched external songs one query builds. A constant so concurrent callers wanting
/// different amounts can share one execution.
pub const BUILD_SIZE: i32 = 60;
