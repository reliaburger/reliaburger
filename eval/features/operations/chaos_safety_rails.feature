@axis_operations @claim_integrated_platform
Feature: Chaos safety rails
  As an operator running fault drills on a live cluster
  I want the fault injector to refuse faults that would cause an outage
  So that a drill cannot take down every replica or the council

  @req_OPS-005 @claim_chaos_safety_policy
  Scenario Outline: relish fault <attempt> as <role>
    Given a healthy disposable Reliaburger cluster
    And the native fixture application is running
    When I request the fault "<attempt>" with a <role> token
    Then the fault injector should <verdict> it
    And the fixture should be back to 3 running frontend replicas within the configured recovery time
    And an accepted fault should appear in the audit log

    Examples:
      | case             | attempt                                     | role      | verdict |
      | kill-one         | kill frontend --count 1 --acknowledge       | admin     | accept  |
      | kill-all         | kill frontend --count 3 --acknowledge       | admin     | refuse  |
      | no-ack           | kill frontend --count 1                     | admin     | refuse  |
      | pause-all        | pause frontend --acknowledge                | admin     | refuse  |
      | readonly-kill    | kill frontend --count 1 --acknowledge       | read-only | refuse  |
      | leader-node-kill | node-kill LEADER --acknowledge              | admin     | refuse  |
