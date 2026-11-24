//! Non-Block Hashline parser and pure applier.
//!
//! This tracks the current oh-my-pi `PUT`/`CUT` protocol. Syntactic-block
//! locators remain intentionally unsupported because Mink does not ship the
//! tree-sitter resolver used upstream.

use anyhow::{Result, anyhow, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};

const MAX_RANGE_LINES: usize = 100_000;
const MAX_OPERATIONS: usize = 10_000;
const MAX_REGISTER_NAME: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cursor {
    Head,
    Tail,
    Before(usize),
    After(usize),
}

/// A range endpoint: a 1-based line number, or an anchor line text matched
/// (trimmed, exact) against the file. Anchor locators (`'start'..'end'`) let
/// the model name the boundary lines instead of computing line numbers, so
/// off-by-one range errors become impossible and failures are always visible
/// (anchor not found / not unique) instead of silent corruption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeBound {
    Line(usize),
    Anchor(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteTarget {
    Gap(Cursor),
    Range { start: RangeBound, end: RangeBound },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Put {
        start: RangeBound,
        end: RangeBound,
        body: Vec<String>,
    },
    Cut {
        start: RangeBound,
        end: RangeBound,
        register: Option<String>,
    },
    Insert {
        cursor: Cursor,
        body: Vec<String>,
    },
    Paste {
        target: PasteTarget,
        register: Option<String>,
    },
    Remove,
    Move {
        destination: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub path: String,
    pub tag: String,
    pub operations: Vec<Operation>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Clipboard {
    anonymous: Option<Vec<String>>,
    pending_anonymous_cuts: Vec<String>,
    named: BTreeMap<String, Vec<String>>,
}

impl Clipboard {
    pub fn with_named(named: BTreeMap<String, Vec<String>>) -> Self {
        Self {
            named,
            ..Self::default()
        }
    }

    pub fn named(&self) -> &BTreeMap<String, Vec<String>> {
        &self.named
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyResult {
    pub text: String,
    pub warnings: Vec<String>,
}

mod apply;
mod balance;
mod parser;

pub use apply::{
    already_applied, anchor_lines, anchor_lines_in_text, apply, is_head_tail_only, remap_anchors,
};
#[cfg(test)]
use balance::balance;
pub use parser::parse;
pub(crate) use parser::section_header_path;

#[cfg(test)]
#[path = "../hashline_tests.rs"]
mod tests;
