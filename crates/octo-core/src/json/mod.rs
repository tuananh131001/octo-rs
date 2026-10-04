//! JSON as System.Text.Json wrote and read it, so state files and API answers stay byte
//! for byte what existing installs and clients already have.

pub mod datetime;
pub mod dom;
pub mod format;
pub mod web;

pub use format::{
    Escaping, Options, StjFormatter, format_double, format_single, quote, to_string, to_string_indented,
    to_string_with, to_vec_with,
};
