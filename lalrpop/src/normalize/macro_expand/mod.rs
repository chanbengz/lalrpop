use std::collections::{HashMap, HashSet};
use intern::{intern, read, InternedString};
use grammar::parse_tree::{Alternative, Condition, ConditionOp, ExprSymbol, Grammar, GrammarItem,
                          MacroSymbol, NonterminalData, NonterminalString, RepeatOp, RepeatSymbol,
                          Span, Symbol, SymbolKind, TypeRef};
use normalize::{NormResult, NormError};
use normalize::norm_util::{self, Symbols};
use regex::Regex;
use std::mem;

#[cfg(test)]
mod test;

pub fn expand_macros(input: Grammar) -> NormResult<Grammar> {
    let items = input.items;

    let (macro_defs, mut items): (Vec<_>, Vec<_>) =
        items.into_iter().partition(|mi| mi.is_macro_def());

    let macro_defs: HashMap<_, _> =
        macro_defs.into_iter()
                  .map(|md| match md {
                      GrammarItem::Nonterminal(data) => (data.name, data),
                      _ => unreachable!()
                  })
                  .collect();

    let mut expander = MacroExpander::new(macro_defs);
    try!(expander.expand(&mut items));

    Ok(Grammar { items: items, ..input})
}

struct MacroExpander {
    macro_defs: HashMap<NonterminalString, NonterminalData>,
    expansion_set: HashSet<NonterminalString>,
    expansion_stack: Vec<Symbol>,
}

impl MacroExpander {
    fn new(macro_defs: HashMap<NonterminalString, NonterminalData>) -> MacroExpander {
        MacroExpander {
            macro_defs: macro_defs,
            expansion_stack: Vec::new(),
            expansion_set: HashSet::new()
        }
    }

    fn expand(&mut self, items: &mut Vec<GrammarItem>) -> NormResult<()> {
        let mut counter = 0;
        loop {
            // Find any macro uses in items added since last round and
            // replace them in place with the expanded version:
            for item in &mut items[counter..] {
                self.replace_item(item);
            }
            counter = items.len();

            // No more expansion to do.
            if self.expansion_stack.is_empty() {
                return Ok(());
            }

            // Drain expansion stack:
            while let Some(sym) = self.expansion_stack.pop() {
                match sym.kind {
                    SymbolKind::Macro(msym) =>
                        items.push(try!(self.expand_macro_symbol(sym.span, msym))),
                    SymbolKind::Expr(expr) =>
                        items.push(try!(self.expand_expr_symbol(sym.span, expr))),
                    SymbolKind::Repeat(repeat) =>
                        items.push(try!(self.expand_repeat_symbol(sym.span, *repeat))),
                    _ =>
                        assert!(false, "don't know how to expand `{:?}`", sym)
                }
            }
        }
    }

    fn replace_item(&mut self, item: &mut GrammarItem) {
        match *item {
            GrammarItem::TokenType(..) => { }
            GrammarItem::Use(..) => { }
            GrammarItem::Nonterminal(ref mut data) => {
                // Should not encounter macro definitions here,
                // they've already been siphoned off.
                assert!(!data.is_macro_def());

                for alternative in &mut data.alternatives {
                    self.replace_symbols(&mut alternative.expr.symbols);
                }
            }
        }
    }

    fn replace_symbols(&mut self, symbols: &mut [Symbol]) {
        for symbol in symbols {
            self.replace_symbol(symbol);
        }
    }

    fn replace_symbol(&mut self, symbol: &mut Symbol) {
        match symbol.kind {
            SymbolKind::Macro(ref mut m) => {
                for sym in &mut m.args {
                    self.replace_symbol(sym);
                }
            }
            SymbolKind::Expr(ref mut expr) => {
                self.replace_symbols(&mut expr.symbols);
            }
            SymbolKind::Repeat(ref mut repeat) => {
                self.replace_symbol(&mut repeat.symbol);
            }
            SymbolKind::Terminal(_) |
            SymbolKind::Nonterminal(_) => {
                return;
            }
            SymbolKind::Choose(ref mut sym) |
            SymbolKind::Name(_, ref mut sym) => {
                self.replace_symbol(sym);
                return;
            }
        }

        // only symbols we intend to expand fallthrough to here

        let key = NonterminalString(intern(&symbol.canonical_form()));
        let replacement = Symbol { span: symbol.span, kind: SymbolKind::Nonterminal(key) };
        let to_expand = mem::replace(symbol, replacement);
        if self.expansion_set.insert(key) {
            self.expansion_stack.push(to_expand);
        }
    }

    ///////////////////////////////////////////////////////////////////////////
    // Macro expansion

    fn expand_macro_symbol(&mut self, span: Span, msym: MacroSymbol) -> NormResult<GrammarItem> {
        let msym_name = NonterminalString(intern(&msym.canonical_form()));

        let mdef = match self.macro_defs.get(&msym.name) {
            Some(v) => v,
            None => return_err!(span, "no macro definition found for `{}`", msym.name)
        };

        if mdef.args.len() != msym.args.len() {
            return_err!(span, "expected {} arguments to `{}` but found {}",
                        mdef.args.len(), msym.name, msym.args.len());
        }

        let args: HashMap<NonterminalString, SymbolKind> =
            mdef.args.iter()
                     .cloned()
                     .zip(msym.args.into_iter().map(|s| s.kind))
                     .collect();

        let type_decl = mdef.type_decl.as_ref().map(|tr| self.macro_expand_type_ref(&args, tr));

        // due to the use of `try!`, it's a bit awkward to write this with an iterator
        let mut alternatives: Vec<Alternative> = vec![];

        for alternative in &mdef.alternatives {
            if !try!(self.evaluate_cond(&args, &alternative.condition)) {
                continue;
            }
            alternatives.push(Alternative {
                span: span,
                expr: self.macro_expand_expr_symbol(&args, &alternative.expr),
                condition: None,
                action: alternative.action.clone(),
            });
        }

        Ok(GrammarItem::Nonterminal(NonterminalData {
            public: mdef.public,
            span: span,
            name: msym_name,
            args: vec![],
            type_decl: type_decl,
            alternatives: alternatives
        }))
    }

    fn macro_expand_type_refs(&self,
                              args: &HashMap<NonterminalString, SymbolKind>,
                              type_refs: &[TypeRef])
                              -> Vec<TypeRef>
    {
        type_refs.iter().map(|tr| self.macro_expand_type_ref(args, tr)).collect()
    }

    fn macro_expand_type_ref(&self,
                             args: &HashMap<NonterminalString, SymbolKind>,
                             type_ref: &TypeRef)
                             -> TypeRef
    {
        match *type_ref {
            TypeRef::Tuple(ref trs) =>
                TypeRef::Tuple(self.macro_expand_type_refs(args, trs)),
            TypeRef::Nominal { ref path, ref types } =>
                TypeRef::Nominal { path: path.clone(),
                                   types: self.macro_expand_type_refs(args, types) },
            TypeRef::Lifetime(id) =>
                TypeRef::Lifetime(id),
            TypeRef::OfSymbol(ref sym) =>
                TypeRef::OfSymbol(sym.clone()),
            TypeRef::Id(id) => {
                match args.get(&NonterminalString(id)) {
                    Some(sym) => TypeRef::OfSymbol(sym.clone()),
                    None => TypeRef::Nominal { path: vec![id], types: vec![] },
                }
            }
        }
    }

    fn evaluate_cond(&self,
                     args: &HashMap<NonterminalString, SymbolKind>,
                     opt_cond: &Option<Condition>)
                     -> NormResult<bool>
    {
        if let Some(ref c) = *opt_cond {
            match args[&c.lhs] {
                SymbolKind::Terminal(lhs) => {
                    match c.op {
                        ConditionOp::Equals => Ok(lhs.0 == c.rhs),
                        ConditionOp::NotEquals => Ok(lhs.0 != c.rhs),
                        ConditionOp::Match => self.re_match(c.span, lhs.0, c.rhs),
                        ConditionOp::NotMatch => Ok(!try!(self.re_match(c.span, lhs.0, c.rhs))),
                    }
                }
                ref lhs => {
                    return_err!(
                        c.span,
                        "invalid condition LHS `{}`, expected a terminal, not `{}`", c.lhs, lhs);
                }
            }
        } else {
            Ok(true)
        }
    }

    fn re_match(&self, span: Span, lhs: InternedString, regex: InternedString) -> NormResult<bool> {
        read(|interner| {
            let re = match Regex::new(interner.data(regex)) {
                Ok(re) => re,
                Err(err) => return_err!(span, "invalid regular expression `{}`: {}", regex, err),
            };
            Ok(re.is_match(interner.data(lhs)))
        })
    }

    fn macro_expand_symbols(&self,
                            args: &HashMap<NonterminalString, SymbolKind>,
                            expr: &[Symbol])
                            -> Vec<Symbol>
    {
        expr.iter().map(|s| self.macro_expand_symbol(args, s)).collect()
    }

    fn macro_expand_expr_symbol(&self,
                                args: &HashMap<NonterminalString, SymbolKind>,
                                expr: &ExprSymbol)
                                -> ExprSymbol
    {
        ExprSymbol { symbols: self.macro_expand_symbols(args, &expr.symbols) }
    }

    fn macro_expand_symbol(&self,
                           args: &HashMap<NonterminalString, SymbolKind>,
                           symbol: &Symbol)
                           -> Symbol
    {
        let kind = match symbol.kind {
            SymbolKind::Expr(ref expr) =>
                SymbolKind::Expr(self.macro_expand_expr_symbol(args, expr)),
            SymbolKind::Terminal(id) =>
                SymbolKind::Terminal(id),
            SymbolKind::Nonterminal(id) =>
                match args.get(&id) {
                    Some(sym) => sym.clone(),
                    None => SymbolKind::Nonterminal(id),
                },
            SymbolKind::Macro(ref msym) =>
                SymbolKind::Macro(MacroSymbol {
                    name: msym.name,
                    args: self.macro_expand_symbols(args, &msym.args),
                }),
            SymbolKind::Repeat(ref r) =>
                SymbolKind::Repeat(Box::new(RepeatSymbol {
                    op: r.op,
                    symbol: self.macro_expand_symbol(args, &r.symbol)
                })),
            SymbolKind::Choose(ref sym) =>
                SymbolKind::Choose(Box::new(self.macro_expand_symbol(args, sym))),
            SymbolKind::Name(id, ref sym) =>
                SymbolKind::Name(id, Box::new(self.macro_expand_symbol(args, sym))),
        };

        Symbol { span: symbol.span, kind: kind }
    }

    ///////////////////////////////////////////////////////////////////////////
    // Expr expansion

    fn expand_expr_symbol(&mut self, span: Span, expr: ExprSymbol) -> NormResult<GrammarItem> {
        let name = NonterminalString(intern(&expr.canonical_form()));

        let ty_ref = match norm_util::analyze_expr(&expr) {
            Symbols::Named(names) => {
                let (_, ex_id, ex_sym) = names[0];
                return_err!(
                    span,
                    "named symbols like `~{}:{}` are only allowed at the top-level of a nonterminal",
                    ex_id, ex_sym)
            }
            Symbols::Anon(syms) => {
                maybe_tuple(
                    syms.into_iter()
                        .map(|(_, s)| TypeRef::OfSymbol(s.kind.clone()))
                        .collect())
            }
        };

        Ok(GrammarItem::Nonterminal(NonterminalData {
            public: false,
            span: span,
            name: name,
            args: vec![],
            type_decl: Some(ty_ref),
            alternatives: vec![Alternative { span: span,
                                             expr: expr,
                                             condition: None,
                                             action: Some(format!("(~~)")) }]
        }))
    }

    ///////////////////////////////////////////////////////////////////////////
    // Expr expansion

    fn expand_repeat_symbol(&mut self, span: Span, repeat: RepeatSymbol) -> NormResult<GrammarItem> {
        let name = NonterminalString(intern(&repeat.canonical_form()));
        let v = intern("v");
        let e = intern("e");

        let base_symbol_ty = TypeRef::OfSymbol(repeat.symbol.kind.clone());

        match repeat.op {
            RepeatOp::Star => {
                let path = vec![intern("std"), intern("vec"), intern("Vec")];
                let ty_ref = TypeRef::Nominal { path: path, types: vec![base_symbol_ty] };

                Ok(GrammarItem::Nonterminal(NonterminalData {
                    public: false,
                    span: span,
                    name: name,
                    args: vec![],
                    type_decl: Some(ty_ref),
                    alternatives: vec![
                        // X* =
                        Alternative {
                            span: span,
                            expr: ExprSymbol { symbols: vec![] },
                            condition: None,
                            action: Some(format!("vec![]"))
                        },

                        // X* = ~v:X+ ~e:X
                        Alternative {
                            span: span,
                            expr: ExprSymbol {
                                symbols: vec![
                                    Symbol::new(
                                        span,
                                        SymbolKind::Name(
                                            v,
                                            Box::new(
                                                Symbol::new(span,
                                                            SymbolKind::Nonterminal(name))))),
                                    Symbol::new(
                                        span,
                                        SymbolKind::Name(
                                            e,
                                            Box::new(repeat.symbol.clone())))]
                            },
                            condition: None,
                            action: Some(format!("{{ let mut v = v; v.push(e); v }}"))
                        }],
                }))
            }

            RepeatOp::Plus => {
                let path = vec![intern("std"), intern("vec"), intern("Vec")];
                let ty_ref = TypeRef::Nominal { path: path, types: vec![base_symbol_ty] };

                Ok(GrammarItem::Nonterminal(NonterminalData {
                    public: false,
                    span: span,
                    name: name,
                    args: vec![],
                    type_decl: Some(ty_ref),
                    alternatives: vec![
                        // X+ = X
                        Alternative {
                            span: span,
                            expr: ExprSymbol {
                                symbols: vec![repeat.symbol.clone()]
                            },
                            condition: None,
                            action: Some(format!("vec![~~]"))
                        },

                        // X+ = ~v:X+ ~e:X
                        Alternative {
                            span: span,
                            expr: ExprSymbol {
                                symbols: vec![
                                    Symbol::new(span, SymbolKind::Name(
                                        v, Box::new(
                                            Symbol::new(span, SymbolKind::Nonterminal(name))))),
                                    Symbol::new(span, SymbolKind::Name(
                                        e, Box::new(repeat.symbol.clone())))]
                            },
                            condition: None,
                            action: Some(format!("{{ let mut v = v; v.push(e); v }}"))
                        }],
                }))
            }

            RepeatOp::Question => {
                let path = vec![intern("std"), intern("option"), intern("Option")];
                let ty_ref = TypeRef::Nominal { path: path, types: vec![base_symbol_ty] };

                Ok(GrammarItem::Nonterminal(NonterminalData {
                    public: false,
                    span: span,
                    name: name,
                    args: vec![],
                    type_decl: Some(ty_ref),
                    alternatives: vec![
                        // X? = X => Some(~~)
                        Alternative { span: span,
                                      expr: ExprSymbol {
                                          symbols: vec![repeat.symbol.clone()]
                                      },
                                      condition: None,
                                      action: Some(format!("Some(~~)")) },

                        // X? = { => None; }
                        Alternative { span: span,
                                      expr: ExprSymbol {
                                          symbols: vec![]
                                      },
                                      condition: None,
                                      action: Some(format!("None")) }]
                }))
            }
        }
    }
}

fn maybe_tuple(v: Vec<TypeRef>) -> TypeRef {
    if v.len() == 1 {
        v.into_iter().next().unwrap()
    } else {
        TypeRef::Tuple(v)
    }
}