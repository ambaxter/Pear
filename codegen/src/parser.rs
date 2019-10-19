use syn::{punctuated::Punctuated, Token};
use syn::parse::{Parse as SynParse, ParseStream as SynParseStream};
use proc_macro2::{Span, Delimiter};

use crate::diagnostics::{Diagnostic, SpanExt, Spanned};

pub type PResult<T> = Result<T, Diagnostic>;

pub trait Parse: Sized {
    fn parse(input: syn::parse::ParseStream) -> PResult<Self>;

    fn syn_parse(input: syn::parse::ParseStream) -> syn::parse::Result<Self> {
        Self::parse(input).map_err(|e| e.into())
    }
}

trait ParseStreamExt {
    fn parse_group<F, G>(self, delimiter: Delimiter, parser: F) -> syn::parse::Result<G>
        where F: FnOnce(SynParseStream) -> syn::parse::Result<G>;

    fn try_parse<F, G>(self, parser: F) -> syn::parse::Result<G>
        where F: Fn(SynParseStream) -> syn::parse::Result<G>;
}

impl<'a> ParseStreamExt for SynParseStream<'a> {
    fn parse_group<F, G>(self, delimiter: Delimiter, parser: F) -> syn::parse::Result<G>
        where F: FnOnce(SynParseStream) -> syn::parse::Result<G>
    {
        let content;
        match delimiter {
            Delimiter::Brace => { syn::braced!(content in self); },
            Delimiter::Bracket => { syn::bracketed!(content in self); },
            Delimiter::Parenthesis => { syn::parenthesized!(content in self); },
            Delimiter::None => return parser(self),
        }

        parser(&content)
    }

    fn try_parse<F, G>(self, parser: F) -> syn::parse::Result<G>
        where F: Fn(SynParseStream) -> syn::parse::Result<G>
    {
        let input = self.fork();
        parser(&input)?;
        parser(self)
    }
}

#[derive(Debug)]
pub struct CallPattern {
    pub name: Option<syn::Ident>,
    pub at: Option<Token![@]>,
    pub expr: syn::ExprCall,
}

impl syn::parse::Parse for CallPattern {
    fn parse(input: syn::parse::ParseStream) -> syn::parse::Result<Self> {
        Self::syn_parse(input)
    }
}

impl quote::ToTokens for CallPattern {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let (expr, at) = (&self.expr, &self.at);
        match self.name {
            Some(ref name) => quote!(#name #at #expr).to_tokens(tokens),
            None => expr.to_tokens(tokens)
        }
    }
}

#[derive(Debug)]
pub enum Pattern {
    Wild(Token![_]),
    Calls(Punctuated<CallPattern, Token![|]>)
}

#[derive(Debug)]
pub struct Case {
    pub pattern: Pattern,
    pub expr: syn::Expr,
    pub span: Span,
}

#[derive(Debug)]
pub struct Switch {
    pub input: syn::Expr,
    pub output: syn::Type,
    pub cases: Punctuated<Case, Token![,]>
}

// FIXME(syn): Something like this should be in `syn`
fn parse_expr_call(input: SynParseStream) -> syn::parse::Result<syn::ExprCall> {
    let path: syn::ExprPath = input.parse()?;
    let paren_span = input.cursor().span();
    let args: Punctuated<syn::Expr, Token![,]> = input.parse_group(Delimiter::Parenthesis, |i| {
        i.parse_terminated(syn::Expr::parse)
    })?;

    Ok(syn::ExprCall {
        attrs: vec![],
        func: Box::new(syn::Expr::Path(path)),
        paren_token: syn::token::Paren(paren_span),
        args: args
    })
}

impl Parse for CallPattern {
    fn parse(input: SynParseStream) -> PResult<Self> {
        let name_at = input.try_parse(|input| {
            let ident: syn::Ident = input.parse()?;
            let at = input.parse::<Token![@]>()?;
            Ok((ident, at))
        }).ok();

        let (name, at) = match name_at {
            Some((name, at)) => (Some(name), Some(at)),
            None => (None, None)
        };

        Ok(CallPattern { name, at, expr: parse_expr_call(input)? })
    }
}

impl Pattern {
    fn validate(&self) -> PResult<()> {
        if let Pattern::Calls(ref calls) = self {
            let first_name = calls.first().and_then(|call| call.name.clone());
            for call in calls.iter() {
                if first_name != call.name {
                    let mut err = if let Some(ref ident) = call.name {
                        ident.span()
                            .error("captured name differs from declaration")
                    } else {
                        call.expr.span()
                            .error("expected capture name due to previous declaration")
                    };

                    err = match first_name {
                        Some(p) => err.span_note(p.span(), "declared here"),
                        None => err
                    };

                    return Err(err);
                }
            }
        }

        Ok(())
    }
}

impl Parse for Case {
    fn parse(input: SynParseStream) -> PResult<Self> {
        let case_span_start = input.cursor().span();

        let pattern = if let Ok(wild) = input.parse::<Token![_]>() {
            Pattern::Wild(wild)
        } else {
            let call_patterns =
                input.call(<Punctuated<CallPattern, Token![|]>>::parse_separated_nonempty)?;

            Pattern::Calls(call_patterns)
        };

        pattern.validate()?;
        input.parse::<Token![=>]>()?;
        let expr: syn::Expr = input.parse()?;
        let span = case_span_start.join(input.cursor().span()).unwrap();

        Ok(Case { pattern, expr, span })
    }
}

impl Parse for Switch {
    fn parse(stream: SynParseStream) -> PResult<Switch> {
        let (_info, input, _marker, output) = stream.parse_group(Delimiter::Bracket, |inner| {
            let info: syn::Ident = inner.parse()?;
            inner.parse::<Token![;]>()?;
            let input: syn::Expr = inner.parse()?;
            inner.parse::<Token![;]>()?;
            let marker: syn::Expr = inner.parse()?;
            inner.parse::<Token![;]>()?;
            let output: syn::Type = inner.parse()?;
            Ok((info, input, marker, output))
        })?;

        let cases: Punctuated<Case, Token![,]> = stream.parse_terminated(Case::syn_parse)?;
        if !stream.is_empty() {
            Err(stream.error("trailing characters; expected eof"))?;
        }

        if cases.is_empty() {
            Err(stream.error("switch cannot be empty"))?;
        }

        for case in cases.iter().take(cases.len() - 1) {
            if let Pattern::Wild(..) = case.pattern {
                Err(case.span.error("`_` matches can only appear as the last case"))?;
            }
        }

        Ok(Switch { input, output, cases })
    }
}

#[derive(Debug, Clone)]
pub struct AttrArgs {
    pub raw: Option<Span>,
    pub rewind: Option<Span>,
    pub peek: Option<Span>,
}

impl Parse for AttrArgs {
    fn parse(input: SynParseStream) -> PResult<Self> {
        let args = input.call(<Punctuated<syn::Ident, Token![,]>>::parse_terminated)?;
        let (mut raw, mut rewind, mut peek) = Default::default();
        for case in args.iter() {
            if case == "raw" {
                raw = Some(case.span());
            } else if case == "rewind" {
                rewind = Some(case.span());
            } else if case == "peek" {
                peek = Some(case.span());
            } else {
                return Err(case.span()
                           .error(format!("unknown attribute argument `{}`", case))
                           .help("supported arguments are: `rewind`, `peek`"));
            }
        }

        Ok(AttrArgs { raw, rewind, peek })
    }
}
