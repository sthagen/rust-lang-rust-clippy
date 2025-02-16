#![warn(clippy::empty_line_after_outer_attr, clippy::empty_line_after_doc_comments)]

/// Meant to be an
/// inner doc comment
/// for the crate
//~^ empty_line_after_doc_comments

fn first_in_crate() {}

mod m {

    /// Meant to be an
    /// inner doc comment
    /// for the module
    //~^ empty_line_after_doc_comments

    fn first_in_module() {}
}

mod some_mod {
    //! This doc comment should *NOT* produce a warning

    mod some_inner_mod {
        fn some_noop() {}
    }

    /// # Indented
    //~^ empty_line_after_doc_comments

    /// Blank line
    fn indented() {}
}

/// This should produce a warning
//~^ empty_line_after_doc_comments

fn with_doc_and_newline() {}

// This should *NOT* produce a warning
#[crate_type = "lib"]
/// some comment
fn with_no_newline_and_comment() {}

/// This doc comment should produce a warning
//~^ empty_line_after_doc_comments

/** This is also a doc comment and is part of the warning
 */

#[allow(non_camel_case_types)]
#[allow(missing_docs)]
#[allow(dead_code)]
fn three_attributes() {}

mod misattributed {

    /// docs for `old_code`
    //~^ empty_line_after_doc_comments
    // fn old_code() {}

    fn new_code() {}

    /// Docs
    /// for OldA
    //~^ empty_line_after_doc_comments
    // struct OldA;

    /// Docs
    /// for OldB
    // struct OldB;

    /// Docs
    /// for Multiple
    #[allow(dead_code)]
    struct Multiple;
}

mod block_comments {

    /**
     * Meant to be inner doc comment
     */
    //~^^^ empty_line_after_doc_comments

    fn first_in_module() {}

    /**
     * Docs for `old_code`
     */
    //~^^^ empty_line_after_doc_comments
    /* fn old_code() {} */

    /**
     * Docs for `new_code`
     */
    fn new_code() {}

    /// Docs for `old_code2`
    //~^ empty_line_after_doc_comments
    /* fn old_code2() {} */

    /// Docs for `new_code2`
    fn new_code2() {}
}

// This should *NOT* produce a warning
#[doc = "
Returns the escaped value of the textual representation of

"]
pub fn function() -> bool {
    true
}

// This should *NOT* produce a warning
#[derive(Clone, Copy)]
pub enum FooFighter {
    Bar1,

    Bar2,

    Bar3,

    Bar4,
}

/// Should not lint
// some line comment
/// gaps without an empty line
struct LineComment;

/// This should *NOT* produce a warning because the empty line is inside a block comment
/*

*/
pub struct EmptyInBlockComment;

/// This should *NOT* produce a warning
/* test */
pub struct BlockComment;

/// Ignore the empty line inside a cfg_attr'd out attribute
#[cfg_attr(any(), multiline(
    foo = 1

    bar = 2
))]
fn empty_line_in_cfg_attr() {}

fn main() {}
