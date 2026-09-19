//! Main-thread parser stack-guard probe for B8-r4.
//!
//! libtest always runs `#[test]` bodies on the runtime's worker threads, whose
//! stacks are fixed-size non-growable mappings. The parser guard's
//! growable-main-stack branch (and its `RLIMIT_STACK=unlimited` handling) can
//! therefore only be exercised by a standalone process whose parse runs on the
//! real main thread. The integration tests in `tests/parser_stack_depth.rs`
//! spawn this binary under chosen `ulimit -s` values.
//!
//! Modes:
//! * `shallow N`  — evaluate `(`×N `1` `)`×N on the main thread and print
//!   `ACCEPTED` (exit 0) or `THROW:<name>:<message>` (exit 0). Any process
//!   abort is inherited as a non-zero/signal exit status.
//! * `native F N`  — warm up with a shallow eval near the top of the main
//!   stack, recurse `F` native frames of ~64 KiB, then parse
//!   `class C{static{`×N `0` `}}`×N (a production with no per-level logical
//!   guard charge). Prints `THROW:<name>:<message>` and exits 0 when the guard
//!   surfaces the catchable overflow; an unwarned host overflow aborts.
//!
//! ```sh
//! cargo run --release --example parser_main_thread_stack_probe -- native 1 6000
//! ```

use quickjs_oxide::engine::api::{Runtime, RuntimeError, Value};
use std::hint::black_box;
use std::process::ExitCode;

#[inline(never)]
fn descend(depth: usize, run: &mut dyn FnMut() -> String) -> String {
    let mut pad = [0u8; 64 * 1024];
    pad[depth % pad.len()] = depth as u8;
    black_box(&mut pad);
    if depth == 0 {
        return run();
    }
    let result = descend(depth - 1, run);
    black_box(pad[0]);
    result
}

fn label_of(value: Result<Value, RuntimeError>) -> String {
    match value {
        Ok(Value::String(label)) => label.to_utf8_lossy(),
        Ok(other) => format!("unexpected:{other:?}"),
        Err(RuntimeError::Exception) => "UNCAUGHT".to_owned(),
        Err(error) => format!("ERROR:{error}"),
    }
}

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let mode = arguments.next().unwrap_or_else(|| "native".to_owned());
    let first: usize = arguments
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let second: usize = arguments
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(6000);

    let runtime =
        Runtime::new_with_host_services(quickjs_oxide_host::SystemHostServices::default());
    let mut context = runtime.new_context();

    match mode.as_str() {
        "shallow" => {
            let source = format!("({}1{})", "(".repeat(first), ")".repeat(first));
            let program =
                format!("try{{eval({source:?});\"ACCEPTED\"}}catch(e){{e.name+\":\"+e.message}}");
            println!("{}", label_of(context.eval(&program)));
        }
        "native" => {
            // The first parser on the thread populates the per-thread stack
            // metadata cache near the top, exactly as a host embedding which
            // constructs its runtime before recursing natively would.
            match context.eval("1+1") {
                Ok(Value::Int(2)) => {}
                other => {
                    eprintln!("warm-up eval failed: {other:?}");
                    return ExitCode::from(2);
                }
            }
            // `class C{static{` is a production whose nested levels carry no
            // per-level logical guard charge, so only the physical token-edge
            // backstop can stop the parse before the host stack overflows.
            let nested_source = format!(
                "{}0{}",
                "class C{static{".repeat(second),
                "}}".repeat(second)
            );
            let mut encoded = String::with_capacity(nested_source.len() + 2);
            encoded.push('"');
            for ch in nested_source.chars() {
                match ch {
                    '"' => encoded.push_str("\\\""),
                    '\\' => encoded.push_str("\\\\"),
                    '\n' => encoded.push_str("\\n"),
                    '\r' => encoded.push_str("\\r"),
                    _ => encoded.push(ch),
                }
            }
            encoded.push('"');
            let program = format!(
                "try{{eval({encoded});\"UNEXPECTED:ACCEPTED\"}}catch(e){{e.name+\":\"+e.message}}"
            );
            let mut run = || label_of(context.eval(&program));
            println!("{}", descend(first, &mut run));
        }
        other => {
            eprintln!("unknown mode {other:?}; expected `shallow` or `native`");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}
