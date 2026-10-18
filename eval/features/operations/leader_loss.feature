@axis_operations @claim_self_healing
Feature: Council leader loss
  As an operator of a three-node cluster
  I want the council to elect a new leader and accept writes again quickly
  So that losing the leader machine does not block deploys

  @req_OPS-006 @claim_leader_election_and_learning_period_targets @claim_council_raft_quorum_and_self_healing @destructive
  Scenario: Writes resume after the council leader loses power
    Given a healthy disposable 3-node Reliaburger cluster with sufficient spare capacity
    And the native fixture application is running
    And an external probe recording availability once per second
    When I cut the power to the council leader
    Then a new leader should be elected within the configured election target
    And a newly applied app should be committed and running within the configured write target
    And the fixture should keep serving through the surviving ingresses
