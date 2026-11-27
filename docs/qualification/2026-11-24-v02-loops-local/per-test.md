<!-- v02-loops-summary.md from the 23-24 Nov local run, kept as the summariser wrote it. -->

# V02 loop lanes

Candidate: `00e4d204bccbc5173b7afe61be67787b7cdda4de`

| Lane | Test | Status | Runs | Failures | 95% failure-rate bound | Note |
|---|---|---|---|---|---|---|
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::a_controller_fails_a_templated_database_pair_over_when_its_node_dies` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::a_follower_that_has_not_applied_a_proposal_passes_each_decision_to_the_leader` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::a_leader_killed_during_an_apply_under_policy_leaves_the_policy_enforced` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::a_lease_history_through_a_leader_kill_is_linearizable` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::a_lease_history_through_a_leader_partition_is_linearizable` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::a_proposal_applied_before_a_leader_change_is_undone_on_the_new_leader` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::a_voter_that_shut_down_cleanly_stays_listed_as_left` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::an_admin_removes_a_policy_that_denies_everything_through_any_node` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::an_agent_lease_survives_a_leader_change_and_the_new_leader_expires_it` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::decommissioned_worker_releases_cleanup_and_stays_retired_after_leader_change` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::eight_plus_node_cluster_reconciles_and_reports_through_leader_failover` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::invalid_ingress_on_one_app_leaves_discovery_working_cluster_wide` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::lease_retirement_waits_for_paused_worker_across_leader_change` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::one_replica_rollout_keeps_a_healthy_backend_on_every_node` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::service_on_one_node_resolves_and_survives_leader_change_from_another` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::the_dashboard_clears_a_fault_another_node_holds` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::the_dashboard_decides_a_proposal_on_the_leader_only` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::the_platform_policies_hold_through_apply_gitops_and_a_leader_kill` | pass | 20 | 0 | < 15.00% |  |
| cluster_failover (linux-arm64-lima) | `reliaburger::cluster_failover::workers_outside_raft_keep_the_leader_after_many_elections` | pass | 20 | 0 | < 15.00% |  |
| council_disaster_recovery (linux-arm64-lima) | `reliaburger::council_disaster_recovery::disk_pressure_advertised_over_gossip_drives_resignation` | pass | 93 | 0 | < 3.23% |  |
| council_disaster_recovery (linux-arm64-lima) | `reliaburger::council_disaster_recovery::full_council_loss_recovers_from_backup` | pass | 93 | 0 | < 3.23% |  |
| council_disaster_recovery (linux-arm64-lima) | `reliaburger::council_disaster_recovery::leadership_moves_off_the_leader_every_time_under_write_load` | pass | 93 | 0 | < 3.23% |  |
| council_disaster_recovery (linux-arm64-lima) | `reliaburger::council_disaster_recovery::old_voters_returning_after_recovery_cannot_form_a_quorum` | pass | 93 | 0 | < 3.23% |  |
| council_disaster_recovery (linux-arm64-lima) | `reliaburger::council_disaster_recovery::pressured_leader_resigns_and_leadership_moves` | pass | 93 | 0 | < 3.23% |  |
| council_disaster_recovery (linux-arm64-lima) | `reliaburger::council_disaster_recovery::recovered_council_regrows_onto_a_fresh_node` | pass | 93 | 0 | < 3.23% |  |
| council_disaster_recovery (linux-arm64-lima) | `reliaburger::council_disaster_recovery::recovering_two_survivors_independently_fences_one` | pass | 93 | 0 | < 3.23% |  |
| council_disaster_recovery (linux-arm64-lima) | `reliaburger::council_disaster_recovery::slow_gossip_restart_never_acknowledges_on_the_old_epoch` | pass | 93 | 0 | < 3.23% |  |
| council_disaster_recovery (linux-arm64-lima) | `reliaburger::council_disaster_recovery::trigger_elect_advances_the_term_and_leadership_settles` | pass | 93 | 0 | < 3.23% |  |
| export-lock-checkpoint (linux-arm64-lima) | `reliaburger::suite::log_export::active_export_lock_refuses_another_writer_without_uploading` | pass | 200 | 0 | < 1.50% |  |
| export-lock-checkpoint (linux-arm64-lima) | `reliaburger::suite::log_export::export_persists_and_reloads_the_authoritative_checkpoint` | pass | 200 | 0 | < 1.50% |  |
| registry-lease-expiry (linux-arm64-lima) | `reliaburger::registry_recovery::expired_registry_lease_recovers_after_bun_sigkill_and_preserves_shared_content` | pass | 200 | 0 | < 1.50% |  |
| registry-new-leader (linux-arm64-lima) | `reliaburger::registry_recovery::new_leader_retains_registry_cleanup_until_the_killed_writer_returns` | pass | 200 | 0 | < 1.50% |  |
| test-storage-owner-death (linux-arm64-lima) | `reliaburger::test_storage::lease_loop_storage_survives_owner_process_death` | pass | 200 | 0 | < 1.50% |  |
| test-upgrade-cluster (linux-arm64-lima) | `reliaburger::self_upgrade_cluster::a_registry_outage_at_directive_time_does_not_pause_the_upgrade` | pass | 43 | 0 | < 6.98% |  |
| test-upgrade-cluster (linux-arm64-lima) | `reliaburger::self_upgrade_cluster::cluster_rollback_returns_every_node_to_previous_version` | pass | 43 | 0 | < 6.98% |  |
| test-upgrade-cluster (linux-arm64-lima) | `reliaburger::self_upgrade_cluster::paused_upgrade_can_be_aborted_or_replaced_by_a_rollback` | pass | 43 | 0 | < 6.98% |  |
| test-upgrade-cluster (linux-arm64-lima) | `reliaburger::self_upgrade_cluster::relish_pushes_through_a_forward_while_nodes_fetch_from_the_cluster_address` | pass | 43 | 0 | < 6.98% |  |
| test-upgrade-cluster (linux-arm64-lima) | `reliaburger::self_upgrade_cluster::rollback_to_a_version_the_nodes_lack_is_refused_before_recording_a_run` | pass | 43 | 0 | < 6.98% |  |
| test-upgrade-cluster (linux-arm64-lima) | `reliaburger::self_upgrade_cluster::rolling_upgrade_walks_workers_council_then_leader` | pass | 43 | 0 | < 6.98% |  |
| test-upgrade-cluster (linux-arm64-lima) | `reliaburger::self_upgrade_cluster::start_refuses_a_candidate_with_other_formats_before_recording_a_run` | pass | 43 | 0 | < 6.98% |  |
| test-upgrade-cluster (linux-arm64-lima) | `reliaburger::self_upgrade_cluster::start_refuses_same_version_other_bytes_and_unrequested_downgrades` | pass | 43 | 0 | < 6.98% |  |
| test-upgrade-cluster (linux-arm64-lima) | `reliaburger::self_upgrade_cluster::start_refuses_when_a_node_cannot_verify_network_upgrades` | pass | 43 | 0 | < 6.98% |  |
| test-upgrade-cluster (linux-arm64-lima) | `reliaburger::self_upgrade_cluster::upgrade_failure_pauses_cluster_and_reverts_node` | pass | 43 | 0 | < 6.98% |  |
| test-upgrade-cluster (linux-arm64-lima) | `reliaburger::self_upgrade_cluster::version_form_rolls_the_nodes_platform_build_countersigned_by_relish` | pass | 43 | 0 | < 6.98% |  |
| test-upgrade-node (linux-arm64-lima) | `reliaburger::self_upgrade::failed_upgrade_triggers_automatic_rollback` | pass | 175 | 0 | < 1.71% |  |
| test-upgrade-node (linux-arm64-lima) | `reliaburger::self_upgrade::isolated_replacement_reverts_without_restarting_its_workload` | pass | 175 | 0 | < 1.71% |  |
| test-upgrade-node (linux-arm64-lima) | `reliaburger::self_upgrade::same_version_upgrade_never_swaps_silently` | pass | 175 | 0 | < 1.71% |  |
| test-upgrade-node (linux-arm64-lima) | `reliaburger::self_upgrade::single_node_rollback_reverts_to_previous_version` | pass | 175 | 0 | < 1.71% |  |
| test-upgrade-node (linux-arm64-lima) | `reliaburger::self_upgrade::single_node_upgrade_preserves_running_containers` | fail | 175 | 4 | FAILED (4/175) | 4 of 175 runs failed |
| test-upgrade-node (linux-arm64-lima) | `reliaburger::self_upgrade::upgrade_rejects_bad_external_signature` | pass | 175 | 0 | < 1.71% |  |
| test-upgrade-node (linux-arm64-lima) | `reliaburger::self_upgrade::version_retention_gc_keeps_last_three` | pass | 175 | 0 | < 1.71% |  |
| export-lock-checkpoint (macos-studio) | `reliaburger::suite::log_export::active_export_lock_refuses_another_writer_without_uploading` | pass | 200 | 0 | < 1.50% |  |
| export-lock-checkpoint (macos-studio) | `reliaburger::suite::log_export::export_persists_and_reloads_the_authoritative_checkpoint` | pass | 200 | 0 | < 1.50% |  |
| registry-lease-expiry (macos-studio) | `reliaburger::registry_recovery::expired_registry_lease_recovers_after_bun_sigkill_and_preserves_shared_content` | pass | 200 | 0 | < 1.50% |  |
| registry-new-leader (macos-studio) | `reliaburger::registry_recovery::new_leader_retains_registry_cleanup_until_the_killed_writer_returns` | fail | 200 | 1 | FAILED (1/200) | 1 of 200 runs failed |
| test-upgrade-cluster (macos-studio) | `reliaburger::self_upgrade_cluster::a_registry_outage_at_directive_time_does_not_pause_the_upgrade` | pass | 34 | 0 | < 8.82% |  |
| test-upgrade-cluster (macos-studio) | `reliaburger::self_upgrade_cluster::cluster_rollback_returns_every_node_to_previous_version` | pass | 34 | 0 | < 8.82% |  |
| test-upgrade-cluster (macos-studio) | `reliaburger::self_upgrade_cluster::paused_upgrade_can_be_aborted_or_replaced_by_a_rollback` | fail | 34 | 1 | FAILED (1/34) | 1 of 34 runs failed |
| test-upgrade-cluster (macos-studio) | `reliaburger::self_upgrade_cluster::relish_pushes_through_a_forward_while_nodes_fetch_from_the_cluster_address` | pass | 34 | 0 | < 8.82% |  |
| test-upgrade-cluster (macos-studio) | `reliaburger::self_upgrade_cluster::rollback_to_a_version_the_nodes_lack_is_refused_before_recording_a_run` | pass | 34 | 0 | < 8.82% |  |
| test-upgrade-cluster (macos-studio) | `reliaburger::self_upgrade_cluster::rolling_upgrade_walks_workers_council_then_leader` | pass | 34 | 0 | < 8.82% |  |
| test-upgrade-cluster (macos-studio) | `reliaburger::self_upgrade_cluster::start_refuses_a_candidate_with_other_formats_before_recording_a_run` | pass | 34 | 0 | < 8.82% |  |
| test-upgrade-cluster (macos-studio) | `reliaburger::self_upgrade_cluster::start_refuses_same_version_other_bytes_and_unrequested_downgrades` | pass | 34 | 0 | < 8.82% |  |
| test-upgrade-cluster (macos-studio) | `reliaburger::self_upgrade_cluster::start_refuses_when_a_node_cannot_verify_network_upgrades` | pass | 34 | 0 | < 8.82% |  |
| test-upgrade-cluster (macos-studio) | `reliaburger::self_upgrade_cluster::upgrade_failure_pauses_cluster_and_reverts_node` | pass | 34 | 0 | < 8.82% |  |
| test-upgrade-cluster (macos-studio) | `reliaburger::self_upgrade_cluster::version_form_rolls_the_nodes_platform_build_countersigned_by_relish` | pass | 34 | 0 | < 8.82% |  |
| test-upgrade-node (macos-studio) | `reliaburger::self_upgrade::failed_upgrade_triggers_automatic_rollback` | pass | 142 | 0 | < 2.11% |  |
| test-upgrade-node (macos-studio) | `reliaburger::self_upgrade::isolated_replacement_reverts_without_restarting_its_workload` | fail | 142 | 76 | FAILED (76/142) | 76 of 142 runs failed |
| test-upgrade-node (macos-studio) | `reliaburger::self_upgrade::same_version_upgrade_never_swaps_silently` | pass | 142 | 0 | < 2.11% |  |
| test-upgrade-node (macos-studio) | `reliaburger::self_upgrade::single_node_rollback_reverts_to_previous_version` | pass | 142 | 0 | < 2.11% |  |
| test-upgrade-node (macos-studio) | `reliaburger::self_upgrade::single_node_upgrade_preserves_running_containers` | pass | 142 | 0 | < 2.11% |  |
| test-upgrade-node (macos-studio) | `reliaburger::self_upgrade::upgrade_rejects_bad_external_signature` | pass | 142 | 0 | < 2.11% |  |
| test-upgrade-node (macos-studio) | `reliaburger::self_upgrade::version_retention_gc_keeps_last_three` | pass | 142 | 0 | < 2.11% |  |
| test-upgrade-cluster (linux-x86-64) | (lane skipped) | skip | 0 | 0 | no runs | skipped by the manifest: the Studio has no x86-64 host; v02-loops.yml's ubuntu-latest lane runs it |
| test-upgrade-node (linux-x86-64) | (lane skipped) | skip | 0 | 0 | no runs | skipped by the manifest: the Studio has no x86-64 host; v02-loops.yml's ubuntu-latest lane runs it |
| council_disaster_recovery (linux-x86-64) | (lane skipped) | skip | 0 | 0 | no runs | skipped by the manifest: the Studio has no x86-64 host; v02-loops.yml's ubuntu-latest lane runs it |
| cluster_failover (linux-x86-64) | (lane skipped) | skip | 0 | 0 | no runs | skipped by the manifest: the Studio has no x86-64 host; v02-loops.yml's ubuntu-latest lane runs it |
| registry-lease-expiry (linux-x86-64) | (lane skipped) | skip | 0 | 0 | no runs | skipped by the manifest: the Studio has no x86-64 host; v02-loops.yml's ubuntu-latest lane runs it |
| registry-new-leader (linux-x86-64) | (lane skipped) | skip | 0 | 0 | no runs | skipped by the manifest: the Studio has no x86-64 host; v02-loops.yml's ubuntu-latest lane runs it |
| export-lock-checkpoint (linux-x86-64) | (lane skipped) | skip | 0 | 0 | no runs | skipped by the manifest: the Studio has no x86-64 host; v02-loops.yml's ubuntu-latest lane runs it |
| test-storage-owner-death (linux-x86-64) | (lane skipped) | skip | 0 | 0 | no runs | skipped by the manifest: the Studio has no x86-64 host; v02-loops.yml's ubuntu-latest lane runs it |

Verdict: **FAIL**. 8 lanes skipped by the manifest. The bound is the rule of three: 0 failures in n runs means a failure rate below 3/n at 95% confidence.
