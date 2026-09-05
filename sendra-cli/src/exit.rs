//! What the process returns, and the classification every subcommand's verdict
//! is folded out of. Pure policy: no I/O, no printing, nothing async.

use std::process::ExitCode;

use sendra_core::AssertionReport;

/// Sendra's exit-code convention, in one place, for every subcommand.
///
/// ```text
/// code  run  test  meaning
///
/// 0      ·    ·    nothing went wrong. For `run`: every request was sent and
///                  no response was an error status (or the user opted out
///                  with --allow-error-status). For `test`: every request got
///                  a response, and every assertion any of them declared,
///                  passed.
/// 1      ·    ·    some request never got a response: file missing or
///                  malformed, no such request name, `--env` naming an
///                  environment with no file behind it, a `{{variable}}` or
///                  `${VAR}` with no value, invalid header, DNS/TLS/connection
///                  failure.
/// 2      ·    ·    bad command-line usage — clap exits with this itself.
/// 3      ·         `run` only: every request got a response, but at least one
///                  was 4xx/5xx.
/// 4           ·    `test` only: every request got a response, but at least one
///                  had a failing assertion.
/// ```
///
/// A collection run sends many requests under one exit code, so these are
/// aggregates; [`worst`] is where they combine, over [`Outcome`]s produced by
/// the loop both subcommands share.
///
/// **One enum for both subcommands, not one each.** `sendra run` and
/// `sendra test` answer different questions, but they answer them to the same
/// shell, and a number that means one thing under `run` and another under
/// `test` is a trap for anyone writing `case $? in` around either. So the codes
/// are globally unique across the binary: `1` means "never got a response"
/// whichever command produced it, and the two commands' *own* verdicts get
/// their own numbers — `3` for `run`'s bad status, `4` for `test`'s failed
/// assertion. Reusing `3` for a failed assertion was the alternative, and it
/// would have made the same number mean "the server said 500" in one command
/// and "the server said exactly what you asked for, and it was wrong" in the
/// other.
///
/// Every exit path in the binary returns one of these variants rather than
/// calling `std::process::exit` inline, so adding a code later means adding a
/// row here and nothing else. Codes 5 and up stay free.
///
/// **`test` never returns 3, and `run` never returns 4.** `run` does not read
/// assertions when deciding what to return — see [`exit_for_response`], which
/// is the single place that decision lives — so a `run` that prints "1 failed"
/// still exits `0`. That asymmetry is deliberate and permanent, not a stage on
/// the way to unifying them: wiring assertions into `run`'s exit code would
/// silently change what every existing `sendra run x && deploy.sh` means the
/// moment an `assertions` block is added to a file, and `sendra test` exists
/// precisely so that nobody has to.
///
/// Symmetrically, `test` does not read raw status: a request that declared no
/// assertions and came back `404` exits `0` under `test`. See [`Summary`] for
/// why, and for the three-way split — passed, failed, no assertions — that
/// makes it visible rather than silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    Ok = 0,
    Failure = 1,
    // 2 belongs to clap; see the table above.
    ErrorStatus = 3,
    TestFailed = 4,
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        ExitCode::from(exit as u8)
    }
}

/// Decide the exit code for a response that came back.
///
/// 1xx/2xx/3xx are "the server answered and did not object"; 4xx and 5xx are
/// failures unless the caller opted out. Anything at or above 400 counts,
/// including non-standard 6xx codes — a status we do not recognise is not a
/// status we should report as success.
///
/// Takes a bare `u16` rather than a `&Response` so the decision stays pure and
/// testable without constructing a response or touching the network.
pub(crate) fn exit_for_status(status: u16, allow_error_status: bool) -> Exit {
    if allow_error_status || status < 400 {
        Exit::Ok
    } else {
        Exit::ErrorStatus
    }
}

/// The outcome `sendra run` reports for a request that came back, assertions
/// included — which is to say, assertions *excluded*.
///
/// This exists rather than calling [`exit_for_status`] directly at the call
/// site so that "assertions do not affect `run`'s exit code" is a stated
/// decision with a test on it, in one place, instead of an absence nobody can
/// point at. [`Summary::exit`] is the other half of the pair: the same report,
/// read rather than discarded, for the command whose job is to read it.
fn exit_for_response(status: u16, assertions: &AssertionReport, allow_error_status: bool) -> Exit {
    // Read and deliberately discarded: see above, and the `Exit` table.
    let _ = assertions;
    exit_for_status(status, allow_error_status)
}

/// Fold the outcomes of several requests into the single code the process can
/// return. The worst outcome wins, ranked
/// `Ok` < `ErrorStatus` < `TestFailed` < `Failure`.
///
/// "Worst wins" rather than "the last request wins": the exit code should
/// answer "did anything go wrong?", and tying it to the last request would make
/// it depend on the order the file happens to list requests in, so reordering a
/// collection could change whether a script proceeds. This keeps
/// `sendra run collection.yaml && deploy.sh` meaning for a collection what it
/// means for a single request — exit 0 is a promise that nothing in the run
/// failed.
///
/// `Failure` outranks both middle tiers because "never got a response" is the
/// bigger problem: a 404 is an answer, a DNS failure is not. A run reports the
/// most serious thing that happened, not the most recent.
///
/// `ErrorStatus` and `TestFailed` cannot meet today — one is produced only by
/// `run` and the other only by `test`, per the [`Exit`] table — so their
/// relative order is a convention rather than an observable. It is set the way
/// it is because if they ever did meet, the explicit failed expectation is the
/// more informative answer than the status nobody wrote down.
fn worst(a: Exit, b: Exit) -> Exit {
    // Rank by severity, not by the exit numbers: 3 (ErrorStatus) and 4
    // (TestFailed) are the milder outcomes, so the numeric order is the wrong
    // order.
    fn severity(exit: Exit) -> u8 {
        match exit {
            Exit::Ok => 0,
            Exit::ErrorStatus => 1,
            Exit::TestFailed => 2,
            Exit::Failure => 3,
        }
    }

    if severity(b) > severity(a) {
        b
    } else {
        a
    }
}

/// What became of one request in a run.
///
/// `run` and `test` disagree about what to print and about what to return, but
/// not about what happened, so [`run_requests`] produces these and each
/// subcommand folds them its own way — [`exit_for_run`] for `run`, [`Summary`]
/// for `test`. Keeping the shared loop's output a fact rather than an exit code
/// is what let the two commands share it at all: `test` needs to know *why* a
/// request contributed a failure, and an `Exit` has already thrown that away.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The request never got a response: a `{{variable}}` with nothing behind
    /// it, an invalid header, a refused connection. There is no status and no
    /// assertion report, because neither of them exists without a response.
    NoResponse,

    /// A response came back, and the request's assertions — if it declared any
    /// — were evaluated against it.
    ///
    /// The report is empty when the file declared none, which is a third thing
    /// from "passed" and from "failed"; see [`Summary`].
    Responded {
        status: u16,
        assertions: AssertionReport,
    },
}

/// `run`'s verdict on one outcome. Assertions are carried through and ignored;
/// see [`exit_for_response`].
fn exit_for_outcome(outcome: &Outcome, allow_error_status: bool) -> Exit {
    match outcome {
        Outcome::NoResponse => Exit::Failure,
        Outcome::Responded { status, assertions } => {
            exit_for_response(*status, assertions, allow_error_status)
        }
    }
}

/// Fold a whole `run` into the one code the process returns.
pub(crate) fn exit_for_run(outcomes: &[Outcome], allow_error_status: bool) -> Exit {
    outcomes.iter().fold(Exit::Ok, |exit, outcome| {
        worst(exit, exit_for_outcome(outcome, allow_error_status))
    })
}

/// The counts `sendra test` prints at the end of a run, and the exit code it
/// derives from them.
///
/// **Five numbers, and the middle three are separate categories on purpose.**
/// A request that declared no assertions is not a pass and not a failure: it is
/// a request nobody said anything about. Folding it into `passed` would make a
/// collection with no assertions anywhere report a perfect green run, which is
/// the single most misleading thing a test command can do; folding it into
/// `failed` would make adding a request to a collection break the build until
/// somebody wrote expectations for it. Counting it on its own line says the
/// true thing — "these ran, and nothing was checked" — and leaves what to do
/// about it to the person reading.
///
/// **What fails the run.** `failed` and `no_response`, and nothing else:
///
/// - A request whose assertions did not all hold is the whole point of the
///   command. [`Exit::TestFailed`].
/// - A request that never got a response cannot have its assertions evaluated,
///   so a run containing one cannot honestly say the expectations held. It is
///   [`Exit::Failure`], the same code `run` gives it, because it is the same
///   event: the tool could not do its job, as against the API failing to meet
///   expectations. In CI those two want different handling — one is "fix your
///   test setup", the other is "fix your API" — which is exactly why they get
///   different numbers instead of one generic non-zero.
///
/// **`without_assertions` does not fail the run, whatever the status was.**
/// This is the debatable one, so: a request with no `assertions` block that
/// comes back `404` exits `0` under `sendra test`. The command's contract is
/// that the *file* says what it expects and `test` reports whether it got it.
/// Failing on a bare 404 means asserting something the file never wrote down —
/// inventing an expectation on the author's behalf — which is the same class of
/// mistake as a silently-ignored assertion typo, only inverted. Sendra already
/// refuses to guess anywhere else in its schema, and the check is one line to
/// write when it is wanted:
///
/// ```yaml
/// assertions:
///   status: 200
/// ```
///
/// It also keeps `test` from having two independent verdicts that can
/// disagree — "assertions passed but the status was bad" has no sensible
/// single answer — and it leaves a real use intact: a request that is in the
/// collection to *reach* an endpoint (a login, a setup call) rather than to be
/// checked. The raw-status question already has a command that answers it, and
/// answers it well: `sendra run`, exit `3`. Nothing is lost by `test` declining
/// to answer it a second time with a different number.
///
/// The safeguard against that decision hiding a problem is the summary itself:
/// `without_assertions` is printed, so a run whose expectations were never
/// written is visibly not the same thing as a run that passed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Summary {
    /// Every request the run attempted. Always the sum of the four below.
    pub(crate) total: usize,
    /// Got a response, declared at least one assertion, and all of them held.
    pub(crate) passed: usize,
    /// Got a response, declared assertions, and at least one did not hold.
    pub(crate) failed: usize,
    /// Got a response and declared no assertions at all.
    pub(crate) without_assertions: usize,
    /// Never got a response, so there was nothing to evaluate against.
    pub(crate) no_response: usize,
}

impl Summary {
    /// Classify each outcome into exactly one of the four categories.
    pub(crate) fn of(outcomes: &[Outcome]) -> Self {
        let mut summary = Summary {
            total: outcomes.len(),
            ..Summary::default()
        };

        for outcome in outcomes {
            match outcome {
                Outcome::NoResponse => summary.no_response += 1,
                // An empty report is a request that declared nothing, whether
                // it had no `assertions` key or an empty one. Either way there
                // is nothing to have passed.
                Outcome::Responded { assertions, .. } if assertions.is_empty() => {
                    summary.without_assertions += 1
                }
                Outcome::Responded { assertions, .. } if assertions.passed() => summary.passed += 1,
                Outcome::Responded { .. } => summary.failed += 1,
            }
        }

        summary
    }

    /// The code the process returns. Worst-wins over the two failing
    /// categories, through the same [`worst`] every other aggregate uses, so
    /// the ordering lives in one place.
    pub(crate) fn exit(&self) -> Exit {
        let mut exit = Exit::Ok;

        if self.failed > 0 {
            exit = worst(exit, Exit::TestFailed);
        }
        if self.no_response > 0 {
            exit = worst(exit, Exit::Failure);
        }

        exit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{all_passed, assertions_from, checked, responded, response};

    #[test]
    fn success_and_redirect_statuses_exit_zero() {
        for status in [100, 200, 201, 204, 301, 302, 304, 399] {
            assert_eq!(
                exit_for_status(status, false),
                Exit::Ok,
                "{status} should not fail the run"
            );
        }
    }

    #[test]
    fn client_and_server_error_statuses_exit_non_zero() {
        for status in [400, 401, 404, 418, 500, 503] {
            assert_eq!(
                exit_for_status(status, false),
                Exit::ErrorStatus,
                "{status} should fail the run"
            );
        }
    }

    #[test]
    fn allow_error_status_forces_zero_for_every_status() {
        for status in [200, 301, 404, 500] {
            assert_eq!(
                exit_for_status(status, true),
                Exit::Ok,
                "--allow-error-status should keep {status} at exit 0"
            );
        }
    }

    /// The exit code a collection run produces: fold each request's outcome in,
    /// in order, the way `run` does.
    fn aggregate(outcomes: impl IntoIterator<Item = Exit>) -> Exit {
        outcomes.into_iter().fold(Exit::Ok, worst)
    }

    #[test]
    fn an_all_ok_collection_run_exits_zero() {
        assert_eq!(aggregate([Exit::Ok, Exit::Ok, Exit::Ok]), Exit::Ok);
    }

    #[test]
    fn one_error_status_anywhere_fails_the_whole_collection_run() {
        // Whether the 4xx is first, last or in the middle must not matter.
        for outcomes in [
            [Exit::ErrorStatus, Exit::Ok, Exit::Ok],
            [Exit::Ok, Exit::ErrorStatus, Exit::Ok],
            [Exit::Ok, Exit::Ok, Exit::ErrorStatus],
        ] {
            assert_eq!(
                aggregate(outcomes),
                Exit::ErrorStatus,
                "a 4xx/5xx anywhere in {outcomes:?} should fail the run"
            );
        }
    }

    #[test]
    fn a_mixed_status_collection_reports_the_error_status_not_the_last_one() {
        // examples/mixed-status-collection.yaml: 200, then 404, then 500.
        let outcomes = [200, 404, 500].map(|status| exit_for_status(status, false));
        assert_eq!(aggregate(outcomes), Exit::ErrorStatus);

        // A 200 last would be just as much of a failed run.
        let outcomes = [404, 500, 200].map(|status| exit_for_status(status, false));
        assert_eq!(aggregate(outcomes), Exit::ErrorStatus);
    }

    #[test]
    fn allow_error_status_keeps_a_mixed_collection_at_zero() {
        let outcomes = [200, 404, 500].map(|status| exit_for_status(status, true));
        assert_eq!(aggregate(outcomes), Exit::Ok);
    }

    #[test]
    fn a_request_that_never_got_a_response_outranks_a_bad_status() {
        // "could not send" is the more serious outcome, whichever order the two
        // happen in, because a status at least means the server answered.
        assert_eq!(aggregate([Exit::ErrorStatus, Exit::Failure]), Exit::Failure);
        assert_eq!(aggregate([Exit::Failure, Exit::ErrorStatus]), Exit::Failure);
    }

    #[test]
    fn worst_is_order_independent() {
        // Every pair, both ways round: aggregation must not depend on file
        // order, which is the whole point of not using "last request wins".
        for a in [Exit::Ok, Exit::ErrorStatus, Exit::TestFailed, Exit::Failure] {
            for b in [Exit::Ok, Exit::ErrorStatus, Exit::TestFailed, Exit::Failure] {
                assert_eq!(
                    worst(a, b),
                    worst(b, a),
                    "worst({a:?}, {b:?}) is asymmetric"
                );
            }
        }
    }

    // --- assertions do not touch the exit code ---------------------------
    //
    // The non-goal of the issue that added assertions, tested rather than
    // assumed. `sendra run` reports what came back; `sendra test` will be the
    // command that passes or fails on expectations.

    /// A report in which everything that could fail, did.
    fn a_failing_report(status: u16) -> AssertionReport {
        let report = assertions_from(
            "\
method: GET
url: https://example.com
assertions:
  status: 599
  headers:
    x-nope: whatever
  body_contains: definitely-not-in-the-body
  json:
    $.nope: 1
",
        )
        .evaluate(&response(status));

        assert_eq!(report.failed_count(), 4, "all four should have failed");
        report
    }

    #[test]
    fn failing_assertions_do_not_change_the_exit_code_of_a_successful_response() {
        // The intentional, temporary asymmetry: four failed assertions printed,
        // exit 0 all the same.
        let exit = exit_for_response(200, &a_failing_report(200), false);
        assert_eq!(exit, Exit::Ok);
        assert_eq!(exit as u8, 0);
    }

    #[test]
    fn failing_assertions_do_not_change_the_exit_code_of_an_error_response() {
        // Nor do they promote a 404 to something else, or rescue it: the status
        // is still the only thing being read.
        assert_eq!(
            exit_for_response(404, &a_failing_report(404), false),
            Exit::ErrorStatus
        );
        assert_eq!(
            exit_for_response(404, &a_failing_report(404), true),
            Exit::Ok,
            "--allow-error-status still forgives the status, and nothing else"
        );
    }

    #[test]
    fn passing_assertions_do_not_rescue_an_error_status_either() {
        let report =
            assertions_from("method: GET\nurl: https://example.com\nassertions:\n  status: 500\n")
                .evaluate(&response(500));
        assert!(
            report.passed(),
            "the assertion asked for exactly this status"
        );

        assert_eq!(
            exit_for_response(500, &report, false),
            Exit::ErrorStatus,
            "a 500 the file expected is still a 500"
        );
    }

    #[test]
    fn the_exit_code_is_the_same_with_and_without_an_assertions_block() {
        // The no-op guarantee, at the level that decides the process's answer.
        for status in [200, 301, 404, 500] {
            for allow in [false, true] {
                assert_eq!(
                    exit_for_response(status, &AssertionReport::default(), allow),
                    exit_for_response(status, &a_failing_report(status), allow),
                    "assertions changed the exit code for {status} (allow_error_status={allow})"
                );
            }
        }
    }

    #[test]
    fn exit_codes_match_the_documented_convention() {
        // The numbers are the contract with anyone scripting sendra, so pin
        // them here rather than only asserting on the variants.
        assert_eq!(Exit::Ok as u8, 0);
        assert_eq!(Exit::Failure as u8, 1);
        assert_eq!(Exit::ErrorStatus as u8, 3);
        assert_eq!(Exit::TestFailed as u8, 4);
    }

    // --- `sendra test`: the summary, and the exit code it comes from ------
    //
    // The command's whole contract is in `Summary`: which of the four
    // categories each request lands in, and which of them make the run fail.
    // These test that against outcomes built by hand, and — where the point is
    // that a request that never got a response is not special-cased anywhere —
    // through the real `run_requests` loop.

    /// An outcome that came back with `status` and declared assertions, none of
    /// which held.
    fn some_failed(status: u16) -> Outcome {
        checked(status, a_failing_report(status))
    }

    #[test]
    fn a_mixed_collection_counts_each_category_separately() {
        // One of each of the three things a response can be, so no two
        // categories can be collapsed without this noticing.
        let outcomes = vec![all_passed(200), some_failed(200), responded(200)];

        assert_eq!(
            Summary::of(&outcomes),
            Summary {
                total: 3,
                passed: 1,
                failed: 1,
                without_assertions: 1,
                no_response: 0,
            }
        );
        assert_eq!(Summary::of(&outcomes).exit(), Exit::TestFailed);
        assert_eq!(Exit::TestFailed as u8, 4);
    }

    #[test]
    fn a_collection_where_everything_passes_exits_zero() {
        let outcomes = vec![all_passed(200), all_passed(201), all_passed(204)];

        assert_eq!(
            Summary::of(&outcomes),
            Summary {
                total: 3,
                passed: 3,
                ..Summary::default()
            }
        );
        assert_eq!(Summary::of(&outcomes).exit(), Exit::Ok);
    }

    #[test]
    fn a_request_with_no_assertions_is_neither_a_pass_nor_a_failure() {
        // The third category, on its own: three requests that came back fine
        // and were never checked. Nothing failed, so the run exits 0 — and
        // nothing passed either, so the summary cannot be read as three green
        // checks.
        let outcomes = vec![responded(200), responded(200), responded(200)];
        let summary = Summary::of(&outcomes);

        assert_eq!(
            summary,
            Summary {
                total: 3,
                without_assertions: 3,
                ..Summary::default()
            }
        );
        assert_eq!(summary.passed, 0, "an unchecked request is not a pass");
        assert_eq!(summary.failed, 0, "nor is it a failure");
        assert_eq!(summary.exit(), Exit::Ok);
    }

    #[test]
    fn an_empty_assertions_block_counts_as_no_assertions_at_all() {
        // `assertions: {}` is a block that asserts nothing, and is the same
        // thing to this command as having written no block: an empty report
        // either way. See `Assertions::is_empty` in core.
        let assertions = assertions_from("method: GET\nurl: https://example.com\nassertions: {}\n");
        assert!(assertions.is_empty());

        let outcomes = vec![checked(200, assertions.evaluate(&response(200)))];

        assert_eq!(
            Summary::of(&outcomes),
            Summary {
                total: 1,
                without_assertions: 1,
                ..Summary::default()
            }
        );
    }

    #[test]
    fn a_bad_status_with_no_assertions_does_not_fail_a_test_run() {
        // The debatable decision, pinned. A request that declared nothing and
        // came back 404 or 500 exits 0 under `test`: the file said nothing
        // about the status, so `test` says nothing about it either. See
        // `Summary` for the reasoning.
        let outcomes = vec![responded(404), responded(500)];
        let summary = Summary::of(&outcomes);

        assert_eq!(
            summary,
            Summary {
                total: 2,
                without_assertions: 2,
                ..Summary::default()
            }
        );
        assert_eq!(
            summary.exit(),
            Exit::Ok,
            "an unasserted status must not fail a test run"
        );

        // And the contrast that makes it a decision rather than an oversight:
        // the very same run, under `sendra run`, still exits 3. The raw-status
        // question has a command that answers it; `test` declining to answer it
        // a second time loses nothing.
        assert_eq!(exit_for_run(&outcomes, false), Exit::ErrorStatus);
    }

    #[test]
    fn an_asserted_bad_status_behaves_exactly_as_written() {
        // The corollary of the rule above: the status is not ignored, it is
        // only ever read through an assertion. Asserting `status: 404` and
        // getting one is a pass; asserting `status: 200` and getting a 404 is
        // a failure. Both under the same command that shrugs at an unasserted
        // 404.
        assert_eq!(Summary::of(&[all_passed(404)]).exit(), Exit::Ok);

        let wrong =
            assertions_from("method: GET\nurl: https://example.com\nassertions:\n  status: 200\n")
                .evaluate(&response(404));
        assert!(!wrong.passed());
        assert_eq!(Summary::of(&[checked(404, wrong)]).exit(), Exit::TestFailed);
    }

    #[test]
    fn a_failed_assertion_on_a_perfectly_good_status_still_fails_the_run() {
        // The other half of "status is not the input": a 200 does not rescue a
        // check that did not hold.
        let outcomes = vec![some_failed(200)];

        assert_eq!(
            Summary::of(&outcomes),
            Summary {
                total: 1,
                failed: 1,
                ..Summary::default()
            }
        );
        assert_eq!(Summary::of(&outcomes).exit(), Exit::TestFailed);
    }

    #[test]
    fn passing_assertions_on_an_error_status_pass_the_test_run() {
        // A request that expects a 500 and gets one has met its expectations.
        // `sendra run` would still exit 3 on the same response, and that is the
        // difference between the two commands stated as a test rather than as a
        // paragraph.
        let outcomes = vec![all_passed(500)];

        assert_eq!(Summary::of(&outcomes).exit(), Exit::Ok);
        assert_eq!(exit_for_run(&outcomes, false), Exit::ErrorStatus);
    }

    #[test]
    fn a_request_that_never_got_a_response_fails_the_run() {
        // No response means no assertions could be evaluated, so the run cannot
        // claim its expectations held — whatever the requests around it did.
        let outcomes = vec![all_passed(200), Outcome::NoResponse, all_passed(200)];
        let summary = Summary::of(&outcomes);

        assert_eq!(
            summary,
            Summary {
                total: 3,
                passed: 2,
                no_response: 1,
                ..Summary::default()
            }
        );
        assert_eq!(summary.exit(), Exit::Failure);
        assert_eq!(Exit::Failure as u8, 1);
    }

    #[test]
    fn never_got_a_response_outranks_a_failed_assertion() {
        // Both are failures and both are non-zero; the code says which kind,
        // and "the tool could not do its job" is the more serious of the two —
        // the same ranking `run` uses.
        let outcomes = vec![some_failed(200), Outcome::NoResponse];
        assert_eq!(Summary::of(&outcomes).exit(), Exit::Failure);

        // And it does not depend on which came first.
        let outcomes = vec![Outcome::NoResponse, some_failed(200)];
        assert_eq!(Summary::of(&outcomes).exit(), Exit::Failure);
    }

    #[test]
    fn every_outcome_lands_in_exactly_one_category() {
        // The four counts are a partition, not four overlapping questions, so
        // the printed line always adds up.
        let outcomes = vec![
            all_passed(200),
            some_failed(200),
            responded(200),
            responded(404),
            Outcome::NoResponse,
            all_passed(500),
        ];
        let summary = Summary::of(&outcomes);

        assert_eq!(summary.total, outcomes.len());
        assert_eq!(
            summary.passed + summary.failed + summary.without_assertions + summary.no_response,
            summary.total,
            "the categories must partition the run: {summary:?}"
        );
    }

    #[test]
    fn the_summary_does_not_depend_on_the_order_of_the_requests() {
        // Same reasoning as `worst`: reordering a collection must not change
        // whether a script proceeds.
        let forwards = Summary::of(&[all_passed(200), some_failed(200), responded(200)]);
        let backwards = Summary::of(&[responded(200), some_failed(200), all_passed(200)]);

        assert_eq!(forwards, backwards);
        assert_eq!(forwards.exit(), backwards.exit());
    }

    #[test]
    fn a_test_run_never_returns_the_code_that_belongs_to_run() {
        // `3` is `run`'s answer to a question `test` does not ask. Over every
        // shape of summary the classifier can produce, `test` returns one of
        // exactly three codes.
        for passed in 0..2 {
            for failed in 0..2 {
                for without_assertions in 0..2 {
                    for no_response in 0..2 {
                        let summary = Summary {
                            total: passed + failed + without_assertions + no_response,
                            passed,
                            failed,
                            without_assertions,
                            no_response,
                        };

                        assert!(
                            matches!(summary.exit(), Exit::Ok | Exit::TestFailed | Exit::Failure),
                            "{summary:?} produced {:?}",
                            summary.exit()
                        );
                    }
                }
            }
        }
    }
}
