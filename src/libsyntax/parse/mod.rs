//! The main parser interface.

use crate::ast;
use crate::parse::parser::{Parser, emit_unclosed_delims};
use crate::parse::token::Nonterminal;
use crate::tokenstream::{self, TokenStream, TokenTree};
use crate::print::pprust;
use crate::sess::ParseSess;

use errors::{FatalError, Level, Diagnostic, DiagnosticBuilder};
#[cfg(target_arch = "x86_64")]
use rustc_data_structures::static_assert_size;
use rustc_data_structures::sync::Lrc;
use syntax_pos::{Span, SourceFile, FileName};

use std::borrow::Cow;
use std::path::Path;
use std::str;

use log::info;

#[cfg(test)]
mod tests;

#[macro_use]
pub mod parser;
pub mod lexer;
pub mod token;

crate mod classify;
crate mod literal;
crate mod unescape_error_reporting;

pub type PResult<'a, T> = Result<T, DiagnosticBuilder<'a>>;

// `PResult` is used a lot. Make sure it doesn't unintentionally get bigger.
// (See also the comment on `DiagnosticBuilderInner`.)
#[cfg(target_arch = "x86_64")]
static_assert_size!(PResult<'_, bool>, 16);

#[derive(Clone)]
pub struct Directory<'a> {
    pub path: Cow<'a, Path>,
    pub ownership: DirectoryOwnership,
}

#[derive(Copy, Clone)]
pub enum DirectoryOwnership {
    Owned {
        // None if `mod.rs`, `Some("foo")` if we're in `foo.rs`.
        relative: Option<ast::Ident>,
    },
    UnownedViaBlock,
    UnownedViaMod(bool /* legacy warnings? */),
}

// A bunch of utility functions of the form `parse_<thing>_from_<source>`
// where <thing> includes crate, expr, item, stmt, tts, and one that
// uses a HOF to parse anything, and <source> includes file and
// `source_str`.

pub fn parse_crate_from_file<'a>(input: &Path, sess: &'a ParseSess) -> PResult<'a, ast::Crate> {
    let mut parser = new_parser_from_file(sess, input);
    parser.parse_crate_mod()
}

pub fn parse_crate_attrs_from_file<'a>(input: &Path, sess: &'a ParseSess)
                                       -> PResult<'a, Vec<ast::Attribute>> {
    let mut parser = new_parser_from_file(sess, input);
    parser.parse_inner_attributes()
}

pub fn parse_crate_from_source_str(name: FileName, source: String, sess: &ParseSess)
                                       -> PResult<'_, ast::Crate> {
    new_parser_from_source_str(sess, name, source).parse_crate_mod()
}

pub fn parse_crate_attrs_from_source_str(name: FileName, source: String, sess: &ParseSess)
                                             -> PResult<'_, Vec<ast::Attribute>> {
    new_parser_from_source_str(sess, name, source).parse_inner_attributes()
}

pub fn parse_stream_from_source_str(
    name: FileName,
    source: String,
    sess: &ParseSess,
    override_span: Option<Span>,
) -> TokenStream {
    let (stream, mut errors) = source_file_to_stream(
        sess,
        sess.source_map().new_source_file(name, source),
        override_span,
    );
    emit_unclosed_delims(&mut errors, &sess.span_diagnostic);
    stream
}

/// Creates a new parser from a source string.
pub fn new_parser_from_source_str(sess: &ParseSess, name: FileName, source: String) -> Parser<'_> {
    panictry_buffer!(&sess.span_diagnostic, maybe_new_parser_from_source_str(sess, name, source))
}

/// Creates a new parser from a source string. Returns any buffered errors from lexing the initial
/// token stream.
pub fn maybe_new_parser_from_source_str(sess: &ParseSess, name: FileName, source: String)
    -> Result<Parser<'_>, Vec<Diagnostic>>
{
    let mut parser = maybe_source_file_to_parser(sess,
                                                 sess.source_map().new_source_file(name, source))?;
    parser.recurse_into_file_modules = false;
    Ok(parser)
}

/// Creates a new parser, handling errors as appropriate if the file doesn't exist.
pub fn new_parser_from_file<'a>(sess: &'a ParseSess, path: &Path) -> Parser<'a> {
    source_file_to_parser(sess, file_to_source_file(sess, path, None))
}

/// Creates a new parser, returning buffered diagnostics if the file doesn't exist,
/// or from lexing the initial token stream.
pub fn maybe_new_parser_from_file<'a>(sess: &'a ParseSess, path: &Path)
    -> Result<Parser<'a>, Vec<Diagnostic>> {
    let file = try_file_to_source_file(sess, path, None).map_err(|db| vec![db])?;
    maybe_source_file_to_parser(sess, file)
}

/// Given a session, a crate config, a path, and a span, add
/// the file at the given path to the `source_map`, and returns a parser.
/// On an error, uses the given span as the source of the problem.
pub fn new_sub_parser_from_file<'a>(sess: &'a ParseSess,
                                    path: &Path,
                                    directory_ownership: DirectoryOwnership,
                                    module_name: Option<String>,
                                    sp: Span) -> Parser<'a> {
    let mut p = source_file_to_parser(sess, file_to_source_file(sess, path, Some(sp)));
    p.directory.ownership = directory_ownership;
    p.root_module_name = module_name;
    p
}

/// Given a `source_file` and config, returns a parser.
fn source_file_to_parser(sess: &ParseSess, source_file: Lrc<SourceFile>) -> Parser<'_> {
    panictry_buffer!(&sess.span_diagnostic,
                     maybe_source_file_to_parser(sess, source_file))
}

/// Given a `source_file` and config, return a parser. Returns any buffered errors from lexing the
/// initial token stream.
fn maybe_source_file_to_parser(
    sess: &ParseSess,
    source_file: Lrc<SourceFile>,
) -> Result<Parser<'_>, Vec<Diagnostic>> {
    let end_pos = source_file.end_pos;
    let (stream, unclosed_delims) = maybe_file_to_stream(sess, source_file, None)?;
    let mut parser = stream_to_parser(sess, stream, None);
    parser.unclosed_delims = unclosed_delims;
    if parser.token == token::Eof && parser.token.span.is_dummy() {
        parser.token.span = Span::new(end_pos, end_pos, parser.token.span.ctxt());
    }

    Ok(parser)
}

// Must preserve old name for now, because `quote!` from the *existing*
// compiler expands into it.
pub fn new_parser_from_tts(sess: &ParseSess, tts: Vec<TokenTree>) -> Parser<'_> {
    stream_to_parser(sess, tts.into_iter().collect(), crate::MACRO_ARGUMENTS)
}


// Base abstractions

/// Given a session and a path and an optional span (for error reporting),
/// add the path to the session's source_map and return the new source_file or
/// error when a file can't be read.
fn try_file_to_source_file(sess: &ParseSess, path: &Path, spanopt: Option<Span>)
                   -> Result<Lrc<SourceFile>, Diagnostic> {
    sess.source_map().load_file(path)
    .map_err(|e| {
        let msg = format!("couldn't read {}: {}", path.display(), e);
        let mut diag = Diagnostic::new(Level::Fatal, &msg);
        if let Some(sp) = spanopt {
            diag.set_span(sp);
        }
        diag
    })
}

/// Given a session and a path and an optional span (for error reporting),
/// adds the path to the session's `source_map` and returns the new `source_file`.
fn file_to_source_file(sess: &ParseSess, path: &Path, spanopt: Option<Span>)
                   -> Lrc<SourceFile> {
    match try_file_to_source_file(sess, path, spanopt) {
        Ok(source_file) => source_file,
        Err(d) => {
            sess.span_diagnostic.emit_diagnostic(&d);
            FatalError.raise();
        }
    }
}

/// Given a `source_file`, produces a sequence of token trees.
pub fn source_file_to_stream(
    sess: &ParseSess,
    source_file: Lrc<SourceFile>,
    override_span: Option<Span>,
) -> (TokenStream, Vec<lexer::UnmatchedBrace>) {
    panictry_buffer!(&sess.span_diagnostic, maybe_file_to_stream(sess, source_file, override_span))
}

/// Given a source file, produces a sequence of token trees. Returns any buffered errors from
/// parsing the token stream.
pub fn maybe_file_to_stream(
    sess: &ParseSess,
    source_file: Lrc<SourceFile>,
    override_span: Option<Span>,
) -> Result<(TokenStream, Vec<lexer::UnmatchedBrace>), Vec<Diagnostic>> {
    let srdr = lexer::StringReader::new(sess, source_file, override_span);
    let (token_trees, unmatched_braces) = srdr.into_token_trees();

    match token_trees {
        Ok(stream) => Ok((stream, unmatched_braces)),
        Err(err) => {
            let mut buffer = Vec::with_capacity(1);
            err.buffer(&mut buffer);
            // Not using `emit_unclosed_delims` to use `db.buffer`
            for unmatched in unmatched_braces {
                let mut db = sess.span_diagnostic.struct_span_err(unmatched.found_span, &format!(
                    "incorrect close delimiter: `{}`",
                    pprust::token_kind_to_string(&token::CloseDelim(unmatched.found_delim)),
                ));
                db.span_label(unmatched.found_span, "incorrect close delimiter");
                if let Some(sp) = unmatched.candidate_span {
                    db.span_label(sp, "close delimiter possibly meant for this");
                }
                if let Some(sp) = unmatched.unclosed_span {
                    db.span_label(sp, "un-closed delimiter");
                }
                db.buffer(&mut buffer);
            }
            Err(buffer)
        }
    }
}

/// Given a stream and the `ParseSess`, produces a parser.
pub fn stream_to_parser<'a>(
    sess: &'a ParseSess,
    stream: TokenStream,
    subparser_name: Option<&'static str>,
) -> Parser<'a> {
    Parser::new(sess, stream, None, true, false, subparser_name)
}

/// Given a stream, the `ParseSess` and the base directory, produces a parser.
///
/// Use this function when you are creating a parser from the token stream
/// and also care about the current working directory of the parser (e.g.,
/// you are trying to resolve modules defined inside a macro invocation).
///
/// # Note
///
/// The main usage of this function is outside of rustc, for those who uses
/// libsyntax as a library. Please do not remove this function while refactoring
/// just because it is not used in rustc codebase!
pub fn stream_to_parser_with_base_dir<'a>(
    sess: &'a ParseSess,
    stream: TokenStream,
    base_dir: Directory<'a>,
) -> Parser<'a> {
    Parser::new(sess, stream, Some(base_dir), true, false, None)
}

// NOTE(Centril): The following probably shouldn't be here but it acknowledges the
// fact that architecturally, we are using parsing (read on below to understand why).

pub fn nt_to_tokenstream(nt: &Nonterminal, sess: &ParseSess, span: Span) -> TokenStream {
    // A `Nonterminal` is often a parsed AST item. At this point we now
    // need to convert the parsed AST to an actual token stream, e.g.
    // un-parse it basically.
    //
    // Unfortunately there's not really a great way to do that in a
    // guaranteed lossless fashion right now. The fallback here is to just
    // stringify the AST node and reparse it, but this loses all span
    // information.
    //
    // As a result, some AST nodes are annotated with the token stream they
    // came from. Here we attempt to extract these lossless token streams
    // before we fall back to the stringification.
    let tokens = match *nt {
        Nonterminal::NtItem(ref item) => {
            prepend_attrs(sess, &item.attrs, item.tokens.as_ref(), span)
        }
        Nonterminal::NtTraitItem(ref item) => {
            prepend_attrs(sess, &item.attrs, item.tokens.as_ref(), span)
        }
        Nonterminal::NtImplItem(ref item) => {
            prepend_attrs(sess, &item.attrs, item.tokens.as_ref(), span)
        }
        Nonterminal::NtIdent(ident, is_raw) => {
            Some(tokenstream::TokenTree::token(token::Ident(ident.name, is_raw), ident.span).into())
        }
        Nonterminal::NtLifetime(ident) => {
            Some(tokenstream::TokenTree::token(token::Lifetime(ident.name), ident.span).into())
        }
        Nonterminal::NtTT(ref tt) => {
            Some(tt.clone().into())
        }
        _ => None,
    };

    // FIXME(#43081): Avoid this pretty-print + reparse hack
    let source = pprust::nonterminal_to_string(nt);
    let filename = FileName::macro_expansion_source_code(&source);
    let tokens_for_real = parse_stream_from_source_str(filename, source, sess, Some(span));

    // During early phases of the compiler the AST could get modified
    // directly (e.g., attributes added or removed) and the internal cache
    // of tokens my not be invalidated or updated. Consequently if the
    // "lossless" token stream disagrees with our actual stringification
    // (which has historically been much more battle-tested) then we go
    // with the lossy stream anyway (losing span information).
    //
    // Note that the comparison isn't `==` here to avoid comparing spans,
    // but it *also* is a "probable" equality which is a pretty weird
    // definition. We mostly want to catch actual changes to the AST
    // like a `#[cfg]` being processed or some weird `macro_rules!`
    // expansion.
    //
    // What we *don't* want to catch is the fact that a user-defined
    // literal like `0xf` is stringified as `15`, causing the cached token
    // stream to not be literal `==` token-wise (ignoring spans) to the
    // token stream we got from stringification.
    //
    // Instead the "probably equal" check here is "does each token
    // recursively have the same discriminant?" We basically don't look at
    // the token values here and assume that such fine grained token stream
    // modifications, including adding/removing typically non-semantic
    // tokens such as extra braces and commas, don't happen.
    if let Some(tokens) = tokens {
        if tokens.probably_equal_for_proc_macro(&tokens_for_real) {
            return tokens
        }
        info!("cached tokens found, but they're not \"probably equal\", \
                going with stringified version");
    }
    return tokens_for_real
}

fn prepend_attrs(
    sess: &ParseSess,
    attrs: &[ast::Attribute],
    tokens: Option<&tokenstream::TokenStream>,
    span: syntax_pos::Span
) -> Option<tokenstream::TokenStream> {
    let tokens = tokens?;
    if attrs.len() == 0 {
        return Some(tokens.clone())
    }
    let mut builder = tokenstream::TokenStreamBuilder::new();
    for attr in attrs {
        assert_eq!(attr.style, ast::AttrStyle::Outer,
                   "inner attributes should prevent cached tokens from existing");

        let source = pprust::attribute_to_string(attr);
        let macro_filename = FileName::macro_expansion_source_code(&source);
        if attr.is_sugared_doc {
            let stream = parse_stream_from_source_str(macro_filename, source, sess, Some(span));
            builder.push(stream);
            continue
        }

        // synthesize # [ $path $tokens ] manually here
        let mut brackets = tokenstream::TokenStreamBuilder::new();

        // For simple paths, push the identifier directly
        if attr.path.segments.len() == 1 && attr.path.segments[0].args.is_none() {
            let ident = attr.path.segments[0].ident;
            let token = token::Ident(ident.name, ident.as_str().starts_with("r#"));
            brackets.push(tokenstream::TokenTree::token(token, ident.span));

        // ... and for more complicated paths, fall back to a reparse hack that
        // should eventually be removed.
        } else {
            let stream = parse_stream_from_source_str(macro_filename, source, sess, Some(span));
            brackets.push(stream);
        }

        brackets.push(attr.tokens.clone());

        // The span we list here for `#` and for `[ ... ]` are both wrong in
        // that it encompasses more than each token, but it hopefully is "good
        // enough" for now at least.
        builder.push(tokenstream::TokenTree::token(token::Pound, attr.span));
        let delim_span = tokenstream::DelimSpan::from_single(attr.span);
        builder.push(tokenstream::TokenTree::Delimited(
            delim_span, token::DelimToken::Bracket, brackets.build().into()));
    }
    builder.push(tokens.clone());
    Some(builder.build())
}
