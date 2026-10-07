# Portable fixture timeout evidence

Date: 4 October 2026. PR: #266. Cause: unconfirmed.

An earlier portable macOS nextest run completed 5,449 cases in 184.849 seconds:
5,436 passed, 11 failed and two timed out. Formatting and both Clippy
configurations passed. A serial retry of all thirteen unsuccessful cases
passed eleven in 27.543 seconds, without source changes. The remaining two
quickstart console-watchdog failures are already tracked in #517. The
managed-status failure and serial pass are tracked in #573.

Ten newly observed transient cases require investigation:

- Six `relish::quickstart::lifecycle::tests` cases: `a_stopped_node_is_reported_rather_than_stopped_again`, `starting_one_node_boots_only_that_vm_and_its_service`, `stopping_a_second_node_needs_yes_because_quorum_goes_with_it`, `stopping_node_one_needs_yes_because_it_carries_the_host_forwards`, `stopping_one_node_stops_only_that_vm`, `stopping_the_whole_cluster_needs_no_confirmation`. Their mock subprocess deadlines expired after about five seconds.
- `process_recovery::cancellation_of_start_preserves_the_owned_launch_transaction`: its 15-second fixture deadline expired.
- `app_metrics::relish_metrics_shows_samples_scraped_from_each_instance`: no scraped request rate within 30 seconds; the agent could not reach the fixture service.
- `dependency_audit::audit_refuses_an_active_or_uninspectable_rkyv_dependency`: nextest's 60-second timeout.
- `dev_robustness::checkout_and_filter_shell_metacharacters_remain_literal_arguments`: nextest's 60-second timeout.

Other compilation was running concurrently. The underlying delay is unconfirmed;
do not assume CPU load or simply lengthen deadlines. Investigate executable
startup, scheduling, subprocess cleanup and fixtures. These observations do
not establish a batch-resource regression. Retain the failed full-run evidence
alongside the serial passes.

Tracked in [#582](https://github.com/reliaburger/reliaburger/issues/582), published
with the maintainer's approval on 4 October 2026.
