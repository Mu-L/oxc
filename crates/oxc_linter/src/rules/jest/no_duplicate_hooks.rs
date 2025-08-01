use oxc_ast::AstKind;
use oxc_diagnostics::OxcDiagnostic;
use oxc_macros::declare_oxc_lint;
use oxc_semantic::NodeId;
use oxc_span::Span;
use rustc_hash::FxHashMap;

use crate::{
    context::LintContext,
    rule::Rule,
    utils::{
        JestFnKind, JestGeneralFnKind, ParsedJestFnCallNew, PossibleJestNode,
        collect_possible_jest_call_node, parse_jest_fn_call,
    },
};

fn no_duplicate_hooks_diagnostic(hook_name: &str, span: Span) -> OxcDiagnostic {
    OxcDiagnostic::warn(format!("Duplicate {hook_name:?} in describe block."))
        .with_help("Describe blocks can only have one of each hook. Consider consolidating the duplicate hooks into a single call.")
        .with_label(span)
}

#[derive(Debug, Default, Clone)]
pub struct NoDuplicateHooks;

declare_oxc_lint!(
    /// ### What it does
    ///
    /// Disallows duplicate hooks in describe blocks.
    ///
    /// ### Why is this bad?
    ///
    /// Having duplicate hooks in a describe block can lead to confusion and unexpected behavior.
    /// When multiple hooks of the same type exist, they all execute in order, which can make it
    /// difficult to understand the test setup flow and may result in redundant or conflicting
    /// operations. This makes tests harder to maintain and debug.
    ///
    /// ### Examples
    ///
    /// Examples of **incorrect** code for this rule:
    /// ```javascript
    /// describe('foo', () => {
    ///     beforeEach(() => {
    ///         // some setup
    ///     });
    ///     beforeEach(() => {
    ///         // some setup
    ///     });
    ///     test('foo_test', () => {
    ///         // some test
    ///     });
    /// });
    ///
    /// // Nested describe scenario
    /// describe('foo', () => {
    ///     beforeEach(() => {
    ///         // some setup
    ///     });
    ///     test('foo_test', () => {
    ///         // some test
    ///     });
    ///     describe('bar', () => {
    ///         test('bar_test', () => {
    ///             afterAll(() => {
    ///                 // some teardown
    ///             });
    ///             afterAll(() => {
    ///                 // some teardown
    ///             });
    ///         });
    ///     });
    /// });
    /// ```
    ///
    /// Examples of **correct** code for this rule:
    /// ```javascript
    /// describe('foo', () => {
    ///     beforeEach(() => {
    ///         // some setup
    ///     });
    ///     test('foo_test', () => {
    ///         // some test
    ///     });
    /// });
    ///
    /// // Nested describe scenario
    /// describe('foo', () => {
    ///     beforeEach(() => {
    ///         // some setup
    ///     });
    ///     test('foo_test', () => {
    ///         // some test
    ///     });
    ///     describe('bar', () => {
    ///         test('bar_test', () => {
    ///             beforeEach(() => {
    ///                 // some setup
    ///             });
    ///         });
    ///     });
    /// });
    /// ```
    NoDuplicateHooks,
    jest,
    style,
);

impl Rule for NoDuplicateHooks {
    fn run_once(&self, ctx: &LintContext) {
        let mut hook_contexts: FxHashMap<NodeId, Vec<FxHashMap<String, i32>>> =
            FxHashMap::default();
        hook_contexts.insert(NodeId::ROOT, Vec::new());

        let mut possibles_jest_nodes = collect_possible_jest_call_node(ctx);
        possibles_jest_nodes.sort_by_key(|n| n.node.id());

        for possible_jest_node in possibles_jest_nodes {
            Self::run(&possible_jest_node, NodeId::ROOT, &mut hook_contexts, ctx);
        }
    }
}

impl NoDuplicateHooks {
    fn run<'a>(
        possible_jest_node: &PossibleJestNode<'a, '_>,
        root_node_id: NodeId,
        hook_contexts: &mut FxHashMap<NodeId, Vec<FxHashMap<String, i32>>>,
        ctx: &LintContext<'a>,
    ) {
        let node = possible_jest_node.node;
        let AstKind::CallExpression(call_expr) = node.kind() else {
            return;
        };
        let Some(ParsedJestFnCallNew::GeneralJest(jest_fn_call)) =
            parse_jest_fn_call(call_expr, possible_jest_node, ctx)
        else {
            return;
        };

        if matches!(jest_fn_call.kind, JestFnKind::General(JestGeneralFnKind::Describe)) {
            hook_contexts.insert(node.id(), Vec::default());
        }

        if !matches!(jest_fn_call.kind, JestFnKind::General(JestGeneralFnKind::Hook)) {
            return;
        }

        let hook_name = jest_fn_call.name.to_string();
        let parent_id = ctx
            .nodes()
            .ancestor_ids(node.id())
            .find(|n| hook_contexts.contains_key(n))
            .unwrap_or(root_node_id);

        let Some(contexts) = hook_contexts.get_mut(&parent_id) else {
            return;
        };
        let last_context = if let Some(val) = contexts.last_mut() {
            Some(val)
        } else {
            let mut context = FxHashMap::default();
            context.insert(hook_name.clone(), 0);
            contexts.push(context);
            contexts.last_mut()
        };
        let Some(last_context) = last_context else {
            return;
        };
        let Some((_, count)) = last_context.get_key_value(&hook_name) else {
            last_context.insert(hook_name, 1);
            return;
        };

        if *count > 0 {
            ctx.diagnostic(no_duplicate_hooks_diagnostic(
                jest_fn_call.name.to_string().as_str(),
                call_expr.span,
            ));
        } else {
            last_context.insert(hook_name, 1);
        }
    }
}

#[test]
fn test() {
    use crate::tester::Tester;

    let mut pass = vec![
        (
            "
                describe(\"foo\", () => {
                    beforeEach(() => {})
                    test(\"bar\", () => {
                        someFn();
                    })
                })
            ",
            None,
        ),
        (
            "
                beforeEach(() => {})
                test(\"bar\", () => {
                    someFn();
                })
            ",
            None,
        ),
        (
            "
                describe(\"foo\", () => {
                    beforeAll(() => {}),
                    beforeEach(() => {})
                    afterEach(() => {})
                    afterAll(() => {})

                    test(\"bar\", () => {
                        someFn();
                    })
                })
            ",
            None,
        ),
        (
            "
                describe.skip(\"foo\", () => {
                    beforeEach(() => {}),
                    beforeAll(() => {}),
                    test(\"bar\", () => {
                        someFn();
                    })
                })
                describe(\"foo\", () => {
                    beforeEach(() => {}),
                    afterAll(() => {}),
                    test(\"bar\", () => {
                        someFn();
                    })
                })
            ",
            None,
        ),
        (
            "
                describe(\"foo\", () => {
                    beforeEach(() => {}),
                    test(\"bar\", () => {
                        someFn();
                    })
                    describe(\"inner_foo\", () => {
                        beforeEach(() => {})
                        test(\"inner bar\", () => {
                            someFn();
                        })
                    })
                })
            ",
            None,
        ),
        (
            "
                describe.each(['hello'])('%s', () => {
                    beforeEach(() => {});

                    it(\"is fine\", () => {});
                });
            ",
            None,
        ),
        (
            "
                describe(\"something\", () => {
                    describe.each(['hello'])('%s', () => {
                        beforeEach(() => {});

                        it('is fine', () => {});
                    });

                    describe.each(['world'])('%s', () => {
                        beforeEach(() => {});

                        it('is fine', () => {});
                    });
                });
            ",
            None,
        ),
        (
            "
                describe.each``('%s', () => {
                    beforeEach(() => {});

                    it(\"is fine\", () => {});
                });
            ",
            None,
        ),
        (
            "
                describe(\"something\", () => {
                    describe.each``(\"%s\", () => {
                        beforeEach(() => {});

                        it('is fine', () => {});
                    });

                    describe.each``('%s', () => {
                        beforeEach(() => {});

                        it('is fine', () => {});
                    });
                });
            ",
            None,
        ),
    ];

    let mut fail = vec![
        (
            "
                describe(\"foo\", () => {
                    beforeEach(() => {});
                    beforeEach(() => {});
                    test(\"bar\", () => {
                        someFn();
                    })
                })
            ",
            None,
        ),
        (
            "
                describe.skip(\"foo\", () => {
                    beforeEach(() => {}),
                    beforeAll(() => {}),
                    beforeAll(() => {}),
                    test(\"bar\", () => {
                        someFn();
                    })
                })
            ",
            None,
        ),
        (
            "
                describe.skip(\"foo\", () => {
                    afterEach(() => {}),
                    afterEach(() => {}),
                    test(\"bar\", () => {
                        someFn();
                    })
                })
            ",
            None,
        ),
        (
            "
                import { afterEach } from '@jest/globals';

                describe.skip(\"foo\", () => {
                    afterEach(() => {}),
                    afterEach(() => {}),
                    test(\"bar\", () => {
                        someFn();
                    })
                })
            ",
            None,
        ),
        (
            "
                import { afterEach, afterEach as somethingElse } from '@jest/globals';

                describe.skip(\"foo\", () => {
                    afterEach(() => {}),
                    somethingElse(() => {}),
                    test(\"bar\", () => {
                        someFn();
                    })
                })
            ",
            None,
        ),
        (
            "
                describe.skip(\"foo\", () => {
                    afterAll(() => {}),
                    afterAll(() => {}),
                    test(\"bar\", () => {
                        someFn();
                    })
                })
            ",
            None,
        ),
        (
            "
                afterAll(() => {}),
                afterAll(() => {}),
                test(\"bar\", () => {
                    someFn();
                })
            ",
            None,
        ),
        (
            "
                describe(\"foo\", () => {
                    beforeEach(() => {}),
                    beforeEach(() => {}),
                    beforeEach(() => {}),
                    test(\"bar\", () => {
                        someFn();
                    })
                })
            ",
            None,
        ),
        (
            "
                describe.skip(\"foo\", () => {
                    afterAll(() => {}),
                    afterAll(() => {}),
                    beforeAll(() => {}),
                    beforeAll(() => {}),
                    test(\"bar\", () => {
                        someFn();
                    })
                })
            ",
            None,
        ),
        (
            "
                describe.skip(\"foo\", () => {
                    beforeEach(() => {}),
                    beforeAll(() => {}),
                    test(\"bar\", () => {
                        someFn();
                    })
                })
                describe(\"foo\", () => {
                    beforeEach(() => {}),
                    beforeEach(() => {}),
                    beforeAll(() => {}),
                    test(\"bar\", () => {
                        someFn();
                    })
                })
            ",
            None,
        ),
        (
            "
                describe(\"foo\", () => {
                    beforeAll(() => {}),
                    test(\"bar\", () => {
                        someFn();
                    })
                    describe(\"inner_foo\", () => {
                        beforeEach(() => {})
                        beforeEach(() => {})
                        test(\"inner bar\", () => {
                            someFn();
                        })
                    })
                })
            ",
            None,
        ),
        (
            "
                describe.each(['hello'])('%s', () => {
                    beforeEach(() => {});
                    beforeEach(() => {});

                    it(\"is not fine\", () => {});
                });
            ",
            None,
        ),
        (
            "
                describe('something', () => {
                    describe.each(['hello'])('%s', () => {
                        beforeEach(() => {});

                        it('is fine', () => {});
                    });

                    describe.each(['world'])('%s', () => {
                        beforeEach(() => {});
                        beforeEach(() => {});

                        it('is not fine', () => {});
                    });
                });
            ",
            None,
        ),
        (
            "
                describe('something', () => {
                    describe.each(['hello'])('%s', () => {
                        beforeEach(() => {});

                        it('is fine', () => {});
                    });

                    describe.each(['world'])('%s', () => {
                        describe('some more', () => {
                            beforeEach(() => {});
                            beforeEach(() => {});

                            it('is not fine', () => {});
                        });
                    });
                });
            ",
            None,
        ),
        (
            "
                describe.each``('%s', () => {
                    beforeEach(() => {});
                    beforeEach(() => {});

                    it('is fine', () => {});
                });
            ",
            None,
        ),
        (
            "
                describe('something', () => {
                    describe.each``('%s', () => {
                        beforeEach(() => {});

                        it('is fine', () => {});
                    });

                    describe.each``('%s', () => {
                        beforeEach(() => {});
                        beforeEach(() => {});

                        it('is not fine', () => {});
                    });
                });
            ",
            None,
        ),
    ];

    let pass_vitest = vec![
        (
            r#"
                describe("foo", () => {
                    beforeEach(() => {})
                    test("bar", () => {
                        someFn();
                    })
			    })
            "#,
            None,
        ),
        (
            r#"
                beforeEach(() => {})
			    test("bar", () => {
                    someFn();
			    })
            "#,
            None,
        ),
        (
            r#"
                describe("foo", () => {
                    beforeAll(() => {}),
                    beforeEach(() => {})
                    afterEach(() => {})
                    afterAll(() => {})
                
                    test("bar", () => {
                        someFn();
                    })
                })
            "#,
            None,
        ),
        (
            r#"
                describe.skip("foo", () => {
                    beforeEach(() => {}),
                    beforeAll(() => {}),
                    test("bar", () => {
                        someFn();
                    })
                })
                describe("foo", () => {
                    beforeEach(() => {}),
                    beforeAll(() => {}),
                    test("bar", () => {
                        someFn();
                    })
                })
            "#,
            None,
        ),
        (
            r#"
                describe("foo", () => {
                    beforeEach(() => {}),
                    test("bar", () => {
                        someFn();
                    })
                    describe("inner_foo", () => {
                        beforeEach(() => {})
                        test("inner bar", () => {
                            someFn();
                        })
                    })
                })
            "#,
            None,
        ),
        (
            "
                describe.each(['hello'])('%s', () => {
                    beforeEach(() => {});
                    it('is fine', () => {});
                });
            ",
            None,
        ),
    ];

    let fail_vitest = vec![
        (
            r#"
                describe("foo", () => {
                    beforeEach(() => {}),
                    beforeEach(() => {}),
                    test("bar", () => {
                        someFn();
                    })
                })
            "#,
            None,
        ),
        (
            r#"
                describe.skip("foo", () => {
                    afterEach(() => {}),
                    afterEach(() => {}),
                    test("bar", () => {
                        someFn();
                    })
                })
            "#,
            None,
        ),
        (
            r#"
                describe.skip("foo", () => {
                    beforeEach(() => {}),
                    beforeAll(() => {}),
                    test("bar", () => {
                        someFn();
                    })
                })
                describe("foo", () => {
                    beforeEach(() => {}),
                    beforeEach(() => {}),
                    beforeAll(() => {}),
                    test("bar", () => {
                        someFn();
                    })
                })
            "#,
            None,
        ),
        (
            r#"
                describe.skip("foo", () => {
                    beforeEach(() => {}),
                    beforeAll(() => {}),
                    test("bar", () => {
                        someFn();
                    })
                })
                describe("foo", () => {
                    beforeEach(() => {}),
                    beforeEach(() => {}),
                    beforeAll(() => {}),
                    test("bar", () => {
                        someFn();
                    })
                })
            "#,
            None,
        ),
        (
            "
                describe.each(['hello'])('%s', () => {
                    beforeEach(() => {});
                    beforeEach(() => {});
                    
                    it('is not fine', () => {});
                });
            ",
            None,
        ),
        (
            "
                describe('something', () => {
                    describe.each(['hello'])('%s', () => {
                        beforeEach(() => {});
                    
                        it('is fine', () => {});
                    });
			    
                    describe.each(['world'])('%s', () => {
                        beforeEach(() => {});
                        beforeEach(() => {});
                    
                        it('is not fine', () => {});
                    });
                });
            ",
            None,
        ),
    ];

    pass.extend(pass_vitest);
    fail.extend(fail_vitest);

    Tester::new(NoDuplicateHooks::NAME, NoDuplicateHooks::PLUGIN, pass, fail)
        .with_jest_plugin(true)
        .test_and_snapshot();
}
