@axis_operations @claim_self_healing
Feature: Network partition
  As an operator of a three-node cluster
  I want a cut-off node to stop accepting writes while the majority carries on
  So that a partition can never split the cluster's state

  @req_OPS-007 @claim_partitions_no_split_brain_and_degradation @destructive
  Scenario: One non-leader node is partitioned from its peers
    Given a healthy disposable 3-node Reliaburger cluster with sufficient spare capacity
    And the native fixture application is running
    When I partition one non-leader node from the other two for the configured isolation time
    Then the majority side should commit and run a newly applied app
    And the minority node should refuse to commit an apply sent to it
    And the minority node should keep serving the fixture through its own ingress
    And after the partition heals the node should rejoin within the configured rejoin time
    And the cluster should have exactly one leader and no app that only the minority accepted
