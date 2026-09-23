//! Parser deep-nesting guard (task B8).
//!
//! Deeply nested source (parenthesized/array/object/function/statement
//! productions) must surface a *catchable* `SyntaxError: stack overflow` at
//! exactly the depth pinned QuickJS 2026-06-04 throws it, instead of
//! overflowing the host stack and aborting. The parser reproduces the pinned
//! logical nesting depths with a weighted depth budget and backs it with a
//! physical host-stack check, so these tests run on an explicitly sized
//! thread the same way the `qjs` CLI does.
//!
//! When `QJS_ORACLE` points at the pinned `qjs`, each boundary is additionally
//! compared byte-for-byte against it.

use quickjs_oxide::{Context, Runtime, Value};

/// Host stack for the evaluation thread. Rust parser frames are larger than
/// the C engine's, so reaching the pinned nesting depths for the heaviest
/// productions reserves a proportionally larger (virtual) stack; debug frames
/// are larger again. The reservation is virtual address space, not RSS.
#[cfg(debug_assertions)]
const TEST_STACK_SIZE: usize = 256 * 1024 * 1024;
#[cfg(not(debug_assertions))]
const TEST_STACK_SIZE: usize = 32 * 1024 * 1024;

/// `(prefix builder, leaf, suffix builder, pinned first-throw depth)`.
/// `b` is the JS expression used to build one side of the nesting.
struct Form {
    name: &'static str,
    /// JS statements run before the guarded eval (e.g. defining `F`).
    setup: &'static str,
    left: &'static str,
    leaf: &'static str,
    right: &'static str,
    pinned_depth: usize,
}

const FORMS: &[Form] = &[
    Form {
        name: "parenthesized",
        setup: "",
        left: "(",
        leaf: "1",
        right: ")",
        pinned_depth: 718,
    },
    Form {
        name: "array",
        setup: "",
        left: "[",
        leaf: "1",
        right: "]",
        pinned_depth: 743,
    },
    Form {
        name: "object",
        setup: "",
        left: "({a:",
        leaf: "1",
        right: "})",
        pinned_depth: 355,
    },
    Form {
        name: "call",
        setup: "function f(x){return x}",
        left: "f(",
        leaf: "1",
        right: ")",
        pinned_depth: 743,
    },
    Form {
        name: "new",
        setup: "F=function(){}",
        left: "new F(",
        leaf: "1",
        right: ")",
        pinned_depth: 743,
    },
    Form {
        name: "unary",
        setup: "",
        left: "!",
        leaf: "1",
        right: "",
        pinned_depth: 9330,
    },
    Form {
        name: "conditional",
        setup: "",
        left: "1?",
        leaf: "1",
        right: ":0",
        pinned_depth: 8164,
    },
    Form {
        name: "arrow",
        setup: "",
        left: "x=>",
        leaf: "x",
        right: "",
        pinned_depth: 4665,
    },
    Form {
        name: "template",
        setup: "",
        left: "`${",
        leaf: "1",
        right: "}`",
        pinned_depth: 635,
    },
    Form {
        name: "if",
        setup: "",
        left: "if(1)",
        leaf: "0",
        right: "",
        pinned_depth: 3438,
    },
    Form {
        name: "block",
        setup: "",
        left: "{",
        leaf: "0",
        right: "}",
        pinned_depth: 3266,
    },
    Form {
        name: "while",
        setup: "",
        left: "while(0){",
        leaf: "0",
        right: "}",
        pinned_depth: 1675,
    },
    Form {
        name: "function-body",
        setup: "",
        left: "function f(){",
        leaf: "0",
        right: "}",
        pinned_depth: 2613,
    },
    Form {
        name: "member-access",
        setup: "var x={}",
        left: "x[",
        leaf: "1",
        right: "]",
        pinned_depth: 726,
    },
];

/// Build the label expression that evaluates one nesting `depth` and yields
/// either `"OK"` or `"ErrorName:ErrorMessage"`.
fn guarded_label(form: &Form, depth: usize) -> String {
    format!(
        "try{{eval({left:?}.repeat({depth})+{leaf:?}+{right:?}.repeat({depth}));\
         \"OK\"}}catch(e){{e.name+\":\"+e.message}}",
        left = form.left,
        leaf = form.leaf,
        right = form.right,
        depth = depth,
    )
}

/// Build a standalone program (with any needed setup) that evaluates the
/// guarded label expression.
fn guarded_program(form: &Form, depth: usize) -> String {
    let setup = if form.setup.is_empty() {
        String::new()
    } else {
        format!("{};", form.setup)
    };
    format!(
        "{setup}{label}",
        setup = setup,
        label = guarded_label(form, depth)
    )
}

fn run_on_parser_stack<T: Send + 'static>(task: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .name("parser-stack-depth".to_owned())
        .stack_size(TEST_STACK_SIZE)
        .spawn(task)
        .unwrap()
        .join()
        .unwrap()
}

fn eval_string(context: &mut Context, source: &str) -> String {
    match context.eval(source) {
        Ok(Value::String(value)) => value.to_utf8_lossy(),
        Ok(other) => panic!("expected a string completion, got {other:?} for {source}"),
        Err(error) => panic!("evaluation failed: {error} for {source}"),
    }
}

#[test]
fn pinned_depth_boundaries_throw_a_catchable_syntax_error() {
    run_on_parser_stack(|| {
        let runtime = Runtime::new();
        let mut context = runtime.new_context();
        for form in FORMS {
            // One below the pinned boundary still parses.
            let accepted = eval_string(&mut context, &guarded_program(form, form.pinned_depth - 1));
            assert_eq!(
                accepted,
                "OK",
                "{} accepted at depth {}",
                form.name,
                form.pinned_depth - 1
            );
            // At the pinned boundary the throw is catchable and has the exact
            // pinned name and message.
            let thrown = eval_string(&mut context, &guarded_program(form, form.pinned_depth));
            assert_eq!(
                thrown, "SyntaxError:stack overflow",
                "{} at pinned depth {}",
                form.name, form.pinned_depth
            );
        }
    });
}

#[test]
fn the_runtime_recovers_after_a_caught_parser_stack_overflow() {
    run_on_parser_stack(|| {
        let runtime = Runtime::new();
        let mut context = runtime.new_context();
        for form in FORMS {
            let thrown = eval_string(&mut context, &guarded_program(form, form.pinned_depth));
            assert_eq!(thrown, "SyntaxError:stack overflow");
            // The next, shallow evaluation on the same context still works.
            assert_eq!(eval_string(&mut context, "String(6*7)"), "42");
        }
    });
}

#[test]
fn deep_nesting_never_aborts_far_beyond_the_logical_boundary() {
    run_on_parser_stack(|| {
        let runtime = Runtime::new();
        let mut context = runtime.new_context();
        // Even absurdly deep input must throw the catchable error rather than
        // abort the process; the physical backstop is the last line of
        // defense for productions the weighted budget classifies late.
        for form in [&FORMS[0], &FORMS[2], &FORMS[10]] {
            let thrown = eval_string(&mut context, &guarded_program(form, 1_000_000));
            assert_eq!(
                thrown, "SyntaxError:stack overflow",
                "{} at depth 1000000",
                form.name
            );
        }
    });
}

#[test]
fn pinned_oracle_agrees_at_every_boundary() {
    let Some(oracle) = std::env::var_os("QJS_ORACLE") else {
        eprintln!("SKIP parser-depth differential: set QJS_ORACLE to the pinned qjs");
        return;
    };
    run_on_parser_stack(move || {
        let runtime = Runtime::new();
        let mut context = runtime.new_context();
        for form in FORMS {
            for depth in [form.pinned_depth - 1, form.pinned_depth] {
                let source = guarded_program(form, depth);
                let rust = eval_string(&mut context, &source);
                // The CLI prints the caught result via a statement-scoped
                // variable (`try` is a statement, not an expression).
                let setup = if form.setup.is_empty() {
                    String::new()
                } else {
                    format!("{};", form.setup)
                };
                let oracle_source = format!(
                    "{setup}var __r;try{{eval({left:?}.repeat({depth})+{leaf:?}+{right:?}.repeat({depth}));\
                     __r=\"OK\"}}catch(e){{__r=e.name+\":\"+e.message}}print(__r);",
                    setup = setup,
                    left = form.left,
                    leaf = form.leaf,
                    right = form.right,
                    depth = depth,
                );
                let output = std::process::Command::new(&oracle)
                    .arg("-e")
                    .arg(&oracle_source)
                    .output()
                    .unwrap_or_else(|error| panic!("run pinned oracle {oracle:?}: {error}"));
                let oracle_out = String::from_utf8(output.stdout).expect("oracle utf8");
                assert_eq!(
                    rust.trim_end(),
                    oracle_out.trim_end(),
                    "{} at depth {} differs from pinned QuickJS",
                    form.name,
                    depth
                );
                assert!(
                    output.status.success(),
                    "pinned oracle exited nonzero for a caught throw"
                );
            }
        }
    });
}
