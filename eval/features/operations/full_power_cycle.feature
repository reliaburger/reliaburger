@axis_operations @claim_self_healing
Feature: Whole-cluster power loss
  As an operator of a small site that can lose power entirely
  I want the cluster to come back by itself when power returns
  So that a site outage does not need a rebuild

  @req_OPS-009 @claim_council_raft_quorum_and_self_healing @destructive
  Scenario: Every node loses power at once and is powered on again
    Given a healthy disposable 3-node Reliaburger cluster with sufficient spare capacity
    And the native fixture application is running
    When I cut the power to every node at once and power them all on again
    Then the council should be healthy within the configured recovery time
    And the fixture should serve on every ingress within the configured recovery time
    And the applied apps should still be defined
