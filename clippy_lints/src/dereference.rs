use clippy_utils::diagnostics::{span_lint_and_sugg, span_lint_hir_and_then};
use clippy_utils::mir::{enclosing_mir, expr_local, local_assignments, used_exactly_once, PossibleBorrowerMap};
use clippy_utils::msrvs::{self, Msrv};
use clippy_utils::source::{snippet_with_applicability, snippet_with_context};
use clippy_utils::sugg::has_enclosing_paren;
use clippy_utils::ty::{is_copy, peel_mid_ty_refs};
use clippy_utils::{
    expr_use_ctxt, get_parent_expr, get_parent_node, is_lint_allowed, path_to_local, DefinedTy, ExprUseNode,
};

use hir::def::DefKind;
use hir::MatchSource;
use rustc_ast::util::parser::{PREC_POSTFIX, PREC_PREFIX};
use rustc_data_structures::fx::FxIndexMap;
use rustc_data_structures::graph::iterate::{CycleDetector, TriColorDepthFirstSearch};
use rustc_errors::Applicability;
use rustc_hir::def::Res;
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_hir::intravisit::{walk_ty, Visitor};
use rustc_hir::{
    self as hir, BindingAnnotation, Body, BodyId, BorrowKind, Expr, ExprKind, HirId, Mutability, Node, Pat, PatKind,
    Path, QPath, TyKind, UnOp,
};
use rustc_index::bit_set::BitSet;
use rustc_infer::infer::TyCtxtInferExt;
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::mir::{Rvalue, StatementKind};
use rustc_middle::ty::adjustment::{Adjust, Adjustment, AutoBorrow, AutoBorrowMutability};
use rustc_middle::ty::{
    self, ClauseKind, EarlyBinder, FnSig, GenericArg, GenericArgKind, List, ParamEnv, ParamTy, ProjectionPredicate, Ty,
    TyCtxt, TypeVisitableExt, TypeckResults,
};
use rustc_session::{declare_tool_lint, impl_lint_pass};
use rustc_span::symbol::sym;
use rustc_span::{Span, Symbol};
use rustc_trait_selection::infer::InferCtxtExt as _;
use rustc_trait_selection::traits::query::evaluate_obligation::InferCtxtExt as _;
use rustc_trait_selection::traits::{Obligation, ObligationCause};
use std::collections::VecDeque;

declare_clippy_lint! {
    /// ### What it does
    /// Checks for explicit `deref()` or `deref_mut()` method calls.
    ///
    /// ### Why is this bad?
    /// Dereferencing by `&*x` or `&mut *x` is clearer and more concise,
    /// when not part of a method chain.
    ///
    /// ### Example
    /// ```rust
    /// use std::ops::Deref;
    /// let a: &mut String = &mut String::from("foo");
    /// let b: &str = a.deref();
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// let a: &mut String = &mut String::from("foo");
    /// let b = &*a;
    /// ```
    ///
    /// This lint excludes all of:
    /// ```rust,ignore
    /// let _ = d.unwrap().deref();
    /// let _ = Foo::deref(&foo);
    /// let _ = <Foo as Deref>::deref(&foo);
    /// ```
    #[clippy::version = "1.44.0"]
    pub EXPLICIT_DEREF_METHODS,
    pedantic,
    "Explicit use of deref or deref_mut method while not in a method chain."
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for address of operations (`&`) that are going to
    /// be dereferenced immediately by the compiler.
    ///
    /// ### Why is this bad?
    /// Suggests that the receiver of the expression borrows
    /// the expression.
    ///
    /// ### Known problems
    /// The lint cannot tell when the implementation of a trait
    /// for `&T` and `T` do different things. Removing a borrow
    /// in such a case can change the semantics of the code.
    ///
    /// ### Example
    /// ```rust
    /// fn fun(_a: &i32) {}
    ///
    /// let x: &i32 = &&&&&&5;
    /// fun(&x);
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// # fn fun(_a: &i32) {}
    /// let x: &i32 = &5;
    /// fun(x);
    /// ```
    #[clippy::version = "pre 1.29.0"]
    pub NEEDLESS_BORROW,
    style,
    "taking a reference that is going to be automatically dereferenced"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for `ref` bindings which create a reference to a reference.
    ///
    /// ### Why is this bad?
    /// The address-of operator at the use site is clearer about the need for a reference.
    ///
    /// ### Example
    /// ```rust
    /// let x = Some("");
    /// if let Some(ref x) = x {
    ///     // use `x` here
    /// }
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// let x = Some("");
    /// if let Some(x) = x {
    ///     // use `&x` here
    /// }
    /// ```
    #[clippy::version = "1.54.0"]
    pub REF_BINDING_TO_REFERENCE,
    pedantic,
    "`ref` binding to a reference"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for dereferencing expressions which would be covered by auto-deref.
    ///
    /// ### Why is this bad?
    /// This unnecessarily complicates the code.
    ///
    /// ### Example
    /// ```rust
    /// let x = String::new();
    /// let y: &str = &*x;
    /// ```
    /// Use instead:
    /// ```rust
    /// let x = String::new();
    /// let y: &str = &x;
    /// ```
    #[clippy::version = "1.64.0"]
    pub EXPLICIT_AUTO_DEREF,
    complexity,
    "dereferencing when the compiler would automatically dereference"
}

impl_lint_pass!(Dereferencing<'_> => [
    EXPLICIT_DEREF_METHODS,
    NEEDLESS_BORROW,
    REF_BINDING_TO_REFERENCE,
    EXPLICIT_AUTO_DEREF,
]);

#[derive(Default)]
pub struct Dereferencing<'tcx> {
    state: Option<(State, StateData<'tcx>)>,

    // While parsing a `deref` method call in ufcs form, the path to the function is itself an
    // expression. This is to store the id of that expression so it can be skipped when
    // `check_expr` is called for it.
    skip_expr: Option<HirId>,

    /// The body the first local was found in. Used to emit lints when the traversal of the body has
    /// been finished. Note we can't lint at the end of every body as they can be nested within each
    /// other.
    current_body: Option<BodyId>,

    /// The list of locals currently being checked by the lint.
    /// If the value is `None`, then the binding has been seen as a ref pattern, but is not linted.
    /// This is needed for or patterns where one of the branches can be linted, but another can not
    /// be.
    ///
    /// e.g. `m!(x) | Foo::Bar(ref x)`
    ref_locals: FxIndexMap<HirId, Option<RefPat>>,

    /// Stack of (body owner, `PossibleBorrowerMap`) pairs. Used by
    /// `needless_borrow_impl_arg_position` to determine when a borrowed expression can instead
    /// be moved.
    possible_borrowers: Vec<(LocalDefId, PossibleBorrowerMap<'tcx, 'tcx>)>,

    // `IntoIterator` for arrays requires Rust 1.53.
    msrv: Msrv,
}

impl<'tcx> Dereferencing<'tcx> {
    #[must_use]
    pub fn new(msrv: Msrv) -> Self {
        Self {
            msrv,
            ..Dereferencing::default()
        }
    }
}

#[derive(Debug)]
struct StateData<'tcx> {
    /// Span of the top level expression
    span: Span,
    hir_id: HirId,
    adjusted_ty: Ty<'tcx>,
}

struct DerefedBorrow {
    count: usize,
    msg: &'static str,
    stability: TyCoercionStability,
    for_field_access: Option<Symbol>,
}

enum State {
    // Any number of deref method calls.
    DerefMethod {
        // The number of calls in a sequence which changed the referenced type
        ty_changed_count: usize,
        is_ufcs: bool,
        /// The required mutability
        mutbl: Mutability,
    },
    DerefedBorrow(DerefedBorrow),
    ExplicitDeref {
        mutability: Option<Mutability>,
    },
    ExplicitDerefField {
        name: Symbol,
    },
    Reborrow {
        mutability: Mutability,
    },
    Borrow {
        mutability: Mutability,
    },
}

// A reference operation considered by this lint pass
enum RefOp {
    Method { mutbl: Mutability, is_ufcs: bool },
    Deref,
    AddrOf(Mutability),
}

struct RefPat {
    /// Whether every usage of the binding is dereferenced.
    always_deref: bool,
    /// The spans of all the ref bindings for this local.
    spans: Vec<Span>,
    /// The applicability of this suggestion.
    app: Applicability,
    /// All the replacements which need to be made.
    replacements: Vec<(Span, String)>,
    /// The [`HirId`] that the lint should be emitted at.
    hir_id: HirId,
}

impl<'tcx> LateLintPass<'tcx> for Dereferencing<'tcx> {
    #[expect(clippy::too_many_lines)]
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'_>) {
        // Skip path expressions from deref calls. e.g. `Deref::deref(e)`
        if Some(expr.hir_id) == self.skip_expr.take() {
            return;
        }

        if let Some(local) = path_to_local(expr) {
            self.check_local_usage(cx, expr, local);
        }

        // Stop processing sub expressions when a macro call is seen
        if expr.span.from_expansion() {
            if let Some((state, data)) = self.state.take() {
                report(cx, expr, state, data);
            }
            return;
        }

        let typeck = cx.typeck_results();
        let Some((kind, sub_expr)) = try_parse_ref_op(cx.tcx, typeck, expr) else {
            // The whole chain of reference operations has been seen
            if let Some((state, data)) = self.state.take() {
                report(cx, expr, state, data);
            }
            return;
        };

        match (self.state.take(), kind) {
            (None, kind) => {
                let expr_ty = typeck.expr_ty(expr);
                let use_cx = expr_use_ctxt(cx, expr);
                let adjusted_ty = match &use_cx {
                    Some(use_cx) => match use_cx.adjustments {
                        [.., a] => a.target,
                        _ => expr_ty,
                    },
                    _ => typeck.expr_ty_adjusted(expr),
                };

                match (use_cx, kind) {
                    (Some(use_cx), RefOp::Deref) => {
                        let sub_ty = typeck.expr_ty(sub_expr);
                        if let ExprUseNode::FieldAccess(name) = use_cx.node
                            && adjusted_ty.ty_adt_def().map_or(true, |adt| !adt.is_union())
                            && !ty_contains_field(sub_ty, name.name)
                        {
                            self.state = Some((
                                State::ExplicitDerefField { name: name.name },
                                StateData {
                                    span: expr.span,
                                    hir_id: expr.hir_id,
                                    adjusted_ty,
                                },
                            ));
                        } else if sub_ty.is_ref()
                            // Linting method receivers would require verifying that name lookup
                            // would resolve the same way. This is complicated by trait methods.
                            && !use_cx.node.is_recv()
                            && let Some(ty) = use_cx.node.defined_ty(cx)
                            && TyCoercionStability::for_defined_ty(cx, ty, use_cx.node.is_return()).is_deref_stable()
                        {
                            self.state = Some((
                                State::ExplicitDeref { mutability: None },
                                StateData {
                                    span: expr.span,
                                    hir_id: expr.hir_id,
                                    adjusted_ty,
                                },
                            ));
                        }
                    },
                    (_, RefOp::Method { mutbl, is_ufcs })
                        if !is_lint_allowed(cx, EXPLICIT_DEREF_METHODS, expr.hir_id)
                            // Allow explicit deref in method chains. e.g. `foo.deref().bar()`
                            && (is_ufcs || !in_postfix_position(cx, expr)) =>
                    {
                        let ty_changed_count = usize::from(!deref_method_same_type(expr_ty, typeck.expr_ty(sub_expr)));
                        self.state = Some((
                            State::DerefMethod {
                                ty_changed_count,
                                is_ufcs,
                                mutbl,
                            },
                            StateData {
                                span: expr.span,
                                hir_id: expr.hir_id,
                                adjusted_ty,
                            },
                        ));
                    },
                    (Some(use_cx), RefOp::AddrOf(mutability)) => {
                        let defined_ty = use_cx.node.defined_ty(cx);

                        // Check needless_borrow for generic arguments.
                        if !use_cx.is_ty_unified
                            && let Some(DefinedTy::Mir(ty)) = defined_ty
                            && let ty::Param(ty) = *ty.value.skip_binder().kind()
                            && let Some((hir_id, fn_id, i)) = match use_cx.node {
                                ExprUseNode::MethodArg(_, _, 0) => None,
                                ExprUseNode::MethodArg(hir_id, None, i) => {
                                    typeck.type_dependent_def_id(hir_id).map(|id| (hir_id, id, i))
                                },
                                ExprUseNode::FnArg(&Expr { kind: ExprKind::Path(ref p), hir_id, .. }, i)
                                if !path_has_args(p) => match typeck.qpath_res(p, hir_id) {
                                    Res::Def(DefKind::Fn | DefKind::Ctor(..) | DefKind::AssocFn, id) => {
                                        Some((hir_id, id, i))
                                    },
                                    _ => None,
                                },
                                _ => None,
                            } && let count = needless_borrow_generic_arg_count(
                                cx,
                                &mut self.possible_borrowers,
                                fn_id,
                                typeck.node_args(hir_id),
                                i,
                                ty,
                                expr,
                                &self.msrv,
                            ) && count != 0
                        {
                            self.state = Some((
                                State::DerefedBorrow(DerefedBorrow {
                                    count: count - 1,
                                    msg: "the borrowed expression implements the required traits",
                                    stability: TyCoercionStability::None,
                                    for_field_access: None,
                                }),
                                StateData {
                                    span: expr.span,
                                    hir_id: expr.hir_id,
                                    adjusted_ty: use_cx.adjustments.last().map_or(expr_ty, |a| a.target),
                                },
                            ));
                            return;
                        }

                        // Find the number of times the borrow is auto-derefed.
                        let mut iter = use_cx.adjustments.iter();
                        let mut deref_count = 0usize;
                        let next_adjust = loop {
                            match iter.next() {
                                Some(adjust) => {
                                    if !matches!(adjust.kind, Adjust::Deref(_)) {
                                        break Some(adjust);
                                    } else if !adjust.target.is_ref() {
                                        deref_count += 1;
                                        break iter.next();
                                    }
                                    deref_count += 1;
                                },
                                None => break None,
                            };
                        };

                        let stability = defined_ty.map_or(TyCoercionStability::None, |ty| {
                            TyCoercionStability::for_defined_ty(cx, ty, use_cx.node.is_return())
                        });
                        let can_auto_borrow = match use_cx.node {
                            ExprUseNode::Callee => true,
                            ExprUseNode::FieldAccess(_) => adjusted_ty.ty_adt_def().map_or(true, |adt| !adt.is_union()),
                            ExprUseNode::MethodArg(hir_id, _, 0) if !use_cx.moved_before_use => {
                                // Check for calls to trait methods where the trait is implemented
                                // on a reference.
                                // Two cases need to be handled:
                                // * `self` methods on `&T` will never have auto-borrow
                                // * `&self` methods on `&T` can have auto-borrow, but `&self` methods on `T` will take
                                //   priority.
                                if let Some(fn_id) = typeck.type_dependent_def_id(hir_id)
                                    && let Some(trait_id) = cx.tcx.trait_of_item(fn_id)
                                    && let arg_ty
                                        = cx.tcx.erase_regions(use_cx.adjustments.last().map_or(expr_ty, |a| a.target))
                                    && let ty::Ref(_, sub_ty, _) = *arg_ty.kind()
                                    && let args = cx
                                        .typeck_results()
                                        .node_args_opt(hir_id).map(|args| &args[1..]).unwrap_or_default()
                                    && let impl_ty = if cx.tcx.fn_sig(fn_id)
                                        .instantiate_identity()
                                        .skip_binder()
                                        .inputs()[0].is_ref()
                                    {
                                        // Trait methods taking `&self`
                                        sub_ty
                                    } else {
                                        // Trait methods taking `self`
                                        arg_ty
                                    } && impl_ty.is_ref()
                                    && cx.tcx.infer_ctxt().build()
                                        .type_implements_trait(
                                            trait_id,
                                            [impl_ty.into()].into_iter().chain(args.iter().copied()),
                                            cx.param_env,
                                        )
                                        .must_apply_modulo_regions()
                                {
                                    false
                                } else {
                                    true
                                }
                            },
                            _ => false,
                        };

                        let deref_msg =
                            "this expression creates a reference which is immediately dereferenced by the compiler";
                        let borrow_msg = "this expression borrows a value the compiler would automatically borrow";

                        // Determine the required number of references before any can be removed. In all cases the
                        // reference made by the current expression will be removed. After that there are four cases to
                        // handle.
                        //
                        // 1. Auto-borrow will trigger in the current position, so no further references are required.
                        // 2. Auto-deref ends at a reference, or the underlying type, so one extra needs to be left to
                        //    handle the automatically inserted re-borrow.
                        // 3. Auto-deref hits a user-defined `Deref` impl, so at least one reference needs to exist to
                        //    start auto-deref.
                        // 4. If the chain of non-user-defined derefs ends with a mutable re-borrow, and re-borrow
                        //    adjustments will not be inserted automatically, then leave one further reference to avoid
                        //    moving a mutable borrow. e.g.
                        //
                        //    ```rust
                        //    fn foo<T>(x: &mut Option<&mut T>, y: &mut T) {
                        //        let x = match x {
                        //            // Removing the borrow will cause `x` to be moved
                        //            Some(x) => &mut *x,
                        //            None => y
                        //        };
                        //    }
                        //    ```
                        let (required_refs, msg) = if can_auto_borrow {
                            (1, if deref_count == 1 { borrow_msg } else { deref_msg })
                        } else if let Some(&Adjustment {
                                kind: Adjust::Borrow(AutoBorrow::Ref(_, mutability)),
                                ..
                            }) = next_adjust
                            && matches!(mutability, AutoBorrowMutability::Mut { .. })
                            && !stability.is_reborrow_stable()
                        {
                            (3, deref_msg)
                        } else {
                            (2, deref_msg)
                        };

                        if deref_count >= required_refs {
                            self.state = Some((
                                State::DerefedBorrow(DerefedBorrow {
                                    // One of the required refs is for the current borrow expression, the remaining ones
                                    // can't be removed without breaking the code. See earlier comment.
                                    count: deref_count - required_refs,
                                    msg,
                                    stability,
                                    for_field_access: match use_cx.node {
                                        ExprUseNode::FieldAccess(name) => Some(name.name),
                                        _ => None,
                                    },
                                }),
                                StateData {
                                    span: expr.span,
                                    hir_id: expr.hir_id,
                                    adjusted_ty: use_cx.adjustments.last().map_or(expr_ty, |a| a.target),
                                },
                            ));
                        } else if stability.is_deref_stable()
                            // Auto-deref doesn't combine with other adjustments
                            && next_adjust.map_or(true, |a| matches!(a.kind, Adjust::Deref(_) | Adjust::Borrow(_)))
                            && iter.all(|a| matches!(a.kind, Adjust::Deref(_) | Adjust::Borrow(_)))
                        {
                            self.state = Some((
                                State::Borrow { mutability },
                                StateData {
                                    span: expr.span,
                                    hir_id: expr.hir_id,
                                    adjusted_ty: use_cx.adjustments.last().map_or(expr_ty, |a| a.target),
                                },
                            ));
                        }
                    },
                    (None, _) | (_, RefOp::Method { .. }) => (),
                }
            },
            (
                Some((
                    State::DerefMethod {
                        mutbl,
                        ty_changed_count,
                        ..
                    },
                    data,
                )),
                RefOp::Method { is_ufcs, .. },
            ) => {
                self.state = Some((
                    State::DerefMethod {
                        ty_changed_count: if deref_method_same_type(typeck.expr_ty(expr), typeck.expr_ty(sub_expr)) {
                            ty_changed_count
                        } else {
                            ty_changed_count + 1
                        },
                        is_ufcs,
                        mutbl,
                    },
                    data,
                ));
            },
            (Some((State::DerefedBorrow(state), data)), RefOp::AddrOf(_)) if state.count != 0 => {
                self.state = Some((
                    State::DerefedBorrow(DerefedBorrow {
                        count: state.count - 1,
                        ..state
                    }),
                    data,
                ));
            },
            (Some((State::DerefedBorrow(state), data)), RefOp::AddrOf(mutability)) => {
                let adjusted_ty = data.adjusted_ty;
                let stability = state.stability;
                report(cx, expr, State::DerefedBorrow(state), data);
                if stability.is_deref_stable() {
                    self.state = Some((
                        State::Borrow { mutability },
                        StateData {
                            span: expr.span,
                            hir_id: expr.hir_id,
                            adjusted_ty,
                        },
                    ));
                }
            },
            (Some((State::DerefedBorrow(state), data)), RefOp::Deref) => {
                let adjusted_ty = data.adjusted_ty;
                let stability = state.stability;
                let for_field_access = state.for_field_access;
                report(cx, expr, State::DerefedBorrow(state), data);
                if let Some(name) = for_field_access
                    && !ty_contains_field(typeck.expr_ty(sub_expr), name)
                {
                    self.state = Some((
                        State::ExplicitDerefField { name },
                        StateData {
                            span: expr.span,
                            hir_id: expr.hir_id,
                            adjusted_ty,
                        },
                    ));
                } else if stability.is_deref_stable()
                    && let Some(parent) = get_parent_expr(cx, expr)
                {
                    self.state = Some((
                        State::ExplicitDeref { mutability: None },
                        StateData {
                            span: parent.span,
                            hir_id: parent.hir_id,
                            adjusted_ty,
                        },
                    ));
                }
            },

            (Some((State::Borrow { mutability }, data)), RefOp::Deref) => {
                if typeck.expr_ty(sub_expr).is_ref() {
                    self.state = Some((State::Reborrow { mutability }, data));
                } else {
                    self.state = Some((
                        State::ExplicitDeref {
                            mutability: Some(mutability),
                        },
                        data,
                    ));
                }
            },
            (Some((State::Reborrow { mutability }, data)), RefOp::Deref) => {
                self.state = Some((
                    State::ExplicitDeref {
                        mutability: Some(mutability),
                    },
                    data,
                ));
            },
            (state @ Some((State::ExplicitDeref { .. }, _)), RefOp::Deref) => {
                self.state = state;
            },
            (Some((State::ExplicitDerefField { name }, data)), RefOp::Deref)
                if !ty_contains_field(typeck.expr_ty(sub_expr), name) =>
            {
                self.state = Some((State::ExplicitDerefField { name }, data));
            },

            (Some((state, data)), _) => report(cx, expr, state, data),
        }
    }

    fn check_pat(&mut self, cx: &LateContext<'tcx>, pat: &'tcx Pat<'_>) {
        if let PatKind::Binding(BindingAnnotation::REF, id, name, _) = pat.kind {
            if let Some(opt_prev_pat) = self.ref_locals.get_mut(&id) {
                // This binding id has been seen before. Add this pattern to the list of changes.
                if let Some(prev_pat) = opt_prev_pat {
                    if pat.span.from_expansion() {
                        // Doesn't match the context of the previous pattern. Can't lint here.
                        *opt_prev_pat = None;
                    } else {
                        prev_pat.spans.push(pat.span);
                        prev_pat.replacements.push((
                            pat.span,
                            snippet_with_context(cx, name.span, pat.span.ctxt(), "..", &mut prev_pat.app)
                                .0
                                .into(),
                        ));
                    }
                }
                return;
            }

            if_chain! {
                if !pat.span.from_expansion();
                if let ty::Ref(_, tam, _) = *cx.typeck_results().pat_ty(pat).kind();
                // only lint immutable refs, because borrowed `&mut T` cannot be moved out
                if let ty::Ref(_, _, Mutability::Not) = *tam.kind();
                then {
                    let mut app = Applicability::MachineApplicable;
                    let snip = snippet_with_context(cx, name.span, pat.span.ctxt(), "..", &mut app).0;
                    self.current_body = self.current_body.or(cx.enclosing_body);
                    self.ref_locals.insert(
                        id,
                        Some(RefPat {
                            always_deref: true,
                            spans: vec![pat.span],
                            app,
                            replacements: vec![(pat.span, snip.into())],
                            hir_id: pat.hir_id,
                        }),
                    );
                }
            }
        }
    }

    fn check_body_post(&mut self, cx: &LateContext<'tcx>, body: &'tcx Body<'_>) {
        if self.possible_borrowers.last().map_or(false, |&(local_def_id, _)| {
            local_def_id == cx.tcx.hir().body_owner_def_id(body.id())
        }) {
            self.possible_borrowers.pop();
        }

        if Some(body.id()) == self.current_body {
            for pat in self.ref_locals.drain(..).filter_map(|(_, x)| x) {
                let replacements = pat.replacements;
                let app = pat.app;
                let lint = if pat.always_deref {
                    NEEDLESS_BORROW
                } else {
                    REF_BINDING_TO_REFERENCE
                };
                span_lint_hir_and_then(
                    cx,
                    lint,
                    pat.hir_id,
                    pat.spans,
                    "this pattern creates a reference to a reference",
                    |diag| {
                        diag.multipart_suggestion("try", replacements, app);
                    },
                );
            }
            self.current_body = None;
        }
    }

    extract_msrv_attr!(LateContext);
}

fn try_parse_ref_op<'tcx>(
    tcx: TyCtxt<'tcx>,
    typeck: &'tcx TypeckResults<'_>,
    expr: &'tcx Expr<'_>,
) -> Option<(RefOp, &'tcx Expr<'tcx>)> {
    let (is_ufcs, def_id, arg) = match expr.kind {
        ExprKind::MethodCall(_, arg, [], _) => (false, typeck.type_dependent_def_id(expr.hir_id)?, arg),
        ExprKind::Call(
            Expr {
                kind: ExprKind::Path(path),
                hir_id,
                ..
            },
            [arg],
        ) => (true, typeck.qpath_res(path, *hir_id).opt_def_id()?, arg),
        ExprKind::Unary(UnOp::Deref, sub_expr) if !typeck.expr_ty(sub_expr).is_unsafe_ptr() => {
            return Some((RefOp::Deref, sub_expr));
        },
        ExprKind::AddrOf(BorrowKind::Ref, mutability, sub_expr) => return Some((RefOp::AddrOf(mutability), sub_expr)),
        _ => return None,
    };
    if tcx.is_diagnostic_item(sym::deref_method, def_id) {
        Some((
            RefOp::Method {
                mutbl: Mutability::Not,
                is_ufcs,
            },
            arg,
        ))
    } else if tcx.trait_of_item(def_id)? == tcx.lang_items().deref_mut_trait()? {
        Some((
            RefOp::Method {
                mutbl: Mutability::Mut,
                is_ufcs,
            },
            arg,
        ))
    } else {
        None
    }
}

// Checks whether the type for a deref call actually changed the type, not just the mutability of
// the reference.
fn deref_method_same_type<'tcx>(result_ty: Ty<'tcx>, arg_ty: Ty<'tcx>) -> bool {
    match (result_ty.kind(), arg_ty.kind()) {
        (ty::Ref(_, result_ty, _), ty::Ref(_, arg_ty, _)) => result_ty == arg_ty,

        // The result type for a deref method is always a reference
        // Not matching the previous pattern means the argument type is not a reference
        // This means that the type did change
        _ => false,
    }
}

fn path_has_args(p: &QPath<'_>) -> bool {
    match *p {
        QPath::Resolved(_, Path { segments: [.., s], .. }) | QPath::TypeRelative(_, s) => s.args.is_some(),
        _ => false,
    }
}

fn in_postfix_position<'tcx>(cx: &LateContext<'tcx>, e: &'tcx Expr<'tcx>) -> bool {
    if let Some(parent) = get_parent_expr(cx, e)
        && parent.span.ctxt() == e.span.ctxt()
    {
        match parent.kind {
            ExprKind::Call(child, _) | ExprKind::MethodCall(_, child, _, _) | ExprKind::Index(child, _, _)
                if child.hir_id == e.hir_id => true,
            ExprKind::Match(.., MatchSource::TryDesugar(_) | MatchSource::AwaitDesugar)
                | ExprKind::Field(_, _) => true,
            _ => false,
        }
    } else {
        false
    }
}

#[derive(Clone, Copy)]
enum TyCoercionStability {
    Deref,
    Reborrow,
    None,
}
impl TyCoercionStability {
    fn is_deref_stable(self) -> bool {
        matches!(self, Self::Deref)
    }

    fn is_reborrow_stable(self) -> bool {
        matches!(self, Self::Deref | Self::Reborrow)
    }

    fn for_defined_ty<'tcx>(cx: &LateContext<'tcx>, ty: DefinedTy<'tcx>, for_return: bool) -> Self {
        match ty {
            DefinedTy::Hir(ty) => Self::for_hir_ty(ty),
            DefinedTy::Mir(ty) => Self::for_mir_ty(
                cx.tcx,
                ty.param_env,
                cx.tcx.erase_late_bound_regions(ty.value),
                for_return,
            ),
        }
    }

    // Checks the stability of type coercions when assigned to a binding with the given explicit type.
    //
    // e.g.
    // let x = Box::new(Box::new(0u32));
    // let y1: &Box<_> = x.deref();
    // let y2: &Box<_> = &x;
    //
    // Here `y1` and `y2` would resolve to different types, so the type `&Box<_>` is not stable when
    // switching to auto-dereferencing.
    fn for_hir_ty<'tcx>(ty: &'tcx hir::Ty<'tcx>) -> Self {
        let TyKind::Ref(_, ty) = &ty.kind else {
            return Self::None;
        };
        let mut ty = ty;

        loop {
            break match ty.ty.kind {
                TyKind::Ref(_, ref ref_ty) => {
                    ty = ref_ty;
                    continue;
                },
                TyKind::Path(
                    QPath::TypeRelative(_, path)
                    | QPath::Resolved(
                        _,
                        Path {
                            segments: [.., path], ..
                        },
                    ),
                ) => {
                    if let Some(args) = path.args
                        && args.args.iter().any(|arg| match arg {
                            hir::GenericArg::Infer(_) => true,
                            hir::GenericArg::Type(ty) => ty_contains_infer(ty),
                            _ => false,
                        })
                    {
                        Self::Reborrow
                    } else {
                        Self::Deref
                    }
                },
                TyKind::Slice(_)
                | TyKind::Array(..)
                | TyKind::Ptr(_)
                | TyKind::BareFn(_)
                | TyKind::Never
                | TyKind::Tup(_)
                | TyKind::Path(_) => Self::Deref,
                TyKind::OpaqueDef(..)
                | TyKind::Infer
                | TyKind::Typeof(..)
                | TyKind::TraitObject(..)
                | TyKind::Err(_) => Self::Reborrow,
            };
        }
    }

    fn for_mir_ty<'tcx>(tcx: TyCtxt<'tcx>, param_env: ParamEnv<'tcx>, ty: Ty<'tcx>, for_return: bool) -> Self {
        let ty::Ref(_, mut ty, _) = *ty.kind() else {
            return Self::None;
        };

        ty = tcx.try_normalize_erasing_regions(param_env, ty).unwrap_or(ty);
        loop {
            break match *ty.kind() {
                ty::Ref(_, ref_ty, _) => {
                    ty = ref_ty;
                    continue;
                },
                ty::Param(_) if for_return => Self::Deref,
                ty::Alias(ty::Weak | ty::Inherent, _) => unreachable!("should have been normalized away above"),
                ty::Alias(ty::Projection, _) if !for_return && ty.has_non_region_param() => Self::Reborrow,
                ty::Infer(_)
                | ty::Error(_)
                | ty::Bound(..)
                | ty::Alias(ty::Opaque, ..)
                | ty::Placeholder(_)
                | ty::Dynamic(..)
                | ty::Param(_) => Self::Reborrow,
                ty::Adt(_, args)
                    if ty.has_placeholders()
                        || ty.has_opaque_types()
                        || (!for_return && args.has_non_region_param()) =>
                {
                    Self::Reborrow
                },
                ty::Bool
                | ty::Char
                | ty::Int(_)
                | ty::Uint(_)
                | ty::Array(..)
                | ty::Float(_)
                | ty::RawPtr(..)
                | ty::FnPtr(_)
                | ty::Str
                | ty::Slice(..)
                | ty::Adt(..)
                | ty::Foreign(_)
                | ty::FnDef(..)
                | ty::Generator(..)
                | ty::GeneratorWitness(..)
                | ty::GeneratorWitnessMIR(..)
                | ty::Closure(..)
                | ty::Never
                | ty::Tuple(_)
                | ty::Alias(ty::Projection, _) => Self::Deref,
            };
        }
    }
}

// Checks whether a type is inferred at some point.
// e.g. `_`, `Box<_>`, `[_]`
fn ty_contains_infer(ty: &hir::Ty<'_>) -> bool {
    struct V(bool);
    impl Visitor<'_> for V {
        fn visit_ty(&mut self, ty: &hir::Ty<'_>) {
            if self.0
                || matches!(
                    ty.kind,
                    TyKind::OpaqueDef(..) | TyKind::Infer | TyKind::Typeof(_) | TyKind::Err(_)
                )
            {
                self.0 = true;
            } else {
                walk_ty(self, ty);
            }
        }

        fn visit_generic_arg(&mut self, arg: &hir::GenericArg<'_>) {
            if self.0 || matches!(arg, hir::GenericArg::Infer(_)) {
                self.0 = true;
            } else if let hir::GenericArg::Type(ty) = arg {
                self.visit_ty(ty);
            }
        }
    }
    let mut v = V(false);
    v.visit_ty(ty);
    v.0
}

/// Checks for the number of borrow expressions which can be removed from the given expression
/// where the expression is used as an argument to a function expecting a generic type.
///
/// The following constraints will be checked:
/// * The borrowed expression meets all the generic type's constraints.
/// * The generic type appears only once in the functions signature.
/// * The borrowed value will not be moved if it is used later in the function.
#[expect(clippy::too_many_arguments)]
fn needless_borrow_generic_arg_count<'tcx>(
    cx: &LateContext<'tcx>,
    possible_borrowers: &mut Vec<(LocalDefId, PossibleBorrowerMap<'tcx, 'tcx>)>,
    fn_id: DefId,
    callee_args: &'tcx List<GenericArg<'tcx>>,
    arg_index: usize,
    param_ty: ParamTy,
    mut expr: &Expr<'tcx>,
    msrv: &Msrv,
) -> usize {
    let destruct_trait_def_id = cx.tcx.lang_items().destruct_trait();
    let sized_trait_def_id = cx.tcx.lang_items().sized_trait();

    let fn_sig = cx.tcx.fn_sig(fn_id).instantiate_identity().skip_binder();
    let predicates = cx.tcx.param_env(fn_id).caller_bounds();
    let projection_predicates = predicates
        .iter()
        .filter_map(|predicate| {
            if let ClauseKind::Projection(projection_predicate) = predicate.kind().skip_binder() {
                Some(projection_predicate)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    let mut trait_with_ref_mut_self_method = false;

    // If no traits were found, or only the `Destruct`, `Sized`, or `Any` traits were found, return.
    if predicates
        .iter()
        .filter_map(|predicate| {
            if let ClauseKind::Trait(trait_predicate) = predicate.kind().skip_binder()
                && trait_predicate.trait_ref.self_ty() == param_ty.to_ty(cx.tcx)
            {
                Some(trait_predicate.trait_ref.def_id)
            } else {
                None
            }
        })
        .inspect(|trait_def_id| {
            trait_with_ref_mut_self_method |= has_ref_mut_self_method(cx, *trait_def_id);
        })
        .all(|trait_def_id| {
            Some(trait_def_id) == destruct_trait_def_id
                || Some(trait_def_id) == sized_trait_def_id
                || cx.tcx.is_diagnostic_item(sym::Any, trait_def_id)
        })
    {
        return 0;
    }

    // See:
    // - https://github.com/rust-lang/rust-clippy/pull/9674#issuecomment-1289294201
    // - https://github.com/rust-lang/rust-clippy/pull/9674#issuecomment-1292225232
    if projection_predicates
        .iter()
        .any(|projection_predicate| is_mixed_projection_predicate(cx, fn_id, projection_predicate))
    {
        return 0;
    }

    // `args_with_referent_ty` can be constructed outside of `check_referent` because the same
    // elements are modified each time `check_referent` is called.
    let mut args_with_referent_ty = callee_args.to_vec();

    let mut check_reference_and_referent = |reference, referent| {
        let referent_ty = cx.typeck_results().expr_ty(referent);

        if !is_copy(cx, referent_ty)
            && (referent_ty.has_significant_drop(cx.tcx, cx.param_env)
                || !referent_used_exactly_once(cx, possible_borrowers, reference))
        {
            return false;
        }

        // https://github.com/rust-lang/rust-clippy/pull/9136#pullrequestreview-1037379321
        if trait_with_ref_mut_self_method && !matches!(referent_ty.kind(), ty::Ref(_, _, Mutability::Mut)) {
            return false;
        }

        if !replace_types(
            cx,
            param_ty,
            referent_ty,
            fn_sig,
            arg_index,
            &projection_predicates,
            &mut args_with_referent_ty,
        ) {
            return false;
        }

        predicates.iter().all(|predicate| {
            if let ClauseKind::Trait(trait_predicate) = predicate.kind().skip_binder()
                && cx.tcx.is_diagnostic_item(sym::IntoIterator, trait_predicate.trait_ref.def_id)
                && let ty::Param(param_ty) = trait_predicate.self_ty().kind()
                && let GenericArgKind::Type(ty) = args_with_referent_ty[param_ty.index as usize].unpack()
                && ty.is_array()
                && !msrv.meets(msrvs::ARRAY_INTO_ITERATOR)
            {
                return false;
            }

            let predicate = EarlyBinder::bind(predicate).instantiate(cx.tcx, &args_with_referent_ty);
            let obligation = Obligation::new(cx.tcx, ObligationCause::dummy(), cx.param_env, predicate);
            let infcx = cx.tcx.infer_ctxt().build();
            infcx.predicate_must_hold_modulo_regions(&obligation)
        })
    };

    let mut count = 0;
    while let ExprKind::AddrOf(_, _, referent) = expr.kind {
        if !check_reference_and_referent(expr, referent) {
            break;
        }
        expr = referent;
        count += 1;
    }
    count
}

fn has_ref_mut_self_method(cx: &LateContext<'_>, trait_def_id: DefId) -> bool {
    cx.tcx
        .associated_items(trait_def_id)
        .in_definition_order()
        .any(|assoc_item| {
            if assoc_item.fn_has_self_parameter {
                let self_ty = cx
                    .tcx
                    .fn_sig(assoc_item.def_id)
                    .instantiate_identity()
                    .skip_binder()
                    .inputs()[0];
                matches!(self_ty.kind(), ty::Ref(_, _, Mutability::Mut))
            } else {
                false
            }
        })
}

fn is_mixed_projection_predicate<'tcx>(
    cx: &LateContext<'tcx>,
    callee_def_id: DefId,
    projection_predicate: &ProjectionPredicate<'tcx>,
) -> bool {
    let generics = cx.tcx.generics_of(callee_def_id);
    // The predicate requires the projected type to equal a type parameter from the parent context.
    if let Some(term_ty) = projection_predicate.term.ty()
        && let ty::Param(term_param_ty) = term_ty.kind()
        && (term_param_ty.index as usize) < generics.parent_count
    {
        // The inner-most self type is a type parameter from the current function.
        let mut projection_ty = projection_predicate.projection_ty;
        loop {
            match projection_ty.self_ty().kind() {
                ty::Alias(ty::Projection, inner_projection_ty) => {
                    projection_ty = *inner_projection_ty;
                }
                ty::Param(param_ty) => {
                    return (param_ty.index as usize) >= generics.parent_count;
                }
                _ => {
                    return false;
                }
            }
        }
    } else {
        false
    }
}

fn referent_used_exactly_once<'tcx>(
    cx: &LateContext<'tcx>,
    possible_borrowers: &mut Vec<(LocalDefId, PossibleBorrowerMap<'tcx, 'tcx>)>,
    reference: &Expr<'tcx>,
) -> bool {
    if let Some(mir) = enclosing_mir(cx.tcx, reference.hir_id)
        && let Some(local) = expr_local(cx.tcx, reference)
        && let [location] = *local_assignments(mir, local).as_slice()
        && let Some(statement) = mir.basic_blocks[location.block].statements.get(location.statement_index)
        && let StatementKind::Assign(box (_, Rvalue::Ref(_, _, place))) = statement.kind
        && !place.is_indirect_first_projection()
        // Ensure not in a loop (https://github.com/rust-lang/rust-clippy/issues/9710)
        && TriColorDepthFirstSearch::new(&mir.basic_blocks).run_from(location.block, &mut CycleDetector).is_none()
    {
        let body_owner_local_def_id = cx.tcx.hir().enclosing_body_owner(reference.hir_id);
        if possible_borrowers
            .last()
            .map_or(true, |&(local_def_id, _)| local_def_id != body_owner_local_def_id)
        {
            possible_borrowers.push((body_owner_local_def_id, PossibleBorrowerMap::new(cx, mir)));
        }
        let possible_borrower = &mut possible_borrowers.last_mut().unwrap().1;
        // If `only_borrowers` were used here, the `copyable_iterator::warn` test would fail. The reason is
        // that `PossibleBorrowerVisitor::visit_terminator` considers `place.local` a possible borrower of
        // itself. See the comment in that method for an explanation as to why.
        possible_borrower.bounded_borrowers(&[local], &[local, place.local], place.local, location)
            && used_exactly_once(mir, place.local).unwrap_or(false)
    } else {
        false
    }
}

// Iteratively replaces `param_ty` with `new_ty` in `args`, and similarly for each resulting
// projected type that is a type parameter. Returns `false` if replacing the types would have an
// effect on the function signature beyond substituting `new_ty` for `param_ty`.
// See: https://github.com/rust-lang/rust-clippy/pull/9136#discussion_r927212757
fn replace_types<'tcx>(
    cx: &LateContext<'tcx>,
    param_ty: ParamTy,
    new_ty: Ty<'tcx>,
    fn_sig: FnSig<'tcx>,
    arg_index: usize,
    projection_predicates: &[ProjectionPredicate<'tcx>],
    args: &mut [ty::GenericArg<'tcx>],
) -> bool {
    let mut replaced = BitSet::new_empty(args.len());

    let mut deque = VecDeque::with_capacity(args.len());
    deque.push_back((param_ty, new_ty));

    while let Some((param_ty, new_ty)) = deque.pop_front() {
        // If `replaced.is_empty()`, then `param_ty` and `new_ty` are those initially passed in.
        if !fn_sig
            .inputs_and_output
            .iter()
            .enumerate()
            .all(|(i, ty)| (replaced.is_empty() && i == arg_index) || !ty.contains(param_ty.to_ty(cx.tcx)))
        {
            return false;
        }

        args[param_ty.index as usize] = ty::GenericArg::from(new_ty);

        // The `replaced.insert(...)` check provides some protection against infinite loops.
        if replaced.insert(param_ty.index) {
            for projection_predicate in projection_predicates {
                if projection_predicate.projection_ty.self_ty() == param_ty.to_ty(cx.tcx)
                    && let Some(term_ty) = projection_predicate.term.ty()
                    && let ty::Param(term_param_ty) = term_ty.kind()
                {
                    let projection = cx.tcx.mk_ty_from_kind(ty::Alias(
                        ty::Projection,
                        projection_predicate.projection_ty.with_self_ty(cx.tcx, new_ty),
                    ));

                    if let Ok(projected_ty) = cx.tcx.try_normalize_erasing_regions(cx.param_env, projection)
                        && args[term_param_ty.index as usize] != ty::GenericArg::from(projected_ty)
                    {
                        deque.push_back((*term_param_ty, projected_ty));
                    }
                }
            }
        }
    }

    true
}

fn ty_contains_field(ty: Ty<'_>, name: Symbol) -> bool {
    if let ty::Adt(adt, _) = *ty.kind() {
        adt.is_struct() && adt.all_fields().any(|f| f.name == name)
    } else {
        false
    }
}

#[expect(clippy::needless_pass_by_value, clippy::too_many_lines)]
fn report<'tcx>(cx: &LateContext<'tcx>, expr: &'tcx Expr<'_>, state: State, data: StateData<'tcx>) {
    match state {
        State::DerefMethod {
            ty_changed_count,
            is_ufcs,
            mutbl,
        } => {
            let mut app = Applicability::MachineApplicable;
            let (expr_str, _expr_is_macro_call) = snippet_with_context(cx, expr.span, data.span.ctxt(), "..", &mut app);
            let ty = cx.typeck_results().expr_ty(expr);
            let (_, ref_count) = peel_mid_ty_refs(ty);
            let deref_str = if ty_changed_count >= ref_count && ref_count != 0 {
                // a deref call changing &T -> &U requires two deref operators the first time
                // this occurs. One to remove the reference, a second to call the deref impl.
                "*".repeat(ty_changed_count + 1)
            } else {
                "*".repeat(ty_changed_count)
            };
            let addr_of_str = if ty_changed_count < ref_count {
                // Check if a reborrow from &mut T -> &T is required.
                if mutbl == Mutability::Not && matches!(ty.kind(), ty::Ref(_, _, Mutability::Mut)) {
                    "&*"
                } else {
                    ""
                }
            } else if mutbl == Mutability::Mut {
                "&mut "
            } else {
                "&"
            };

            // expr_str (the suggestion) is never shown if is_final_ufcs is true, since it's
            // `expr.kind == ExprKind::Call`. Therefore, this is, afaik, always unnecessary.
            /*
            expr_str = if !expr_is_macro_call && is_final_ufcs && expr.precedence().order() < PREC_PREFIX {
                Cow::Owned(format!("({expr_str})"))
            } else {
                expr_str
            };
            */

            // Fix #10850, do not lint if it's `Foo::deref` instead of `foo.deref()`.
            if is_ufcs {
                return;
            }

            span_lint_and_sugg(
                cx,
                EXPLICIT_DEREF_METHODS,
                data.span,
                match mutbl {
                    Mutability::Not => "explicit `deref` method call",
                    Mutability::Mut => "explicit `deref_mut` method call",
                },
                "try",
                format!("{addr_of_str}{deref_str}{expr_str}"),
                app,
            );
        },
        State::DerefedBorrow(state) => {
            let mut app = Applicability::MachineApplicable;
            let (snip, snip_is_macro) = snippet_with_context(cx, expr.span, data.span.ctxt(), "..", &mut app);
            span_lint_hir_and_then(cx, NEEDLESS_BORROW, data.hir_id, data.span, state.msg, |diag| {
                let (precedence, calls_field) = match get_parent_node(cx.tcx, data.hir_id) {
                    Some(Node::Expr(e)) => match e.kind {
                        ExprKind::Call(callee, _) if callee.hir_id != data.hir_id => (0, false),
                        ExprKind::Call(..) => (PREC_POSTFIX, matches!(expr.kind, ExprKind::Field(..))),
                        _ => (e.precedence().order(), false),
                    },
                    _ => (0, false),
                };
                let sugg = if !snip_is_macro
                    && (calls_field || expr.precedence().order() < precedence)
                    && !has_enclosing_paren(&snip)
                {
                    format!("({snip})")
                } else {
                    snip.into()
                };
                diag.span_suggestion(data.span, "change this to", sugg, app);
            });
        },
        State::ExplicitDeref { mutability } => {
            if matches!(
                expr.kind,
                ExprKind::Block(..)
                    | ExprKind::ConstBlock(_)
                    | ExprKind::If(..)
                    | ExprKind::Loop(..)
                    | ExprKind::Match(..)
            ) && let ty::Ref(_, ty, _) = data.adjusted_ty.kind()
                && ty.is_sized(cx.tcx, cx.param_env)
            {
                // Rustc bug: auto deref doesn't work on block expression when targeting sized types.
                return;
            }

            let (prefix, precedence) = if let Some(mutability) = mutability
                && !cx.typeck_results().expr_ty(expr).is_ref()
            {
                let prefix = match mutability {
                    Mutability::Not => "&",
                    Mutability::Mut => "&mut ",
                };
                (prefix, PREC_PREFIX)
            } else {
                ("", 0)
            };
            span_lint_hir_and_then(
                cx,
                EXPLICIT_AUTO_DEREF,
                data.hir_id,
                data.span,
                "deref which would be done by auto-deref",
                |diag| {
                    let mut app = Applicability::MachineApplicable;
                    let (snip, snip_is_macro) = snippet_with_context(cx, expr.span, data.span.ctxt(), "..", &mut app);
                    let sugg =
                        if !snip_is_macro && expr.precedence().order() < precedence && !has_enclosing_paren(&snip) {
                            format!("{prefix}({snip})")
                        } else {
                            format!("{prefix}{snip}")
                        };
                    diag.span_suggestion(data.span, "try", sugg, app);
                },
            );
        },
        State::ExplicitDerefField { .. } => {
            if matches!(
                expr.kind,
                ExprKind::Block(..)
                    | ExprKind::ConstBlock(_)
                    | ExprKind::If(..)
                    | ExprKind::Loop(..)
                    | ExprKind::Match(..)
            ) && data.adjusted_ty.is_sized(cx.tcx, cx.param_env)
            {
                // Rustc bug: auto deref doesn't work on block expression when targeting sized types.
                return;
            }

            span_lint_hir_and_then(
                cx,
                EXPLICIT_AUTO_DEREF,
                data.hir_id,
                data.span,
                "deref which would be done by auto-deref",
                |diag| {
                    let mut app = Applicability::MachineApplicable;
                    let snip = snippet_with_context(cx, expr.span, data.span.ctxt(), "..", &mut app).0;
                    diag.span_suggestion(data.span, "try", snip.into_owned(), app);
                },
            );
        },
        State::Borrow { .. } | State::Reborrow { .. } => (),
    }
}

impl<'tcx> Dereferencing<'tcx> {
    fn check_local_usage(&mut self, cx: &LateContext<'tcx>, e: &Expr<'tcx>, local: HirId) {
        if let Some(outer_pat) = self.ref_locals.get_mut(&local) {
            if let Some(pat) = outer_pat {
                // Check for auto-deref
                if !matches!(
                    cx.typeck_results().expr_adjustments(e),
                    [
                        Adjustment {
                            kind: Adjust::Deref(_),
                            ..
                        },
                        Adjustment {
                            kind: Adjust::Deref(_),
                            ..
                        },
                        ..
                    ]
                ) {
                    match get_parent_expr(cx, e) {
                        // Field accesses are the same no matter the number of references.
                        Some(Expr {
                            kind: ExprKind::Field(..),
                            ..
                        }) => (),
                        Some(&Expr {
                            span,
                            kind: ExprKind::Unary(UnOp::Deref, _),
                            ..
                        }) if !span.from_expansion() => {
                            // Remove explicit deref.
                            let snip = snippet_with_context(cx, e.span, span.ctxt(), "..", &mut pat.app).0;
                            pat.replacements.push((span, snip.into()));
                        },
                        Some(parent) if !parent.span.from_expansion() => {
                            // Double reference might be needed at this point.
                            if parent.precedence().order() == PREC_POSTFIX {
                                // Parentheses would be needed here, don't lint.
                                *outer_pat = None;
                            } else {
                                pat.always_deref = false;
                                let snip = snippet_with_context(cx, e.span, parent.span.ctxt(), "..", &mut pat.app).0;
                                pat.replacements.push((e.span, format!("&{snip}")));
                            }
                        },
                        _ if !e.span.from_expansion() => {
                            // Double reference might be needed at this point.
                            pat.always_deref = false;
                            let snip = snippet_with_applicability(cx, e.span, "..", &mut pat.app);
                            pat.replacements.push((e.span, format!("&{snip}")));
                        },
                        // Edge case for macros. The span of the identifier will usually match the context of the
                        // binding, but not if the identifier was created in a macro. e.g. `concat_idents` and proc
                        // macros
                        _ => *outer_pat = None,
                    }
                }
            }
        }
    }
}
