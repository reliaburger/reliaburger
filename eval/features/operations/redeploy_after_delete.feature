@axis_operations @claim_self_healing
Feature: Re-applying an app after deleting it
  As an operator iterating on an application
  I want relish apply to converge again after relish delete
  So that a routine delete and redeploy never needs a node restart

  @req_OPS-002 @claim_apply_converges_after_delete
  Scenario Outline: Delete and re-apply the fixture <timing>
    Given a healthy disposable Reliaburger cluster
    And the fixture application is running with 2 backend and 3 frontend replicas
    When I delete and re-apply the fixture for the configured number of cycles, <timing>
    Then every cycle should reach the desired replicas within the configured convergence timeout
    And no stopped fixture instances should remain after the last cycle
    And the evidence should record each cycle's timings and the node logs

    Examples:
      | case      | timing                                          |
      | settled   | after relish status shows no instances          |
      | immediate | immediately after relish delete returns         |
