use rustc_errors::DiagnosticBuilder;
use rustc_middle::lint::LintDiagnosticBuilder;
use rustc_middle::mir::visit::Visitor;
use rustc_middle::mir::*;
use rustc_middle::ty::TyCtxt;
use rustc_session::lint::builtin::CONST_ITEM_MUTATION;
use rustc_span::def_id::DefId;

use crate::transform::{MirPass, MirSource};

pub struct CheckConstItemMutation;

impl<'tcx> MirPass<'tcx> for CheckConstItemMutation {
    fn run_pass(&self, tcx: TyCtxt<'tcx>, _src: MirSource<'tcx>, body: &mut Body<'tcx>) {
        let mut checker = ConstMutationChecker { body, tcx, target_local: None };
        checker.visit_body(&body);
    }
}

struct ConstMutationChecker<'a, 'tcx> {
    body: &'a Body<'tcx>,
    tcx: TyCtxt<'tcx>,
    target_local: Option<Local>,
}

impl<'a, 'tcx> ConstMutationChecker<'a, 'tcx> {
    fn is_const_item(&self, local: Local) -> Option<DefId> {
        if let Some(box LocalInfo::ConstRef { def_id }) = self.body.local_decls[local].local_info {
            Some(def_id)
        } else {
            None
        }
    }
    fn lint_const_item_usage(
        &self,
        const_item: DefId,
        location: Location,
        decorate: impl for<'b> FnOnce(LintDiagnosticBuilder<'b>) -> DiagnosticBuilder<'b>,
    ) {
        let source_info = self.body.source_info(location);
        let lint_root = self.body.source_scopes[source_info.scope]
            .local_data
            .as_ref()
            .assert_crate_local()
            .lint_root;

        self.tcx.struct_span_lint_hir(CONST_ITEM_MUTATION, lint_root, source_info.span, |lint| {
            decorate(lint)
                .span_note(self.tcx.def_span(const_item), "`const` item defined here")
                .emit()
        });
    }
}

impl<'a, 'tcx> Visitor<'tcx> for ConstMutationChecker<'a, 'tcx> {
    fn visit_statement(&mut self, stmt: &Statement<'tcx>, loc: Location) {
        if let StatementKind::Assign(box (lhs, _)) = &stmt.kind {
            // Check for assignment to fields of a constant
            // Assigning directly to a constant (e.g. `FOO = true;`) is a hard error,
            // so emitting a lint would be redundant.
            if !lhs.projection.is_empty() {
                if let Some(def_id) = self.is_const_item(lhs.local) {
                    self.lint_const_item_usage(def_id, loc, |lint| {
                        let mut lint = lint.build("attempting to modify a `const` item");
                        lint.note("each usage of a `const` item creates a new temporary - the original `const` item will not be modified");
                        lint
                    })
                }
            }
            // We are looking for MIR of the form:
            //
            // ```
            // _1 = const FOO;
            // _2 = &mut _1;
            // method_call(_2, ..)
            // ```
            //
            // Record our current LHS, so that we can detect this
            // pattern in `visit_rvalue`
            self.target_local = lhs.as_local();
        }
        self.super_statement(stmt, loc);
        self.target_local = None;
    }
    fn visit_rvalue(&mut self, rvalue: &Rvalue<'tcx>, loc: Location) {
        if let Rvalue::Ref(_, BorrowKind::Mut { .. }, place) = rvalue {
            let local = place.local;
            if let Some(def_id) = self.is_const_item(local) {
                // If this Rvalue is being used as the right-hand side of a
                // `StatementKind::Assign`, see if it ends up getting used as
                // the `self` parameter of a method call (as the terminator of our current
                // BasicBlock). If so, we emit a more specific lint.
                let method_did = self.target_local.and_then(|target_local| {
                    crate::util::find_self_call(self.tcx, &self.body, target_local, loc.block)
                });
                let lint_loc =
                    if method_did.is_some() { self.body.terminator_loc(loc.block) } else { loc };
                self.lint_const_item_usage(def_id, lint_loc, |lint| {
                    let mut lint = lint.build("taking a mutable reference to a `const` item");
                    lint
                        .note("each usage of a `const` item creates a new temporary")
                        .note("the mutable reference will refer to this temporary, not the original `const` item");

                    if let Some(method_did) = method_did {
                        lint.span_note(self.tcx.def_span(method_did), "mutable reference created due to call to this method");
                    }

                    lint
                });
            }
        }
        self.super_rvalue(rvalue, loc);
    }
}
