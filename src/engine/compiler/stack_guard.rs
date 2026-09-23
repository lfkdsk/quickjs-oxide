//! Deterministic parser recursion budgeting.
//!
//! Pinned QuickJS bounds every recursive grammar production in `next_token()`
//! (`quickjs.c:22719`) with one physical byte budget: when the C stack pointer
//! crosses `rt->stack_top - JS_DEFAULT_STACK_SIZE` (1 MiB,
//! `quickjs.h:327`), the parser returns `js_parse_error(s, "stack overflow")`,
//! a catchable `SyntaxError`. Different productions therefore fail at
//! different nesting depths purely because their C frames differ in size.
//!
//! Rust recursive-descent frames are materially larger than the C frames and
//! vary in debug/release, so a single byte budget measured from the Rust
//! stack pointer cannot reproduce those depths. This module mirrors the
//! hybrid the VM already uses in `runtime/native_stack.rs`:
//!
//! 1. A weighted logical budget. Each recursive production contributes a
//!    weight proportional to the pinned C frame size, calibrated so that an
//!    `eval`-wrapped nesting fails at the pinned depth on an 8 MiB thread.
//!    Mixed nesting composes by summing weights, exactly like the single C
//!    byte budget it models.
//! 2. A physical host-stack backstop. The weighted budget only classifies
//!    enumerated productions; the backstop guarantees a catchable error
//!    before a Rust stack overflow for any other recursion (or a small host
//!    thread).
//!
//! Both limits surface the same `SyntaxError: stack overflow` the pinned
//! engine throws; the process never aborts.

/// Total logical recursion budget, in [`WEIGHT_SCALE`] units. It models the
/// pinned 1 MiB `JS_DEFAULT_STACK_SIZE`: weights are `SCALE / pinned_depth`,
/// so an all-one-production nesting reaches this total at the pinned
/// first-throw depth measured on an 8 MiB host thread.
const PARSER_STACK_BUDGET: u64 = WEIGHT_SCALE;

/// Weight units per budget byte. `2^32` is larger than the square of every
/// observed pinned depth (max ~9330), so integer-division weights reproduce
/// every first-throw depth exactly: `floor(SCALE/depth)` first rejects at
/// `depth + 1`.
const WEIGHT_SCALE: u64 = 1 << 32;

/// Stack kept between the physical backstop trigger and the host guard page.
/// It only has to cover the immediate frame and one shallow error return;
/// unwinding frames drop locals without re-entering the parser. Debug frames
/// are larger, so keep a calibrated debug allowance as the VM guard does.
/// Headroom kept between the deepest stack check and the host guard page.
///
/// The check runs in a parent frame *before* the next production descends, so
/// this margin only has to cover the largest single parser frame built before
/// the following check, plus constructing and unwinding the catchable error.
/// Measured abort boundary on a 256–1024 KiB thread: release is abort-free at
/// 8 KiB and aborts at 4 KiB; debug frames are larger and need 64 KiB. Keep a
/// safety factor above those floors (16 KiB release, 96 KiB debug) as the VM
/// guard does for its calibrated debug allowance.
const PARSER_STACK_RESERVE: usize = if cfg!(debug_assertions) {
    96 * 1024
} else {
    16 * 1024
};

/// On non-Linux hosts (where the stack region cannot be read from
/// `/proc/self/maps`) the backstop allows this much Rust-stack growth from
/// the parse entry point before throwing. Conservative by design: a
/// catchable early error is preferred over an unwind across the guard page.
#[cfg(not(target_os = "linux"))]
const PARSER_FALLBACK_GROWTH: usize = if cfg!(debug_assertions) {
    1024 * 1024
} else {
    5 * 1024 * 1024
};

/// Marginal weight of one recursive grammar edge, in [`WEIGHT_SCALE`] units.
///
/// Weights are charged at the exact parser edge that executes once per
/// nesting level (a container's *entry*, never once per flat sibling), so
/// sequential non-nested constructs return the budget to its baseline while
/// genuinely nested productions accumulate. Compositions (e.g.
/// `if(1){` = statement head + block) therefore sum like the single C byte
/// budget they model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ParserStackFrame {
    /// `( Expression )` primary — pinned first throw at depth 718.
    Parenthesized,
    /// `[ Element ]` array literal entry — pinned 743.
    ArrayLiteral,
    /// `{ Property }` object literal entry. The `({a: … })` stress form
    /// crosses one parenthesis plus one of these per level and fails at 355;
    /// bare nested objects fail at 701.
    ObjectLiteral,
    /// A call/construct argument list entry (`f(` / `new F(`) — pinned 743.
    /// Charged once per argument list, so flat many-argument calls do not
    /// accumulate; only nested calls do.
    CallArguments,
    /// Prefix unary `!`/`void`/`typeof`/`await` operand edge — pinned 9330.
    Unary,
    /// `Conditional ? consequent : alternate` edge — pinned 8164.
    Conditional,
    /// Every arrow function edge (`x =>`) — concise chains fail at 4665.
    Arrow,
    /// Extra cost of an arrow with a block body beyond [`Self::Arrow`]
    /// (`x => {` chains fail at 1420).
    ArrowBlockBody,
    /// Template substitution edge `` ` `` `${` — pinned 635.
    Template,
    /// Head of an `if`/`while`/`do`/`with`/`for`/`switch` statement without
    /// its body block. Brace-free heads fail at 3438; a braced body adds
    /// [`Self::Block`] and the combination fails at ~1675.
    StatementHead,
    /// Block body entry (`{`), shared by blocks and `try` bodies. Bare
    /// blocks fail at 3266; `try { } catch` fails at the same depth.
    Block,
    /// Function/generator body entry (`function f(){`). Nested function
    /// declarations fail at 2613.
    FunctionBody,
    /// Bracket member-access key `base[ Expression ]`. Nested computed keys
    /// (`x[x[…1]]`) recurse through the expression tower and fail at 726.
    MemberAccess,
}

impl ParserStackFrame {
    const fn weight(self) -> u64 {
        match self {
            Self::Parenthesized => 5_990_191,
            Self::ArrayLiteral => 5_788_365,
            Self::ObjectLiteral => 6_114_011,
            Self::CallArguments => 5_788_365,
            Self::Unary => 460_388,
            Self::Conditional => 526_150,
            Self::Arrow => 920_876,
            Self::ArrowBlockBody => 2_105_880,
            Self::Template => 6_774_396,
            Self::StatementHead => 1_249_264,
            Self::Block => 1_315_055,
            Self::FunctionBody => 1_644_321,
            Self::MemberAccess => 5_924_092,
        }
    }
}

/// Return a comparable address near the current host stack pointer without
/// dereferencing it or relying on platform-specific APIs.
#[inline(never)]
fn current_stack_address() -> usize {
    let marker = 0_u8;
    std::ptr::from_ref(&marker).addr()
}

/// Lowest usable stack address for this parse thread. The reserve is added by
/// the caller.
///
/// On Linux this reads the region containing the entry pointer from
/// `/proc/self/maps`: a reserved worker thread reports its low bound
/// directly, while the growable main `[stack]` region bottoms out at
/// `region_high - RLIMIT_STACK`. The parse runs on one thread, so the region
/// identity cannot change between entry and recursion.
#[cfg(target_os = "linux")]
fn stack_floor(entry: usize) -> usize {
    use std::fs;

    let Ok(maps) = fs::read_to_string("/proc/self/maps") else {
        return fallback_floor(entry);
    };
    let mut rlimit_soft = 0_usize;
    if let Ok(limits) = fs::read_to_string("/proc/self/limits") {
        for line in limits.lines() {
            if !line.starts_with("Max stack size") {
                continue;
            }
            // Max stack size  <soft bytes>  <hard bytes>  bytes
            if let Some(soft) = line.split_whitespace().nth(3)
                && let Ok(bytes) = soft.parse::<usize>()
            {
                rlimit_soft = bytes;
            }
        }
    }
    for line in maps.lines() {
        let Some(range) = line.split_whitespace().next() else {
            continue;
        };
        let Some((low, high)) = range.split_once('-') else {
            continue;
        };
        let (Ok(low), Ok(high)) = (
            usize::from_str_radix(low, 16),
            usize::from_str_radix(high, 16),
        ) else {
            continue;
        };
        if entry >= low && entry < high {
            let is_main_stack = line.split_whitespace().last() == Some("[stack]");
            if is_main_stack && rlimit_soft != 0 {
                return high.saturating_sub(rlimit_soft);
            }
            return low;
        }
    }
    fallback_floor(entry)
}

#[cfg(target_os = "linux")]
fn fallback_floor(entry: usize) -> usize {
    entry.saturating_sub(6 * 1024 * 1024)
}

#[cfg(not(target_os = "linux"))]
fn stack_floor(entry: usize) -> usize {
    entry.saturating_sub(PARSER_FALLBACK_GROWTH)
}

/// Marker returned when a parser recursion limit is reached. The parser maps
/// it to the pinned `SyntaxError: stack overflow` diagnostic at the current
/// token; keeping it a dedicated type avoids borrowing `self` to build the
/// error while the guard itself is mutably borrowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ParserStackOverflow;

/// Owns the parser's two recursion limits.
pub(super) struct ParserStackGuard {
    /// Accumulated weighted logical cost across nested productions.
    logical: u64,
    /// Lowest address the parser may approach.
    floor: usize,
}

impl ParserStackGuard {
    pub(super) fn new() -> Self {
        let entry = current_stack_address();
        Self {
            logical: 0,
            floor: stack_floor(entry),
        }
    }

    /// Charge one recursive production before descending into it. Returns the
    /// charged weight so the caller can release it on the way back up a
    /// successful parse; [`ParserStackOverflow`] means either the weighted
    /// budget or the physical backstop was reached.
    pub(super) fn enter(&mut self, frame: ParserStackFrame) -> Result<u64, ParserStackOverflow> {
        let weight = frame.weight();
        if self.logical.saturating_add(weight) > PARSER_STACK_BUDGET {
            return Err(ParserStackOverflow);
        }
        if current_stack_address() <= self.floor.saturating_add(PARSER_STACK_RESERVE) {
            return Err(ParserStackOverflow);
        }
        self.logical += weight;
        Ok(weight)
    }

    /// Release the weight charged by a successful [`Self::enter`].
    ///
    /// Recursion which returns an error abandons the whole parse, so its
    /// charge is intentionally left on the guard.
    pub(super) fn leave(&mut self, weight: u64) {
        self.logical = self.logical.saturating_sub(weight);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_reproduce_pinned_first_throw_depths() {
        // A depth of `d - 1` is accepted and the pinned first-throw depth `d`
        // is rejected, for every standalone edge.
        for (frame, pinned_depth) in [
            (ParserStackFrame::Parenthesized, 718),
            (ParserStackFrame::ArrayLiteral, 743),
            (ParserStackFrame::CallArguments, 743),
            (ParserStackFrame::Unary, 9330),
            (ParserStackFrame::Conditional, 8164),
            (ParserStackFrame::Arrow, 4665),
            (ParserStackFrame::Template, 635),
            (ParserStackFrame::StatementHead, 3438),
            (ParserStackFrame::Block, 3266),
            (ParserStackFrame::FunctionBody, 2613),
            (ParserStackFrame::MemberAccess, 726),
        ] {
            let weight = frame.weight();
            assert!(
                (pinned_depth - 1) * weight <= PARSER_STACK_BUDGET,
                "{frame:?} must accept depth {}",
                pinned_depth - 1
            );
            assert!(
                pinned_depth * weight > PARSER_STACK_BUDGET,
                "{frame:?} must reject at the pinned depth {pinned_depth}"
            );
        }
        // A concise arrow plus its block-body surcharge first rejects at the
        // pinned block-bodied arrow depth 1420.
        let block_arrow =
            ParserStackFrame::Arrow.weight() + ParserStackFrame::ArrowBlockBody.weight();
        assert!(1419 * block_arrow <= PARSER_STACK_BUDGET);
        assert!(1420 * block_arrow > PARSER_STACK_BUDGET);
        // A braced statement is a head plus a block and first rejects at 1675.
        let braced = ParserStackFrame::StatementHead.weight() + ParserStackFrame::Block.weight();
        assert!(1674 * braced <= PARSER_STACK_BUDGET);
        assert!(1675 * braced > PARSER_STACK_BUDGET);
        // The `({a: … })` stress form crosses a paren plus an object edge per
        // level and first rejects at 355. One outer paren-object plus bare
        // nested objects (`({` then `a:{` repeated) first rejects at seven
        // hundred one nested objects: 701 object edges (plus the one outer
        // paren) are accepted and 702 rejected.
        let object_value =
            ParserStackFrame::Parenthesized.weight() + ParserStackFrame::ObjectLiteral.weight();
        assert!(354 * object_value <= PARSER_STACK_BUDGET);
        assert!(355 * object_value > PARSER_STACK_BUDGET);
        let outer = ParserStackFrame::Parenthesized.weight();
        let object = ParserStackFrame::ObjectLiteral.weight();
        assert!(outer + 701 * object <= PARSER_STACK_BUDGET);
        assert!(outer + 702 * object > PARSER_STACK_BUDGET);
    }

    #[test]
    fn mixed_nesting_sums_weights_like_one_byte_budget() {
        let mut guard = ParserStackGuard::new();
        // Two productions whose frames are each about half the pinned object
        // depth must exhaust the shared budget well before a pure run.
        let mut charged = Vec::new();
        for _ in 0..170 {
            charged.push(guard.enter(ParserStackFrame::ObjectLiteral).unwrap());
        }
        let mut paren_depth = 0_u64;
        while let Ok(weight) = guard.enter(ParserStackFrame::Parenthesized) {
            charged.push(weight);
            paren_depth += 1;
            assert!(
                paren_depth <= 718,
                "mixed nesting was not bounded by the shared budget"
            );
        }
        assert!(
            paren_depth < 718,
            "mixed object+paren nesting must reject before a pure paren run"
        );
    }
}
