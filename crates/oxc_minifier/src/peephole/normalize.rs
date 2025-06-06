use oxc_allocator::{TakeIn, Vec};
use oxc_ast::ast::*;
use oxc_ecmascript::{
    constant_evaluation::{DetermineValueType, ValueType},
    side_effects::MayHaveSideEffects,
};
use oxc_semantic::IsGlobalReference;
use oxc_span::GetSpan;
use oxc_syntax::scope::ScopeFlags;
use oxc_traverse::{Ancestor, ReusableTraverseCtx, Traverse, TraverseCtx, traverse_mut_with_ctx};

use crate::{CompressOptions, ctx::Ctx};

#[derive(Default)]
pub struct NormalizeOptions {
    pub convert_while_to_fors: bool,
    pub convert_const_to_let: bool,
}

/// Normalize AST
///
/// Make subsequent AST passes easier to analyze:
///
/// * remove `Statement::EmptyStatement`
/// * remove `ParenthesizedExpression`
/// * convert whiles to fors
/// * convert `const` to `let` for non-exported variables
/// * convert `Infinity` to `f64::INFINITY`
/// * convert `NaN` and `Number.NaN` to `f64::NaN`
/// * convert `var x; void x` to `void 0`
/// * convert `undefined` to `void 0`
/// * apply `pure` to side-effect free global constructors (e.g. `new WeakMap()`)
///
/// Also
///
/// * remove `debugger` and `console.log` (optional)
///
/// <https://github.com/google/closure-compiler/blob/v20240609/src/com/google/javascript/jscomp/Normalize.java>
pub struct Normalize {
    options: NormalizeOptions,
    compress_options: CompressOptions,
}

impl<'a> Normalize {
    pub fn build(&mut self, program: &mut Program<'a>, ctx: &mut ReusableTraverseCtx<'a>) {
        traverse_mut_with_ctx(self, program, ctx);
    }
}

impl<'a> Traverse<'a> for Normalize {
    fn exit_statements(&mut self, stmts: &mut Vec<'a, Statement<'a>>, _ctx: &mut TraverseCtx<'a>) {
        stmts.retain(|stmt| {
            !(matches!(stmt, Statement::EmptyStatement(_))
                || self.drop_debugger(stmt)
                || self.drop_console(stmt))
        });
    }

    fn exit_variable_declaration(
        &mut self,
        decl: &mut VariableDeclaration<'a>,
        ctx: &mut TraverseCtx<'a>,
    ) {
        if self.options.convert_const_to_let {
            Self::convert_const_to_let(decl, ctx);
        }
    }

    fn exit_statement(&mut self, stmt: &mut Statement<'a>, ctx: &mut TraverseCtx<'a>) {
        match stmt {
            Statement::WhileStatement(_) if self.options.convert_while_to_fors => {
                Self::convert_while_to_for(stmt, ctx);
            }
            _ => {}
        }
    }

    fn exit_expression(&mut self, expr: &mut Expression<'a>, ctx: &mut TraverseCtx<'a>) {
        if let Expression::ParenthesizedExpression(paren_expr) = expr {
            *expr = paren_expr.expression.take_in(ctx.ast);
        }
        if let Some(e) = match expr {
            Expression::Identifier(ident) => Self::try_compress_identifier(ident, ctx),
            Expression::UnaryExpression(e) if e.operator.is_void() => {
                Self::fold_void_ident(e, ctx);
                None
            }
            Expression::ArrowFunctionExpression(e) => {
                self.recover_arrow_expression_after_drop_console(e);
                None
            }
            Expression::CallExpression(_) if self.compress_options.drop_console => {
                self.compress_console(expr, ctx)
            }
            Expression::StaticMemberExpression(e) => Self::fold_number_nan_to_nan(e, ctx),
            _ => None,
        } {
            *expr = e;
        }
    }

    fn exit_call_expression(&mut self, e: &mut CallExpression<'a>, ctx: &mut TraverseCtx<'a>) {
        Self::set_no_side_effects_to_call_expr(e, ctx);
    }

    fn exit_new_expression(&mut self, e: &mut NewExpression<'a>, ctx: &mut TraverseCtx<'a>) {
        Self::set_pure_or_no_side_effects_to_new_expr(e, ctx);
    }
}

impl<'a> Normalize {
    pub fn new(options: NormalizeOptions, compress_options: CompressOptions) -> Self {
        Self { options, compress_options }
    }

    /// Drop `drop_debugger` statement.
    ///
    /// Enabled by `compress.drop_debugger`
    fn drop_debugger(&self, stmt: &Statement<'a>) -> bool {
        matches!(stmt, Statement::DebuggerStatement(_)) && self.compress_options.drop_debugger
    }

    fn compress_console(
        &self,
        expr: &Expression<'a>,
        ctx: &TraverseCtx<'a>,
    ) -> Option<Expression<'a>> {
        debug_assert!(self.compress_options.drop_console);
        Self::is_console(expr).then(|| ctx.ast.void_0(expr.span()))
    }

    fn drop_console(&self, stmt: &Statement<'a>) -> bool {
        self.compress_options.drop_console
            && matches!(stmt, Statement::ExpressionStatement(expr) if Self::is_console(&expr.expression))
    }

    fn recover_arrow_expression_after_drop_console(&self, expr: &mut ArrowFunctionExpression<'a>) {
        if self.compress_options.drop_console && expr.expression && expr.body.is_empty() {
            expr.expression = false;
        }
    }

    fn is_console(expr: &Expression<'_>) -> bool {
        let Expression::CallExpression(call_expr) = &expr else { return false };
        let Some(member_expr) = call_expr.callee.as_member_expression() else { return false };
        let obj = member_expr.object();
        let Some(ident) = obj.get_identifier_reference() else { return false };
        ident.name == "console"
    }

    fn convert_while_to_for(stmt: &mut Statement<'a>, ctx: &mut TraverseCtx<'a>) {
        let Statement::WhileStatement(while_stmt) = stmt.take_in(ctx.ast) else { return };
        let while_stmt = while_stmt.unbox();
        let for_stmt = ctx.ast.alloc_for_statement_with_scope_id(
            while_stmt.span,
            None,
            Some(while_stmt.test),
            None,
            while_stmt.body,
            ctx.create_child_scope_of_current(ScopeFlags::empty()),
        );
        *stmt = Statement::ForStatement(for_stmt);
    }

    fn convert_const_to_let(decl: &mut VariableDeclaration<'a>, ctx: &TraverseCtx<'a>) {
        // checking whether the current scope is the root scope instead of
        // checking whether any variables are exposed to outside (e.g. `export` in ESM)
        if decl.kind.is_const() && ctx.current_scope_id() != ctx.scoping().root_scope_id() {
            let all_declarations_are_only_read =
                decl.declarations.iter().flat_map(|d| d.id.get_binding_identifiers()).all(|id| {
                    ctx.scoping()
                        .get_resolved_references(id.symbol_id())
                        .all(|reference| reference.flags().is_read_only())
                });
            if all_declarations_are_only_read {
                decl.kind = VariableDeclarationKind::Let;
            }
            for decl in &mut decl.declarations {
                decl.kind = VariableDeclarationKind::Let;
            }
        }
    }

    /// Transforms `undefined` => `void 0`, `Infinity` => `f64::Infinity`, `NaN` -> `f64::NaN`.
    /// So subsequent passes don't need to look up whether these variables are shadowed or not.
    fn try_compress_identifier(
        ident: &IdentifierReference<'a>,
        ctx: &TraverseCtx<'a>,
    ) -> Option<Expression<'a>> {
        match ident.name.as_str() {
            "undefined" if ident.is_global_reference(ctx.scoping()) => {
                // `delete undefined` returns `false`
                // `delete void 0` returns `true`
                if Self::is_unary_delete_ancestor(ctx.ancestors()) {
                    return None;
                }
                Some(ctx.ast.void_0(ident.span))
            }
            "Infinity" if ident.is_global_reference(ctx.scoping()) => {
                // `delete Infinity` returns `false`
                // `delete 1/0` returns `true`
                if Self::is_unary_delete_ancestor(ctx.ancestors()) {
                    return None;
                }
                Some(ctx.ast.expression_numeric_literal(
                    ident.span,
                    f64::INFINITY,
                    None,
                    NumberBase::Decimal,
                ))
            }
            "NaN" if ident.is_global_reference(ctx.scoping()) => {
                // `delete NaN` returns `false`
                // `delete 0/0` returns `true`
                if Self::is_unary_delete_ancestor(ctx.ancestors()) {
                    return None;
                }
                Some(ctx.ast.nan(ident.span))
            }
            _ => None,
        }
    }

    fn is_unary_delete_ancestor<'t>(ancestors: impl Iterator<Item = Ancestor<'a, 't>>) -> bool {
        for ancestor in ancestors {
            match ancestor {
                Ancestor::UnaryExpressionArgument(e) if e.operator().is_delete() => {
                    return true;
                }
                Ancestor::ParenthesizedExpressionExpression(_)
                | Ancestor::SequenceExpressionExpressions(_) => {}
                _ => return false,
            }
        }
        false
    }

    fn fold_void_ident(e: &mut UnaryExpression<'a>, ctx: &TraverseCtx<'a>) {
        debug_assert!(e.operator.is_void());
        let Expression::Identifier(ident) = &e.argument else { return };
        if Ctx(ctx).is_global_reference(ident) {
            return;
        }
        e.argument = ctx.ast.expression_numeric_literal(ident.span, 0.0, None, NumberBase::Decimal);
    }

    fn fold_number_nan_to_nan(
        e: &StaticMemberExpression<'a>,
        ctx: &TraverseCtx<'a>,
    ) -> Option<Expression<'a>> {
        let Expression::Identifier(ident) = &e.object else { return None };
        if ident.name != "Number" {
            return None;
        }
        if e.property.name != "NaN" {
            return None;
        }
        if !Ctx(ctx).is_global_reference(ident) {
            return None;
        }
        Some(ctx.ast.nan(ident.span))
    }

    fn set_no_side_effects_to_call_expr(call_expr: &mut CallExpression<'a>, ctx: &TraverseCtx<'a>) {
        if call_expr.pure {
            return;
        }
        let Some(ident) = call_expr.callee.get_identifier_reference() else {
            return;
        };
        if let Some(symbol_id) = ctx.scoping().get_reference(ident.reference_id()).symbol_id() {
            // Apply `/* #__NO_SIDE_EFFECTS__ */`
            if ctx.scoping().no_side_effects().contains(&symbol_id) {
                call_expr.pure = true;
            }
        }
    }

    fn set_pure_or_no_side_effects_to_new_expr(
        new_expr: &mut NewExpression<'a>,
        ctx: &TraverseCtx<'a>,
    ) {
        if new_expr.pure {
            return;
        }
        let Some(ident) = new_expr.callee.get_identifier_reference() else {
            return;
        };
        if let Some(symbol_id) = ctx.scoping().get_reference(ident.reference_id()).symbol_id() {
            // Apply `/* #__NO_SIDE_EFFECTS__ */`
            if ctx.scoping().no_side_effects().contains(&symbol_id) {
                new_expr.pure = true;
            }
            return;
        }
        // callee is a global reference.
        let ctx = Ctx(ctx);
        let len = new_expr.arguments.len();
        if match ident.name.as_str() {
            "WeakSet" | "WeakMap" if ctx.is_global_reference(ident) => match len {
                0 => true,
                1 => match new_expr.arguments[0].as_expression() {
                    Some(Expression::NullLiteral(_)) => true,
                    Some(Expression::ArrayExpression(e)) => e.elements.is_empty(),
                    Some(e) if ctx.is_expression_undefined(e) => true,
                    _ => false,
                },
                _ => false,
            },
            "Date" if ctx.is_global_reference(ident) => match len {
                0 => true,
                1 => {
                    let Some(arg) = new_expr.arguments[0].as_expression() else { return };
                    let ty = arg.value_type(&ctx);
                    matches!(
                        ty,
                        ValueType::Null
                            | ValueType::Undefined
                            | ValueType::Boolean
                            | ValueType::Number
                            | ValueType::String
                    ) && !arg.may_have_side_effects(&ctx)
                }
                _ => false,
            },
            "Set" | "Map" if ctx.is_global_reference(ident) => match len {
                0 => true,
                1 => match new_expr.arguments[0].as_expression() {
                    Some(Expression::NullLiteral(_)) => true,
                    Some(e) if ctx.is_expression_undefined(e) => true,
                    _ => false,
                },
                _ => false,
            },
            _ => false,
        } {
            new_expr.pure = true;
        }
    }
}

#[cfg(test)]
mod test {
    use crate::tester::{test, test_same};

    #[test]
    fn test_while() {
        // Verify while loops are converted to FOR loops.
        test("while(c < b) foo()", "for(; c < b;) foo()");
    }

    #[test]
    fn test_const_to_let() {
        test_same("const x = 1"); // keep top-level (can be replaced with "let" if it's ESM and not exported)
        test("{ const x = 1 }", "{ let x = 1 }");
        test_same("{ const x = 1; x = 2 }"); // keep assign error
        test("{ const x = 1, y = 2 }", "{ let x = 1, y = 2 }");
        test("{ const { x } = { x: 1 } }", "{ let { x } = { x: 1 } }");
        test("{ const [x] = [1] }", "{ let [x] = [1] }");
        test("{ const [x = 1] = [] }", "{ let [x = 1] = [] }");
        test("for (const x in y);", "for (let x in y);");
        // TypeError: Assignment to constant variable.
        test_same("for (const i = 0; i < 1; i++);");
        test_same("for (const x in [1, 2, 3]) x++");
        test_same("for (const x of [1, 2, 3]) x++");
        test("{ let foo; const bar = undefined; }", "{ let foo, bar; }");
    }

    #[test]
    fn test_void_ident() {
        test("var x; void x", "var x");
        test("void x", "x"); // reference error
    }

    #[test]
    fn parens() {
        test("(((x)))", "x");
        test("(((a + b))) * c", "(a + b) * c");
    }

    #[test]
    fn drop_console() {
        test("console.log()", "");
        test("(() => console.log())()", "");
        test(
            "(() => { try { return console.log() } catch {} })()",
            "(() => { try { return } catch {} })()",
        );
    }

    #[test]
    fn drop_debugger() {
        test("debugger", "");
    }

    #[test]
    fn fold_number_nan() {
        test("foo(Number.NaN)", "foo(NaN)");
        test_same("let Number; foo(Number.NaN)");
    }
}
