@axis_operations @claim_self_healing @smoke
Feature: Recovery after abrupt node loss
  As an operator of a small production-like cluster
  I want desired workloads to recover after a node disappears
  So that a single machine failure does not require manual repair

  @req_OPS-001 @claim_node_loss_reschedules_in_about_a_minute @destructive
  Scenario Outline: Recover replicas after a <victim> node is forcibly stopped
    Given a healthy disposable 3-node Reliaburger cluster with sufficient spare capacity
    And a 3-replica fixture application distributed across at least 2 nodes
    And an external probe recording availability once per second
    When I forcibly power off a <victim> node hosting at least 1 fixture replica
    Then the cluster should detect the missing node within the configured detection timeout
    And the desired healthy replica count should be restored within the configured recovery timeout
    And the application should satisfy the configured availability threshold during recovery
    And the evidence should contain replica placements, probe results, and cluster state transitions

    Examples:
      | case     | victim          |
      | worker   | non-leader      |
      | leader   | council-leader  |
