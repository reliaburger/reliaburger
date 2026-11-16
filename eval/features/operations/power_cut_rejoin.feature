@axis_operations @claim_self_healing
Feature: Power cut under write load
  As an operator of machines without a UPS
  I want a node that loses power during writes to rejoin on its own
  So that a power cut never needs a manual store repair

  @req_OPS-008 @claim_council_raft_quorum_and_self_healing @destructive
  Scenario: A council voter loses power repeatedly while the cluster takes writes
    Given a healthy disposable 3-node Reliaburger cluster with sufficient spare capacity
    When I cut the power to one non-leader voter under write load for the configured number of cycles
    Then the voter should rejoin after every cycle within the configured rejoin time
    And its journal should show no store corruption or refused start
